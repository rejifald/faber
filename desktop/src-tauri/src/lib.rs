//! faber menu-bar app.
//!
//! A thin native client over the faber daemon's Unix control socket. The
//! daemon already serves live state on `GET /api/state` over that socket
//! (see `faber/src/web.rs`); this app polls that endpoint, renders the current
//! status into the macOS menu bar (a single fixed-width template image), and
//! drives two windows — a compact popover (left-click) and the panel (⌘⇧F),
//! which is the daemon's dashboard on a fresh one-time link (see [`panel`]).
//! Controls (reconcile / pause / resume) POST to the daemon's existing `/api/*`
//! endpoints.
//!
//! The popover cannot fetch the daemon directly (cross-origin, no CORS headers),
//! so all its HTTP goes through this Rust process, which then `emit`s snapshots
//! to it and answers `invoke` commands from it. The panel gets neither.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, Rect, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_opener::OpenerExt;

mod dashboard_link;
mod panel;
mod trust;
mod updater;
pub use updater::{maybe_print_policy, maybe_run_watchdog};

/// URL prefix for every daemon call. The host is ignored: the client is bound
/// to the control socket (`socket_path`), so only the path matters.
fn base_url() -> String {
    "http://faber".to_string()
}

/// Where the daemon's control socket lives (ADR-0028). `FABER_SOCKET` wins;
/// else the system path when `/etc/faber/setup.json` exists (a hardened
/// install); else the legacy per-user path. Pure: env lookup and the
/// existence check are arguments.
fn socket_path(
    env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&str) -> bool,
    macos: bool,
) -> String {
    if let Some(p) = env("FABER_SOCKET").filter(|p| !p.is_empty()) {
        return p;
    }
    if exists("/etc/faber/setup.json") {
        return if macos {
            "/Library/Application Support/faber/control.sock".to_string()
        } else {
            "/var/lib/faber/control.sock".to_string()
        };
    }
    let home = env("HOME").unwrap_or_default();
    if macos {
        format!("{home}/Library/Application Support/faber/control.sock")
    } else {
        let state = env("XDG_STATE_HOME")
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| format!("{home}/.local/state"));
        format!("{state}/faber/control.sock")
    }
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TRAY_ID: &str = "faber-tray";
const SNAPSHOT_EVENT: &str = "snapshot";

/// How many poll samples the load trend remembers (30 × 2s ≈ last minute). The
/// rolling window is handed to the popover in each snapshot for its sparkline.
const HISTORY_LEN: usize = 30;

/// This app's own build version (`desktop/src-tauri/Cargo.toml`), handed to
/// the windows in every [`Snapshot`] so they can flag drift against the
/// daemon's `update.current` — the two ship on separate release cadences, and
/// that drift is exactly the fleet-skew signal ADR-0006 exists to surface.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// --- the daemon's StateView contract (mirror of faber/src/reconcile.rs) ------

/// `#[serde(default)]` so a version skew across the two binaries degrades to a
/// zero rather than a failed deserialize: the daemon and the app are deployed
/// separately, and a missing count must never blank the whole snapshot to
/// "offline". Field meanings live with [`faber::reconcile::Counts`]; note that
/// `completed` is faber's delivery, not the CI verdict.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct Counts {
    provisioned: u64,
    completed: u64,
    faults: u64,
    refused: u64,
    retried: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileView {
    name: String,
    labels: Vec<String>,
    backend: String,
}

/// The live step a job is on (mirror of `faber::reconcile::StepView`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepView {
    name: String,
    index: u32,
    total: u32,
}

/// Why a parked claim is waiting (mirror of the daemon's `waiting` object on an
/// in-flight row). `gate` is one of `held`, `paused`, `docker`, `disk`,
/// `adoption`, `ceiling`, `memory`, `cpu`, `vm-memory`, `cpu-pressure`, `image`,
/// `pool`, `native`;
/// `detail` is a short human sentence meant for direct display. Both fields are
/// always present together, so neither needs its own `#[serde(default)]` — the
/// whole `Option<WaitingView>` on `InflightView` is what degrades to `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaitingView {
    gate: String,
    detail: String,
}

/// One in-flight runner (mirror of `faber::reconcile::InflightView`). The extra
/// fields beyond `job_id`/`profile` are `#[serde(default)]` so an older daemon
/// that only sends those two still parses (the row just shows less).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InflightView {
    job_id: u64,
    profile: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    pr: Option<u64>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    job_name: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    elapsed_s: u64,
    #[serde(default)]
    admitted: bool,
    #[serde(default)]
    step: Option<StepView>,
    /// Why this claim is waiting — always `None` once `admitted` is true, and
    /// from an older daemon that predates this field.
    #[serde(default)]
    waiting: Option<WaitingView>,
    /// Set while the job's runner is paused (ADR-0032 disk floor, ADR-0033
    /// memory freeze). `None` when running, and from an older daemon.
    #[serde(default)]
    frozen: Option<FrozenView>,
    /// `queued` | `starting` | `running` | `frozen` (mirror of the daemon's
    /// `JobPhase`). `None` from an older daemon, read from `admitted`/`frozen`.
    #[serde(default)]
    phase: Option<String>,
}

impl InflightView {
    /// The daemon's phase, or the closest an older daemon without one can say:
    /// it only knows admitted, which counts a booting runner as running.
    fn phase(&self) -> &str {
        match self.phase.as_deref() {
            Some(p) => p,
            None if !self.admitted => "queued",
            None if self.frozen.is_some() => "frozen",
            None => "running",
        }
    }

    /// Its steps are executing: started on GitHub, not frozen.
    fn running(&self) -> bool {
        self.phase() == "running"
    }
}

/// Why and since when a job is frozen (mirror of `faber::freeze::FrozenView`).
/// `reason` is `"disk"` or `"memory"`; empty from a daemon that predates the
/// memory freeze, which the popover reads as disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct FrozenView {
    since_ms: u64,
    reason: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Decision {
    ts_ms: u64,
    repo: String,
    job_id: u64,
    event: String,
    actor: String,
    verdict: String,
    /// The routing profile that judged the job. `default` so an older daemon,
    /// which had no such field, still parses.
    #[serde(default)]
    profile: String,
}

/// A finished run (mirror of `faber::reconcile::OutcomeEvent`): what happened to
/// a job faber took on, and — for anything that isn't a plain success — the
/// one-line `cause` saying why.
///
/// Every field is `#[serde(default)]`: this whole feed postdates the daemon it
/// talks to on some machines, and a missing feed must degrade to an empty list,
/// never to a snapshot that fails to parse and takes the entire panel offline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OutcomeEvent {
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    job_id: u64,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    runner: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    result: String,
    #[serde(default)]
    duration_s: u64,
    #[serde(default)]
    wait_s: u64,
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    cause: Option<String>,
}

/// A fault in the daemon itself (mirror of `faber::reconcile::FaultEvent`) —
/// faber unable to do its job, rather than a job that failed. `action` is
/// `failed` or `recovered`, so a pair brackets the outage it describes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FaultEvent {
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    cause: String,
}

/// An operator-set hold on Docker admission (mirror of the daemon's `hold`
/// object in `budget`). `since_ms` is epoch milliseconds. Both fields are
/// always present together, so neither needs its own `#[serde(default)]` —
/// it's the whole `Option<HoldView>` on `BudgetView` that degrades to `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HoldView {
    reason: String,
    since_ms: u64,
}

/// The autolimiter snapshot (mirror of `faber::limiter::LimiterView`): the memory
/// budget, live availability, the ledger, per-class caps, and pending warnings.
/// All `#[serde(default)]` so an older daemon (no `budget` field) still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BudgetView {
    #[serde(default)]
    budget_mib: u64,
    #[serde(default)]
    available_mib: Option<u64>,
    #[serde(default)]
    reserved_mib: u64,
    #[serde(default)]
    docker_inflight: u32,
    #[serde(default)]
    frozen: bool,
    /// An operator-set hold on Docker admission — surfaced in the tray so a
    /// long hold (the 2026-09-24 outage this field exists to prevent a repeat
    /// of) is never silent. `None` when nothing is held, including on an
    /// older daemon that predates this field.
    #[serde(default)]
    hold: Option<HoldView>,
    #[serde(default)]
    reserve_mib: u64,
    #[serde(default)]
    headroom_mib: u64,
    #[serde(default)]
    margin: f64,
    #[serde(default)]
    native_concurrency: u32,
    /// The ceiling in force, resolved to a number (`None` = off). Kept for an
    /// older menu-bar app build; this one prefers `hard_ceiling_setting`.
    #[serde(default)]
    hard_ceiling: Option<u32>,
    /// The ceiling *setting* (ADR-0019): `"auto"`, `"off"` or a number. Left as
    /// untyped JSON, like [`UpdateView::phase`] below — this app only ever
    /// reads it back out, never constructs one, so a typed mirror would just be
    /// a second copy of the daemon's `HardCeiling` to keep in sync.
    /// `#[serde(default)]` so an older daemon (no such field) still parses,
    /// distinguishable from this one's own `"off"` by being `None` rather than
    /// `Some(json!("off"))`.
    #[serde(default)]
    hard_ceiling_setting: Option<serde_json::Value>,
    #[serde(default)]
    classes: Vec<ClassCapView>,
    #[serde(default)]
    warnings: Vec<String>,
    /// CPU-aware admission (ADR-0019). `None` from an older daemon, or before
    /// the operator has stated a CPU mode — the popover then shows nothing
    /// extra rather than a row of zeros.
    #[serde(default)]
    cpu: Option<CpuView>,
    /// The ADR-0033 reservation mode and its fallback (`budget.reserve`).
    /// `None` from a daemon that predates it; the popover then shows nothing.
    #[serde(default)]
    reserve: Option<ReserveView>,
}

/// `budget.reserve` (mirror of `faber::reserve::ReserveView`), the fields the
/// popover reads. Every field defaults so a partial object still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ReserveView {
    /// Configured mode: `"max"` or `"p75"`.
    mode: String,
    /// What admission debits right now: `"max"` under `"max"` and in a cool-off.
    in_force: String,
    cooling: bool,
    squeeze: bool,
    freeze_armed: bool,
    trips_7d: u32,
    last_trip: Option<TripView>,
    /// Epoch ms the host re-arms at, while cooling off.
    next_rearm_ms: Option<u64>,
}

/// One trip of the fallback; `cause` is `"freeze"`, `"host_oom"` or `"probe"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct TripView {
    at_ms: u64,
    cause: String,
    cooloff_ms: u64,
    until_ms: u64,
}

/// The CPU half of the autolimiter snapshot (ADR-0019; mirror of the `cpu`
/// object `faber::web` adds to `budget` in `GET /api/state`). Millicores:
/// 1000 = one core.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CpuView {
    /// `"auto"` | `"off"` | `"unstated"`.
    #[serde(default)]
    mode: String,
    #[serde(default)]
    ncpu: Option<u32>,
    #[serde(default)]
    budget_mcpu: Option<u64>,
    #[serde(default)]
    reserved_mcpu: Option<u64>,
    /// PSI `some avg60` percentage, or `None` when the kernel exposes none.
    #[serde(default)]
    pressure: Option<f64>,
    #[serde(default)]
    frozen: bool,
    #[serde(default)]
    overcommit: f64,
    #[serde(default)]
    reserve_cpus: f64,
    #[serde(default)]
    profiling_cpus: f64,
}

/// One class's learned/pinned cap (mirror of `faber::limiter::ClassCapView`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClassCapView {
    #[serde(default)]
    key: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    runner_mib: u64,
    #[serde(default)]
    dind_mib: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    samples: u32,
    /// CPU share (ADR-0019), millicores. `None` from a daemon that predates
    /// CPU admission, or hasn't measured this class's CPU yet.
    #[serde(default)]
    runner_mcpu: Option<u64>,
    #[serde(default)]
    dind_mcpu: Option<u64>,
    /// `"profiling"` | `"learned"` | `"warned"` | `"pinned"`, independent of
    /// the memory-side `status` above — a class can be learned on memory and
    /// still profiling (or pinned) on CPU.
    #[serde(default)]
    cpu_status: Option<String>,
    #[serde(default)]
    cpu_pinned: bool,
}

/// Fleet-autoupdate status (mirror of `faber::update::UpdateView`, ADR-0006).
/// Struct-level `#[serde(default)]` for the same reason as [`Counts`] above —
/// the daemon and the app are deployed separately, so a daemon that predates
/// this feature (and sends no `update` key at all) must still parse into a
/// working default rather than blanking the whole snapshot to "offline".
///
/// `phase` is left as raw `serde_json::Value` rather than mirrored as a typed
/// enum: on the daemon it's `#[serde(tag = "phase")]`-internally-tagged and
/// gains new phases as the updater evolves, so giving it a fixed Rust shape
/// here would both duplicate that definition *and* fail to deserialize the
/// instant a daemon ships a phase this build predates — precisely the
/// failure this whole mirror exists to avoid. The JS layer reads `phase.phase`
/// for the tag and whatever fields that tag carries.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct UpdateView {
    /// The running daemon's version, e.g. "0.2.0+g1a2b3c4" — shown verbatim
    /// in both windows as calm reference info. See `APP_VERSION` for this
    /// *app's* own build, which is a separate number on a separate cadence.
    current: String,
    enabled: bool,
    /// A newer release the updater has seen, independent of `phase` (it can
    /// be set before staging finishes, or while `phase` is `deferred`).
    available: Option<String>,
    phase: serde_json::Value,
    last_check_ms: Option<u64>,
    last_error: Option<String>,
    quarantined: Vec<String>,
    /// Set once, right after a boot that had to restore `bin/faber.prev`
    /// because the previous update crash-looped. The UI must never stay
    /// quiet about this — see the "rolled back from …" row in ui/src/popover/attention.ts.
    rolled_back_from: Option<String>,
    pin: Option<String>,
    /// The daemon's boot self-check (#92), passed through opaque so the popover
    /// can warn when a release could not prove its own updater.
    self_check: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// Typed mirror of the daemon's state, for Rust-side logic only (tray icon,
/// tooltip, history). The webview does NOT get this struct: `Snapshot::state`
/// carries the daemon's JSON verbatim, so a new daemon field needs no change
/// here unless Rust itself reads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateView {
    owner: String,
    topic: String,
    profiles: Vec<ProfileView>,
    concurrency: u32,
    paused: bool,
    draining: bool,
    uptime_s: u64,
    audit_path: String,
    inflight: Vec<InflightView>,
    counts: Counts,
    recent: Vec<Decision>,
    /// Recent finished runs, newest first.
    #[serde(default)]
    outcomes: Vec<OutcomeEvent>,
    /// Recent faults in faber itself, newest first.
    #[serde(default)]
    faults: Vec<FaultEvent>,
    /// False when the daemon's last reconcile cycle couldn't reach GitHub, so an
    /// empty in-flight list means "can't obtain jobs", not "idle". Defaults to
    /// healthy so an older daemon (no such field) isn't flagged as broken.
    #[serde(default = "default_true")]
    healthy: bool,
    #[serde(default)]
    last_error: Option<String>,
    /// Docker profiles whose own image is missing from the host (#63), passed
    /// through opaque for the popover's banner. Absent on an older daemon = none.
    #[serde(default)]
    missing_images: Vec<serde_json::Value>,
    /// Profiles whose runner GitHub has announced it will retire (#64), passed
    /// through opaque for the popover's banner; the tooltip reads only
    /// `days_remaining`. Absent on an older daemon = none.
    #[serde(default)]
    runner_retirements: Vec<serde_json::Value>,
    /// The autolimiter snapshot — budget, ledger, per-class caps, warnings.
    #[serde(default)]
    budget: BudgetView,
    /// Fleet-autoupdate status (ADR-0006) — see [`UpdateView`]'s doc comment
    /// for why `phase` is untyped JSON rather than a mirrored enum.
    #[serde(default)]
    update: UpdateView,
    /// Queued jobs the trust gate refuses. Absent on an older daemon = empty.
    #[serde(default)]
    refused_queued: Vec<RefusedQueued>,
    /// Refused jobs the daemon counted but did not list (its cap).
    #[serde(default)]
    refused_queued_overflow: usize,
    /// Repos whose minting is held after an enforced gate bypass (ADR-0030).
    /// Absent on an older daemon = none.
    #[serde(default)]
    repo_holds: Vec<RepoHold>,
    /// Trust-class gate bypasses in the last 7 days (ADR-0030).
    #[serde(default)]
    trust_bypasses_7d: usize,
    /// The trust boundary in force, passed through untyped: the windows read
    /// `opt_in_events` and `on_bypass` from it. Absent on an older daemon.
    #[serde(default)]
    gate: Option<serde_json::Value>,
}

/// A per-repo minting hold (ADR-0030). `GET /api/state` → `repo_holds`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
struct RepoHold {
    repo: String,
    reason: String,
    by: String,
    since_ms: u64,
}

/// A job queued on GitHub that targets this host but is refused by its trust
/// gate, so it has no runner of its own. `GET /api/state` → `refused_queued`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
struct RefusedQueued {
    repo: String,
    job_id: u64,
    run_id: u64,
    job_name: String,
    workflow: String,
    event: String,
    verdict: String,
    since_ms: u64,
}

impl StateView {
    /// Jobs whose steps are executing on this host right now — not merely
    /// admitted (a booting runner, a frozen one).
    fn running(&self) -> u32 {
        self.inflight.iter().filter(|j| j.running()).count() as u32
    }

    /// Every job this host holds a claim on, running or not. Under demand
    /// provisioning (ADR-0004) every host claims every queued job it sees, so the
    /// part of this that isn't running mirrors the shared GitHub queue — work the
    /// peer may take — not a backlog of this host's own.
    fn claimed(&self) -> u32 {
        self.inflight.len() as u32
    }
}

/// What we hand to the windows and cache for `get_snapshot`. `state` is `None`
/// when the daemon is unreachable, so the UI can render an explicit offline
/// state (which the localhost dashboard itself cannot, since it fails to load).
/// `history` is the rolling running-count window (oldest first) so the popover
/// can draw its load trend without keeping its own series. `app_version` is
/// this app's own build (`APP_VERSION`), carried on every snapshot — including
/// the offline one — since it describes this binary, not the daemon.
#[derive(Debug, Clone, Serialize)]
struct Snapshot {
    online: bool,
    base_url: String,
    error: Option<String>,
    /// The daemon's `/api/state` body, verbatim — this is what the webview sees.
    state: Option<serde_json::Value>,
    /// Typed parse of `state` for Rust-side readers; never sent to the webview.
    #[serde(skip)]
    typed: Option<StateView>,
    history: Vec<u32>,
    app_version: String,
    /// The app's own self-update (ADR-0034, #80): status, its maturity floor and
    /// any warning, for the popover. Describes this binary, not the daemon.
    app_update: updater::AppUpdateView,
    /// ADR-0028: the app's own copy of the daemon's `trust_changes` lost an
    /// entry that is still inside 7 days. The list itself is in `state`.
    trust_shrank: bool,
}

impl Snapshot {
    fn offline(base_url: String, error: Option<String>) -> Self {
        Self {
            online: false,
            base_url,
            error,
            state: None,
            typed: None,
            history: Vec::new(),
            app_version: APP_VERSION.to_string(),
            app_update: updater::AppUpdateView::default(),
            trust_shrank: false,
        }
    }
}

/// Shared app state: one HTTP client, the daemon URL, the last snapshot, the
/// load history, the last-applied tray surface, and a couple of tray handles.
struct AppState {
    client: reqwest::Client,
    base_url: String,
    /// The control socket path, shown where an offline snapshot names the
    /// daemon (carried in `Snapshot::base_url`, a field the webview reads).
    socket: String,
    snapshot: Mutex<Snapshot>,
    /// Rolling window of recent in-flight counts (one per poll tick), newest at
    /// the back. Drives the popover load trend. Frozen while offline.
    history: Mutex<VecDeque<u32>>,
    /// What we last pushed to the tray, so a poll tick that changed nothing
    /// leaves the tray untouched (no needless icon redraw).
    last: Mutex<LastApplied>,
    /// When the popover was last hidden (by blur or by us). A left-click that
    /// lands right after a blur-hide is the same gesture that dismissed it, so we
    /// swallow it instead of immediately re-opening.
    popover_hidden_at: Mutex<Option<Instant>>,
    /// Whether the popover has actually become key since it was last shown.
    ///
    /// macOS 27 refuses `activate(ignoringOtherApps:)` for a background
    /// accessory app (cooperative activation), so `set_focus()` is a no-op there
    /// and the window is shown without ever taking focus. The blur-to-dismiss
    /// handler must not read that as "the user clicked away" — honouring it hides
    /// the popover before it has drawn a frame, which is exactly how the popover
    /// went invisible on macOS 27 with no faber change behind it.
    popover_took_focus: Mutex<bool>,
    /// When this process started: the popover counts as closed since then until
    /// it is first shown (the self-update's idle window, ADR-0034).
    started: Instant,
    /// The self-updater's latest view, folded into every snapshot.
    app_update: Mutex<updater::AppUpdateView>,
}

/// The tray surface we last rendered. `None` = never applied (first tick wins).
#[derive(Default)]
struct LastApplied {
    tooltip: Option<String>,
    icon_sig: Option<String>,
    /// Pixel width of the widget last drawn: the job count grows it.
    icon_w: usize,
}

// --- HTTP -------------------------------------------------------------------

async fn fetch_state(
    client: &reqwest::Client,
    base: &str,
) -> Result<(serde_json::Value, StateView), String> {
    let url = format!("{base}/api/state");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let raw: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    // A body that is valid JSON but not a StateView stays an error, as before.
    let typed = serde_json::from_value::<StateView>(raw.clone()).map_err(|e| e.to_string())?;
    Ok((raw, typed))
}

async fn post(client: &reqwest::Client, base: &str, path: &str) -> Result<(), String> {
    let url = format!("{base}/api/{path}");
    let resp = client
        .post(&url)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

/// The daemon's error body as a `Result::Err` string. Several `/api/*` routes
/// (`config`, `hold`, `unhold`) answer a `400` with a plain-text message worth
/// showing the operator verbatim, rather than the bare status code `post`
/// above reports. Falls back to `HTTP {status}` when the body is empty or
/// fails to decode, so a non-2xx with no readable body still says something.
async fn body_or_status(resp: reqwest::Response) -> String {
    let status = resp.status();
    match resp.text().await {
        Ok(body) if !body.trim().is_empty() => body,
        _ => format!("HTTP {status}"),
    }
}

// --- tray widget: geometry --------------------------------------------------
//
// The whole menu-bar item is ONE monochrome template image: the faber mark (a
// cube with its front-corner core cut free) on the left, and a status modifier —
// dash, job count, pause bars, or alert — to its right. Offline is the one
// integrated glyph: the mark faded and struck through. Every glyph but the job
// count fits WIDGET_W × WIDGET_H; the count is one line ("1 | 10") and widens
// the canvas only when it needs more digits than that width holds.
//
// Everything is drawn at retina scale (the item displays at half-size, ~22pt
// tall). The mark is the app icon's 100-unit geometry (`design/mark/`); the
// modifiers are the shared 24-unit sprite geometry (pause bars, the alert "!").
// Each is mapped into pixel space by a per-element (offset, scale); digits are
// drawn as stroked polylines. Template mode = alpha only; macOS tints it.

const WIDGET_W: usize = 78;
const WIDGET_H: usize = 44;

// Mark: the icon's 100-unit centre (50,50) maps to (MARK_CX, MARK_CY) at
// MARK_SCALE px per unit. The scale makes the cube's edge exactly 16px, so its
// vertical edges sit on pixel boundaries and the 2:1 edges step cleanly; the
// mark spans x 5..37 and y 6..38.
const MARK_CX: f32 = 21.0;
const MARK_CY: f32 = 22.0;
const MARK_SCALE: f32 = 16.0 / MARK_EDGE;
/// The cube's edge in mark units, as in `design/mark/`: a cube in 2:1 isometric
/// centred on (50,50), so its outline is a hexagon from 21 to 79.
const MARK_EDGE: f32 = 29.0;
/// The core is the front-corner cube `[MARK_CORE, 1]^3` of the unit cube.
const MARK_CORE: f32 = 0.5;
/// The moat between core and shell, in mark units. The icon's is 3; at ~0.28pt
/// per unit that is under a point, so the menu-bar cut opens it to 3.6 (2px).
const MARK_MOAT: f32 = 3.6;
/// Coverage of the shell's faces and the core's, top / left / right. The core
/// carries the weight; the face steps keep both reading as cubes in one colour.
/// The top face takes the middle step: macOS tints the template black on a light
/// bar and white on a dark one, so the most-covered face is darkest in one and
/// brightest in the other. Lit from the side, the cube reads right in both (light
/// from the left on a dark bar, from the right on a light one); lit from above,
/// it would look lit from below on a light bar.
const MARK_SHELL_ALPHA: [f32; 3] = [0.38, 0.55, 0.24];
const MARK_CORE_ALPHA: [f32; 3] = [0.78, 1.0, 0.56];
/// Samples per pixel side when rasterising the mark: its faces meet along
/// shared edges, and supersampling keeps those seams free of AA dips.
const MARK_SUPERSAMPLE: usize = 4;
/// Offline fades the mark to this coverage and strikes it at full strength.
const OFFLINE_MARK_ALPHA: f32 = 0.45;
const OFFLINE_SLASH_HALF: f32 = 2.0;
/// Extra clearance cut either side of the slash, so it reads as cutting through
/// the mark instead of merging into the VMs it crosses.
const OFFLINE_SLASH_CLEAR: f32 = 1.6;

// Modifier slot: the 24-space glyph centre maps to (MOD_CX, MOD_CY) at MOD_SCALE.
// Pushed right of the mark (which ends near x≈37) so the status bit reads as a
// separate glyph, not something touching the mark.
const MOD_CX: f32 = 52.0;
const MOD_CY: f32 = 22.0;
const MOD_SCALE: f32 = 1.3;
const MOD_OX: f32 = MOD_CX - 12.0 * MOD_SCALE;
const MOD_OY: f32 = MOD_CY - 12.0 * MOD_SCALE;

// The job count: running, a thin bar, then every job claimed — one line of
// tabular digits, vertically centred on the mark ("1 | 10"). Running is drawn at
// full strength, the total half-tone. Anything above 999 is drawn as 999.
const DIGIT_W: f32 = 12.0;
const DIGIT_GAP: f32 = 2.0;
const DIGIT_HGT: f32 = 20.0;
const DIGIT_HALF: f32 = 1.7;
const JOBS_TOP: f32 = (WIDGET_H as f32 - DIGIT_HGT) / 2.0;
/// Where the count starts: clear of the mark, which ends near x 37.
const JOBS_X: f32 = 44.0;
/// Space either side of the bar, and the bar itself.
const JOBS_BAR_PAD: f32 = 7.0;
const JOBS_BAR_HALF: f32 = 0.9;
const JOBS_BAR_TOP: f32 = 11.0;
const JOBS_BAR_BOTTOM: f32 = 33.0;
const JOBS_BAR_ALPHA: f32 = 0.5;
/// The total, and the bar, are drawn at this coverage.
const JOBS_DIM_ALPHA: f32 = 0.5;
/// Clear columns kept right of the last digit.
const JOBS_RIGHT_PAD: f32 = 3.0;
const JOBS_MAX: u32 = 999;

/// What the tray widget shows, in precedence order:
/// offline > unhealthy > paused/draining/held > jobs > idle. `Jobs` is a count:
/// jobs running here, then every job claimed, so "3 | 10" reads as three
/// executing and seven waiting (queued, starting or frozen). A host with
/// no claim at all is idle. An
/// operator-set Docker-admission hold (`budget.hold`) reads as the same
/// `Paused` glyph — the popover banner and the tooltip's `status_word` carry
/// the reason; the fixed-width menu-bar glyph doesn't grow a new one for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WidgetKind {
    Offline,
    Unhealthy,
    Paused,
    Jobs { running: u32, total: u32 },
    Idle,
}

fn widget_kind(snap: &Snapshot) -> WidgetKind {
    match &snap.typed {
        None => WidgetKind::Offline,
        Some(st) if !st.healthy => WidgetKind::Unhealthy,
        Some(st) if st.paused || st.draining || st.budget.hold.is_some() => WidgetKind::Paused,
        Some(st) if st.claimed() > 0 => WidgetKind::Jobs {
            running: st.running(),
            total: st.claimed(),
        },
        Some(_) => WidgetKind::Idle,
    }
}

/// A fingerprint of the rendered widget, so the icon redraws exactly when its
/// appearance changes and stays put when it doesn't.
fn widget_signature(kind: &WidgetKind) -> String {
    match kind {
        WidgetKind::Offline => "offline".into(),
        WidgetKind::Unhealthy => "unhealthy".into(),
        WidgetKind::Paused => "paused".into(),
        WidgetKind::Jobs { running, total } => format!("jobs:{running}/{total}"),
        WidgetKind::Idle => "idle".into(),
    }
}

// --- tray widget: rasterization ---------------------------------------------

/// Distance from point `(px,py)` to segment `a–b`, for anti-aliased strokes.
fn dist_pt_seg(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (ax, ay) = a;
    let (bx, by) = b;
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 1e-6 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// A coverage buffer we paint drawing primitives into, combining with `max` so
/// overlapping strokes never darken (it's an alpha mask, not paint). Every
/// primitive is anti-aliased by a half-pixel distance-field ramp.
struct Raster {
    a: Vec<f32>,
    /// Pixel width; the height is always `WIDGET_H`.
    w: usize,
}

impl Raster {
    fn new(w: usize) -> Self {
        Self {
            a: vec![0.0; w * WIDGET_H],
            w,
        }
    }

    #[inline]
    fn cover(&mut self, x: usize, y: usize, c: f32) {
        let i = y * self.w + x;
        if c > self.a[i] {
            self.a[i] = c;
        }
    }

    /// Stroke a polyline (round caps / joins fall out of the min-distance field).
    fn stroke(&mut self, pts: &[(f32, f32)], half: f32) {
        self.stroke_alpha(pts, half, 1.0);
    }

    /// `stroke` at a reduced coverage, for a dimmed element.
    fn stroke_alpha(&mut self, pts: &[(f32, f32)], half: f32, alpha: f32) {
        if pts.len() < 2 {
            return;
        }
        for y in 0..WIDGET_H {
            for x in 0..self.w {
                let cov = polyline_coverage(pts, half, x, y) * alpha;
                if cov > 0.0 {
                    self.cover(x, y, cov);
                }
            }
        }
    }

    /// Clear a channel along a polyline: the opposite of `stroke`, so a stroke
    /// drawn over a filled shape afterwards reads as cutting through it.
    fn knock(&mut self, pts: &[(f32, f32)], half: f32) {
        if pts.len() < 2 {
            return;
        }
        for y in 0..WIDGET_H {
            for x in 0..self.w {
                let cov = polyline_coverage(pts, half, x, y);
                if cov > 0.0 {
                    self.a[y * self.w + x] *= 1.0 - cov;
                }
            }
        }
    }

    /// A filled disc (the alert's point).
    fn dot(&mut self, c: (f32, f32), r: f32) {
        for y in 0..WIDGET_H {
            for x in 0..self.w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = ((fx - c.0).powi(2) + (fy - c.1).powi(2)).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.cover(x, y, cov);
                }
            }
        }
    }

    /// Bake the coverage into an alpha-only RGBA template image.
    fn into_image(self) -> Image<'static> {
        let mut rgba = vec![0u8; self.w * WIDGET_H * 4];
        for (i, &a) in self.a.iter().enumerate() {
            rgba[i * 4 + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        Image::new_owned(rgba, self.w as u32, WIDGET_H as u32)
    }
}

/// Map a point from the 24-unit modifier sprite space into the pixel slot.
fn md(ux: f32, uy: f32) -> (f32, f32) {
    (MOD_OX + ux * MOD_SCALE, MOD_OY + uy * MOD_SCALE)
}

/// Anti-aliased coverage of pixel `(x, y)` by a polyline stroked `half` px wide.
fn polyline_coverage(pts: &[(f32, f32)], half: f32, x: usize, y: usize) -> f32 {
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
    let d = pts
        .windows(2)
        .map(|seg| dist_pt_seg(fx, fy, seg[0], seg[1]))
        .fold(f32::MAX, f32::min);
    (half + 0.5 - d).clamp(0.0, 1.0)
}

/// Map a point from the 100-unit mark space into pixel space.
fn mk(ux: f32, uy: f32) -> (f32, f32) {
    (
        MARK_CX + (ux - 50.0) * MARK_SCALE,
        MARK_CY + (uy - 50.0) * MARK_SCALE,
    )
}

/// The mark's coverage at mark-space point `(u, v)`: which visible face of the
/// cube the point projects from, and whether it lands on shell, moat or core.
fn mark_alpha(u: f32, v: f32) -> f32 {
    let (dx, dy) = ((u - 50.0) / MARK_EDGE, (v - 50.0) / MARK_EDGE);
    // A moat of fixed width is a different step in face parameters depending on
    // whether the cut runs along a 2:1 diagonal or a vertical edge.
    let diag = MARK_MOAT * 1.25_f32.sqrt() / MARK_EDGE;
    let vert = MARK_MOAT / MARK_EDGE;
    // Invert the projection onto each face: (face, s, t, s's cut step, t's).
    let (tx, ty) = ((dx + 2.0 * dy + 2.0) / 2.0, (2.0 * dy + 2.0 - dx) / 2.0);
    let (face, s, t, ds, dt) = if (0.0..=1.0).contains(&tx) && (0.0..=1.0).contains(&ty) {
        (0, tx, ty, diag, diag)
    } else if dx < 0.0 {
        let x = 1.0 + dx;
        (1, x, (x + 1.0) / 2.0 - dy, vert, diag)
    } else {
        let y = 1.0 - dx;
        (2, y, (y + 1.0) / 2.0 - dy, vert, diag)
    };
    if !((0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t)) {
        return 0.0;
    }
    if s >= MARK_CORE && t >= MARK_CORE {
        MARK_CORE_ALPHA[face]
    } else if s >= MARK_CORE - ds && t >= MARK_CORE - dt {
        0.0
    } else {
        MARK_SHELL_ALPHA[face]
    }
}

/// Draw the faber mark at `alpha` coverage, supersampled.
fn draw_mark(r: &mut Raster, alpha: f32) {
    let n = MARK_SUPERSAMPLE;
    for y in 0..WIDGET_H {
        for x in 0..r.w {
            let mut sum = 0.0;
            for j in 0..n {
                for i in 0..n {
                    let px = x as f32 + (i as f32 + 0.5) / n as f32;
                    let py = y as f32 + (j as f32 + 0.5) / n as f32;
                    sum += mark_alpha(
                        50.0 + (px - MARK_CX) / MARK_SCALE,
                        50.0 + (py - MARK_CY) / MARK_SCALE,
                    );
                }
            }
            let cov = sum / (n * n) as f32 * alpha;
            if cov > 0.0 {
                r.cover(x, y, cov);
            }
        }
    }
}

/// `n` points along an ellipse arc (degrees, y-down), in a normalized cell.
fn narc(cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32, n: usize) -> Vec<(f32, f32)> {
    (0..=n)
        .map(|i| {
            let t = (a0 + (a1 - a0) * (i as f32 / n as f32)).to_radians();
            (cx + rx * t.cos(), cy + ry * t.sin())
        })
        .collect()
}

/// Each digit as stroked polylines in a normalized [0,1] cell (x right, y down).
/// Curves are polyline-sampled arcs; at this size the anti-aliasing hides the
/// facets. Kept deliberately geometric so the count reads as part of the mark.
fn digit_strokes(d: u8) -> Vec<Vec<(f32, f32)>> {
    match d {
        0 => vec![narc(0.5, 0.5, 0.33, 0.46, 0.0, 360.0, 28)],
        1 => vec![
            vec![(0.30, 0.22), (0.52, 0.06), (0.52, 0.94)],
            vec![(0.28, 0.94), (0.76, 0.94)],
        ],
        2 => {
            let mut p = narc(0.5, 0.33, 0.34, 0.25, 163.0, 372.0, 16);
            p.push((0.17, 0.94));
            p.push((0.85, 0.94));
            vec![p]
        }
        3 => vec![
            narc(0.44, 0.30, 0.33, 0.25, -145.0, 60.0, 14),
            narc(0.44, 0.70, 0.34, 0.26, -62.0, 145.0, 14),
        ],
        4 => vec![
            vec![(0.66, 0.06), (0.66, 0.94)],
            vec![(0.66, 0.06), (0.12, 0.66), (0.88, 0.66)],
        ],
        5 => {
            let mut p = vec![(0.80, 0.07), (0.30, 0.07), (0.28, 0.44)];
            p.extend(narc(0.47, 0.64, 0.35, 0.30, -128.0, 118.0, 14));
            vec![p]
        }
        6 => vec![
            narc(0.5, 0.66, 0.34, 0.28, 0.0, 360.0, 24),
            narc(0.52, 0.62, 0.44, 0.58, 178.0, 298.0, 12),
        ],
        7 => vec![vec![(0.15, 0.07), (0.85, 0.07), (0.40, 0.94)]],
        8 => vec![
            narc(0.5, 0.29, 0.28, 0.23, 0.0, 360.0, 20),
            narc(0.5, 0.71, 0.34, 0.27, 0.0, 360.0, 22),
        ],
        9 => vec![
            narc(0.5, 0.34, 0.34, 0.28, 0.0, 360.0, 24),
            narc(0.48, 0.38, 0.44, 0.58, 2.0, 122.0, 12),
        ],
        _ => vec![],
    }
}

/// Draw one digit, its cell's top-left corner at pixel `(x0, y0)`.
fn draw_digit(r: &mut Raster, d: u8, x0: f32, y0: f32, alpha: f32) {
    for stroke in digit_strokes(d) {
        let pts: Vec<(f32, f32)> = stroke
            .iter()
            .map(|(nx, ny)| (x0 + nx * DIGIT_W, y0 + ny * DIGIT_HGT))
            .collect();
        r.stroke_alpha(&pts, DIGIT_HALF, alpha);
    }
}

/// Pixel width of `n` drawn in tabular digits.
fn number_width(n: u32) -> f32 {
    let digits = n.to_string().len() as f32;
    digits * DIGIT_W + (digits - 1.0) * DIGIT_GAP
}

/// Draw `n` with its left edge at `x0` and its top edge at `y0`.
fn draw_number(r: &mut Raster, n: u32, x0: f32, y0: f32, alpha: f32) {
    for (i, ch) in n.to_string().bytes().enumerate() {
        draw_digit(
            r,
            ch - b'0',
            x0 + i as f32 * (DIGIT_W + DIGIT_GAP),
            y0,
            alpha,
        );
    }
}

/// Where the job count's bar and total sit, and the canvas width it needs.
fn jobs_layout(running: u32, total: u32) -> (f32, f32, usize) {
    let (running, total) = (running.min(JOBS_MAX), total.min(JOBS_MAX));
    let bar = JOBS_X + number_width(running) + JOBS_BAR_PAD;
    let total_x = bar + JOBS_BAR_PAD;
    let end = total_x + number_width(total) + JOBS_RIGHT_PAD;
    (bar, total_x, WIDGET_W.max(end.ceil() as usize))
}

/// Draw the job count: running at full strength, a faint bar, the total dimmed.
fn draw_jobs(r: &mut Raster, running: u32, total: u32) {
    let (bar, total_x, _) = jobs_layout(running, total);
    draw_number(r, running.min(JOBS_MAX), JOBS_X, JOBS_TOP, 1.0);
    r.stroke_alpha(
        &[(bar, JOBS_BAR_TOP), (bar, JOBS_BAR_BOTTOM)],
        JOBS_BAR_HALF,
        JOBS_BAR_ALPHA,
    );
    draw_number(r, total.min(JOBS_MAX), total_x, JOBS_TOP, JOBS_DIM_ALPHA);
}

/// The canvas width a state draws into.
fn widget_width(kind: &WidgetKind) -> usize {
    match kind {
        WidgetKind::Jobs { running, total } => jobs_layout(*running, *total).2,
        _ => WIDGET_W,
    }
}

/// Draw the full menu-bar widget for a state into a coverage buffer.
fn render_raster(kind: &WidgetKind) -> Raster {
    let mut r = Raster::new(widget_width(kind));
    if let WidgetKind::Offline = kind {
        // The struck mark: faded, with a diagonal slash cut through it.
        draw_mark(&mut r, OFFLINE_MARK_ALPHA);
        let slash = [mk(28.0, 28.0), mk(72.0, 72.0)];
        r.knock(&slash, OFFLINE_SLASH_HALF + OFFLINE_SLASH_CLEAR);
        r.stroke(&slash, OFFLINE_SLASH_HALF);
        return r;
    }

    // Every online state carries the full mark …
    draw_mark(&mut r, 1.0);
    // … plus its status modifier in the slot.
    match kind {
        WidgetKind::Idle => {
            // a steady dash — idle's own resting mark, holding the width
            r.stroke(&[md(7.5, 12.0), md(16.5, 12.0)], 2.6 * MOD_SCALE / 2.0);
        }
        WidgetKind::Paused => {
            // pause bars
            let half = 3.4 * MOD_SCALE / 2.0;
            r.stroke(&[md(8.7, 6.7), md(8.7, 17.3)], half);
            r.stroke(&[md(15.3, 6.7), md(15.3, 17.3)], half);
        }
        WidgetKind::Unhealthy => {
            // alert "!"
            r.stroke(&[md(12.0, 6.8), md(12.0, 14.4)], 2.8 * MOD_SCALE / 2.0);
            r.dot(md(12.0, 18.9), 1.7 * MOD_SCALE);
        }
        WidgetKind::Jobs { running, total } => draw_jobs(&mut r, *running, *total),
        WidgetKind::Offline => unreachable!("handled above"),
    }
    r
}

/// Render the full menu-bar widget for a state into a fixed-size template image.
fn render_widget(kind: &WidgetKind) -> Image<'static> {
    render_raster(kind).into_image()
}

// --- tray tooltip -----------------------------------------------------------

/// The human status word for the hover tooltip: image missing > unhealthy >
/// draining > paused > held > running > idle.
fn status_word(snap: &Snapshot) -> &'static str {
    match &snap.typed {
        None => "offline",
        Some(st) if !st.missing_images.is_empty() => "image missing",
        Some(st) if !st.healthy => "unhealthy",
        Some(st) if st.draining => "draining",
        Some(st) if st.paused => "paused",
        Some(st) if st.budget.hold.is_some() => "held",
        Some(st) if st.claimed() == 0 => "idle",
        Some(st) if st.running() == 0 => "waiting",
        Some(_) => "running",
    }
}

/// A compact budget readout (`12.3/18.0 GiB`) for the tooltip. Falls back to the
/// Docker in-flight count when the budget isn't probed (native-only / old daemon).
fn budget_summary(b: &BudgetView) -> String {
    if b.budget_mib == 0 {
        format!("{} docker", b.docker_inflight)
    } else {
        format!(
            "{:.1}/{:.1} GiB",
            b.reserved_mib as f64 / 1024.0,
            b.budget_mib as f64 / 1024.0
        )
    }
}

/// The tooltip's runner-retirement suffix (#64): the soonest cutoff among the
/// profiles GitHub is retiring a runner on. A warning, so it rides alongside the
/// status word rather than replacing it.
fn retirement_summary(list: &[serde_json::Value]) -> String {
    let Some(days) = list
        .iter()
        .filter_map(|r| r.get("days_remaining").and_then(serde_json::Value::as_i64))
        .min()
    else {
        return String::new();
    };
    match days {
        d if d > 0 => format!(" · runner retires in {d}d"),
        _ => " · runner retired".to_string(),
    }
}

/// The tooltip's waiting split: ` · 7 waiting (4 queued, 2 starting, 1 frozen)`,
/// empty when nothing waits. Each part is named only when it is non-zero.
fn waiting_summary(st: &StateView) -> String {
    let waiting = st.claimed() - st.running();
    if waiting == 0 {
        return String::new();
    }
    let count = |phase: &str| st.inflight.iter().filter(|j| j.phase() == phase).count();
    let parts: Vec<String> = ["queued", "starting", "frozen"]
        .iter()
        .map(|p| (count(p), p))
        .filter(|(n, _)| *n > 0)
        .map(|(n, p)| format!("{n} {p}"))
        .collect();
    format!(" · {waiting} waiting ({})", parts.join(", "))
}

fn tray_tooltip(snap: &Snapshot) -> String {
    match &snap.typed {
        None => format!("faber · offline ({})", snap.base_url),
        Some(st) => format!(
            "faber · {} · {} running{} · {} · {} provisioned{}{}",
            status_word(snap),
            st.running(),
            waiting_summary(st),
            budget_summary(&st.budget),
            st.counts.provisioned,
            if st.refused_queued.is_empty() {
                String::new()
            } else {
                format!(
                    " · {} refused",
                    st.refused_queued.len() + st.refused_queued_overflow
                )
            },
            retirement_summary(&st.runner_retirements)
        ),
    }
}

// --- tray icon apply --------------------------------------------------------

/// Push a fresh snapshot to the tray and the popover. The tray half runs on the
/// main thread via `run_on_main_thread` (AppKit icon mutations must be
/// main-threaded); the popover gets the snapshot event, by label: the panel
/// window is sent no event (ADR-0028 addendum C, decision 1).
fn apply_snapshot(app: &AppHandle, snap: &Snapshot) {
    let app_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        // Re-read the latest snapshot so several ticks landing in one runloop turn
        // collapse to the current state.
        let latest = app_main
            .state::<AppState>()
            .snapshot
            .lock()
            .unwrap()
            .clone();
        apply_tray(&app_main, &latest);
    });
    let _ = app.emit_to("popover", SNAPSHOT_EVENT, snap);
}

/// Reconcile the tray surfaces with a snapshot — **main thread only**. Icon and
/// tooltip update only when their rendered value changed, so an unchanged tick
/// is a no-op. The right-click menu is static (built once); nothing to rebuild.
fn apply_tray(app: &AppHandle, snap: &Snapshot) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let state = app.state::<AppState>();
    let mut last = state.last.lock().unwrap();

    let kind = widget_kind(snap);
    let isig = widget_signature(&kind);
    if last.icon_sig.as_deref() != Some(isig.as_str()) {
        let _ = tray.set_icon(Some(render_widget(&kind)));
        let _ = tray.set_icon_as_template(true);
        last.icon_sig = Some(isig);
        last.icon_w = widget_width(&kind);
    }

    let tooltip = tray_tooltip(snap);
    if last.tooltip.as_deref() != Some(tooltip.as_str()) {
        let _ = tray.set_tooltip(Some(tooltip.clone()));
        last.tooltip = Some(tooltip);
    }
}

// --- the popover window -----------------------------------------------------

/// Room either side of the popover's card (logical px) for its drop shadow, so
/// it renders into transparent margin instead of being clipped at the window
/// edge. `SIDE_ROOM` in ui/src/popover/wiring.ts.
const POPOVER_SIDE: f64 = 36.0;
/// The popover window's narrow width (logical px): the 308 px card,
/// `--width-popover` in ui/src/theme/theme.css, plus its side room. The window
/// opens at it and follows the card from there (`resize_popover`).
const POPOVER_W: f64 = 308.0 + 2.0 * POPOVER_SIDE;
/// The widest the window gets: the 620 px two-column card, `--width-popover-wide`,
/// plus its side room (docs/design/popover.md § Two columns).
const POPOVER_W_WIDE: f64 = 620.0 + 2.0 * POPOVER_SIDE;
/// The beak's centre, from the popover window's right edge (logical px). Must
/// match `Beak` in ui/src/screens/Popover.tsx: the card's right edge 36px in
/// from the window's, the beak's centre 40px in from that. Measured from the
/// right so it holds at either width; we line it up under the tray icon.
const POPOVER_BEAK_FROM_RIGHT: f64 = POPOVER_SIDE + 40.0;

/// Build the popover: a borderless, transparent, always-on-top window that
/// auto-closes on blur. Created hidden; shown under the tray icon on left-click.
fn build_popover(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, "popover", WebviewUrl::App("popover/index.html".into()))
        .title("faber")
        .inner_size(POPOVER_W, 540.0)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .resizable(false)
        .skip_taskbar(true)
        .visible(false)
        .build()
}

/// Position the popover so its beak points up at the tray icon, clamped to stay
/// on-screen. `rect` is the tray icon's bounds from the click event.
fn position_popover(win: &WebviewWindow, rect: &Rect, icon_w: usize) {
    let scale = win.scale_factor().unwrap_or(1.0);
    let pos = rect.position.to_physical::<f64>(scale);
    let size = rect.size.to_physical::<f64>(scale);
    // Point the beak at the mark, not the item's geometric centre. The mark sits
    // left of centre in the widget canvas (MARK_CX within its width), with the
    // status modifier filling the slot to its right, so a centre-aligned beak
    // lands in the gap right of the mark and reads as off.
    let logo_cx = pos.x + size.width * (MARK_CX as f64 / icon_w.max(1) as f64);
    let icon_bottom = pos.y + size.height;

    // The window is as wide as its card last asked (`resize_popover`).
    let pop_w = win
        .outer_size()
        .map(|s| s.width as f64)
        .unwrap_or(POPOVER_W * scale);
    let x = logo_cx - (pop_w - POPOVER_BEAK_FROM_RIGHT * scale);
    let y = icon_bottom + 2.0 * scale;
    let x = clamp_to_monitor(win, x, pop_w, scale);
    let _ = win.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

/// `x` (physical px) moved, if it must be, so a window `width` wide stays 4pt
/// inside the current monitor.
fn clamp_to_monitor(win: &WebviewWindow, x: f64, width: f64, scale: f64) -> f64 {
    let Ok(Some(mon)) = win.current_monitor() else {
        return x;
    };
    let left = mon.position().x as f64 + 4.0 * scale;
    let right = (mon.position().x as f64 + mon.size().width as f64) - width - 4.0 * scale;
    x.clamp(left, right.max(left))
}

/// Left-click toggles the popover. Deferred onto a fresh main-thread runloop turn
/// (same tao borrow_mut → SIGABRT reason as showing any window from a tray
/// callback). A click that lands right after a blur-hide is swallowed, so the
/// click that dismissed an open popover doesn't immediately re-open it.
fn toggle_popover(app: &AppHandle, rect: Rect) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(win) = app.get_webview_window("popover") else {
            return;
        };
        let state = app.state::<AppState>();
        let visible = win.is_visible().unwrap_or(false);
        let recently_hidden = state
            .popover_hidden_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed() < Duration::from_millis(250))
            .unwrap_or(false);

        if visible {
            let _ = win.hide();
            *state.popover_hidden_at.lock().unwrap() = Some(Instant::now());
        } else if !recently_hidden {
            let icon_w = state.last.lock().unwrap().icon_w.max(WIDGET_W);
            position_popover(&win, &rect, icon_w);
            // Cleared *before* the show: a focus-loss with no matching gain is
            // then distinguishable from a genuine click-outside.
            *state.popover_took_focus.lock().unwrap() = false;
            let _ = win.show();
            let _ = win.set_focus();
        }
    });
}

// --- tauri commands (called from the windows) -------------------------------

#[tauri::command]
fn get_snapshot(state: State<'_, AppState>) -> Snapshot {
    state.snapshot.lock().unwrap().clone()
}

#[tauri::command]
async fn reconcile(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    post(&state.client, &state.base_url, "reconcile").await?;
    poll_once(&app).await;
    Ok(())
}

#[tauri::command]
async fn set_paused(
    app: AppHandle,
    state: State<'_, AppState>,
    paused: bool,
) -> Result<(), String> {
    let path = if paused { "pause" } else { "resume" };
    post(&state.client, &state.base_url, path).await?;
    poll_once(&app).await;
    Ok(())
}

/// The body of a `POST /api/hold` request.
#[derive(Debug, Serialize)]
struct HoldRequest {
    reason: String,
}

/// `POST /api/hold` — set an operator hold on Docker admission, with a reason
/// the tray shows verbatim. Same fire-then-poll shape as `set_paused`, except
/// a rejected hold's `400` carries a message worth surfacing, so the body is
/// read out instead of collapsing to a bare status code.
#[tauri::command]
async fn hold(app: AppHandle, state: State<'_, AppState>, reason: String) -> Result<(), String> {
    let url = format!("{}/api/hold", state.base_url);
    let resp = state
        .client
        .post(&url)
        .json(&HoldRequest { reason })
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        poll_once(&app).await;
        Ok(())
    } else {
        Err(body_or_status(resp).await)
    }
}

/// `POST /api/unhold` — release an operator-set Docker-admission hold (no
/// body). Same shape as `hold` above. The windows only ever invoke this one —
/// the tray's own affordance is a Release button, not a Hold control — but
/// both commands are wired so the app can speak the daemon's full hold API.
#[tauri::command]
async fn unhold(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let url = format!("{}/api/unhold", state.base_url);
    let resp = state
        .client
        .post(&url)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        poll_once(&app).await;
        Ok(())
    } else {
        Err(body_or_status(resp).await)
    }
}

/// The body of a `POST /api/repo-hold/clear` request.
#[derive(Debug, Serialize)]
struct RepoHoldClear {
    repo: String,
}

/// `POST /api/repo-hold/clear` — clear a repo's minting hold (ADR-0030). Same
/// shape as `unhold`.
#[tauri::command]
async fn clear_repo_hold(
    app: AppHandle,
    state: State<'_, AppState>,
    repo: String,
) -> Result<(), String> {
    let url = format!("{}/api/repo-hold/clear", state.base_url);
    let resp = state
        .client
        .post(&url)
        .json(&RepoHoldClear { repo })
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        poll_once(&app).await;
        Ok(())
    } else {
        Err(body_or_status(resp).await)
    }
}

// --- attention dismissals (ADR-0064) -----------------------------------------

/// The body of a `POST /api/attention/dismiss` request.
#[derive(Debug, Serialize)]
struct AttentionDismiss {
    key: String,
    ident: String,
}

/// The body of a `POST /api/attention/restore` request: no key restores every row.
#[derive(Debug, Serialize)]
struct AttentionRestore {
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// The body of a `POST /api/attention/ack-trust` request: `null` withdraws it.
#[derive(Debug, Serialize)]
struct TrustAck {
    until_ms: Option<i64>,
}

/// POST `body` to `/api/attention/<route>`, then poll, so the popover redraws
/// from the daemon's list rather than waiting for the next tick. Same shape as
/// `clear_repo_hold`: a refusal comes back as the daemon's own text.
async fn attention_post<B: Serialize>(
    app: &AppHandle,
    state: &AppState,
    route: &str,
    body: &B,
) -> Result<(), String> {
    let url = format!("{}/api/attention/{route}", state.base_url);
    let resp = state
        .client
        .post(&url)
        .json(body)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        poll_once(app).await;
        Ok(())
    } else {
        Err(body_or_status(resp).await)
    }
}

/// Hide an attention row while its key keeps this identity (ADR-0064 § 2).
/// The daemon keeps the list for every surface (§ 3).
#[tauri::command]
async fn dismiss_attention(
    app: AppHandle,
    state: State<'_, AppState>,
    key: String,
    ident: String,
) -> Result<(), String> {
    attention_post(&app, &state, "dismiss", &AttentionDismiss { key, ident }).await
}

/// Show a dismissed row again, or every one when `key` is absent.
#[tauri::command]
async fn restore_attention(
    app: AppHandle,
    state: State<'_, AppState>,
    key: Option<String>,
) -> Result<(), String> {
    attention_post(&app, &state, "restore", &AttentionRestore { key }).await
}

/// Acknowledge every trust change at or before `until_ms` (ADR-0064 § 4).
#[tauri::command]
async fn ack_trust(
    app: AppHandle,
    state: State<'_, AppState>,
    until_ms: Option<i64>,
) -> Result<(), String> {
    attention_post(&app, &state, "ack-trust", &TrustAck { until_ms }).await
}

/// Where a one-time dashboard link lands once redeemed. A fixed set, so no
/// caller can steer the page anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// The dashboard at `/`.
    Dashboard,
    /// The Settings view at `/settings` (ADR-0028 addendum C, as amended
    /// 2026-10-22).
    Settings,
}

impl Next {
    /// `link`, a minted link already checked by
    /// [`dashboard_link::is_session_url`], sent on to `self`.
    fn link(self, link: &str) -> String {
        match self {
            Next::Dashboard => link.to_string(),
            Next::Settings => format!("{link}{}", dashboard_link::NEXT_SETTINGS),
        }
    }
}

#[tauri::command]
async fn open_dashboard(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    open_session(&app, &state, Next::Dashboard).await
}

/// Open the dashboard's Settings view through a fresh one-time link.
#[tauri::command]
async fn open_settings(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    open_session(&app, &state, Next::Settings).await
}

/// Mint a one-time dashboard link on the socket, landing on `next`. The result
/// has passed [`dashboard_link::is_session_url`].
async fn mint_link(state: &AppState, next: Next) -> Result<String, String> {
    // The socket mints a one-time loopback link (`POST /api/session`).
    let resp = state
        .client
        .post(format!("{}/api/session", state.base_url))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(body_or_status(resp).await);
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let url = body
        .get("url")
        .and_then(|u| u.as_str())
        .filter(|u| !u.contains("&next=") && dashboard_link::is_session_url(u))
        .ok_or_else(|| "daemon returned no usable session url".to_string())?;
    // The suffix goes on only after the daemon's link passed as a bare link,
    // and the result is checked again before it goes anywhere.
    let url = next.link(url);
    if !dashboard_link::is_session_url(&url) {
        return Err("daemon returned no usable session url".to_string());
    }
    Ok(url)
}

/// Mint a one-time dashboard link and hand it to the browser, landing on `next`.
async fn open_session(app: &AppHandle, state: &AppState, next: Next) -> Result<(), String> {
    let url = mint_link(state, next).await?;
    // The link never goes into an opener's argv, where any local account could
    // read it and spend it first: the browser opens a 0600 redirect page in a
    // directory only this account can enter.
    let dir = dashboard_link::private_dir(
        cfg!(target_os = "macos"),
        std::env::temp_dir(),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .filter(|d| dashboard_link::is_private(d))
    .ok_or_else(|| {
        "no private directory to hand the browser the link through; \
         run `faber dashboard` in a terminal instead"
            .to_string()
    })?;
    let page = dashboard_link::write_redirect(&dir, &url).map_err(|e| e.to_string())?;
    app.opener()
        .open_path(page.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Open the panel window on the dashboard (the popover's ⌘⇧F and Open panel):
/// every open of a closed panel mints a fresh one-time link and hands it straight to the webview
/// (ADR-0028 addendum C, decision 1). The window work is deferred onto the main
/// thread (AppKit), since this is invoked from the popover webview.
#[tauri::command]
async fn show_panel(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // An open panel is focused, not re-minted: a mint adds a session, the store
    // keeps 16, and a re-navigate drops the current view.
    if app.get_webview_window(panel::LABEL).is_some() {
        let handle = app.clone();
        return app
            .run_on_main_thread(move || panel::focus(&handle))
            .map_err(|e| e.to_string());
    }
    let link = mint_link(&state, Next::Dashboard).await?;
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = panel::open(&handle, &link) {
            eprintln!("faber: cannot open the panel: {e}");
        }
    })
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Whether this app is registered to start at login. The popover footer owns
/// this toggle now that the tray menu is gone (see the tray builder for why).
#[tauri::command]
fn autostart_enabled(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Turn launch-at-login on or off, and report the state actually in force —
/// not the one that was asked for, so the footer cannot drift from reality if
/// the login item refuses to register.
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> bool {
    let al = app.autolaunch();
    let _ = if enabled { al.enable() } else { al.disable() };
    al.is_enabled().unwrap_or(enabled)
}

/// Size the popover window to its rendered content. The webview measures its
/// card after each render and calls this with the card's width plus its side
/// room (308 or 620 px of card) and its height. The 800 cap is `WINDOW_MAX` in
/// ui/src/popover/wiring.ts, which folds job groups to fit under it.
///
/// The right edge stays where it is, so the beak, measured from it, stays under
/// the tray icon when the card widens or narrows.
#[tauri::command]
fn resize_popover(app: AppHandle, width: f64, height: f64) {
    let Some(win) = app.get_webview_window("popover") else {
        return;
    };
    let width = width.clamp(POPOVER_W, POPOVER_W_WIDE);
    let scale = win.scale_factor().unwrap_or(1.0);
    let moved = match (win.outer_position(), win.outer_size()) {
        (Ok(pos), Ok(size)) => {
            let dx = size.width as f64 - width * scale;
            (dx.abs() >= 1.0).then(|| {
                let x = clamp_to_monitor(&win, pos.x as f64 + dx, width * scale, scale);
                PhysicalPosition::new(x.round() as i32, pos.y)
            })
        }
        _ => None,
    };
    let _ = win.set_size(tauri::LogicalSize::new(width, height.clamp(80.0, 800.0)));
    if let Some(at) = moved {
        let _ = win.set_position(at);
    }
}

/// A ceiling value the Limits panel can send: `"auto"` / `"off"`, or a plain
/// positive number. Untagged so it serializes exactly as the daemon's own
/// `HardCeiling` does on the wire — a bare string or a bare number, never a
/// wrapped object — and the daemon's `ConfigUpdate` deserializer accepts it
/// unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum HardCeilingArg {
    Word(String),
    Number(u32),
}

/// The body of a `POST /api/config` autolimiter tuning update. Each field optional
/// so the panel can change one knob without disturbing the others. `hard_ceiling`
/// is tri-state: absent = unchanged; present = the new setting (`"auto"`, `"off"`
/// or a number) — the panel always sends one on Apply, never `null`, now that
/// "off" is its own selectable mode rather than a separate "clear" action.
#[derive(Debug, Serialize)]
struct ConfigUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve_mib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headroom_mib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hard_ceiling: Option<HardCeilingArg>,
}

/// Push a live autolimiter tuning change. `hard_ceiling` is `"auto"`, `"off"` or
/// a positive number, from the Limits panel's mode select plus number input.
#[tauri::command]
async fn set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    reserve_mib: Option<u64>,
    headroom_mib: Option<u64>,
    margin: Option<f64>,
    native_concurrency: Option<u32>,
    hard_ceiling: Option<HardCeilingArg>,
) -> Result<(), String> {
    let url = format!("{}/api/config", state.base_url);
    let resp = state
        .client
        .post(&url)
        .json(&ConfigUpdate {
            reserve_mib,
            headroom_mib,
            margin,
            native_concurrency,
            hard_ceiling,
        })
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        // Re-poll and push the snapshot, so the popover's Limits view shows the new value now.
        poll_once(&app).await;
        Ok(())
    } else {
        Err(body_or_status(resp).await)
    }
}

// --- fleet autoupdate (ADR-0006) ---------------------------------------------

/// `POST /api/update/apply` — stop waiting for an idle window and drain +
/// install the staged update right away. Same fire-then-poll shape as
/// `reconcile`/`set_paused`, so the row reflects the new phase immediately
/// instead of waiting for the next 2s tick.
#[tauri::command]
async fn apply_update(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    post(&state.client, &state.base_url, "update/apply").await?;
    poll_once(&app).await;
    Ok(())
}

/// `POST /api/update/skip` — quarantine the currently-available version so
/// the updater stops offering it and moves on.
#[tauri::command]
async fn skip_update(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    post(&state.client, &state.base_url, "update/skip").await?;
    poll_once(&app).await;
    Ok(())
}

// --- the app's own update (ADR-0034, #80) -----------------------------------

/// Set the app's own maturity floor, in the operator's app preferences (never
/// `config.toml`, which `_faber` can write). Under an hour is refused.
#[tauri::command]
fn set_update_floor(app: AppHandle, secs: u64) -> Result<u64, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    updater::save_floor(&dir, secs)
}

/// The updater loop's window onto the app.
struct TauriHost(AppHandle);

impl updater::Host for TauriHost {
    fn daemon_facts(&self) -> Option<updater::DaemonFacts> {
        let snap = self.0.state::<AppState>().snapshot.lock().unwrap().clone();
        let u = snap.typed?.update;
        let sc = u.self_check.as_ref();
        let text = |k: &str| {
            sc.and_then(|v| v.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        Some(updater::DaemonFacts {
            current: u.current,
            rolled_back_from: u.rolled_back_from,
            self_check: text("version").zip(text("status")),
        })
    }

    fn popover_closed_for(&self) -> Option<Duration> {
        let state = self.0.state::<AppState>();
        let visible = self
            .0
            .get_webview_window("popover")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        // The panel window is activity too: never swap the app out from under it.
        let panel_open = self
            .0
            .get_webview_window(panel::LABEL)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if visible || panel_open {
            return None;
        }
        let since = state
            .popover_hidden_at
            .lock()
            .unwrap()
            .unwrap_or(state.started);
        Some(since.elapsed())
    }

    fn publish(&self, view: updater::AppUpdateView) {
        *self.0.state::<AppState>().app_update.lock().unwrap() = view;
    }

    fn quit(&self) {
        self.0.exit(0);
    }
}

// --- polling ----------------------------------------------------------------

/// One poll cycle: fetch state, fold it into the load history, cache the
/// snapshot, and push it to the tray + windows. Also called right after a
/// control command so the UI reflects the change without waiting for the tick.
async fn poll_once(app: &AppHandle) {
    let (client, base, socket) = {
        let s = app.state::<AppState>();
        (s.client.clone(), s.base_url.clone(), s.socket.clone())
    };
    let mut snap = match fetch_state(&client, &base).await {
        Ok((raw, st)) => Snapshot {
            online: true,
            base_url: socket.clone(),
            error: None,
            state: Some(raw),
            typed: Some(st),
            history: Vec::new(),
            app_version: APP_VERSION.to_string(),
            app_update: updater::AppUpdateView::default(),
            trust_shrank: false,
        },
        Err(e) => Snapshot::offline(socket.clone(), Some(e)),
    };
    {
        let s = app.state::<AppState>();
        let mut hist = s.history.lock().unwrap();
        // Record this tick's load (freeze while offline, so a dropout doesn't
        // punch a hole in the trend).
        if let Some(st) = &snap.typed {
            if hist.len() >= HISTORY_LEN {
                hist.pop_front();
            }
            hist.push_back(st.running());
        }
        snap.history = hist.iter().copied().collect();
    }
    snap.app_update = app.state::<AppState>().app_update.lock().unwrap().clone();
    if let (Some(raw), Ok(dir)) = (&snap.state, app.path().app_config_dir()) {
        let seen = trust::load_seen(&dir);
        if let Some(out) = trust::decide(&seen, raw, updater::now_ms()) {
            if out.seen != seen {
                let _ = trust::save_seen(&dir, &out.seen);
            }
            snap.trust_shrank = out.shrank;
        }
    }
    *app.state::<AppState>().snapshot.lock().unwrap() = snap.clone();
    apply_snapshot(app, &snap);
}

// --- entrypoint -------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let socket = socket_path(
        |k| std::env::var(k).ok(),
        |p| std::path::Path::new(p).exists(),
        cfg!(target_os = "macos"),
    );
    let client = reqwest::Client::builder()
        .unix_socket(socket.as_str())
        .build()
        .expect("build control-socket client");
    let state = AppState {
        client,
        base_url: base_url(),
        socket: socket.clone(),
        snapshot: Mutex::new(Snapshot::offline(socket, None)),
        history: Mutex::new(VecDeque::with_capacity(HISTORY_LEN)),
        last: Mutex::new(LastApplied::default()),
        popover_hidden_at: Mutex::new(None),
        popover_took_focus: Mutex::new(false),
        started: Instant::now(),
        app_update: Mutex::new(updater::AppUpdateView::default()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            reconcile,
            set_paused,
            hold,
            unhold,
            clear_repo_hold,
            dismiss_attention,
            restore_attention,
            ack_trust,
            open_dashboard,
            open_settings,
            show_panel,
            quit_app,
            autostart_enabled,
            set_autostart,
            resize_popover,
            set_config,
            apply_update,
            skip_update,
            set_update_floor
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();

            // The user asked for launch-at-login: enable it the first time the
            // real bundle runs (never in `tauri dev`, where the login item would
            // point at the throwaway dev binary). The footer toggle can undo it.
            if !tauri::is_dev() {
                let _ = handle.autolaunch().enable();
            }

            // The popover, created hidden; shown under the tray on left-click.
            build_popover(&handle)?;

            let initial = handle.state::<AppState>().snapshot.lock().unwrap().clone();

            let tray_handle = handle.clone();
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(render_widget(&widget_kind(&initial)))
                .icon_as_template(true)
                .tooltip(tray_tooltip(&initial))
                // No menu is attached, and that is load-bearing rather than a
                // simplification. macOS 27 delivers a status item's `mouseDown:`
                // to the attached NSMenu instead of to the button's subview, so
                // tray-icon's own click target never fires and `show_menu_on_
                // left_click(false)` has nothing to suppress: the menu opens on
                // left-click and the popover is never asked to appear. Measured
                // on 0.3.12 — the tray emitted Enter/Move/Leave and not one
                // Click until the menu came off.
                //
                // Upstream fixed this in tray-icon 0.25.1 (PR #365) by attaching
                // the menu only while it is being presented. We cannot take that
                // fix: tauri 2.11.5 requires tray-icon ^0.24 and cargo refuses
                // 0.25.1. When tauri bumps, restore the menu and this comment's
                // reason for existing disappears with it.
                //
                // "Start at login" moved to the popover footer rather than being
                // dropped — AGENTS.md: a release must not withdraw a capability.
                .on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        toggle_popover(&tray_handle, rect);
                    }
                })
                .build(app)?;

            // The app follows the daemon onto new releases by itself (ADR-0034).
            if let Ok(dir) = handle.path().app_config_dir() {
                updater::on_startup(dir.clone(), APP_VERSION);
                let st = handle.state::<AppState>();
                let (client, base) = (st.client.clone(), st.base_url.clone());
                tauri::async_runtime::spawn(updater::run(
                    std::sync::Arc::new(TauriHost(handle.clone())),
                    client,
                    reqwest::Client::new(),
                    base,
                    dir,
                    APP_VERSION,
                ));
            }

            // Poll the daemon forever, updating tray + windows each tick.
            let poll_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    poll_once(&poll_handle).await;
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            });

            Ok(())
        })
        // Closing the panel destroys it, and its session key with it: the next
        // open mints a fresh link. The app lives on in the tray and the popover.
        .on_window_event(|window, event| match event {
            // It really is focused, so a later loss is a real click-outside.
            tauri::WindowEvent::Focused(true) if window.label() == "popover" => {
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    *state.popover_took_focus.lock().unwrap() = true;
                }
            }
            // The popover auto-closes when it loses focus (click-outside) — but
            // only if it ever held focus. Where the platform refuses to make it
            // key, the blur lands immediately after `show()` and means nothing;
            // dismissal there falls back to a second tray click, which toggles it
            // shut. Defaulting to `true` keeps the old behaviour if state is gone.
            tauri::WindowEvent::Focused(false) if window.label() == "popover" => {
                let state = window.app_handle().try_state::<AppState>();
                let took_focus = state
                    .as_ref()
                    .map(|s| *s.popover_took_focus.lock().unwrap())
                    .unwrap_or(true);
                if took_focus {
                    let _ = window.hide();
                    if let Some(s) = state {
                        *s.popover_hidden_at.lock().unwrap() = Some(Instant::now());
                    }
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running faber menu-bar app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_goes_on_only_to_a_fixed_next() {
        let link = format!(
            "http://127.0.0.1:8787/session?token={}",
            "0123456789abcdef".repeat(4)
        );
        assert_eq!(Next::Dashboard.link(&link), link);
        let settings = Next::Settings.link(&link);
        assert_eq!(settings, format!("{link}&next=settings"));
        assert!(dashboard_link::is_session_url(&settings));
        // Sent on twice, it is no longer a link the app will open.
        assert!(!dashboard_link::is_session_url(
            &Next::Settings.link(&settings)
        ));
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn socket_path_env_override_wins() {
        let env = env_of(&[("FABER_SOCKET", "/x/c.sock"), ("HOME", "/h")]);
        assert_eq!(socket_path(env, |_| true, true), "/x/c.sock");
    }

    #[test]
    fn socket_path_system_when_setup_json_exists() {
        let env = env_of(&[("HOME", "/h")]);
        assert_eq!(
            socket_path(&env, |p| p == "/etc/faber/setup.json", true),
            "/Library/Application Support/faber/control.sock"
        );
        assert_eq!(
            socket_path(&env, |p| p == "/etc/faber/setup.json", false),
            "/var/lib/faber/control.sock"
        );
    }

    #[test]
    fn socket_path_legacy_fallbacks() {
        let env = env_of(&[("HOME", "/h")]);
        assert_eq!(
            socket_path(&env, |_| false, true),
            "/h/Library/Application Support/faber/control.sock"
        );
        assert_eq!(
            socket_path(&env, |_| false, false),
            "/h/.local/state/faber/control.sock"
        );
        let env = env_of(&[("HOME", "/h"), ("XDG_STATE_HOME", "/s")]);
        assert_eq!(socket_path(env, |_| false, false), "/s/faber/control.sock");
    }

    /// A snapshot holding `admitted` running claims and `parked` queued ones,
    /// from an older daemon that sends no `phase`.
    fn snap_with(admitted: usize, parked: usize) -> Snapshot {
        let claim = |id: usize, admitted: bool| serde_json::json!({"job_id": id, "profile": "docker", "admitted": admitted});
        let inflight: Vec<_> = (0..admitted)
            .map(|i| claim(i, true))
            .chain((0..parked).map(|i| claim(admitted + i, false)))
            .collect();
        snap_of(inflight)
    }

    /// A snapshot holding one claim per phase listed, as the daemon sends it.
    fn snap_phases(phases: &[&str]) -> Snapshot {
        let inflight: Vec<_> = phases
            .iter()
            .enumerate()
            .map(|(i, p)| serde_json::json!({"job_id": i, "profile": "docker", "admitted": *p != "queued", "phase": p}))
            .collect();
        snap_of(inflight)
    }

    fn snap_of(inflight: Vec<serde_json::Value>) -> Snapshot {
        let st: StateView = serde_json::from_value(serde_json::json!({
            "owner": "owner", "topic": "faber", "profiles": [], "concurrency": 6,
            "paused": false, "draining": false, "uptime_s": 1, "audit_path": "",
            "inflight": inflight,
            "counts": {"provisioned": 0, "completed": 0, "faults": 0, "refused": 0, "retried": 0},
            "recent": [],
        }))
        .unwrap();
        Snapshot {
            online: true,
            base_url: String::new(),
            error: None,
            state: None,
            typed: Some(st),
            history: Vec::new(),
            app_version: String::new(),
            app_update: updater::AppUpdateView::default(),
            trust_shrank: false,
        }
    }

    #[test]
    fn fields_the_typed_mirror_lacks_reach_the_webview_verbatim() {
        let raw = serde_json::json!({
            "owner": "owner", "topic": "faber", "profiles": [], "concurrency": 6,
            "paused": false, "draining": false, "uptime_s": 1, "audit_path": "",
            "inflight": [],
            "counts": {"provisioned": 0, "completed": 0, "faults": 0, "refused": 0, "retried": 0},
            "recent": [], "event": "e", "verdict": "v", "since_ms": 1,
            "peers": [{"name": "builder"}],
            "budget": {"headroom": {"mib": 1234}, "gates": ["memory"]},
        });
        let typed: StateView = serde_json::from_value(raw.clone()).unwrap();
        let snap = Snapshot {
            online: true,
            base_url: String::new(),
            error: None,
            state: Some(raw),
            typed: Some(typed),
            history: Vec::new(),
            app_version: String::new(),
            app_update: updater::AppUpdateView::default(),
            trust_shrank: false,
        };
        let sent = serde_json::to_value(&snap).unwrap();
        assert_eq!(sent["state"]["peers"][0]["name"], "builder");
        assert_eq!(sent["state"]["budget"]["headroom"]["mib"], 1234);
        assert!(sent.get("typed").is_none());
    }

    #[test]
    fn the_tray_shows_running_over_every_claimed_job() {
        // Six admitted out of a hundred claims: the other 94 are the shared queue.
        let snap = snap_with(6, 94);
        assert_eq!(
            widget_kind(&snap),
            WidgetKind::Jobs {
                running: 6,
                total: 100
            }
        );
        let tip = tray_tooltip(&snap);
        assert!(
            tip.contains("· 6 running · 94 waiting (94 queued) ·"),
            "{tip}"
        );
    }

    #[test]
    fn only_a_started_job_counts_as_running() {
        // Admitted but booting, and frozen, both wait; only `running` runs.
        let snap = snap_phases(&["running", "starting", "starting", "frozen", "queued"]);
        assert_eq!(
            widget_kind(&snap),
            WidgetKind::Jobs {
                running: 1,
                total: 5
            }
        );
        let tip = tray_tooltip(&snap);
        assert!(
            tip.contains("· 1 running · 4 waiting (1 queued, 2 starting, 1 frozen) ·"),
            "{tip}"
        );
    }

    #[test]
    fn a_host_holding_only_waiting_claims_shows_them_and_one_with_none_is_idle() {
        let snap = snap_with(0, 12);
        assert_eq!(
            widget_kind(&snap),
            WidgetKind::Jobs {
                running: 0,
                total: 12
            }
        );
        assert_eq!(status_word(&snap), "waiting");
        assert_eq!(widget_kind(&snap_with(0, 0)), WidgetKind::Idle);
        assert_eq!(status_word(&snap_with(0, 0)), "idle");
    }

    #[test]
    fn the_count_clears_the_mark_and_fits_its_canvas() {
        for (running, total) in [(0, 1), (1, 10), (3, 57), (99, 999), (5, 12_000)] {
            let r = render_raster(&WidgetKind::Jobs { running, total });
            // Nothing lands left of the count's start or in the last column.
            for y in 0..WIDGET_H {
                assert_eq!(r.a[y * r.w + r.w - 1], 0.0, "{running}/{total} right edge");
                for x in 38..(JOBS_X as usize - 2) {
                    assert_eq!(r.a[y * r.w + x], 0.0, "{running}/{total} gap at x {x}");
                }
            }
        }
        // The canvas grows with the digits, and only with them.
        let w = |running, total| widget_width(&WidgetKind::Jobs { running, total });
        assert_eq!(w(1, 10), w(7, 42));
        assert!(w(99, 999) > w(1, 10));
    }

    /// Dumps raw alpha for the design preview: `WIDGET_DUMP=<dir> cargo test -- --ignored dump`.
    #[test]
    #[ignore]
    fn dump_widgets() {
        let dir = std::env::var("WIDGET_DUMP").unwrap();
        let kinds = [
            ("idle", WidgetKind::Idle),
            ("paused", WidgetKind::Paused),
            (
                "jobs-1-10",
                WidgetKind::Jobs {
                    running: 1,
                    total: 10,
                },
            ),
            (
                "jobs-0-57",
                WidgetKind::Jobs {
                    running: 0,
                    total: 57,
                },
            ),
            (
                "jobs-3-57",
                WidgetKind::Jobs {
                    running: 3,
                    total: 57,
                },
            ),
            (
                "jobs-12-140",
                WidgetKind::Jobs {
                    running: 12,
                    total: 140,
                },
            ),
        ];
        for (name, kind) in kinds {
            let r = render_raster(&kind);
            let bytes: Vec<u8> = r.a.iter().map(|a| (a * 255.0).round() as u8).collect();
            std::fs::write(format!("{dir}/{name}-{}x{WIDGET_H}.alpha", r.w), bytes).unwrap();
        }
    }

    /// Coverage of the pixel under mark-space point `(u, v)`.
    fn coverage_at(r: &Raster, u: f32, v: f32) -> f32 {
        let (x, y) = mk(u, v);
        r.a[y.floor() as usize * r.w + x.floor() as usize]
    }

    #[test]
    fn tray_mark_draws_shell_and_core_and_keeps_the_moat_open() {
        let r = render_raster(&WidgetKind::Idle);
        let near = |got: f32, want: f32| (got - want).abs() < 0.02;
        let top = coverage_at(&r, 50.0, 27.0);
        assert!(near(top, MARK_SHELL_ALPHA[0]), "shell top face, got {top}");
        let left = coverage_at(&r, 28.0, 55.0);
        assert!(
            near(left, MARK_SHELL_ALPHA[1]),
            "shell left face, got {left}"
        );
        let right = coverage_at(&r, 72.0, 55.0);
        assert!(
            near(right, MARK_SHELL_ALPHA[2]),
            "shell right face, got {right}"
        );
        let core = coverage_at(&r, 50.0, 43.0);
        assert!(near(core, MARK_CORE_ALPHA[0]), "core top face, got {core}");
        let core = coverage_at(&r, 58.0, 55.0);
        assert!(
            near(core, MARK_CORE_ALPHA[2]),
            "core right face, got {core}"
        );
        assert!(coverage_at(&r, 50.0, 34.5) < 0.01, "moat above the core");
        assert!(coverage_at(&r, 34.5, 52.0) < 0.01, "moat left of the core");
        assert!(coverage_at(&r, 12.0, 50.0) < 0.01, "outside the hexagon");
    }

    #[test]
    fn offline_tray_mark_is_faded_and_struck_through() {
        let r = render_raster(&WidgetKind::Offline);
        let off_slash = coverage_at(&r, 28.0, 62.0);
        let want = OFFLINE_MARK_ALPHA * MARK_SHELL_ALPHA[1];
        assert!(
            (off_slash - want).abs() < 0.02,
            "shell away from the slash is faded, got {off_slash}"
        );
        assert!(coverage_at(&r, 50.0, 50.0) > 0.99, "slash at full strength");
    }

    fn state_json(extra: serde_json::Value) -> StateView {
        let mut v = serde_json::json!({
            "owner": "owner", "topic": "faber", "profiles": [], "concurrency": 6,
            "paused": false, "draining": false, "uptime_s": 1, "audit_path": "",
            "inflight": [],
            "counts": {"provisioned": 0, "completed": 0, "faults": 0, "refused": 0, "retried": 0},
            "recent": [],
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn the_reserve_state_and_frozen_reason_pass_through_to_the_popover() {
        let st = state_json(serde_json::json!({
            "budget": {"reserve": {
                "mode": "p75", "in_force": "max", "cooling": true, "squeeze": false,
                "freeze_armed": false, "trips_7d": 2, "next_rearm_ms": 1_700_000_000_000u64,
                "last_trip": {"at_ms": 1, "cause": "freeze", "cooloff_ms": 86_400_000u64, "until_ms": 2},
            }},
            "inflight": [{"job_id": 1, "profile": "docker", "admitted": true,
                "frozen": {"since_ms": 5, "reason": "memory", "detail": "low"}}],
        }));
        let r = st.budget.reserve.as_ref().unwrap();
        assert_eq!((r.mode.as_str(), r.in_force.as_str()), ("p75", "max"));
        assert!(r.cooling && r.trips_7d == 2);
        assert_eq!(r.next_rearm_ms, Some(1_700_000_000_000));
        assert_eq!(r.last_trip.as_ref().unwrap().cause, "freeze");
        assert_eq!(st.inflight[0].frozen.as_ref().unwrap().reason, "memory");
        // The webview sees them again after the re-serialize.
        let out = serde_json::to_value(&st).unwrap();
        assert_eq!(out["budget"]["reserve"]["last_trip"]["cause"], "freeze");
        assert_eq!(out["inflight"][0]["frozen"]["reason"], "memory");
    }

    #[test]
    fn an_older_daemon_without_reserve_or_frozen_still_parses() {
        let st = state_json(serde_json::json!({
            "budget": {"budget_mib": 16384},
            "inflight": [{"job_id": 1, "profile": "docker", "admitted": true}],
        }));
        assert!(st.budget.reserve.is_none());
        assert!(st.inflight[0].frozen.is_none());
    }
}
