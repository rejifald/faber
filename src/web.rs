//! The control panel — a self-contained dashboard the daemon serves on
//! `127.0.0.1` only (never exposed to the network), and the control API behind
//! it, served on the control socket as well. Who may call what is
//! [`crate::control`]'s: both listeners sit behind its front door. It shows live in-flight
//! runners, recent gate decisions, and tallies, offers pause / resume /
//! force-reconcile / hold controls, and links to Settings, the React page at
//! `/settings` that edits the whole config file (as a form, or as text). The
//! page is a single embedded HTML file that polls `GET /api/state`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::{Deserialize, Deserializer, Serialize};

use crate::attention::AttentionStore;
use crate::audit::TrustChanges;
use crate::config::{FileConfig, HardCeiling, Hold, Overrides};
use crate::control::{Caller, Class, FrontDoor, Verdict};
use crate::docker::{Limits, ResourceLimits};
use crate::limiter::{LimiterView, Pause};
use crate::reconcile::{Drain, Notice, Orchestrator, StateView};
use crate::settings::{FormSchema, SettingsStore, FORM_SCHEMA};
use crate::ui_bundle;
use crate::update::UpdateHandle;

/// Everything the panel's handlers reach for: the live orchestrator, and the
/// config file behind the Settings page.
#[derive(Clone)]
struct Web {
    orch: Arc<Orchestrator>,
    settings: Arc<SettingsStore>,
    /// The autoupdate task's control surface. The orchestrator deliberately
    /// knows nothing about updates, so this layer is where the two meet: it
    /// stitches the updater's view into the state snapshot and turns the panel's
    /// buttons into calls on the handle.
    update: Arc<UpdateHandle>,
    /// This boot's standing conditions (ADR-0025), decided once by `main` and
    /// stitched into every state snapshot beside `update`.
    notices: Arc<Vec<Notice>>,
    /// Every trust-widening change allowed in the last 7 days (ADR-0028
    /// addendum B): seeded by `main` from the audit log, appended to by
    /// [`audit_control`], stitched into every state snapshot. No route clears it.
    trust_changes: Arc<TrustChanges>,
    /// The operator's dismissed attention rows (ADR
    /// a-warning-that-does-not-stop-jobs-can-be-dismissed, ADR-0064 §3): opaque to
    /// the daemon, stitched into every state snapshot, persisted by the store.
    attention: Arc<AttentionStore>,
    /// The port we're serving on — read-only on the settings page, since it is a
    /// CLI flag (changing it here would cut the page off from the daemon).
    port: u16,
    /// Held by a settings save from reading the file it measures widenings
    /// against until its write lands, so two saves cannot interleave compare and
    /// write (one judged against a file the other is replacing).
    save_lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether a settings save rewrites `overrides.json` (always, but in
    /// tests, which must never touch the real file).
    write_overrides: bool,
}

/// Serve the dashboard on `127.0.0.1:<port>`, and the control API on the
/// control socket, through `door`, until the process exits.
pub async fn serve(
    orch: Arc<Orchestrator>,
    door: Arc<FrontDoor>,
    settings: Arc<SettingsStore>,
    update: Arc<UpdateHandle>,
    notices: Vec<Notice>,
    trust_changes: Arc<TrustChanges>,
    attention: Arc<AttentionStore>,
) -> anyhow::Result<()> {
    let port = door.port();
    let app = routes(Web {
        orch,
        settings,
        update,
        notices: Arc::new(notices),
        trust_changes,
        attention,
        port,
        save_lock: Arc::default(),
        write_overrides: true,
    });

    let serving = door.start(app).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot serve the control panel on 127.0.0.1:{port} and {}: {e}",
            socket_name(&door)
        )
    })?;
    tracing::info!(
        "dashboard on http://{}, control API on {}",
        serving.addr,
        socket_name(&door)
    );
    serving.wait().await?;
    Ok(())
}

/// The daemon's routes. Who may reach them is the front door's.
fn routes(web: Web) -> Router {
    let views = crate::control::NAVIGATED
        .iter()
        .fold(Router::new(), |r, view| r.route(view, get(shell)));
    views
        .route("/assets/*name", get(bundle_asset))
        .route("/api/state", get(state))
        .route("/api/jobs", get(jobs))
        .route("/api/runs/:runner", get(run))
        .route("/api/settings", get(get_settings).post(save_settings_route))
        .route("/api/settings/key", post(save_settings_key))
        .route(
            "/api/settings/raw",
            get(get_settings_raw).post(save_settings_raw),
        )
        .route("/api/pause", post(pause))
        .route("/api/drain", post(drain))
        .route("/api/golden/verify", post(golden_verify))
        .route("/api/engine/grow", post(engine_grow))
        .route("/api/resume", post(resume))
        .route("/api/reconcile", post(reconcile_now))
        .route("/api/config", post(set_config))
        .route("/api/hold", post(hold))
        .route("/api/unhold", post(unhold))
        .route("/api/repo-hold/clear", post(clear_repo_hold))
        .route("/api/pin", post(set_pin))
        .route("/api/attention/dismiss", post(attention_dismiss))
        .route("/api/attention/restore", post(attention_restore))
        .route("/api/attention/ack-trust", post(attention_ack_trust))
        .route("/api/update/check", post(update_check))
        .route("/api/update/apply", post(update_apply))
        .route("/api/update/skip", post(update_skip))
        .with_state(web)
}

/// The control socket, as a log line names it.
pub(crate) fn socket_name(door: &FrontDoor) -> String {
    door.socket().map_or_else(
        || "no socket (no state directory)".to_string(),
        |p| p.display().to_string(),
    )
}

/// How many times [`bind_listener`] tries a port that is still in use, and how
/// long it waits between tries: two seconds in all.
pub(crate) const BIND_ATTEMPTS: u32 = 20;
pub(crate) const BIND_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// Bind `addr` for the control panel, waiting out a port that is still in use
/// for a moment.
///
/// The panel's port is bound straight after it was let go: by the daemon once
/// standby's stub stops ([`crate::standby`]), and by a restarted daemon once
/// the old one exits. A listener that was just closed can outlive the close,
/// though. A child process being spawned at that moment holds a copy of every
/// descriptor of its parent until it calls `exec`, so the socket stays open,
/// and its port bound, until then. faber spawns `docker` and `df` from
/// tasks of their own, so this happens in the daemon too (issue #47 is the same
/// mechanism, holding a file lock). Measured in the test binary: the port stays
/// taken for 20 µs to 2 ms. So `AddrInUse` is tried again, up to `attempts`
/// times, `pause` apart. Any other error, and a port still taken after that,
/// is returned for the caller to report: a daemon that comes out of standby
/// without its control panel is dark to the tray, `faber drain` and
/// `faber status`, so the failure must be loud, not silent.
pub(crate) async fn bind_listener(
    addr: SocketAddr,
    attempts: u32,
    pause: std::time::Duration,
) -> std::io::Result<tokio::net::TcpListener> {
    let mut tried = 1;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tried < attempts => {
                tried += 1;
                tokio::time::sleep(pause).await;
            }
            bound => return bound,
        }
    }
}

/// `GET` on each of the dashboard's views (`/`, `/activity`, `/jobs`, `/memory`,
/// `/maintenance`, `/settings`): the React dashboard's HTML shell, behind the session cookie
/// (ADR-0028 addendum C, as amended 2026-10-22 and 2026-10-28; ADR-0054 § 5).
/// The shell picks the view from the path. An unbuilt binary answers with
/// [`ui_bundle::NOT_BUILT_HTML`] instead.
///
/// The shell's asset URLs are relative (`./assets/…`), so it is served at each
/// view's exact path only, never under `/settings/`, where they would resolve
/// to a path no route serves.
async fn shell() -> Response {
    let body = ui_bundle::FILES
        .iter()
        .find(|(name, _)| *name == "index.html")
        .map_or(ui_bundle::NOT_BUILT_HTML.as_bytes(), |(_, bytes)| *bytes);
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ui_bundle::content_type("index.html")),
        )],
        body,
    )
        .into_response()
}

/// `GET`/`HEAD /assets/*`: one content-hashed file of the React bundle, served
/// without a session (ADR-0028 addendum C, decision 2).
///
/// The raw request path minus its leading `/` is looked up whole as a key of
/// [`ui_bundle::FILES`], the same path the front door exempted. It is never
/// decoded or normalised, never goes through [`ui_bundle::asset`] (whose index
/// mapping would answer the shell), and nothing is read from disk: anything that
/// is not exactly a bundled `assets/<name>` is `404`. The handler takes no
/// daemon state, so no data can reach it.
async fn bundle_asset(uri: Uri) -> Response {
    let key = uri.path().strip_prefix('/').unwrap_or_default();
    let Some((name, bytes)) = ui_bundle::FILES
        .iter()
        .find(|(name, _)| key.starts_with("assets/") && *name == key)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(ui_bundle::content_type(name)),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        *bytes,
    )
        .into_response()
}

/// The state snapshot, with the updater's view, this boot's notices and the
/// week's trust changes stitched in.
///
/// [`Orchestrator::view`] leaves `update` at its default because the
/// orchestrator has no updater; filling it here is what keeps the whole feature
/// out of `reconcile.rs`. The same goes for `notices`, which `main` decides at
/// boot.
async fn state(State(w): State<Web>) -> Json<StateView> {
    Json(with_update(&w).await)
}

async fn with_update(w: &Web) -> StateView {
    let mut view = w.orch.view().await;
    view.update = w.update.view();
    view.notices = w.notices.as_ref().clone();
    // ADR-0051: live, unlike the boot's: gone once the switched-off VM stops.
    if let Some(text) = w.orch.limiter().linux_vm_waiting() {
        view.notices.push(Notice {
            kind: "linux_vm_waiting".to_string(),
            text,
        });
    }
    view.trust_changes = w.trust_changes.list();
    view.attention = w.attention.view();
    view.cli_copy = crate::upgrade_cli::cli_copy_view().await;
    view
}

#[derive(Debug, Deserialize)]
struct JobsQuery {
    /// The window in days: 7 when absent, at most 90 (ADR-0056 §5).
    days: Option<u64>,
}

/// `GET /api/jobs?days=N`: the Jobs view, every job class's runs in the window
/// (ADR-0056 §5), read from the audit log's `outcome` lines.
async fn jobs(State(w): State<Web>, Query(q): Query<JobsQuery>) -> Response {
    let audit = w.orch.audit_log().clone();
    let days = crate::jobs_view::window_days(q.days);
    let now = crate::audit::now_ms();
    let read = tokio::task::spawn_blocking(move || {
        let runs = crate::jobs_view::read_runs(&audit, now.saturating_sub(days * 86_400_000));
        crate::jobs_view::jobs_view(&runs, days, now)
    })
    .await;
    match read {
        Ok(view) => Json(view).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reading the audit log: {e}"),
        )
            .into_response(),
    }
}

/// `GET /api/runs/:runner`: one run's phases, steps and resources, for
/// Activity's row detail (ADR-0056 §5). `404` when no `outcome` line names that
/// runner within the widest window.
async fn run(State(w): State<Web>, Path(runner): Path<String>) -> Response {
    let audit = w.orch.audit_log().clone();
    let now = crate::audit::now_ms();
    let read = tokio::task::spawn_blocking(move || {
        let from = now.saturating_sub(crate::jobs_view::MAX_WINDOW_DAYS * 86_400_000);
        crate::jobs_view::run_view(&crate::jobs_view::read_runs(&audit, from), &runner)
    })
    .await;
    match read {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no recorded run by that runner").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reading the audit log: {e}"),
        )
            .into_response(),
    }
}

/// Pause: nothing new is claimed or started until resumed — running jobs finish,
/// and parked claims stay parked and say they are paused (ADR-0023). Not
/// persisted, unlike a hold: a restart comes back unpaused.
async fn pause(State(w): State<Web>) -> impl IntoResponse {
    w.orch.pause(Pause::Operator);
    "paused"
}

/// Stand down for good — ADR-0011 §5's decommission path.
///
/// Distinct from `pause`, which is reversible and keeps the process ready to
/// resume. This is what an operator runs before `launchctl bootout`, because
/// SIGTERM now means "I'm coming back" and a bare bootout would therefore leave
/// running containers with no lifetime cap and nobody to tear them down.
///
/// Returns immediately with the count still to finish; `faber drain` polls
/// `/api/state` for the rest. Blocking the request until the last runner exits
/// would hold a connection open for as long as the longest job — up to the 3h
/// lifetime cap — which no HTTP client should be asked to survive.
async fn drain(State(w): State<Web>) -> impl IntoResponse {
    w.orch.begin_drain(Drain::Stop).await;
    let left = w.orch.view().await.inflight.len();
    format!("draining {left}")
}

/// `faber engine grow` (ADR-0061 §3): apply a pending grow of the Linux VM's
/// disk now, by the drain with no wait. Routine: it widens no trust, and only
/// holds Docker admission back while it runs. 409 with why when none is pending.
async fn engine_grow() -> (StatusCode, String) {
    match crate::vm::grow::global().force() {
        Ok(said) => (StatusCode::ACCEPTED, said),
        Err(why) => (StatusCode::CONFLICT, why),
    }
}

/// `faber golden verify <image>` (ADR-0049 §7): verify a golden image now. A
/// routine action under ADR-0028: it widens no trust, and only ever makes
/// admission stricter while it runs. Returns at once; the CLI polls
/// `/api/state`'s `vm.golden` for the result, since a verification boots a VM.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenVerify {
    image: String,
}

async fn golden_verify(Json(req): Json<GoldenVerify>) -> (StatusCode, String) {
    golden_verify_reply(crate::vm::verify::request(&req.image), &req.image)
}

/// The reply to a verify request, from the verifier's answer. Pure.
fn golden_verify_reply(
    answer: Result<bool, crate::vm::verify::RequestError>,
    image: &str,
) -> (StatusCode, String) {
    use crate::vm::verify::RequestError;
    match answer {
        Ok(true) => (StatusCode::ACCEPTED, format!("verifying {image}")),
        Ok(false) => (StatusCode::ACCEPTED, format!("already verifying {image}")),
        Err(e @ RequestError::UnknownImage(_)) => (StatusCode::NOT_FOUND, e.to_string()),
        Err(e @ RequestError::NotConfigured) => (StatusCode::CONFLICT, e.to_string()),
    }
}

/// Lift a pause, whoever set it — the janitor's included, as it always has.
/// Parked claims go at once, and a cycle runs now so claiming resumes too.
async fn resume(State(w): State<Web>) -> impl IntoResponse {
    w.orch.resume(Pause::Operator);
    w.orch.trigger_reconcile();
    "resumed"
}

async fn reconcile_now(State(w): State<Web>) -> impl IntoResponse {
    w.orch.trigger_reconcile();
    "triggered"
}

/// Poll for a release right now instead of waiting out the interval.
async fn update_check(State(w): State<Web>) -> Json<StateView> {
    w.update.check_now();
    Json(with_update(&w).await)
}

/// Stop waiting for a natural idle window: drain and install what is staged.
/// The button behind this is how an operator overrides the patient default on a
/// host that is never quiet.
async fn update_apply(State(w): State<Web>) -> Json<StateView> {
    w.update.apply_now();
    Json(with_update(&w).await)
}

/// Refuse the available version on this host — it joins the quarantine list and
/// the updater moves on to whatever comes after it.
async fn update_skip(State(w): State<Web>) -> Json<StateView> {
    w.update.skip_available();
    Json(with_update(&w).await)
}

/// A live autolimiter tuning update. Each field is optional so the panel can
/// change one knob without disturbing the others. `hard_ceiling` is tri-state:
/// absent = unchanged; present = the new setting — `"auto"`, `"off"`, a number,
/// or `null` (which [`HardCeiling`]'s own `Deserialize` reads as `"off"`,
/// matching what an older menu-bar app sends to clear the ceiling).
#[derive(Debug, Deserialize)]
struct ConfigUpdate {
    #[serde(default)]
    reserve_mib: Option<u64>,
    #[serde(default)]
    headroom_mib: Option<u64>,
    #[serde(default)]
    margin: Option<f64>,
    #[serde(default)]
    native_concurrency: Option<usize>,
    #[serde(default, deserialize_with = "hard_ceiling_update")]
    hard_ceiling: Option<HardCeiling>,
}

/// Distinguish an absent JSON key (`None` — leave the ceiling unchanged) from a
/// present one (`Some(_)`), while still handing a *present* value — including a
/// literal `null` — to [`HardCeiling::deserialize`] itself.
///
/// A plain `Option<HardCeiling>` field would not do this: serde's built-in
/// `Option<T>` handling intercepts JSON `null` before `T::deserialize` ever
/// runs, so `null` would read as "absent" (unchanged) rather than reaching
/// `HardCeiling`'s own null-means-off mapping. `deserialize_with` is only
/// invoked when the key is present at all, so the outer absent/present split
/// still falls out of `#[serde(default)]` for free.
fn hard_ceiling_update<'de, D>(de: D) -> Result<Option<HardCeiling>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(HardCeiling::deserialize(de)?))
}

/// Why `POST /api/config` refuses a zero ceiling, as the caller reads it.
const ZERO_CEILING_REFUSED: &str =
    "hard_ceiling must be at least 1 (\"off\" removes the ceiling). \
     To stop Docker admission, POST /api/hold with a reason.";

impl ConfigUpdate {
    /// What this update may not ask for — checked before any of it is applied, so
    /// a refused update changes nothing at all.
    ///
    /// A zero ceiling is the one refusal (issue #66). It was how an operator
    /// stopped admission before holds existed, and it did stop it — but with no
    /// reason on record, a dashboard reading healthy, and a value `check-config`
    /// itself rejects, persisted and reloaded by every restart. A hold is the way
    /// to stop admission now; this points there.
    fn check(&self) -> Result<(), &'static str> {
        if self.hard_ceiling == Some(HardCeiling::Fixed(0)) {
            return Err(ZERO_CEILING_REFUSED);
        }
        Ok(())
    }
}

/// Apply a tuning update live (new jobs pick it up immediately) and persist the
/// resulting settings so a restart keeps them. Returns the fresh state so the
/// caller sees the applied values without a follow-up poll.
async fn set_config(
    State(w): State<Web>,
    Json(update): Json<ConfigUpdate>,
) -> Result<Json<StateView>, (StatusCode, &'static str)> {
    update.check().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    w.orch.apply_tuning(
        update.reserve_mib,
        update.headroom_mib,
        update.margin,
        update.native_concurrency,
        update.hard_ceiling,
    );
    persist_overrides(&w.orch);
    Ok(Json(w.orch.view().await))
}

/// The longest hold reason the API takes, in characters: room for a sentence and
/// a ticket reference, short enough for the tray's one-line row.
const HOLD_REASON_MAX: usize = 200;

/// A `POST /api/hold` body.
#[derive(Debug, Deserialize)]
struct HoldRequest {
    /// Why admission is stopping. A missing key reads as blank and is refused
    /// like one: a stop with no reason is what a hold exists to replace.
    #[serde(default)]
    reason: String,
}

/// A hold reason as the API takes it: trimmed, never blank, at most
/// [`HOLD_REASON_MAX`] characters.
fn hold_reason(raw: &str) -> Result<String, String> {
    let reason = raw.trim();
    if reason.is_empty() {
        return Err("a hold needs a reason — say why Docker admission is stopping".to_string());
    }
    let len = reason.chars().count();
    if len > HOLD_REASON_MAX {
        return Err(format!(
            "a hold reason is at most {HOLD_REASON_MAX} characters, not {len}"
        ));
    }
    Ok(reason.to_string())
}

/// Hold Docker admission, for a reason the operator gives (issue #66).
///
/// Running jobs finish and claims are still taken — they park and say they are
/// held. Persisted, so a restart keeps it and warns about it. Holding again only
/// replaces the reason; "held since" stays when admission first stopped.
async fn hold(
    State(w): State<Web>,
    Json(req): Json<HoldRequest>,
) -> Result<Json<StateView>, (StatusCode, String)> {
    let reason = hold_reason(&req.reason).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    tracing::info!("Docker admission held: {reason:?}");
    w.orch.limiter().hold(reason, crate::audit::now_ms());
    persist_overrides(&w.orch);
    Ok(Json(with_update(&w).await))
}

/// Release the hold, if there is one. Parked claims re-check at once.
async fn unhold(State(w): State<Web>) -> Json<StateView> {
    if let Some(held) = w.orch.limiter().current_hold() {
        if w.orch.limiter().release_hold() {
            tracing::info!(
                "Docker admission released (held since {}: {:?})",
                held.since_utc(),
                held.reason
            );
            persist_overrides(&w.orch);
        }
    }
    Json(with_update(&w).await)
}

/// A `POST /api/attention/dismiss` body.
#[derive(Debug, Deserialize)]
struct DismissRequest {
    key: String,
    ident: String,
}

/// Dismiss an attention row, or replace its dismissal (ADR
/// a-warning-that-does-not-stop-jobs-can-be-dismissed §3). The daemon stores
/// `key` and `ident` as given; the UI decides what they mean.
async fn attention_dismiss(
    State(w): State<Web>,
    Json(req): Json<DismissRequest>,
) -> Result<Json<StateView>, (StatusCode, String)> {
    w.attention
        .dismiss(&req.key, &req.ident)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(with_update(&w).await))
}

/// A `POST /api/attention/restore` body: absent or `null` restores every row.
#[derive(Debug, Deserialize)]
struct RestoreRequest {
    #[serde(default)]
    key: Option<String>,
}

/// Bring a dismissed row back, or every one when no key is given.
async fn attention_restore(
    State(w): State<Web>,
    Json(req): Json<RestoreRequest>,
) -> Json<StateView> {
    w.attention.restore(req.key.as_deref());
    Json(with_update(&w).await)
}

/// A `POST /api/attention/ack-trust` body: `null` clears the acknowledgement.
#[derive(Debug, Deserialize)]
struct AckTrustRequest {
    until_ms: Option<u64>,
}

/// Acknowledge every trust change at or before `until_ms` (ADR
/// a-warning-that-does-not-stop-jobs-can-be-dismissed §4). Logged with the
/// caller, since it quiets a security notice.
async fn attention_ack_trust(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    Json(req): Json<AckTrustRequest>,
) -> Json<StateView> {
    let uid = caller.and_then(|Extension(c)| c.uid());
    tracing::info!(
        "trust changes acknowledged until {:?} (caller uid {uid:?})",
        req.until_ms
    );
    // Never past the newest change on record: a time in the future would fold every
    // later widening into the quiet row (ADR-0064 § 4). With nothing to acknowledge,
    // nothing is stored.
    let newest = w.trust_changes.list().iter().map(|c| c.ts_ms).max();
    let until = req.until_ms.and_then(|t| newest.map(|n| t.min(n)));
    w.attention.ack_trust(until);
    Json(with_update(&w).await)
}

/// A `POST /api/repo-hold/clear` body.
#[derive(Debug, Deserialize)]
struct RepoHoldClear {
    repo: String,
}

/// Clear a repo's minting hold (ADR-0030). Persisted by the orchestrator; parked
/// claims for the repo start within a few seconds. Clearing a repo that is not
/// held changes nothing, and is routine.
///
/// Clearing a held one widens trust (ADR-0028 addendum A): it is authorized like
/// a settings save, refused with a `403` and the reason, and audited allowed or
/// refused under [`REPO_HOLD_CLEAR_ROUTE`]. Only a repo held when the request is
/// judged is cleared, so a hold set after that is never cleared on a routine
/// verdict.
async fn clear_repo_hold(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    Json(req): Json<RepoHoldClear>,
) -> Response {
    let caller = caller.map(|Extension(c)| c);
    let repo = req.repo.trim();
    let held = w.orch.repo_holds().into_iter().find(|h| h.repo == repo);
    let widenings: Vec<_> = held.iter().map(crate::control::repo_hold_cleared).collect();
    let class = caller.map_or(Class::Refused, Caller::class);
    match crate::control::authorize(class, &widenings) {
        Verdict::Refuse(why) => {
            if !widenings.is_empty() {
                tracing::warn!(
                    "repo-hold clear refused: {why:?} ({})",
                    log_sentences(&widenings)
                );
                audit_control(
                    &w,
                    caller,
                    REPO_HOLD_CLEAR_ROUTE,
                    widenings,
                    false,
                    Some(why.clone()),
                )
                .await;
            }
            return (StatusCode::FORBIDDEN, format!("{why}\n")).into_response();
        }
        Verdict::Allow { .. } if widenings.is_empty() => {}
        Verdict::Allow { .. } => {
            if let Some(cleared) = w.orch.clear_repo_hold(repo).await {
                let widenings = vec![crate::control::repo_hold_cleared(&cleared)];
                tracing::info!(
                    "repo-hold clear widens trust: {}",
                    log_sentences(&widenings)
                );
                audit_control(&w, caller, REPO_HOLD_CLEAR_ROUTE, widenings, true, None).await;
            }
        }
    }
    Json(with_update(&w).await).into_response()
}

/// A pin/unpin of one class's cap from the control panel.
#[derive(Debug, Deserialize)]
struct PinUpdate {
    class: String,
    #[serde(default)]
    runner_mib: u64,
    #[serde(default)]
    dind_mib: u64,
    /// CPU pin (ADR-0019), honoured as `Limits.*.cpus`: `0.0` leaves that
    /// container's CPU learned, same as an unset field. Optional so an older
    /// menu-bar app posting only the memory pair still pins memory alone.
    #[serde(default)]
    runner_cpus: f64,
    #[serde(default)]
    dind_cpus: f64,
    /// `true` pins to (`runner_mib`, `dind_mib`, `runner_cpus`, `dind_cpus`);
    /// `false` returns it to the learner.
    pin: bool,
}

async fn set_pin(State(w): State<Web>, Json(p): Json<PinUpdate>) -> Json<StateView> {
    if p.pin {
        let limits = Limits {
            runner: ResourceLimits {
                cpus: p.runner_cpus,
                memory_mib: p.runner_mib,
            },
            dind: ResourceLimits {
                cpus: p.dind_cpus,
                memory_mib: p.dind_mib,
            },
        };
        w.orch.limiter().pin(&p.class, limits);
    } else {
        w.orch.limiter().unpin(&p.class);
    }
    persist_overrides(&w.orch);
    Json(w.orch.view().await)
}

/// Persist the whole live tuning state + pinned caps + the hold to the overrides
/// file, so the file always reflects the current settings (not just one request's
/// fields).
fn persist_overrides(orch: &Orchestrator) {
    // Built under the overrides lock, so the repo holds read here are the ones in
    // force when the file is written, not ones another writer has since changed.
    let written = Overrides::replace(|| {
        let mut overrides = overrides_for(&orch.limiter().snapshot(), orch.limiter().pinned_caps());
        // Not tuning: the repo holds a gate bypass set (ADR-0030) ride along.
        overrides.repo_holds = orch.repo_holds();
        overrides
    });
    if let Err(e) = written {
        tracing::warn!("persisting overrides failed: {e}");
    }
}

/// The overrides file for this live state: every tuning knob as it stands, the
/// pins, and the hold.
fn overrides_for(snap: &LimiterView, pinned_caps: HashMap<String, Limits>) -> Overrides {
    Overrides {
        native_concurrency: Some(snap.native_concurrency),
        reserve_mib: Some(snap.reserve_mib),
        headroom_mib: Some(snap.headroom_mib),
        margin: Some(snap.margin),
        hard_ceiling: ceiling_mirror(snap.hard_ceiling, snap.hold.is_some()),
        ceiling: Some(snap.hard_ceiling_setting),
        pinned_caps,
        hold: snap.hold.clone(),
        repo_holds: Vec::new(),
    }
}

/// The legacy numeric `hard_ceiling` to write beside a ceiling whose effective
/// limit is `limit`.
///
/// It is the only key a binary from before holds reads, so while admission is
/// held it says `0`: a rollback past holds then keeps admission stopped instead of
/// quietly reopening it. This binary never reads the zero back as a ceiling — the
/// true setting is in `ceiling`, and a zero without one becomes a hold at boot
/// ([`Overrides::migrate_zero_ceiling`]). Not held, it is the effective limit, as
/// it always was.
fn ceiling_mirror(limit: Option<usize>, held: bool) -> Option<usize> {
    if held {
        Some(0)
    } else {
        limit
    }
}

// --- the whole config, over HTTP -------------------------------------------

/// The Settings page's payload: the config file, the schema to render it with,
/// and an honest account of what the running daemon has actually picked up.
#[derive(Serialize)]
struct SettingsView {
    /// Where the config file lives (it may not exist yet).
    path: String,
    exists: bool,
    /// Set when the file on disk doesn't parse. `config` then holds the config
    /// this daemon is *running*, and saving will replace the broken file.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// True when this daemon booted with no config file, i.e. from CLI flags.
    /// Saving writes the first one — which supersedes those flags from then on.
    from_flags: bool,
    /// The control panel's port. A CLI flag, shown read-only.
    port: u16,
    config: FileConfig,
    /// faber's built-in defaults, for placeholders and "reset to default".
    defaults: FileConfig,
    /// Keys that differ from the ones this process booted with — the restart-to-
    /// apply list. Empty means the file and the running daemon agree.
    pending_restart: Vec<String>,
    /// Live autolimiter knobs that don't match the file, because the quick-tune
    /// panel set a machine-local override. Saving this page replaces them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    live_overrides: Vec<LiveOverride>,
    /// The digest a save sent with `?base=` is checked against
    /// ([`crate::control::config_digest`] of `config`): send it back, and a
    /// save onto a config that has moved since is a `409`.
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    schema: FormSchema,
}

/// One knob the running daemon is using at a different value than the file says.
#[derive(Serialize)]
struct LiveOverride {
    key: &'static str,
    file: serde_json::Value,
    live: serde_json::Value,
}

/// Where the live limiter and the config file disagree.
///
/// The five live knobs exist in two layers — the file, plus a machine-local
/// `overrides.json` the quick-tune panel writes. Without this, the settings page
/// would show one number while the daemon ran another, with nothing on screen
/// admitting it.
fn live_overrides(orch: &Orchestrator, cfg: &FileConfig) -> Vec<LiveOverride> {
    use serde_json::json;
    let live = orch.limiter().snapshot();
    let a = &cfg.autolimit;
    [
        (
            "autolimit.reserve_mib",
            json!(a.reserve_mib),
            json!(live.reserve_mib),
        ),
        (
            "autolimit.headroom_mib",
            json!(a.headroom_mib),
            json!(live.headroom_mib),
        ),
        ("autolimit.margin", json!(a.margin), json!(live.margin)),
        (
            "autolimit.native_concurrency",
            json!(a.native_concurrency),
            json!(live.native_concurrency),
        ),
        (
            // Both sides must go through `HardCeiling`'s own Serialize (`"auto"` /
            // `"off"` / a number) — comparing the file's setting against the
            // limiter's numeric *resolved* mirror (`live.hard_ceiling`) would
            // report "auto" as an override of itself the moment it resolves to a
            // concrete number.
            "autolimit.hard_ceiling",
            json!(a.hard_ceiling),
            json!(live.hard_ceiling_setting),
        ),
    ]
    .into_iter()
    .filter(|(_, file, live)| file != live)
    .map(|(key, file, live)| LiveOverride { key, file, live })
    .collect()
}

/// The answer to a save: what went wrong (if anything) and the resulting state.
#[derive(Serialize)]
struct SaveResult {
    saved: bool,
    /// Validation problems, as operator-readable sentences. Non-empty ⇒ nothing
    /// was written.
    errors: Vec<String>,
    /// On a `403`: each trust-widening change the caller's class may not make,
    /// and the class it needs (ADR-0028).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    refused: Vec<crate::control::Needs>,
    /// On a save: each trust-widening change it made.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    widened: Vec<crate::control::Widening>,
    /// On a dry run: the digest of the config it judged against
    /// ([`crate::control::config_digest`]). A save sent with it as `?base=`
    /// is refused with `409` once the config has moved (#221).
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// On a dry run only: the config the save would write (for the raw editor,
    /// what its text parses to), so a UI can show what would change.
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<FileConfig>,
    settings: SettingsView,
}

fn settings_view(w: &Web, config: FileConfig, exists: bool, error: Option<String>) -> SettingsView {
    SettingsView {
        path: w.settings.path().display().to_string(),
        exists,
        error,
        from_flags: !w.settings.boot_from_file(),
        port: w.port,
        pending_restart: w.settings.pending_restart(&config),
        live_overrides: live_overrides(&w.orch, &config),
        base: crate::control::config_digest(&config).ok(),
        config,
        defaults: FileConfig::default(),
        schema: FORM_SCHEMA,
    }
}

async fn get_settings(State(w): State<Web>) -> Json<SettingsView> {
    let cur = w.settings.current();
    Json(settings_view(&w, cur.config, cur.exists, cur.error))
}

/// [`save_settings_on`] with no base to check.
#[cfg(test)]
async fn save_settings(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    Json(next): Json<FileConfig>,
) -> (StatusCode, Json<SaveResult>) {
    save_settings_on(State(w), caller, SETTINGS_ROUTE, None, Json(next), None).await
}

/// Write the whole config file, and apply the parts the running daemon can take
/// without a restart: [`save_config`], one save at a time.
///
/// Refused with `409` when `expected` is given and is not the digest of the
/// config this save would replace: the base a dry run judged has moved since
/// (#221), so what it showed is no longer what this save widens. Nothing is
/// written or audited; the caller re-runs the dry run.
///
/// `text` is the raw editor's: the file to write verbatim, which `next` was
/// parsed from. `None` re-serialises `next`, as the form's save does. With a
/// `text`, `expected` is the digest of the file's bytes ([`text_digest`]), so
/// an edit that only moved a comment is a `409` too: a verbatim write would
/// otherwise drop it unseen.
async fn save_settings_on(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    route: &'static str,
    expected: Option<String>,
    Json(next): Json<FileConfig>,
    text: Option<String>,
) -> (StatusCode, Json<SaveResult>) {
    let caller = caller.map(|Extension(c)| c);
    // One save at a time from here to the write: the widenings are measured
    // against the file this save replaces, not one another save is replacing.
    let save_lock = w.save_lock.clone();
    let _saving = save_lock.lock().await;
    // The raw editor's base is the file's bytes: read them once, and judge the
    // config those same bytes parse to.
    let (base, file) = match text {
        Some(_) => {
            let (base, file) = w.settings.current_and_text();
            (base, file.ok())
        }
        None => (w.settings.current(), None),
    };
    if let Some(expected) = expected {
        let actual = match text {
            Some(_) => file.as_deref().map(text_digest),
            None => crate::control::config_digest(&base.config).ok(),
        };
        if actual.as_deref() != Some(expected.as_str()) {
            tracing::warn!("settings save refused: its base changed since the dry run");
            return (
                StatusCode::CONFLICT,
                Json(SaveResult {
                    saved: false,
                    errors: vec![BASE_MOVED.to_string()],
                    refused: Vec::new(),
                    widened: Vec::new(),
                    base: None,
                    preview: None,
                    settings: settings_view(&w, base.config, base.exists, base.error),
                }),
            );
        }
    }
    save_config(&w, caller, route, base, next, text.as_deref()).await
}

/// Why a save sent with a stale `?base=` is refused.
const BASE_MOVED: &str =
    "the config changed after the dry run judged it, so what it showed may not be what this \
     save widens; nothing was written";

/// `POST /api/settings`'s query: `?dry_run=1` judges a config and writes
/// nothing; `?base=<digest>` saves only onto the config a dry run judged.
#[derive(Debug, Default, Deserialize)]
struct SaveQuery {
    #[serde(default)]
    dry_run: Option<String>,
    #[serde(default)]
    base: Option<String>,
}

/// `POST /api/settings`: a save, or with `?dry_run=1` (or `true`) a
/// [`dry_run`].
async fn save_settings_route(
    state: State<Web>,
    caller: Option<Extension<Caller>>,
    Query(q): Query<SaveQuery>,
    next: Json<FileConfig>,
) -> (StatusCode, Json<SaveResult>) {
    match q.dry_run.as_deref() {
        Some("1" | "true") => dry_run(state, caller, next).await,
        _ => save_settings_on(state, caller, SETTINGS_ROUTE, q.base, next, None).await,
    }
}

/// `GET /api/settings/raw`: the config file as the operator wrote it.
#[derive(Debug, Serialize)]
struct RawView {
    path: String,
    /// The file's bytes as UTF-8; empty when there is no file yet.
    text: String,
    /// The digest `POST /api/settings/raw?base=` checks: of the file's bytes
    /// ([`text_digest`]), not of the config they parse to.
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
}

/// `POST /api/settings/raw`'s body.
#[derive(Debug, Deserialize)]
struct RawText {
    text: String,
}

/// A `400` for text that is not a config: where, and why.
#[derive(Debug, Serialize, PartialEq)]
struct RawParseError {
    /// 1-based.
    line: usize,
    /// 1-based, in characters.
    column: usize,
    message: String,
    /// The keys faber does not read, dotted, when that is the refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keys: Vec<String>,
}

/// Why a daemon started from flags has no raw editor.
const RAW_FROM_FLAGS: &str =
    "this daemon was started from command-line flags, not a config file; there is no \
     config.toml to edit as text";

/// The `409` both raw routes give a daemon started from flags.
fn raw_from_flags() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "from_flags": true, "errors": [RAW_FROM_FLAGS] })),
    )
        .into_response()
}

async fn get_settings_raw(State(w): State<Web>) -> Response {
    if !w.settings.boot_from_file() {
        return raw_from_flags();
    }
    match w.settings.read_text() {
        Ok(text) => Json(RawView {
            path: w.settings.path().display().to_string(),
            base: Some(text_digest(&text)),
            text,
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "errors": [format!("{e:#}")] })),
        )
            .into_response(),
    }
}

/// The raw editor's base: a SHA-256 of the file's bytes, hex.
fn text_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Parse the raw editor's text exactly as the daemon parses its file
/// ([`FileConfig::load_from_str`]), then refuse any key faber does not read
/// ([`unknown_keys`]).
fn parse_raw(text: &str) -> Result<FileConfig, RawParseError> {
    if text.len() > FileConfig::READ_CAP {
        return Err(RawParseError {
            line: 1,
            column: 1,
            message: "larger than 1 MiB, more than any faber config".into(),
            keys: Vec::new(),
        });
    }
    let cfg = FileConfig::load_from_str(text, "from the editor").map_err(|e| {
        let toml = e.root_cause().downcast_ref::<toml::de::Error>();
        let (line, column) =
            line_column(text, toml.and_then(toml::de::Error::span).map(|r| r.start));
        RawParseError {
            line,
            column,
            message: toml.map_or_else(|| format!("{e:#}"), |t| t.message().to_string()),
            keys: Vec::new(),
        }
    })?;
    let root = text.parse::<toml::Table>().map(toml::Value::Table);
    if let Ok(root) = &root {
        let keys = count_keys(root);
        if keys > MAX_PROBED_KEYS {
            return Err(RawParseError {
                line: 1,
                column: 1,
                message: format!(
                    "too many keys: {keys}, more than any faber config (at most \
                     {MAX_PROBED_KEYS})"
                ),
                keys: Vec::new(),
            });
        }
    }
    let unknown = root.map_or_else(|_| Vec::new(), |root| unknown_keys_in(&root, text));
    if let Some((_, first)) = unknown.first() {
        let (line, column) = line_column(text, *first);
        let keys: Vec<String> = unknown.into_iter().map(|(k, _)| k).collect();
        return Err(RawParseError {
            line,
            column,
            message: format!(
                "faber does not read {}: check the spelling, or remove {}",
                keys.join(", "),
                if keys.len() == 1 { "it" } else { "them" }
            ),
            keys,
        });
    }
    Ok(cfg)
}

/// The 1-based line and column (in characters) of byte `at` in `text`; the
/// start of the text when there is no place to name.
fn line_column(text: &str, at: Option<usize>) -> (usize, usize) {
    let before = &text[..text.floor_char_boundary(at.unwrap_or(0))];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// One step into a TOML document: a key of a table, or an element of an array.
#[derive(Debug, Clone)]
enum Seg {
    Key(String),
    Idx(usize),
}

/// The most keys, at every depth, a raw save probes for ones faber does not
/// read ([`unknown_keys`]); more is refused outright. A full config has a few
/// hundred.
const MAX_PROBED_KEYS: usize = 2000;

/// Every key in `v`, at every depth, arrays' tables included.
fn count_keys(v: &toml::Value) -> usize {
    match v {
        toml::Value::Table(t) => t.len() + t.values().map(count_keys).sum::<usize>(),
        toml::Value::Array(a) => a.iter().map(count_keys).sum(),
        _ => 0,
    }
}

/// The keys in `text` that [`FileConfig`] does not read, dotted
/// (`profile[0].dind_imgae`), each with the byte offset of its key when it can
/// be found. The raw editor's save refuses these; boot, `check-config` and the
/// form never ask, so a hand-written file with a stray key still starts
/// (AGENTS.md).
///
/// Most config tables already refuse an unknown key when they parse
/// (`deny_unknown_fields`); this catches the rest without a list of which
/// those are. Each key in turn has its value swapped for a [`probe`] no field
/// takes: when the config still parses, nothing read that key. A key that
/// fails stays known, and its own table or array is walked in turn.
/// User-named entries (a profile's `env`, `[needs.<tool>]`) fail the probe on
/// their value's type, so they count as known.
#[cfg(test)]
fn unknown_keys(text: &str) -> Vec<(String, Option<usize>)> {
    match text.parse::<toml::Table>() {
        Ok(root) => unknown_keys_in(&toml::Value::Table(root), text),
        Err(_) => Vec::new(),
    }
}

/// [`unknown_keys`] over `text` already parsed as `root`.
fn unknown_keys_in(root: &toml::Value, text: &str) -> Vec<(String, Option<usize>)> {
    let mut out = Vec::new();
    probe_table(root, &mut Vec::new(), &mut out);
    if out.is_empty() {
        return Vec::new();
    }
    // Parsed once for every key it places, not once per key.
    let doc = toml_edit::ImDocument::parse(text).ok();
    out.into_iter()
        .map(|path| {
            let at = doc.as_ref().and_then(|d| key_offset(d, &path));
            (dotted(&path), at)
        })
        .collect()
}

/// A value no config field takes: an array of five different kinds. A table
/// would not do (a section whose every key has a default takes any table, and
/// a datetime reads as a string), and no list holds mixed kinds.
fn probe() -> toml::Value {
    toml::Value::Array(vec![
        toml::Value::Boolean(true),
        toml::Value::Float(1.5),
        toml::Value::String("faber-probe".into()),
        toml::Value::Table(toml::Table::new()),
        toml::Value::Array(Vec::new()),
    ])
}

/// Probe each key of the table at `path` ([`unknown_keys`]), one table at a
/// time and in the smallest document that answers for it, never the whole
/// file per key: the table placed alone at its path (each table on the way
/// holding only the next key, each array only the next element), and emptied
/// to the probed key when its type takes an empty table. Each narrowing is
/// used only when the document parses without the probe, so the probe is the
/// one thing a refusal can be about; otherwise the next wider one is.
fn probe_table(root: &toml::Value, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    let Some(table) = at(root, path).and_then(toml::Value::as_table) else {
        return;
    };
    let parses = |doc: &toml::Value| doc.clone().try_into::<FileConfig>().is_ok();
    let empty = || toml::Value::Table(toml::Table::new());
    let whole = || toml::Value::Table(table.clone());
    // Narrowest first: the table emptied with nothing around it; emptied, with
    // what its ancestors also hold (a profile's `name`); whole, likewise; the
    // file itself.
    let (doc, doc_path) = [
        alone_at(path, empty()),
        kept_at(root, path, empty()),
        kept_at(root, path, whole()),
    ]
    .into_iter()
    .find(|(doc, _)| parses(doc))
    .unwrap_or_else(|| (root.clone(), path.clone()));
    for (k, v) in table {
        path.push(Seg::Key(k.clone()));
        let mut probed = doc.clone();
        if let Some(slot) = at_mut(&mut probed, &doc_path).and_then(toml::Value::as_table_mut) {
            slot.insert(k.clone(), probe());
        }
        if probed.try_into::<FileConfig>().is_ok() {
            out.push(path.clone());
        } else {
            match v {
                toml::Value::Table(_) => probe_table(root, path, out),
                toml::Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        if item.is_table() {
                            path.push(Seg::Idx(i));
                            probe_table(root, path, out);
                            path.pop();
                        }
                    }
                }
                _ => {}
            }
        }
        path.pop();
    }
}

/// [`alone_at`], except that each table below the top keeps its other keys:
/// what `root` holds there, with `inner` in place of the value at `path`.
fn kept_at(root: &toml::Value, path: &[Seg], inner: toml::Value) -> (toml::Value, Vec<Seg>) {
    fn build(v: Option<&toml::Value>, path: &[Seg], top: bool, inner: toml::Value) -> toml::Value {
        let Some((seg, rest)) = path.split_first() else {
            return inner;
        };
        match seg {
            Seg::Key(k) => {
                let here = v.and_then(toml::Value::as_table);
                let child = build(here.and_then(|t| t.get(k.as_str())), rest, false, inner);
                let mut out = toml::Table::new();
                if !top {
                    for (key, value) in here.into_iter().flatten() {
                        if key != k {
                            out.insert(key.clone(), value.clone());
                        }
                    }
                }
                out.insert(k.clone(), child);
                toml::Value::Table(out)
            }
            Seg::Idx(i) => {
                let item = v.and_then(|a| a.get(*i));
                toml::Value::Array(vec![build(item, rest, false, inner)])
            }
        }
    }
    let (_, at) = alone_at(path, toml::Value::Boolean(false));
    (build(Some(root), path, true, inner), at)
}

/// A document holding only `inner`, at `path`: each table on the way holds
/// only the next key, each array only the next element. With the path to
/// `inner` in it (an array index is then always 0).
fn alone_at(path: &[Seg], inner: toml::Value) -> (toml::Value, Vec<Seg>) {
    let doc = path.iter().rev().fold(inner, |v, seg| match seg {
        Seg::Key(k) => toml::Value::Table(toml::Table::from_iter([(k.clone(), v)])),
        Seg::Idx(_) => toml::Value::Array(vec![v]),
    });
    let at = path
        .iter()
        .map(|seg| match seg {
            Seg::Key(k) => Seg::Key(k.clone()),
            Seg::Idx(_) => Seg::Idx(0),
        })
        .collect();
    (doc, at)
}

fn at<'a>(v: &'a toml::Value, path: &[Seg]) -> Option<&'a toml::Value> {
    path.iter().try_fold(v, |v, seg| match seg {
        Seg::Key(k) => v.get(k.as_str()),
        Seg::Idx(i) => v.get(*i),
    })
}

fn at_mut<'a>(v: &'a mut toml::Value, path: &[Seg]) -> Option<&'a mut toml::Value> {
    path.iter().try_fold(v, |v, seg| match seg {
        Seg::Key(k) => v.get_mut(k.as_str()),
        Seg::Idx(i) => v.get_mut(*i),
    })
}

fn dotted(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Idx(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// Where the last key of `path` is written in `doc`, as a byte offset.
fn key_offset(doc: &toml_edit::ImDocument<&str>, path: &[Seg]) -> Option<usize> {
    use toml_edit::{Item, Table, TableLike, Value};
    enum Cur<'a> {
        Item(&'a Item),
        Table(&'a Table),
        Value(&'a Value),
    }
    fn table_like<'a>(c: &Cur<'a>) -> Option<&'a dyn TableLike> {
        match *c {
            Cur::Item(i) => i.as_table_like(),
            Cur::Table(t) => Some(t as &dyn TableLike),
            Cur::Value(v) => v.as_inline_table().map(|t| t as &dyn TableLike),
        }
    }
    let mut cur = Cur::Item(doc.as_item());
    let mut offset = None;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => {
                let (key, item) = table_like(&cur)?.get_key_value(k)?;
                offset = key.span().map(|r| r.start);
                Cur::Item(item)
            }
            Seg::Idx(i) => match cur {
                Cur::Item(Item::ArrayOfTables(a)) => Cur::Table(a.get(*i)?),
                Cur::Item(Item::Value(Value::Array(a))) | Cur::Value(Value::Array(a)) => {
                    Cur::Value(a.get(*i)?)
                }
                _ => return None,
            },
        };
    }
    offset
}

/// `POST /api/settings/raw?base=<digest>[&dry_run=1]` with `{text}`: the raw
/// editor's save. The text is parsed as the daemon parses its file (a TOML
/// error is a `400` with its line), then judged by exactly the form's path
/// ([`dry_run`], or [`save_settings_on`] and [`save_config`]: validation,
/// widening, authorization, the live push), and on success written verbatim,
/// comments and layout kept, with the same temp-and-rename and `.bak`.
async fn save_settings_raw(
    state: State<Web>,
    caller: Option<Extension<Caller>>,
    Query(q): Query<SaveQuery>,
    Json(body): Json<RawText>,
) -> Response {
    if !state.settings.boot_from_file() {
        return raw_from_flags();
    }
    // Parsing and the unknown-key probe are CPU work on a body of up to 1 MiB:
    // off the async workers, so a large body cannot stall the daemon's API.
    let text = body.text;
    let (text, parsed) = match tokio::task::spawn_blocking(move || {
        let parsed = parse_raw(&text);
        (text, parsed)
    })
    .await
    {
        Ok(done) => done,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "errors": [format!("parsing the text: {e}")] })),
            )
                .into_response()
        }
    };
    let next = match parsed {
        Ok(next) => next,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(e)).into_response(),
    };
    match q.dry_run.as_deref() {
        Some("1" | "true") => {
            // One read: the config judged and the base digest are one file.
            let (cur, file) = state.settings.current_and_text();
            let (status, Json(mut r)) = judge(&state, caller, cur, next);
            // The raw editor's base is the file's bytes, as its GET gives it.
            r.base = file.ok().as_deref().map(text_digest);
            (status, Json(r)).into_response()
        }
        _ => save_settings_on(
            state,
            caller,
            SETTINGS_RAW_ROUTE,
            q.base,
            Json(next),
            Some(text),
        )
        .await
        .into_response(),
    }
}

/// What a save of `next` would do, doing none of it: no authorization, no
/// write, no audit line. `200` with the widenings it carries (`widened`), what
/// this caller would be refused with the commands that make it (`refused`),
/// and the validation errors (`errors`). `faber config apply` asks this
/// before it shows root what is about to widen.
async fn dry_run(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    Json(next): Json<FileConfig>,
) -> (StatusCode, Json<SaveResult>) {
    let cur = w.settings.current();
    judge(&w, caller, cur, next)
}

/// [`dry_run`] against `cur`, the config as the caller read it.
fn judge(
    w: &Web,
    caller: Option<Extension<Caller>>,
    cur: crate::settings::CurrentConfig,
    next: FileConfig,
) -> (StatusCode, Json<SaveResult>) {
    let caller = caller.map(|Extension(c)| c);
    let widenings = crate::control::trust_widening(&cur.config, &next);
    let class = caller.map_or(Class::Refused, Caller::class);
    let refused = match crate::control::authorize(class, &widenings) {
        Verdict::Refuse(_) => crate::control::needs(&widenings, &cur.config, &next),
        Verdict::Allow { .. } => Vec::new(),
    };
    (
        StatusCode::OK,
        Json(SaveResult {
            saved: false,
            errors: validation_errors(w, &cur.config, &next),
            refused,
            widened: widenings,
            base: crate::control::config_digest(&cur.config).ok(),
            preview: Some(next),
            settings: settings_view(w, cur.config, cur.exists, cur.error),
        }),
    )
}

/// Everything a save over `old` refuses `next` for: [`FileConfig::validate`],
/// a list entry it adds that the form would refuse
/// ([`crate::settings::new_item_errors`]; one already in `old` never blocks a
/// save), and a machine share the machine cannot hold (refused here, where a
/// human is looking; boot only warns about one, ADR-0021).
fn validation_errors(w: &Web, old: &FileConfig, next: &FileConfig) -> Vec<String> {
    let mut errors = next.validate();
    for e in crate::settings::new_item_errors(old, next) {
        if !errors.contains(&e) {
            errors.push(e);
        }
    }
    if let Some(machine) = w.orch.limiter().machine_size() {
        errors.extend(next.host.machine_share().errors(machine));
    }
    errors
}

/// A `POST /api/settings/key` body: one key, and its value as `faber config
/// set` was given it (`None`: `faber config unset`).
#[derive(Debug, Deserialize)]
struct SettingsKey {
    key: String,
    #[serde(default)]
    value: Option<String>,
}

/// Change one config key (`faber config set` / `unset`, issue #78): the file
/// as it is, with [`crate::config_cli::edit`] applied, saved through the same
/// [`save_config`] as the whole-file save, judged against the same read the
/// edit was made on. A key or value `edit` refuses is a `400` with its reason,
/// audited as refused, and nothing is judged or written.
async fn save_settings_key(
    State(w): State<Web>,
    caller: Option<Extension<Caller>>,
    Json(req): Json<SettingsKey>,
) -> (StatusCode, Json<SaveResult>) {
    let caller = caller.map(|Extension(c)| c);
    let save_lock = w.save_lock.clone();
    let _saving = save_lock.lock().await;
    let cur = w.settings.current();
    match crate::config_cli::edit(&cur.config, &req.key, req.value.as_deref()) {
        Ok(next) => save_config(&w, caller, SETTINGS_KEY_ROUTE, cur, next, None).await,
        Err(e) => {
            audit_control(
                &w,
                caller,
                SETTINGS_KEY_ROUTE,
                Vec::new(),
                false,
                Some(e.clone()),
            )
            .await;
            (
                StatusCode::BAD_REQUEST,
                Json(SaveResult {
                    saved: false,
                    errors: vec![e],
                    refused: Vec::new(),
                    widened: Vec::new(),
                    base: None,
                    preview: None,
                    settings: settings_view(&w, cur.config, cur.exists, cur.error),
                }),
            )
        }
    }
}

/// The one save pipeline behind `POST /api/settings` and `POST
/// /api/settings/key`: judge, validate, write, apply live, audit. The caller
/// holds `save_lock`.
///
/// A save that widens trust ([`crate::control::trust_widening`], measured
/// against the file as it is, or the running config when the file is missing or
/// broken) needs [`crate::control::WIDENING_NEEDS`]. Refused, it is a `403`
/// naming each change, the class it needs and the command that makes it; either
/// way it leaves one `control` line in the audit log (allowed ones once they are
/// written). One that fails validation or the write leaves a refused line, with
/// why in `reason`, when it widens trust or came through [`SETTINGS_KEY_ROUTE`].
/// Through that route a routine change leaves one too, allowed or refused: it
/// is the CLI's, and its every use is on record. Only a completed widening
/// reaches the trust-change notice.
///
/// The live part is only the autolimiter's five tunable knobs, and the update
/// policy. Those knobs are also the ones a machine-local `overrides.json`
/// shadows — so a save clears that shadow and pushes the file's values into the
/// limiter instead. Otherwise the page would show one number, the file would
/// hold it, and the daemon would keep running another.
async fn save_config(
    w: &Web,
    caller: Option<Caller>,
    route: &'static str,
    base: crate::settings::CurrentConfig,
    next: FileConfig,
    text: Option<&str>,
) -> (StatusCode, Json<SaveResult>) {
    let old = base.config;
    let widenings = crate::control::trust_widening(&old, &next);
    let audit_routine = route == SETTINGS_KEY_ROUTE;
    let on_record = audit_routine || !widenings.is_empty();
    let class = caller.map_or(Class::Refused, Caller::class);
    let audit = match crate::control::authorize(class, &widenings) {
        Verdict::Allow { audit } => audit || audit_routine,
        Verdict::Refuse(why) => {
            if on_record {
                tracing::warn!(
                    "settings save refused: {why:?} ({})",
                    log_sentences(&widenings)
                );
                audit_control(
                    w,
                    caller,
                    route,
                    widenings.clone(),
                    false,
                    Some(why.clone()),
                )
                .await;
            }
            let cur = w.settings.current();
            return (
                StatusCode::FORBIDDEN,
                Json(SaveResult {
                    saved: false,
                    errors: vec![why],
                    refused: crate::control::needs(&widenings, &old, &next),
                    widened: Vec::new(),
                    base: None,
                    preview: None,
                    settings: settings_view(w, cur.config, cur.exists, cur.error),
                }),
            );
        }
    };

    let errors = validation_errors(w, &old, &next);
    if !errors.is_empty() {
        if on_record {
            audit_control(w, caller, route, widenings, false, Some(errors.join("; "))).await;
        }
        let cur = w.settings.current();
        return (
            StatusCode::BAD_REQUEST,
            Json(SaveResult {
                saved: false,
                errors,
                refused: Vec::new(),
                widened: Vec::new(),
                base: None,
                preview: None,
                settings: settings_view(w, cur.config, cur.exists, cur.error),
            }),
        );
    }

    let written = match text {
        Some(text) => w.settings.save_text(&next, text),
        None => w.settings.save(&next),
    };
    if let Err(e) = written {
        let why = format!("{e:#}");
        if on_record {
            audit_control(w, caller, route, widenings, false, Some(why.clone())).await;
        }
        let cur = w.settings.current();
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SaveResult {
                saved: false,
                errors: vec![why],
                refused: Vec::new(),
                widened: Vec::new(),
                base: None,
                preview: None,
                settings: settings_view(w, cur.config, cur.exists, cur.error),
            }),
        );
    }
    if audit {
        if !widenings.is_empty() {
            tracing::info!("settings save widens trust: {}", log_sentences(&widenings));
        }
        audit_control(w, caller, route, widenings.clone(), true, None).await;
    }

    let a = &next.autolimit;
    w.orch.apply_tuning(
        Some(a.reserve_mib),
        Some(a.headroom_mib),
        Some(a.margin),
        Some(a.native_concurrency),
        Some(a.hard_ceiling),
    );
    if w.write_overrides {
        drop_tuning_overrides(&w.orch);
    }
    // Every `[update]` field is declared `live` in the settings schema, and this
    // is what makes that claim true: the running updater takes the new policy
    // now rather than at the next restart.
    w.update.set_policy(next.update.clone());

    (
        StatusCode::OK,
        Json(SaveResult {
            saved: true,
            errors: Vec::new(),
            refused: Vec::new(),
            widened: widenings,
            base: None,
            preview: None,
            settings: settings_view(w, next, true, None),
        }),
    )
}

/// Widening sentences for a log line, each `Debug`-quoted: they carry
/// operator-supplied names (profiles, actors, versions), and a quoted name
/// cannot break the line or forge another.
fn log_sentences(widenings: &[crate::control::Widening]) -> String {
    widenings
        .iter()
        .map(|v| format!("{:?}", v.sentence))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The route a settings save's `control` line names.
const SETTINGS_ROUTE: &str = "POST /api/settings";
/// The route a raw-text save's `control` line names.
const SETTINGS_RAW_ROUTE: &str = "POST /api/settings/raw";
/// The route a one-key change's `control` line names (`faber config set`).
const SETTINGS_KEY_ROUTE: &str = "POST /api/settings/key";
/// The route a repo-hold clear's `control` line names.
const REPO_HOLD_CLEAR_ROUTE: &str = "POST /api/repo-hold/clear";

/// Append the `control` audit line for a change through `route` (a refused one
/// with why, in `reason`), and put an allowed widening one on the trust-change
/// notice. The notice is fed even when
/// the append fails: the change happened either way.
async fn audit_control(
    w: &Web,
    caller: Option<Caller>,
    route: &str,
    widenings: Vec<crate::control::Widening>,
    allowed: bool,
    reason: Option<String>,
) {
    let mut record = crate::audit::Record::control(caller, route, widenings, allowed);
    record.reason = reason;
    if let Err(e) = w.orch.audit_log().append(&record).await {
        tracing::warn!("audit append (control) failed: {e}");
    }
    if !record.widenings.is_empty() {
        w.trust_changes.record(&record);
    }
}

/// Rewrite `overrides.json` with the live autolimiter knobs cleared, keeping the
/// operator's pinned caps.
///
/// Those five knobs live in two places by design: `config.toml` holds the
/// deployment's intent, `overrides.json` the machine-local tweak layered on top
/// (see [`Overrides`]). A save from the settings page *is* a statement of intent,
/// so the tweak is dropped — leaving the file as the single answer to "what will
/// this daemon start with".
///
/// The hold is not a tuning knob and survives the save: stopping admission is an
/// operator decision the settings page never states. While held, the legacy
/// mirror keeps its zero so a rollback stays held; with no `ceiling` beside it,
/// this binary's next boot reads that zero back as the hold it already has and
/// lets the file's ceiling apply.
fn drop_tuning_overrides(orch: &Orchestrator) {
    let written = Overrides::replace(|| {
        let mut overrides =
            tuning_dropped(orch.limiter().current_hold(), orch.limiter().pinned_caps());
        // Like the hold, repo holds (ADR-0030) are not tuning and survive the save.
        overrides.repo_holds = orch.repo_holds();
        overrides
    });
    if let Err(e) = written {
        tracing::warn!("clearing tuning overrides failed: {e}");
    }
}

/// The overrides file with every tuning knob cleared: only the pins and the hold.
fn tuning_dropped(hold: Option<Hold>, pinned_caps: HashMap<String, Limits>) -> Overrides {
    Overrides {
        native_concurrency: None,
        reserve_mib: None,
        headroom_mib: None,
        margin: None,
        hard_ceiling: ceiling_mirror(None, hold.is_some()),
        ceiling: None,
        pinned_caps,
        hold,
        repo_holds: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    /// ADR-0049 §7: `POST /api/golden/verify` answers from the verifier, and
    /// names an image no profile uses rather than queueing it.
    #[test]
    fn a_verify_request_is_answered_from_the_verifier() {
        use crate::vm::verify::RequestError;
        assert_eq!(
            super::golden_verify_reply(Ok(true), "img"),
            (StatusCode::ACCEPTED, "verifying img".to_string())
        );
        assert_eq!(
            super::golden_verify_reply(Ok(false), "img").0,
            StatusCode::ACCEPTED
        );
        let (code, text) =
            super::golden_verify_reply(Err(RequestError::UnknownImage("x".into())), "x");
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(text.contains("no `macos-vm` profile names the image x"));
        assert_eq!(
            super::golden_verify_reply(Err(RequestError::NotConfigured), "x").0,
            StatusCode::CONFLICT
        );
    }

    use super::*;

    // --- ConfigUpdate.hard_ceiling: "auto" | "off" | N | null | absent -------

    #[test]
    fn hard_ceiling_update_absent_key_means_unchanged() {
        let u: ConfigUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(u.hard_ceiling, None);
    }

    #[test]
    fn hard_ceiling_update_null_means_off() {
        // The menu-bar app's "clear the ceiling" POST.
        let u: ConfigUpdate = serde_json::from_str(r#"{"hard_ceiling": null}"#).unwrap();
        assert_eq!(u.hard_ceiling, Some(HardCeiling::Off));
    }

    #[test]
    fn hard_ceiling_update_auto() {
        let u: ConfigUpdate = serde_json::from_str(r#"{"hard_ceiling": "auto"}"#).unwrap();
        assert_eq!(u.hard_ceiling, Some(HardCeiling::Auto));
    }

    #[test]
    fn hard_ceiling_update_a_number() {
        let u: ConfigUpdate = serde_json::from_str(r#"{"hard_ceiling": 6}"#).unwrap();
        assert_eq!(u.hard_ceiling, Some(HardCeiling::Fixed(6)));
    }

    #[test]
    fn hard_ceiling_update_rejects_garbage_rather_than_changing_nothing() {
        // An invalid value must fail to parse, not silently fall back to
        // "unchanged" — axum then answers 400 and `set_config` never runs, so
        // nothing is applied.
        assert!(serde_json::from_str::<ConfigUpdate>(r#"{"hard_ceiling": "bogus"}"#).is_err());
    }

    // --- the hold (issue #66) ------------------------------------------------

    /// The panel's state over the reconcile tests' bare orchestrator. Only the
    /// refusal paths are driven through it: a successful save persists to the
    /// real overrides file, which a test must never touch.
    fn test_web() -> (Web, crate::reconcile::tests::TempDir) {
        let (orch, tmp) = crate::reconcile::tests::test_orch();
        let config_path = tmp.path.join("config.toml");
        // Booted on a host's first profile, so a test's first save of
        // [`valid`] is routine: a new Docker profile is a widening of its own.
        let settings = Arc::new(SettingsStore::new(config_path.clone(), valid(), false));
        let (_updater, update) = crate::update::Updater::new(
            Arc::new(crate::github::Github::new("test-token".to_string()).unwrap()),
            orch.clone(),
            FileConfig::default().update,
            crate::update::UpdatePaths::with_root(tmp.path.join("update")),
            config_path,
            "0.0.0-test".to_string(),
            None,
        );
        let web = Web {
            orch,
            settings,
            update,
            notices: Arc::new(Vec::new()),
            trust_changes: Arc::new(TrustChanges::new(Arc::new(crate::audit::now_ms))),
            attention: Arc::new(AttentionStore::in_memory()),
            port: 0,
            save_lock: Arc::default(),
            write_overrides: false,
        };
        (web, tmp)
    }

    #[tokio::test]
    async fn a_switched_off_linux_vm_waiting_on_jobs_is_a_live_notice() {
        // ADR-0051: not one of the boot's notices, which never change: it comes
        // and goes with what the switched-off VM still waits for.
        let (web, _tmp) = test_web();
        assert!(with_update(&web).await.notices.is_empty());
        web.orch
            .limiter()
            .set_linux_vm_waiting(Some("still runs 1 docker job".into()));
        let notices = with_update(&web).await.notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, "linux_vm_waiting");
        web.orch.limiter().set_linux_vm_waiting(None);
        assert!(with_update(&web).await.notices.is_empty());
    }

    #[tokio::test]
    async fn attention_routes_store_and_serve_dismissals() {
        let (web, _tmp) = test_web();
        assert_eq!(
            serde_json::to_value(with_update(&web).await.attention).unwrap(),
            serde_json::json!({"dismissed": [], "trust_acked_ms": null})
        );

        let Json(v) = attention_dismiss(
            State(web.clone()),
            Json(DismissRequest {
                key: "memory:a".into(),
                ident: "one".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(v.attention.dismissed.len(), 1);
        let Json(v) = attention_dismiss(
            State(web.clone()),
            Json(DismissRequest {
                key: "memory:a".into(),
                ident: "two".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(v.attention.dismissed.len(), 1);
        assert_eq!(v.attention.dismissed[0].ident, "two");

        let (code, why) = attention_dismiss(
            State(web.clone()),
            Json(DismissRequest {
                key: String::new(),
                ident: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(why.contains("key"), "{why}");

        // Nothing on record: nothing to acknowledge, so nothing is stored.
        let Json(v) = attention_ack_trust(
            State(web.clone()),
            None,
            Json(AckTrustRequest { until_ms: Some(9) }),
        )
        .await;
        assert_eq!(v.attention.trust_acked_ms, None);

        // A time in the future is clamped to the newest change, so a later widening
        // is not folded into the acknowledged row.
        let first = crate::audit::now_ms();
        web.trust_changes.seed(vec![crate::audit::ControlLine {
            ts_ms: first,
            action: Some("allowed".into()),
            widenings: vec![crate::audit::WideningLine {
                key: "k".into(),
                sentence: "s".into(),
            }],
            ..Default::default()
        }]);
        let Json(v) = attention_ack_trust(
            State(web.clone()),
            None,
            Json(AckTrustRequest {
                until_ms: Some(u64::MAX),
            }),
        )
        .await;
        assert_eq!(v.attention.trust_acked_ms, Some(first));
        web.trust_changes.seed(vec![crate::audit::ControlLine {
            ts_ms: first + 5,
            action: Some("allowed".into()),
            widenings: vec![crate::audit::WideningLine {
                key: "k2".into(),
                sentence: "s2".into(),
            }],
            ..Default::default()
        }]);
        let Json(v) = attention_ack_trust(
            State(web.clone()),
            None,
            Json(AckTrustRequest {
                until_ms: Some(first),
            }),
        )
        .await;
        assert!(v.attention.trust_acked_ms.unwrap() < first + 5);
        let Json(v) = attention_ack_trust(
            State(web.clone()),
            None,
            Json(AckTrustRequest { until_ms: None }),
        )
        .await;
        assert_eq!(v.attention.trust_acked_ms, None);

        let _ = attention_dismiss(
            State(web.clone()),
            Json(DismissRequest {
                key: "cpu:b".into(),
                ident: String::new(),
            }),
        )
        .await
        .unwrap();
        let Json(v) = attention_restore(
            State(web.clone()),
            Json(RestoreRequest {
                key: Some("memory:a".into()),
            }),
        )
        .await;
        assert_eq!(v.attention.dismissed.len(), 1);
        let Json(v) = attention_restore(State(web), Json(RestoreRequest { key: None })).await;
        assert!(v.attention.dismissed.is_empty());
    }

    #[test]
    fn the_restore_body_may_leave_the_key_out() {
        let r: RestoreRequest = serde_json::from_str("{}").unwrap();
        assert!(r.key.is_none());
        let r: RestoreRequest = serde_json::from_str(r#"{"key":null}"#).unwrap();
        assert!(r.key.is_none());
    }

    async fn body_json(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn the_jobs_view_and_one_run_are_read_from_the_audit_log() {
        // ADR-0056 §5: the window is clamped to 90 days, and a run is found by
        // its runner's name, from the outcome line alone.
        let (web, _tmp) = test_web();
        let rec = crate::audit::Record {
            runner: Some("faber-7-1000".to_string()),
            label: Some("owner/demo · ci · build".to_string()),
            footprint: crate::footprint::Footprint {
                seen_ms: Some(crate::audit::now_ms() - 60_000),
                ..Default::default()
            },
            ..crate::audit::Record::new("outcome", "owner/demo", 7)
        };
        web.orch.audit_log().append(&rec).await.unwrap();

        let (status, view) =
            body_json(jobs(State(web.clone()), Query(JobsQuery { days: Some(500) })).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["window_days"], 90);
        assert_eq!(view["classes"][0]["label"], "owner/demo · ci · build");
        assert_eq!(view["classes"][0]["profiling"], true);

        let (status, one) =
            body_json(run(State(web.clone()), Path("faber-7-1000".to_string())).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(one["phases"][1]["phase"], "prepare");
        assert_eq!(one["phases"][1]["ms"], serde_json::Value::Null);

        let (status, _) = body_json(run(State(web), Path("faber-8-1000".to_string())).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A limiter on faber's default tuning.
    fn limiter() -> Arc<crate::limiter::Limiter> {
        let tuning = crate::config::AutoLimitFile::default().resolve(&Overrides::default());
        crate::limiter::Limiter::new(tuning, &HashMap::new())
    }

    #[test]
    fn a_zero_ceiling_update_is_refused_with_the_way_to_stop_admission() {
        let zero: ConfigUpdate =
            serde_json::from_str(r#"{"hard_ceiling": 0, "margin": 2.0}"#).unwrap();
        assert_eq!(zero.check(), Err(ZERO_CEILING_REFUSED));
        assert_eq!(
            ZERO_CEILING_REFUSED,
            "hard_ceiling must be at least 1 (\"off\" removes the ceiling). To stop Docker \
             admission, POST /api/hold with a reason."
        );
        for fine in [
            "{}",
            r#"{"hard_ceiling": 1}"#,
            r#"{"hard_ceiling": "off"}"#,
            r#"{"hard_ceiling": null}"#,
            r#"{"hard_ceiling": "auto", "margin": 2.0}"#,
        ] {
            let u: ConfigUpdate = serde_json::from_str(fine).unwrap();
            assert_eq!(u.check(), Ok(()), "{fine}");
        }
    }

    #[tokio::test]
    async fn a_refused_update_applies_none_of_its_fields() {
        let (w, _tmp) = test_web();
        let before = w.orch.limiter().snapshot();
        let update: ConfigUpdate =
            serde_json::from_str(r#"{"hard_ceiling": 0, "margin": 2.5, "reserve_mib": 1}"#)
                .unwrap();
        let refused = set_config(State(w.clone()), Json(update))
            .await
            .unwrap_err();
        assert_eq!(refused, (StatusCode::BAD_REQUEST, ZERO_CEILING_REFUSED));
        let after = w.orch.limiter().snapshot();
        assert_eq!(after.margin, before.margin);
        assert_eq!(after.reserve_mib, before.reserve_mib);
        assert_eq!(after.hard_ceiling_setting, before.hard_ceiling_setting);
    }

    #[tokio::test]
    async fn a_hold_without_a_reason_is_refused_and_holds_nothing() {
        let (w, _tmp) = test_web();
        for reason in ["", "   \n\t"] {
            let req = HoldRequest {
                reason: reason.to_string(),
            };
            let (status, _) = hold(State(w.clone()), Json(req)).await.unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        assert_eq!(w.orch.limiter().current_hold(), None);
    }

    #[test]
    fn a_hold_reason_is_trimmed_and_bounded() {
        assert_eq!(hold_reason("  VM resize  "), Ok("VM resize".to_string()));
        assert!(hold_reason("").is_err());
        assert!(hold_reason(" \n\t ").is_err());
        // Counted in characters, not bytes: the tray shows characters.
        let longest = "é".repeat(HOLD_REASON_MAX);
        assert_eq!(hold_reason(&longest), Ok(longest.clone()));
        assert!(hold_reason(&format!("{longest}x")).is_err());
        // A body without the key is a blank reason, refused like one.
        let req: HoldRequest = serde_json::from_str("{}").unwrap();
        assert!(hold_reason(&req.reason).is_err());
    }

    #[test]
    fn a_held_state_persists_the_hold_and_a_zero_for_older_binaries() {
        let lim = limiter();
        lim.set_tuning(None, None, None, None, Some(HardCeiling::Auto));
        lim.hold("VM resize".to_string(), 1_790_000_000_000);
        let ov = overrides_for(&lim.snapshot(), HashMap::new());
        assert_eq!(ov.hold, lim.current_hold());
        assert_eq!(ov.ceiling, Some(HardCeiling::Auto), "the true setting");
        assert_eq!(
            ov.hard_ceiling,
            Some(0),
            "what a binary from before holds reads"
        );

        let text = serde_json::to_string(&ov).unwrap();
        // Such a binary reads only the numeric key: still nothing admitted.
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["hard_ceiling"], 0);
        // This binary reads it back held, on the ceiling it was set to.
        let mut back: Overrides = serde_json::from_str(&text).unwrap();
        assert!(!back.migrate_zero_ceiling(0));
        assert_eq!(back.hold, ov.hold);
        assert_eq!(back.hard_ceiling_setting(), Some(HardCeiling::Auto));

        // Released, the mirror is the effective limit again and the key is gone.
        assert!(lim.release_hold());
        let ov = overrides_for(&lim.snapshot(), HashMap::new());
        assert_eq!(ov.hold, None);
        assert_eq!(ov.hard_ceiling, Some(crate::config::AUTO_HARD_CEILING));
        assert!(!serde_json::to_string(&ov).unwrap().contains("hold"));
    }

    #[test]
    fn a_settings_save_while_held_keeps_the_hold() {
        let held = Hold {
            reason: "VM resize".to_string(),
            since_ms: 5,
        };
        let ov = tuning_dropped(Some(held.clone()), HashMap::new());
        assert_eq!(ov.hold, Some(held.clone()));
        assert_eq!((ov.ceiling, ov.hard_ceiling), (None, Some(0)));
        // The next boot: still held, with its own date, on the file's ceiling.
        let mut back: Overrides =
            serde_json::from_str(&serde_json::to_string(&ov).unwrap()).unwrap();
        back.migrate_zero_ceiling(99);
        assert_eq!(back.hold, Some(held));
        assert_eq!(back.hard_ceiling_setting(), None);

        let ov = tuning_dropped(None, HashMap::new());
        assert_eq!((ov.hold, ov.ceiling, ov.hard_ceiling), (None, None, None));
    }

    // --- pause (ADR-0023) ----------------------------------------------------

    #[tokio::test]
    async fn state_json_carries_the_live_refused_queued_set() {
        // The dashboard reads `refused_queued` unconditionally (issue #95): an
        // empty host serves an empty array, never a missing key.
        let (w, _tmp) = test_web();
        let Json(view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["refused_queued"], serde_json::json!([]));
        // ADR-0026's dind refusals: their own key, present and empty likewise.
        assert_eq!(v["dind_refused"], serde_json::json!([]));
        // ADR-0046's service refusals: likewise.
        assert_eq!(v["service_refused"], serde_json::json!([]));
        // `[host] dind_requires_label` resolved: the test config never states it.
        assert_eq!(v["dind_label_rule"], "unstated");
    }

    /// ADR-0034: the CLI copy's gap is a field of its own, `null` where there
    /// is no SYSTEM CLI copy to judge (tests always run LEGACY), and the
    /// compact shape the popover reads when there is one.
    #[tokio::test]
    async fn state_json_reports_the_cli_copy_gap() {
        let (w, _tmp) = test_web();
        let Json(mut view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["cli_copy"], serde_json::Value::Null);

        view.cli_copy = Some(crate::upgrade_cli::cli_copy_view_for(
            "0.4.0+gabc1234",
            "0.4.1+gdef5678",
        ));
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(
            v["cli_copy"],
            serde_json::json!({
                "version": "0.4.0+gabc1234",
                "behind": true,
                "line": "CLI 0.4.0 < daemon 0.4.1: run `sudo faber upgrade-cli`",
            })
        );
    }

    /// ADR-0026 slice 11: the dashboard and the popover read `budget.vm`, and
    /// both keys are always present (an empty list, `unknown` images), so a host
    /// with no VM runtime serves a shape they can render without a guard.
    #[tokio::test]
    async fn state_json_carries_the_vm_runtime_the_uis_render() {
        let (w, _tmp) = test_web();
        let Json(view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["budget"]["vm"]["vms"], serde_json::json!([]));
        // Present and null before any supervisor list (popover/vms/anatomy.md § 5).
        assert!(v["budget"]["vm"]["listed_age_s"].is_null());
        assert!(v["budget"]["vm"].get("listed_age_s").is_some());
        assert_eq!(v["budget"]["vm"]["images"]["linux"], "unknown");
        assert_eq!(v["budget"]["vm"]["images"]["macos"], "unknown");
        assert_eq!(v["budget"]["vm"]["wall"], "unread");
        assert_eq!(v["budget"]["vm"]["macos_cap"], 2);
        // The keys the macOS refusal notes read (popover attention.ts, the dashboard).
        assert_eq!(v["budget"]["vm"]["session"], "not_required");
        assert!(v["budget"]["vm"].get("refusal").is_some());
        assert_eq!(v["budget"]["vm"]["golden_required"], false);
        assert!(v["budget"]["gates"].is_object());
    }

    /// ADR-0026 slice 6b: the daemon's judgement of the Linux image reaches the
    /// key the dashboard's "Linux image out of date" note reads, and leaves the
    /// golden image's standing alone.
    #[tokio::test]
    async fn state_json_shows_an_out_of_date_linux_image() {
        let (w, _tmp) = test_web();
        w.orch
            .limiter()
            .observe_linux_image(crate::limiter::ImageStanding::OutOfDate);
        let Json(view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["budget"]["vm"]["images"]["linux"], "out_of_date");
        assert_eq!(v["budget"]["vm"]["images"]["macos"], "unknown");
    }

    /// ADR-0026: what the runner-image loader found wrong reaches the key the
    /// dashboard's runner-image note and the tray's attention row read.
    #[tokio::test]
    async fn state_json_shows_a_missing_runner_image() {
        let (w, _tmp) = test_web();
        let Json(view) = state(State(w.clone())).await;
        let v = serde_json::to_value(&view).unwrap();
        assert!(v["budget"]["vm"]["runner_image"].is_null());
        w.orch
            .limiter()
            .observe_runner_image(Some("runner image demo is missing, no kept backup".into()));
        let Json(view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(
            v["budget"]["vm"]["runner_image"],
            "runner image demo is missing, no kept backup"
        );
    }

    #[tokio::test]
    async fn state_json_says_whether_idle_sleep_is_held_off() {
        // Issue #90: off by default, so a host that never set the key shows it off
        // and not held; what the loop publishes is what the dashboard reads.
        let (w, _tmp) = test_web();
        let Json(view) = state(State(w.clone())).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["awake"]["enabled"], false);
        assert_eq!(v["awake"]["held"], false);

        w.orch.publish_awake(crate::awake::AwakeView {
            enabled: true,
            mode: crate::config::SleepPolicy::Always,
            supported: true,
            held: true,
            held_since_ms: Some(1_000),
            release_at_ms: None,
            error: None,
        });
        let Json(view) = state(State(w)).await;
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["awake"]["held"], true);
        assert_eq!(v["awake"]["held_since_ms"], 1_000);
        assert_eq!(v["awake"]["mode"], "always");
    }

    #[tokio::test]
    async fn the_pause_buttons_speak_for_the_operator() {
        // Pause takes a janitor's pause over, so the end of the reclamation
        // cannot lift it; Resume lifts either, as it always has.
        let (w, _tmp) = test_web();
        assert!(w.orch.pause(Pause::Reclaim));
        let _ = pause(State(w.clone())).await;
        assert!(!w.orch.resume(Pause::Reclaim), "the operator's now");
        assert_eq!(w.orch.limiter().current_pause(), Some(Pause::Operator));
        let _ = resume(State(w.clone())).await;
        assert!(!w.orch.is_paused());

        assert!(w.orch.pause(Pause::Reclaim));
        let _ = resume(State(w.clone())).await;
        assert!(!w.orch.is_paused(), "Resume lifts the janitor's pause too");
    }

    // --- the disk admission floor (ADR-0024) ---------------------------------

    #[tokio::test]
    async fn a_settings_save_refuses_an_admission_floor_above_the_reclamation_floor() {
        // Admission would stop before reclamation had started. Refused where the
        // operator is looking, and nothing reaches the file.
        let (w, tmp) = test_web();
        let mut next = FileConfig {
            profiles: vec![crate::config::FileProfile {
                name: "linux-docker".into(),
                labels: vec!["faber".into(), "docker".into()],
                backend: crate::config::BackendKind::Docker,
                image: Some("ghcr.io/owner/my-runner:latest".into()),
                cpus: None,
                memory_mib: None,
                dind: Some(false),
                dind_image: None,
                cache_volume: false,
                git_mirror: false,
                mirror: false,
                drop_grants: Vec::new(),
                env: Default::default(),
            }],
            ..FileConfig::default()
        };
        next.host.disk_floor_gib = 25;
        next.host.disk_admission_floor_gib = Some(30);
        let (status, Json(result)) = save_settings(
            State(w.clone()),
            Some(Extension(Caller::Session)),
            Json(next),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!result.saved);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("host.disk_admission_floor_gib (30 GiB) is above")),
            "{:?}",
            result.errors
        );
        assert!(!tmp.path.join("config.toml").exists(), "nothing written");
    }

    // --- who may widen trust (ADR-0028) ---------------------------------------

    /// The daemon's routes merge behind both of the front door's listeners
    /// (axum panics on a clashing route only when the router is built), and a
    /// loopback request without a session never reaches them.
    #[tokio::test]
    async fn the_routes_sit_behind_the_front_door() {
        use tower::ServiceExt;
        let (w, _tmp) = test_web();
        let door = FrontDoor::new(
            8787,
            None,
            0o600,
            None,
            crate::standby::euid(),
            Arc::new(crate::control::Sessions::system()),
        );
        let _socket = door.socket_router(routes(w.clone()));
        let loopback = door.loopback_router(routes(w));
        let req = axum::http::Request::builder()
            .uri("/api/state")
            .header("host", "127.0.0.1:8787")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = loopback.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // --- the React bundle behind the front door (ADR-0028 addendum C) ---------

    /// The daemon's real routes behind the loopback guard.
    fn bundle_door() -> (Arc<FrontDoor>, Router, crate::reconcile::tests::TempDir) {
        let (w, tmp) = test_web();
        let door = FrontDoor::new(
            8787,
            None,
            0o600,
            None,
            crate::standby::euid(),
            Arc::new(crate::control::Sessions::system()),
        );
        let loopback = door.loopback_router(routes(w));
        (door, loopback, tmp)
    }

    async fn bundle_call(
        router: &Router,
        method: axum::http::Method,
        path: &str,
        session: Option<&crate::control::Redeemed>,
        origin: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        use tower::ServiceExt;
        let mut b = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8787");
        if let Some(r) = session {
            b = b
                .header("cookie", format!("faber_session={}", r.cookie))
                .header("x-faber-session", &r.key);
        }
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        let resp = router
            .clone()
            .oneshot(b.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    /// Every `assets/` entry of the embedded table answers without a session,
    /// byte for byte, with the immutable cache, `nosniff` and no CORS; nothing
    /// that is not an asset does.
    #[tokio::test]
    async fn each_bundled_asset_is_served_without_a_session() {
        use axum::http::Method;
        let (_door, router, _tmp) = bundle_door();
        let assets: Vec<_> = ui_bundle::FILES
            .iter()
            .filter(|(name, _)| name.starts_with("assets/"))
            .collect();
        assert_eq!(
            assets.is_empty(),
            !ui_bundle::built(),
            "a built bundle has assets"
        );
        for (name, bytes) in assets {
            let path = format!("/{name}");
            let (status, headers, body) =
                bundle_call(&router, Method::GET, &path, None, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(body, *bytes, "{path}");
            assert_eq!(headers["content-type"], ui_bundle::content_type(name));
            assert_eq!(headers["x-content-type-options"], "nosniff");
            assert_eq!(
                headers["cache-control"],
                "public, max-age=31536000, immutable"
            );
            assert!(
                !headers
                    .keys()
                    .any(|k| k.as_str().starts_with("access-control-")),
                "{path}: {headers:?}"
            );
            let (status, _, body) = bundle_call(&router, Method::HEAD, &path, None, None).await;
            assert_eq!(status, StatusCode::OK, "HEAD {path}");
            assert!(body.is_empty(), "HEAD {path}");
        }
        for path in [
            "/",
            "/activity",
            "/jobs",
            "/memory",
            "/settings",
            "/api/state",
            "/api/jobs",
            "/api/settings",
            "/nowhere",
            "/assets",
        ] {
            let (status, _, body) = bundle_call(&router, Method::GET, path, None, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(body, crate::control::SIGN_IN_HTML.as_bytes(), "{path}");
        }
    }

    /// No listing, no traversal, no shell and no disk under `/assets/`: only an
    /// exact key of the embedded table is a file, built or not.
    #[tokio::test]
    async fn nothing_but_an_exact_bundled_name_is_an_asset() {
        use axum::http::Method;
        let (_door, router, _tmp) = bundle_door();
        for path in [
            "/assets/",
            "/assets/no-such-file.js",
            "/assets/index.html",
            "/assets/../index.html",
            "/assets/../api/state",
            "/assets/%2e%2e/index.html",
            "/assets/%2e%2e/x",
            "/assets/%2e%2e%2findex.html",
            "/assets/%252e%252e/index.html",
            "/assets/..%5cindex.html",
            "/assets/..\\index.html",
            "/assets//etc/passwd",
            "/assets/./x.js",
        ] {
            let Ok(uri) = path.parse::<axum::http::Uri>() else {
                continue; // refused as a URI before any route: never a 200
            };
            let (status, _, body) =
                bundle_call(&router, Method::GET, &uri.to_string(), None, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert!(body.is_empty(), "{path}: not the shell, not a file");
        }
        if !ui_bundle::built() {
            let (status, _, _) =
                bundle_call(&router, Method::GET, "/assets/index.js", None, None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "an unbuilt binary has no assets"
            );
        }
    }

    /// A mutation under `/assets/` runs no handler: `401` without a session,
    /// `403` without the exact `Origin`, `405` with both.
    #[tokio::test]
    async fn an_asset_accepts_no_mutation() {
        use axum::http::Method;
        let (door, router, _tmp) = bundle_door();
        let token = door.sessions().mint_link().unwrap();
        let r = door.sessions().redeem(&token).unwrap().unwrap();
        let path = ui_bundle::FILES
            .iter()
            .find(|(n, _)| n.starts_with("assets/"))
            .map_or("/assets/index.js".to_string(), |(n, _)| format!("/{n}"));
        let exact = Some("http://127.0.0.1:8787");
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            let none = bundle_call(&router, method.clone(), &path, None, exact).await;
            assert_eq!(none.0, StatusCode::UNAUTHORIZED, "{method}");
            let bad = bundle_call(&router, method.clone(), &path, Some(&r), None).await;
            assert_eq!(bad.0, StatusCode::FORBIDDEN, "{method}");
            let both = bundle_call(&router, method.clone(), &path, Some(&r), exact).await;
            assert_eq!(both.0, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        }
    }

    /// Every view (`/`, `/activity`, `/memory`, `/settings`) is the React
    /// shell (or the not-built page) behind the cookie, as a navigation carries
    /// no key (ADR-0054 § 5). Nothing beside them is: no trailing slash, and no
    /// view the shell does not have yet.
    #[tokio::test]
    async fn each_view_serves_the_shell_on_the_cookie_alone() {
        use axum::http::Method;
        let (door, router, _tmp) = bundle_door();
        let token = door.sessions().mint_link().unwrap();
        let r = door.sessions().redeem(&token).unwrap().unwrap();
        let shell = ui_bundle::asset("index.html").unwrap().0;
        if !ui_bundle::built() {
            assert_eq!(shell, ui_bundle::NOT_BUILT_HTML.as_bytes());
        }
        let cookie_only = |path: &str| {
            axum::http::Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:8787")
                .header("cookie", format!("faber_session={}", r.cookie))
                .body(axum::body::Body::empty())
                .unwrap()
        };
        for view in [
            "/",
            "/activity",
            "/jobs",
            "/memory",
            "/maintenance",
            "/settings",
        ] {
            let (status, headers, body) =
                bundle_call(&router, Method::GET, view, Some(&r), None).await;
            assert_eq!(status, StatusCode::OK, "{view}");
            assert_eq!(body, shell, "{view}");
            assert_eq!(headers["content-type"], "text/html; charset=utf-8");

            let resp = {
                use tower::ServiceExt;
                router.clone().oneshot(cookie_only(view)).await.unwrap()
            };
            assert_eq!(resp.status(), StatusCode::OK, "{view}: the cookie alone");

            let (status, _, body) = bundle_call(&router, Method::GET, view, None, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{view}: no session");
            assert_eq!(body, crate::control::SIGN_IN_HTML.as_bytes());
        }

        // Never under a trailing slash, where its relative asset URLs would
        // break, and not at a view ADR-0054 has not opened yet.
        for path in [
            "/activity/",
            "/memory/",
            "/maintenance/",
            "/settings/",
            "/repositories",
        ] {
            let (status, _, body) = bundle_call(&router, Method::GET, path, Some(&r), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_ne!(body, shell, "{path}");
            let resp = {
                use tower::ServiceExt;
                router.clone().oneshot(cookie_only(path)).await.unwrap()
            };
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{path}: not a navigated page, so the cookie alone is not enough"
            );
        }

        // The API still wants the key beside the cookie.
        let resp = {
            use tower::ServiceExt;
            router
                .clone()
                .oneshot(cookie_only("/api/state"))
                .await
                .unwrap()
        };
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let (status, _, _) = bundle_call(&router, Method::GET, "/api/state", Some(&r), None).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A name with a newline in it cannot start a forged log line.
    #[test]
    fn widening_sentences_are_quoted_for_the_log() {
        let w = crate::control::Widening {
            key: "profile.x.dind".into(),
            sentence: "profile x\nERROR forged grants its jobs a Docker daemon".into(),
        };
        let line = log_sentences(&[w.clone(), w]);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("\\n"), "{line}");
        assert!(line.starts_with('"'), "{line}");
    }

    /// A save waits for one already comparing and writing.
    #[tokio::test]
    async fn saves_are_serialised() {
        let (w, _tmp) = test_web();
        let held = w.save_lock.clone().lock_owned().await;
        let save = tokio::spawn(save_settings(
            State(w.clone()),
            None,
            Json(FileConfig::default()),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !save.is_finished(),
            "a save ran while another held the lock"
        );
        drop(held);
        let (status, _) = save.await.unwrap();
        // No caller: refused, but only once the lock was free.
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// A save with no caller on record is refused outright, nothing reaches the
    /// file, and the widening it carried is audited as refused.
    #[tokio::test]
    async fn a_widening_save_without_a_caller_is_refused_and_audited() {
        let (w, tmp) = test_web();
        let mut next = FileConfig::default();
        next.update.pin = Some("0.4.0".into());
        let (status, Json(result)) = save_settings(State(w.clone()), None, Json(next)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!result.saved);
        assert_eq!(result.refused.len(), 1);
        assert_eq!(result.refused[0].key, "update.pin");
        assert!(!tmp.path.join("config.toml").exists(), "nothing written");

        let lines = w.orch.audit_log().read_kind(crate::audit::CONTROL);
        assert_eq!(lines.len(), 1);
        let line = serde_json::to_value(&lines[0]).unwrap();
        assert_eq!(line["kind"], "control");
        assert_eq!(line["action"], "refused");
    }

    /// A routine save with no caller is refused too, but it widens nothing, so
    /// it leaves no audit line.
    #[tokio::test]
    async fn a_routine_save_without_a_caller_is_refused_unaudited() {
        let (w, tmp) = test_web();
        let (status, Json(result)) =
            save_settings(State(w.clone()), None, Json(FileConfig::default())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(result.refused.is_empty());
        assert!(!tmp.path.join("config.toml").exists());
        assert!(w
            .orch
            .audit_log()
            .read_kind(crate::audit::CONTROL)
            .is_empty());
    }

    fn root() -> Caller {
        Caller::Socket {
            uid: 0,
            class: Class::Present,
        }
    }

    fn operator() -> Caller {
        Caller::Socket {
            uid: 501,
            class: Class::Operator,
        }
    }

    /// A config a save accepts.
    fn valid() -> FileConfig {
        FileConfig {
            profiles: vec![crate::config::FileProfile {
                name: "linux-docker".into(),
                labels: vec!["faber".into(), "docker".into()],
                backend: crate::config::BackendKind::Docker,
                image: Some("ghcr.io/owner/my-runner:latest".into()),
                cpus: None,
                memory_mib: None,
                dind: Some(false),
                dind_image: None,
                cache_volume: false,
                git_mirror: false,
                mirror: false,
                drop_grants: Vec::new(),
                env: Default::default(),
            }],
            ..FileConfig::default()
        }
    }

    /// Issue #78: an operator's widening save is a `403` naming the command
    /// that makes it, audited as refused, with nothing written; root's is
    /// written, audited as allowed and on the notice; a routine save passes for
    /// the operator, unaudited.
    #[tokio::test]
    async fn a_widening_save_needs_root() {
        let (w, tmp) = test_web();
        let path = tmp.path.join("config.toml");
        let mut wide = valid();
        wide.update.pin = Some("0.4.1".into());
        for caller in [Caller::Session, operator()] {
            let (status, Json(result)) = save_settings(
                State(w.clone()),
                Some(Extension(caller)),
                Json(wide.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?}");
            assert_eq!(result.refused.len(), 1);
            assert_eq!(
                result.refused[0].command,
                "sudo faber config set update.pin 0.4.1"
            );
            assert!(result.errors[0].contains("present caller (sudo)"));
            assert!(!path.exists(), "nothing written");
        }
        let refused = w.orch.audit_log().read_control_since(0);
        assert_eq!(refused.len(), 2);
        assert!(refused
            .iter()
            .all(|l| l.action.as_deref() == Some("refused")));

        let (status, Json(result)) =
            save_settings(State(w.clone()), Some(Extension(operator())), Json(valid())).await;
        assert_eq!(status, StatusCode::OK, "{:?}", result.errors);
        assert!(result.widened.is_empty());
        assert_eq!(
            w.orch.audit_log().read_control_since(0).len(),
            2,
            "unaudited"
        );

        let (status, Json(result)) =
            save_settings(State(w.clone()), Some(Extension(root())), Json(wide)).await;
        assert_eq!(status, StatusCode::OK, "{:?}", result.errors);
        assert_eq!(result.widened.len(), 1);
        assert_eq!(
            FileConfig::load(Some(&path))
                .unwrap()
                .unwrap()
                .update
                .pin
                .as_deref(),
            Some("0.4.1")
        );
        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].action.as_deref(), Some("allowed"));
        assert_eq!(lines[2].route.as_deref(), Some("POST /api/settings"));
        assert_eq!(w.trust_changes.list().len(), 1);
    }

    async fn set_key(
        w: &Web,
        caller: Caller,
        key: &str,
        value: Option<&str>,
    ) -> (StatusCode, SaveResult) {
        let (status, Json(result)) = save_settings_key(
            State(w.clone()),
            Some(Extension(caller)),
            Json(SettingsKey {
                key: key.into(),
                value: value.map(String::from),
            }),
        )
        .await;
        (status, result)
    }

    /// `faber config set`'s route: one key on the file as it is, through the
    /// same pipeline, every attempt on record under its own route.
    #[tokio::test]
    async fn one_key_is_saved_through_the_same_pipeline() {
        let (w, tmp) = test_web();
        let path = tmp.path.join("config.toml");
        valid().save_to(&path).unwrap();

        // Narrowing works as the operator, and is audited without widenings.
        let (status, r) = set_key(&w, operator(), "update.maturity_secs", Some("172800")).await;
        assert_eq!(status, StatusCode::OK, "{:?}", r.errors);
        assert_eq!(
            FileConfig::load(Some(&path))
                .unwrap()
                .unwrap()
                .update
                .maturity_secs,
            172_800
        );
        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action.as_deref(), Some("allowed"));
        assert_eq!(lines[0].route.as_deref(), Some("POST /api/settings/key"));
        assert!(lines[0].widenings.is_empty());
        assert!(w.trust_changes.list().is_empty(), "no widening, no notice");

        // Widening is refused the operator, with the command.
        let (status, r) = set_key(&w, operator(), "update.pin", Some("0.4.1")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            r.refused[0].command,
            "sudo faber config set update.pin 0.4.1"
        );
        assert_eq!(
            FileConfig::load(Some(&path)).unwrap().unwrap().update.pin,
            None
        );

        // ...and allowed root.
        let (status, r) = set_key(&w, root(), "update.pin", Some("0.4.1")).await;
        assert_eq!(status, StatusCode::OK, "{:?}", r.errors);
        assert_eq!(r.widened[0].key, "update.pin");
        let cfg = FileConfig::load(Some(&path)).unwrap().unwrap();
        assert_eq!(cfg.update.pin.as_deref(), Some("0.4.1"));
        assert_eq!(
            cfg.update.maturity_secs, 172_800,
            "the earlier change stays"
        );
        let lines = w.orch.audit_log().read_control_since(0);
        let actions: Vec<_> = lines.iter().map(|l| l.action.as_deref()).collect();
        assert_eq!(actions, [Some("allowed"), Some("refused"), Some("allowed")]);
        assert_eq!(w.trust_changes.list().len(), 1);

        // A key or value `edit` refuses is a 400, judged and written nowhere.
        let (status, r) = set_key(&w, root(), "update.nonsense", Some("1")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(r.errors[0].contains("not a config key"), "{:?}", r.errors);
        // A value validation refuses is a 400 too, once the caller may make it
        // (under the floor, 60 applies as the 24 h default: lower than 48 h).
        let (status, r) = set_key(&w, root(), "update.maturity_secs", Some("60")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            r.errors.iter().any(|e| e.contains("maturity_secs")),
            "{:?}",
            r.errors
        );
        // Both failed attempts are on record as refused, with why; neither is
        // on the notice.
        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[3].action.as_deref(), Some("refused"));
        assert_eq!(lines[4].action.as_deref(), Some("refused"));
        assert_eq!(lines[4].widenings[0].key, "update.maturity_secs");
        let raw = std::fs::read_to_string(w.orch.audit_log().path()).unwrap();
        assert!(raw.contains("not a config key"), "{raw}");
        assert_eq!(w.trust_changes.list().len(), 1);
    }

    /// A widening save by root that fails validation leaves a refused line;
    /// one through `/api/settings` that widens nothing and fails leaves none.
    #[tokio::test]
    async fn a_failed_widening_save_is_on_record() {
        let (w, _tmp) = test_web();
        let mut bad = FileConfig::default(); // no profile: invalid
        bad.update.pin = Some("0.4.1".into());
        let (status, _) = save_settings(State(w.clone()), Some(Extension(root())), Json(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action.as_deref(), Some("refused"));
        assert!(w.trust_changes.list().is_empty());

        let (status, _) = save_settings(
            State(w.clone()),
            Some(Extension(root())),
            Json(FileConfig::default()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(w.orch.audit_log().read_control_since(0).len(), 1);
    }

    /// `?dry_run=1` reports what a save would widen, what this caller would be
    /// refused, and what validation says, and does none of it.
    #[tokio::test]
    async fn a_dry_run_judges_and_writes_nothing() {
        let (w, tmp) = test_web();
        let mut wide = valid();
        wide.update.pin = Some("0.4.1".into());
        let dry = |caller: Caller, cfg: FileConfig| {
            save_settings_route(
                State(w.clone()),
                Some(Extension(caller)),
                Query(SaveQuery {
                    dry_run: Some("1".into()),
                    base: None,
                }),
                Json(cfg),
            )
        };
        let (status, Json(r)) = dry(operator(), wide.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!r.saved);
        assert_eq!(r.widened[0].key, "update.pin");
        assert_eq!(
            r.refused[0].command,
            "sudo faber config set update.pin 0.4.1"
        );
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let (_, Json(r)) = dry(root(), wide).await;
        assert!(r.refused.is_empty());
        assert_eq!(r.widened.len(), 1);
        let (_, Json(r)) = dry(root(), FileConfig::default()).await;
        assert!(!r.errors.is_empty());
        assert!(!tmp.path.join("config.toml").exists(), "nothing written");
        assert!(w.orch.audit_log().read_control_since(0).is_empty());
    }

    /// #221: a save sent with the dry run's base lands while the config is
    /// what the dry run judged, and is a `409` that writes nothing once
    /// another save has moved it.
    #[tokio::test]
    async fn a_save_onto_a_moved_base_is_a_conflict() {
        let (w, tmp) = test_web();
        let mut wide = valid();
        wide.update.pin = Some("0.4.1".into());
        let route = |q: SaveQuery, cfg: FileConfig| {
            save_settings_route(
                State(w.clone()),
                Some(Extension(root())),
                Query(q),
                Json(cfg),
            )
        };
        let dry = || SaveQuery {
            dry_run: Some("1".into()),
            base: None,
        };
        let on = |base: Option<String>| SaveQuery {
            dry_run: None,
            base,
        };
        let (_, Json(r)) = route(dry(), wide.clone()).await;
        let judged = r.base.expect("a dry run names its base");
        assert_eq!(judged.len(), 64);

        // Another save moves the base after the dry run.
        let mut moved = valid();
        moved.poll_interval_secs += 1;
        let (status, _) = route(on(None), moved.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let written = std::fs::read_to_string(tmp.path.join("config.toml")).unwrap();
        let audited = w.orch.audit_log().read_control_since(0).len();

        let (status, Json(r)) = route(on(Some(judged)), wide.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!r.saved);
        assert!(
            r.errors[0].contains("changed after the dry run"),
            "{:?}",
            r.errors
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path.join("config.toml")).unwrap(),
            written,
            "nothing written"
        );
        assert_eq!(w.orch.audit_log().read_control_since(0).len(), audited);

        // A fresh dry run names the moved base, and the save onto it lands.
        let (_, Json(r)) = route(dry(), wide.clone()).await;
        let (status, Json(r)) = route(on(r.base), wide).await;
        assert_eq!(status, StatusCode::OK, "{:?}", r.errors);
        assert!(r.saved);
    }

    // --- the raw editor: config.toml as text -----------------------------------

    /// A web whose daemon booted from `config.toml`, written as `text`.
    fn raw_web(text: &str) -> (Web, crate::reconcile::tests::TempDir, std::path::PathBuf) {
        let (mut w, tmp) = test_web();
        let path = tmp.path.join("config.toml");
        std::fs::write(&path, text).unwrap();
        w.settings = Arc::new(SettingsStore::new(path.clone(), valid(), true));
        (w, tmp, path)
    }

    /// A form save over a file whose `[mirror.lfs]` has no `[mirror]` never
    /// turns the mirror on: sent back as the form got it, it is refused as the
    /// file is; with the mark dropped, it is a widening the operator is refused.
    /// Nothing is written either way.
    #[tokio::test]
    async fn a_form_save_never_turns_an_implicit_mirror_on() {
        let text = format!("{}\n[mirror.lfs]\n", text_of(&valid()));
        let (w, _tmp, path) = raw_web(&text);
        let mut next: FileConfig =
            serde_json::from_value(serde_json::to_value(w.settings.current().config).unwrap())
                .unwrap();
        next.poll_interval_secs += 1;
        let (status, Json(r)) = save_settings(
            State(w.clone()),
            Some(Extension(operator())),
            Json(next.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}", r.errors);
        assert!(r
            .errors
            .iter()
            .any(|e| e.contains("need a [mirror] section")));

        next.mirror.as_mut().unwrap().implicit = false;
        let (status, Json(r)) =
            save_settings(State(w.clone()), Some(Extension(operator())), Json(next)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(r.refused[0].key, "mirror");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "nothing written"
        );
    }

    async fn raw_get(w: &Web) -> (StatusCode, serde_json::Value) {
        let resp = get_settings_raw(State(w.clone())).await;
        let status = resp.status();
        (
            status,
            serde_json::from_str(&body_text(resp).await).unwrap(),
        )
    }

    async fn raw_post(
        w: &Web,
        caller: Caller,
        base: Option<String>,
        dry: bool,
        text: &str,
    ) -> (StatusCode, serde_json::Value) {
        let resp = save_settings_raw(
            State(w.clone()),
            Some(Extension(caller)),
            Query(SaveQuery {
                dry_run: dry.then(|| "1".to_string()),
                base,
            }),
            Json(RawText { text: text.into() }),
        )
        .await;
        let status = resp.status();
        (
            status,
            serde_json::from_str(&body_text(resp).await).unwrap(),
        )
    }

    /// `cfg` as the raw editor would hold it: TOML, with a comment.
    fn text_of(cfg: &FileConfig) -> String {
        format!("# the operator's note\n{}", cfg.to_toml().unwrap())
    }

    /// The text the raw tests start from: a valid config, with a comment.
    fn commented() -> String {
        text_of(&valid())
    }

    /// [`valid`], with one routine change.
    fn routine(by: u64) -> FileConfig {
        let mut c = valid();
        c.poll_interval_secs += by;
        c
    }

    /// [`valid`], widened: its Docker profile granted dind (ADR-0013).
    fn widened() -> FileConfig {
        let mut c = valid();
        c.profiles[0].dind = Some(true);
        c
    }

    #[tokio::test]
    async fn a_raw_save_writes_the_text_verbatim_comments_kept() {
        let (w, _tmp, path) = raw_web(&commented());
        let (status, got) = raw_get(&w).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["text"], commented());
        assert_eq!(got["path"], path.display().to_string());
        let base = got["base"].as_str().unwrap().to_string();

        let next = format!("{}\n# and one more, after the edit\n", text_of(&routine(1)));
        let (status, r) = raw_post(&w, operator(), Some(base), false, &next).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["saved"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            commented(),
            "the displaced file is kept, as the form's save keeps it"
        );
        let (_, got) = raw_get(&w).await;
        assert_eq!(got["text"], next);
    }

    #[tokio::test]
    async fn a_raw_widening_is_refused_and_nothing_is_written() {
        let (w, _tmp, path) = raw_web(&commented());
        let wide = text_of(&widened());
        let (status, r) = raw_post(&w, operator(), None, false, &wide).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{r}");
        assert_eq!(r["saved"], false);
        assert!(
            r["refused"].as_array().is_some_and(|a| !a.is_empty()),
            "{r}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());
        assert!(!path.with_extension("toml.bak").exists());
    }

    #[tokio::test]
    async fn a_raw_toml_error_names_its_line_and_writes_nothing() {
        let (w, _tmp, path) = raw_web(&commented());
        let broken = "topic = \"faber\"\n\n[autolimit\nmargin = 1.2\n";
        let (status, r) = raw_post(&w, operator(), None, false, broken).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        assert_eq!(r["line"], 3, "{r}");
        assert!(r["column"].as_u64().unwrap() >= 1);
        assert!(!r["message"].as_str().unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());
        // A dry run parses first too.
        let (status, _) = raw_post(&w, operator(), None, true, broken).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_raw_parse_error_counts_lines_and_characters() {
        let e = parse_raw("# é\ntopic = = 1\n").unwrap_err();
        assert_eq!(e.line, 2, "{e:?}");
        assert!(e.column > 1, "{e:?}");
        // A value of the wrong type is not TOML syntax, but still has a place.
        let e = parse_raw("poll_interval_secs = \"soon\"\n").unwrap_err();
        assert_eq!(e.line, 1, "{e:?}");
    }

    #[tokio::test]
    async fn a_raw_save_onto_a_stale_base_is_a_conflict() {
        let (w, _tmp, path) = raw_web(&commented());
        let (_, got) = raw_get(&w).await;
        let base = got["base"].as_str().unwrap().to_string();
        // The file moves after the editor loaded it.
        let moved = text_of(&routine(15));
        std::fs::write(&path, &moved).unwrap();
        let next = text_of(&routine(1));
        let (status, r) = raw_post(&w, operator(), Some(base), false, &next).await;
        assert_eq!(status, StatusCode::CONFLICT, "{r}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), moved);
    }

    #[tokio::test]
    async fn a_raw_dry_run_judges_and_writes_nothing() {
        let (w, _tmp, path) = raw_web(&commented());
        let wide = text_of(&widened());
        let (status, r) = raw_post(&w, operator(), None, true, &wide).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["saved"], false);
        assert!(
            r["refused"].as_array().is_some_and(|a| !a.is_empty()),
            "{r}"
        );
        assert_eq!(r["base"].as_str().map(str::len), Some(64));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());
        assert!(!path.with_extension("toml.bak").exists());
    }

    #[tokio::test]
    async fn a_comment_only_change_moves_the_raw_base() {
        let (w, _tmp, path) = raw_web(&commented());
        let (_, got) = raw_get(&w).await;
        let base = got["base"].as_str().unwrap().to_string();
        assert_eq!(base.len(), 64);
        // Someone adds a comment only: the config it parses to is unchanged.
        let moved = format!("{}# a note added by hand\n", commented());
        std::fs::write(&path, &moved).unwrap();
        let next = text_of(&routine(1));
        let (status, r) = raw_post(&w, operator(), Some(base), false, &next).await;
        assert_eq!(status, StatusCode::CONFLICT, "{r}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            moved,
            "nothing written"
        );
        // The fresh base lands.
        let (_, got) = raw_get(&w).await;
        let base = got["base"].as_str().unwrap().to_string();
        let (status, r) = raw_post(&w, operator(), Some(base), false, &next).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
    }

    #[tokio::test]
    async fn a_raw_save_refuses_a_key_faber_does_not_read() {
        let (w, _tmp, path) = raw_web(&commented());
        // `[host]` takes unknown keys when it parses (boot must not refuse a
        // file the release did not write), so the raw save is what catches it.
        assert!(commented().contains("\n[host]\n"));
        let typo = commented().replace("\n[host]\n", "\n[host]\nrevive_dockr = true\n");
        let line = typo
            .lines()
            .position(|l| l.starts_with("revive_dockr"))
            .unwrap()
            + 1;
        assert!(parse_raw(&typo).is_err());
        assert!(
            FileConfig::load_from_str(&typo, "a test").is_ok(),
            "boot is unchanged"
        );
        let (status, r) = raw_post(&w, operator(), None, false, &typo).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        assert_eq!(r["keys"], serde_json::json!(["host.revive_dockr"]), "{r}");
        assert_eq!(r["line"], line, "{r}");
        assert_eq!(r["column"], 1, "{r}");
        assert!(r["message"].as_str().unwrap().contains("host.revive_dockr"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());
        // A misspelt key in a table that refuses one is a TOML error naming it.
        let typo = format!("pol_interval_secs = 5\n{}", commented());
        let (status, r) = raw_post(&w, operator(), None, true, &typo).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        assert!(
            r["message"].as_str().unwrap().contains("pol_interval_secs"),
            "{r}"
        );
    }

    /// A body with more keys than any config is refused at once, before any
    /// probing, and the probe itself is not quadratic in the file: a body just
    /// under the cap, every key unknown, is judged promptly too.
    #[tokio::test]
    async fn a_raw_body_with_too_many_keys_is_refused_promptly() {
        let (w, _tmp, path) = raw_web(&commented());
        let many: String = (0..50_000).map(|i| format!("k{i} = {i}\n")).collect();
        let text = commented().replace("\n[host]\n", &format!("\n[host]\n{many}"));
        let started = std::time::Instant::now();
        let (status, r) = raw_post(&w, operator(), None, true, &text).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        assert!(
            r["message"].as_str().unwrap().starts_with("too many keys"),
            "{r}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());

        let under = MAX_PROBED_KEYS
            - count_keys(&toml::Value::Table(
                commented().parse::<toml::Table>().unwrap(),
            ));
        let some: String = (0..under).map(|i| format!("k{i} = {i}\n")).collect();
        let text = commented().replace("\n[host]\n", &format!("\n[host]\n{some}"));
        let started = std::time::Instant::now();
        let (status, r) = raw_post(&w, operator(), None, true, &text).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        assert_eq!(r["keys"].as_array().map(Vec::len), Some(under), "{r}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    /// A table under a profile (which needs its `name`) is probed in the
    /// profile alone, not the whole file per key: a large `env` is judged
    /// promptly, and its user-named keys stay known.
    #[test]
    fn a_large_table_under_a_profile_is_probed_promptly() {
        let mut c = valid();
        for i in 0..1500 {
            c.profiles[0].env.insert(format!("VAR_{i}"), i.to_string());
        }
        let text = text_of(&c);
        let started = std::time::Instant::now();
        assert_eq!(unknown_keys(&text), Vec::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    /// A deny entry already in the file never blocks a save, a narrowing one
    /// included (removing it would widen trust); one the save adds is judged.
    #[tokio::test]
    async fn only_the_list_entries_a_save_adds_are_judged() {
        let mut old = valid();
        // An Enterprise Managed User login, and one no pattern takes.
        old.denied_actors = vec!["handle_shortcode".into(), "not a login".into()];
        let (w, _tmp, path) = raw_web(&text_of(&old));
        let mut next = old.clone();
        next.denied_actors.push("someone-else".into());
        let (status, r) = raw_post(&w, operator(), None, false, &text_of(&next)).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text_of(&next));

        let mut bad = next.clone();
        bad.denied_actors.push("new one".into());
        let (status, r) = raw_post(&w, operator(), None, false, &text_of(&bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{r}");
        let errors = r["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{r}");
        assert!(
            errors[0].as_str().unwrap().starts_with("denied_actors[3]"),
            "{r}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text_of(&next));
    }

    /// Nothing faber reads is mistaken for an unknown key: user-named entries
    /// (`env`, `[needs.<tool>]`), arrays of tables and optional sections included.
    #[test]
    fn a_full_config_has_no_unknown_keys() {
        let mut c = valid();
        c.profiles[0]
            .env
            .insert("HTTPS_PROXY".into(), "http://10.0.0.2:3128".into());
        c.profiles[0].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()];
        c.profiles.push(c.profiles[0].clone());
        c.profiles[1].name = "second".into();
        c.autolimit.priority_repos = vec!["owner/demo".into()];
        c.trusted_actors = vec!["owner".into()];
        c.runner_timeout_secs = Some(60);
        let text = text_of(&c);
        assert!(text.contains("[[profile]]") && text.contains("HTTPS_PROXY"));
        assert_eq!(unknown_keys(&text), Vec::new());
        let inline = "[[profile]]\nname = \"n\"\nlabels = [\"faber\"]\nbackend = \"docker\"\n\
                      dind = false\nenv = { A = \"1\" }\n[needs.node]\nmemory_mib = 512\n";
        if FileConfig::load_from_str(inline, "a test").is_ok() {
            assert_eq!(unknown_keys(inline), Vec::new());
        }
    }

    #[tokio::test]
    async fn a_raw_dry_run_previews_the_config_its_text_parses_to() {
        let (w, _tmp, path) = raw_web(&commented());
        let next = text_of(&routine(7));
        let (status, r) = raw_post(&w, operator(), None, true, &next).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(
            r["preview"]["poll_interval_secs"],
            valid().poll_interval_secs + 7,
            "{r}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), commented());
        // A real save carries no preview.
        let (status, r) = raw_post(&w, operator(), None, false, &next).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert!(r.get("preview").is_none(), "{r}");
    }

    /// The form gets its base from `GET /api/settings`, and a save sent with
    /// it is refused once the config has moved.
    #[tokio::test]
    async fn the_forms_base_comes_with_the_settings_and_guards_the_save() {
        let (w, tmp) = test_web();
        let path = tmp.path.join("config.toml");
        let save = |base: Option<String>, cfg: FileConfig| {
            save_settings_route(
                State(w.clone()),
                Some(Extension(operator())),
                Query(SaveQuery {
                    dry_run: None,
                    base,
                }),
                Json(cfg),
            )
        };
        let base = || async { get_settings(State(w.clone())).await.0.base.expect("a base") };
        let (status, Json(r)) = save(Some(base().await), routine(1)).await;
        assert_eq!(status, StatusCode::OK, "{:?}", r.errors);
        let stale = base().await;
        let (status, _) = save(None, routine(2)).await;
        assert_eq!(status, StatusCode::OK);
        let written = std::fs::read_to_string(&path).unwrap();
        let (status, Json(r)) = save(Some(stale), routine(3)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{:?}", r.errors);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        let (status, Json(r)) = save(Some(base().await), routine(3)).await;
        assert_eq!(status, StatusCode::OK, "{:?}", r.errors);
        // A dry run of the form carries its preview too.
        let (_, Json(r)) = save_settings_route(
            State(w.clone()),
            Some(Extension(operator())),
            Query(SaveQuery {
                dry_run: Some("1".into()),
                base: None,
            }),
            Json(routine(4)),
        )
        .await;
        assert_eq!(
            r.preview.map(|p| p.poll_interval_secs),
            Some(valid().poll_interval_secs + 4)
        );
    }

    #[tokio::test]
    async fn a_daemon_started_from_flags_has_no_raw_editor() {
        let (w, tmp) = test_web();
        let (status, r) = raw_get(&w).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(r["from_flags"], true);
        let (status, r) = raw_post(&w, root(), None, false, &commented()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{r}");
        assert!(!tmp.path.join("config.toml").exists());
    }

    // --- ADR-0028 addendum A/B: clearing a repo hold widens trust -------------

    fn held(w: &Web) {
        w.orch.insert_repo_hold(crate::config::RepoHold {
            repo: "owner/demo".into(),
            reason: "trust bypass: job 7".into(),
            by: "gate".into(),
            since_ms: 1_000,
        });
    }

    fn clear_body() -> Json<RepoHoldClear> {
        Json(RepoHoldClear {
            repo: " owner/demo ".into(),
        })
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// No caller on record: a held repo stays held, the refusal says why, and
    /// the attempt is audited as refused under its own route. Nothing reaches
    /// the trust-change notice.
    #[tokio::test]
    async fn clearing_a_held_repo_without_a_caller_is_refused_and_audited() {
        let (w, _tmp) = test_web();
        held(&w);
        let resp = clear_repo_hold(State(w.clone()), None, clear_body()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let why = body_text(resp).await;
        assert!(why.contains("operator"), "{why}");
        assert_eq!(w.orch.repo_holds().len(), 1, "still held");

        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action.as_deref(), Some("refused"));
        assert_eq!(lines[0].route.as_deref(), Some("POST /api/repo-hold/clear"));
        assert_eq!(lines[0].widenings[0].key, "repo_hold.owner/demo");
        assert!(w.trust_changes.list().is_empty());
    }

    /// Issue #78: an operator, on the socket or in the dashboard, may not clear
    /// a hold: refused, audited as refused, still held.
    #[tokio::test]
    async fn the_operator_is_refused_a_hold_clear() {
        for caller in [
            Caller::Session,
            Caller::Socket {
                uid: 501,
                class: Class::Operator,
            },
        ] {
            let (w, _tmp) = test_web();
            held(&w);
            let resp =
                clear_repo_hold(State(w.clone()), Some(Extension(caller)), clear_body()).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{caller:?}");
            let why = body_text(resp).await;
            assert!(why.contains("present caller (sudo)"), "{why}");
            assert_eq!(w.orch.repo_holds().len(), 1, "still held");
            let lines = w.orch.audit_log().read_control_since(0);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].action.as_deref(), Some("refused"));
            assert!(w.trust_changes.list().is_empty());
        }
    }

    /// Root clears it: the hold is gone, the clear is audited as allowed, and
    /// `/api/state` lists it under `trust_changes`.
    #[tokio::test]
    async fn root_clears_a_held_repo_and_the_notice_shows_it() {
        let (w, _tmp) = test_web();
        held(&w);
        let resp = clear_repo_hold(State(w.clone()), Some(Extension(root())), clear_body()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(w.orch.repo_holds().is_empty(), "cleared");

        let lines = w.orch.audit_log().read_control_since(0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].action.as_deref(), Some("allowed"));
        assert_eq!(lines[0].caller.as_deref(), Some("socket"));

        let Json(view) = state(State(w)).await;
        let json = serde_json::to_value(&view).unwrap();
        let changes = json["trust_changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["key"], "repo_hold.owner/demo");
        assert_eq!(changes[0]["via"], "socket");
        assert_eq!(changes[0]["uid"], 0);
        assert_eq!(changes[0]["route"], "POST /api/repo-hold/clear");
        let sentence = changes[0]["sentence"].as_str().unwrap();
        assert!(
            sentence.starts_with("minting hold cleared for owner/demo (held since 1970-01-01")
                && sentence.ends_with(": \"trust bypass: job 7\")"),
            "{sentence}"
        );
        assert!(changes[0]["ts_ms"].as_u64().unwrap() > 0);
    }

    /// Clearing a repo that is not held is routine: no audit line, no notice.
    #[tokio::test]
    async fn clearing_a_repo_that_is_not_held_is_routine() {
        let (w, _tmp) = test_web();
        let operator = Caller::Socket {
            uid: 501,
            class: Class::Operator,
        };
        let resp = clear_repo_hold(State(w.clone()), Some(Extension(operator)), clear_body()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(w.orch.audit_log().read_control_since(0).is_empty());
        assert!(w.trust_changes.list().is_empty());

        // Still refused without a caller, but with nothing to audit.
        let resp = clear_repo_hold(State(w.clone()), None, clear_body()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(w.orch.audit_log().read_control_since(0).is_empty());
    }

    // --- binding the panel's port ---------------------------------------------

    /// A port let go a moment ago is waited for, and taken once it is free.
    #[tokio::test]
    async fn the_panel_waits_out_a_port_still_in_use() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = holder.local_addr().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(holder);
        });
        let bound = bind_listener(addr, 100, std::time::Duration::from_millis(10)).await;
        release.join().unwrap();
        assert_eq!(bound.map(|l| l.local_addr().unwrap()).ok(), Some(addr));
    }

    /// The wait is bounded: a holder that never lets go is reported as the
    /// `AddrInUse` it is, after the last try, so the retry cannot hide a port
    /// that was never released.
    #[tokio::test]
    async fn the_panel_gives_up_on_a_port_that_stays_in_use() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = holder.local_addr().unwrap();
        let pause = std::time::Duration::from_millis(10);
        let started = std::time::Instant::now();
        let err = bind_listener(addr, 5, pause).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(started.elapsed() >= pause * 4, "{:?}", started.elapsed());
        drop(holder);
    }

    /// Only `AddrInUse` is waited out: any other failure is returned at once.
    #[tokio::test]
    async fn the_panel_does_not_wait_out_other_failures() {
        // Not an address of this host: refused straight away, never retried.
        let foreign = SocketAddr::from(([192, 0, 2, 1], 0));
        let started = std::time::Instant::now();
        let err = bind_listener(foreign, 20, std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrNotAvailable);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
