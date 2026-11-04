//! The control API knows its caller (ADR-0028, phase 1).
//!
//! The daemon's routes are served on two listeners, behind one front door
//! ([`FrontDoor`]):
//!
//! - **A Unix socket**, `control.sock` in the state directory. Every connection
//!   is classified by the uid the kernel reports for its peer
//!   ([`crate::platform::peer_uid`], [`class_for_uid`]). The operator's account
//!   gets the API, root gets it too, and every other account, `_faber` and
//!   `_faber_vm` included, gets `403` and [`REFUSED_BODY`] for every request:
//!   no route runs.
//! - **TCP `127.0.0.1:<port>`**, for the browser dashboard only. Every request
//!   passes [`FrontDoor::admit`]: a loopback `Host`, a live session cookie, the
//!   session's key in `X-Faber-Session` for anything but the page itself, and
//!   an exact loopback `Origin` on anything that is not a read. A session is
//!   minted only through the socket (`POST /api/session`), as a one-time link.
//!
//! Every decision is a pure function here, tested without a network:
//! [`class_for_uid`], [`authorize`], [`trust_widening`], [`Sessions`] (clock and
//! randomness injected) and [`FrontDoor::admit`].
//!
//! Phase 1 already asks for root: a trust-widening change needs
//! [`WIDENING_NEEDS`], [`Class::Present`], which phase 1 grants only to a root
//! socket caller (`sudo faber config set …`, issue #78). Phase 2 swaps `sudo`
//! for Touch ID or the login password; the constant stays where it is.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{RawQuery, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{any, get, post},
    Extension, Router,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{BackendKind, FileConfig};
use crate::platform::layout::{FABER_ACCOUNT, FABER_OPERATOR_GROUP};
use crate::platform::{Layout, Resolution};

// --- who is calling ---------------------------------------------------------

/// How far a caller is trusted. Ordered: `Refused < Operator < Present`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Any account but the operator's and root's. Answered with
    /// [`REFUSED_BODY`] and nothing else.
    Refused,
    /// The operator's account, or a dashboard session (which only an operator
    /// could have minted).
    Operator,
    /// Proof the operator is present. In phase 1 that is root: `sudo faber …`.
    Present,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Refused => "refused",
            Class::Operator => "operator",
            Class::Present => "present",
        }
    }
}

/// The class of a socket peer running as `uid`, on a host whose operator is
/// `operator_uid` (the setup record's `operator.uid` under SYSTEM, the daemon's
/// own euid under LEGACY).
pub fn class_for_uid(uid: u32, operator_uid: u32) -> Class {
    if uid == 0 {
        Class::Present
    } else if uid == operator_uid {
        Class::Operator
    } else {
        Class::Refused
    }
}

/// Who made a request, as a handler finds it in the request's extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A socket peer, with the uid the kernel reported.
    Socket { uid: u32, class: Class },
    /// A browser holding a live dashboard session on the loopback listener.
    Session,
}

impl Caller {
    pub fn class(self) -> Class {
        match self {
            Caller::Socket { class, .. } => class,
            Caller::Session => Class::Operator,
        }
    }

    pub fn uid(self) -> Option<u32> {
        match self {
            Caller::Socket { uid, .. } => Some(uid),
            Caller::Session => None,
        }
    }

    /// `socket` or `session`, for the audit line.
    pub fn via(self) -> &'static str {
        match self {
            Caller::Socket { .. } => "socket",
            Caller::Session => "session",
        }
    }
}

/// Everything a refused socket caller is told, for every request.
pub const REFUSED_BODY: &str = "faber's control API answers only its operator's account.\n";

// --- what a change needs ----------------------------------------------------

/// One trust-widening change in a settings save: a stable key and what it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Widening {
    /// The config key, e.g. `profile.linux-docker.dind` or `update.pin`.
    pub key: String,
    /// What changes, old -> new, for a person.
    pub sentence: String,
}

fn widening(key: impl Into<String>, sentence: impl Into<String>) -> Widening {
    Widening {
        key: key.into(),
        sentence: sentence.into(),
    }
}

/// The class a trust-widening change needs.
///
/// [`Class::Present`] (issue #78): a process running as the operator, or a
/// dashboard session, may make every routine change but no widening one. In
/// phase 1 the only present caller is root on the socket, so each widening
/// takes `sudo` ([`sudo_command`] says which command); phase 2 lets Touch ID or
/// the login password prove presence instead, under this same constant.
pub const WIDENING_NEEDS: Class = Class::Present;

/// What [`authorize`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead. `audit` is true exactly when the change widens trust.
    Allow { audit: bool },
    /// Refuse, saying why.
    Refuse(String),
}

/// May a caller of `class` make a change with these `widenings`?
pub fn authorize(class: Class, widenings: &[Widening]) -> Verdict {
    authorize_with(class, widenings, WIDENING_NEEDS)
}

/// [`authorize`] against an explicit widening threshold, so the phase-2 rule is
/// tested before the constant moves.
fn authorize_with(class: Class, widenings: &[Widening], needs: Class) -> Verdict {
    if class == Class::Refused {
        return Verdict::Refuse(REFUSED_BODY.trim_end().to_string());
    }
    if widenings.is_empty() {
        return Verdict::Allow { audit: false };
    }
    if class >= needs {
        return Verdict::Allow { audit: true };
    }
    let keys: Vec<&str> = widenings.iter().map(|w| w.key.as_str()).collect();
    Verdict::Refuse(format!(
        "{} widen{} trust and need{} a {} caller{}; this one is {}",
        keys.join(", "),
        if keys.len() == 1 { "s" } else { "" },
        if keys.len() == 1 { "s" } else { "" },
        needs.as_str(),
        if needs == Class::Present {
            " (sudo)"
        } else {
            ""
        },
        class.as_str()
    ))
}

/// A refused widening as the settings API reports it: what, which class it
/// needs, and the command that makes it as that class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Needs {
    pub key: String,
    pub sentence: String,
    pub needs: Class,
    /// The exact command that makes this change ([`sudo_command`]).
    pub command: String,
}

/// Every widening of the change `old` -> `new`, each with the class it needs
/// and the command that makes it.
pub fn needs(widenings: &[Widening], old: &FileConfig, new: &FileConfig) -> Vec<Needs> {
    widenings
        .iter()
        .map(|w| Needs {
            key: w.key.clone(),
            sentence: w.sentence.clone(),
            needs: WIDENING_NEEDS,
            command: sudo_command(w, old, new),
        })
        .collect()
}

/// The command that makes widening `w` of the change `old` -> `new` as root:
/// one `sudo faber config set` (or `unset`) where a single key says it,
/// `sudo faber repo-hold clear` for a repo hold, and `sudo faber config apply`
/// with the edited file for what one key cannot express (profile routing, a
/// new profile, a profile's `env` table, which `config set` refuses as a map,
/// `[needs]`, which has no settings field, a key this function does not know).
///
/// `old` is needed only to tell a new profile from a changed one: `config set
/// profile.<name>.…` edits a profile that exists.
pub fn sudo_command(w: &Widening, old: &FileConfig, new: &FileConfig) -> String {
    const APPLY: &str = "sudo faber config apply <edited config.toml>";
    let set = |key: &str, value: &str| {
        format!(
            "sudo faber config set {} {}",
            shell_quote(key),
            shell_quote(value)
        )
    };
    let unset = |key: &str| format!("sudo faber config unset {}", shell_quote(key));
    let list = |key: &str, items: &[String]| {
        if items.is_empty() {
            unset(key)
        } else {
            set(key, &items.join(","))
        }
    };
    if let Some(repo) = w.key.strip_prefix("repo_hold.") {
        return format!("sudo faber repo-hold clear {}", shell_quote(repo));
    }
    if let Some(name) = w
        .key
        .strip_prefix("profile.")
        .and_then(|rest| rest.strip_suffix(".dind"))
    {
        let existed = old.profiles.iter().any(|p| p.name == name);
        let now = new.profiles.iter().find(|p| p.name == name);
        return match now {
            Some(p) if existed => set(
                &w.key,
                if p.dind.unwrap_or(true) {
                    "true"
                } else {
                    "false"
                },
            ),
            _ => APPLY.to_string(),
        };
    }
    if let Some(name) = w
        .key
        .strip_prefix("profile.")
        .and_then(|rest| rest.strip_suffix(".drop_grants"))
    {
        return match new.profiles.iter().find(|p| p.name == name) {
            Some(p) if old.profiles.iter().any(|o| o.name == name) => list(&w.key, &p.drop_grants),
            _ => APPLY.to_string(),
        };
    }
    if let Some(name) = w
        .key
        .strip_prefix("profile.")
        .and_then(|rest| rest.strip_suffix(".cache_volume"))
    {
        return match new.profiles.iter().find(|p| p.name == name) {
            Some(p) if old.profiles.iter().any(|o| o.name == name) => {
                set(&w.key, &p.cache_volume.to_string())
            }
            _ => APPLY.to_string(),
        };
    }
    for field in ["image", "dind_image"] {
        let Some(name) = w
            .key
            .strip_prefix("profile.")
            .and_then(|rest| rest.strip_suffix(field))
            .and_then(|rest| rest.strip_suffix('.'))
        else {
            continue;
        };
        let value = new.profiles.iter().find(|p| p.name == name).map(|p| {
            if field == "image" {
                &p.image
            } else {
                &p.dind_image
            }
        });
        return match value {
            Some(v) if old.profiles.iter().any(|o| o.name == name) => match v {
                Some(v) => set(&w.key, v),
                None => unset(&w.key),
            },
            _ => APPLY.to_string(),
        };
    }
    let (g, u, r, h) = (&new.gate, &new.update, &new.registry_auth, &new.host);
    match w.key.as_str() {
        "gate.allowed_events" => list(&w.key, &g.allowed_events),
        "gate.allow_public_repos" => set(&w.key, &g.allow_public_repos.to_string()),
        "gate.allow_job_token_dispatches" => set(&w.key, &g.allow_job_token_dispatches.to_string()),
        "gate.on_bypass" => set(
            &w.key,
            match g.on_bypass {
                crate::gate::OnBypass::Record => "record",
                crate::gate::OnBypass::Enforce => "enforce",
            },
        ),
        "trusted_actors" => list(&w.key, &new.trusted_actors),
        "denied_actors" => list(&w.key, &new.denied_actors),
        "host.dind_requires_label" => match h.dind_requires_label {
            Some(v) => set(&w.key, &v.to_string()),
            None => unset(&w.key),
        },
        "update.repo" => set(&w.key, &u.repo),
        "update.pin" => match &u.pin {
            Some(pin) => set(&w.key, pin),
            None => unset(&w.key),
        },
        "update.maturity_secs" => set(&w.key, &u.maturity_secs.to_string()),
        "update.enabled" => set(&w.key, &u.enabled.to_string()),
        "registry_auth.enabled" => set(&w.key, &r.enabled.to_string()),
        "registry_auth.server" => set(&w.key, &r.server),
        "registry_auth.password_env" => set(&w.key, &r.password_env),
        "registry_auth.username" if r.username.is_empty() => unset(&w.key),
        "registry_auth.username" => set(&w.key, &r.username),
        // Reported also when the recipe stays and `build` is switched on, which
        // one key cannot say.
        "runner_image.dockerfile" => match &new.runner_image.dockerfile {
            Some(path) if old.runner_image.dockerfile.as_ref() != Some(path) => set(&w.key, path),
            _ => APPLY.to_string(),
        },
        _ => APPLY.to_string(),
    }
}

/// `value` as one shell word: as it is when every character is in
/// `[A-Za-z0-9._,/:@=+-]`, else in single quotes, each `'` written `'\''`.
pub fn shell_quote(value: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "._,/:@=+-".contains(c);
    if !value.is_empty() && value.chars().all(plain) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// The trust-widening changes from `old` to `new` (ADR-0028 § Phase 2, as
/// amended by ADR-0031): dind granted, a profile's dropped grant withdrawn,
/// the dind label rule turned off
/// (`[host] dind_requires_label` true -> false or unstated), `[gate]` and the actor lists widened,
/// the update channel moved, pinned, hurried
/// or switched on, and registry credentials switched on or repointed while on
/// (addendum A), the images beside a Docker daemon changed (#220), any
/// profile routing changed while a profile drops grants (#221), an operator
/// recipe for runner-image variants put in force, and (#238) the `[needs]`
/// ceiling raised, a profile's `env` gaining or changing an entry, and a
/// profile's `cache_volume` turned on, and (ADR-0046) a `[services.*]` table
/// added, its `image` changed, or an extension or Postgres major added. Empty
/// means the change is routine.
///
/// The test for each field (#238): does it widen what runs next to a
/// privileged container, a Docker socket or a secret? The narrowing direction
/// of each stays routine: a `[needs]` tool removed or an exact list that only
/// loses versions, an `env` entry removed, `cache_volume` turned off, a
/// `[services.*]` table removed or one losing extensions or versions.
///
/// Comparisons are exact: a respelled entry reads as one gone and one gained,
/// which can only over-report.
pub fn trust_widening(old: &FileConfig, new: &FileConfig) -> Vec<Widening> {
    let mut out = Vec::new();

    // 1. dind granted. Absent `dind` is effective `true` (ADR-0013).
    for p in new
        .profiles
        .iter()
        .filter(|p| p.backend == BackendKind::Docker)
    {
        if !p.dind.unwrap_or(true) {
            continue;
        }
        let prev = old
            .profiles
            .iter()
            .find(|o| o.name == p.name && o.backend == BackendKind::Docker);
        let key = format!("profile.{}.dind", p.name);
        match prev.map(|o| (o, o.dind.unwrap_or(true))) {
            // Still dind: routing changes are checked after the loop.
            Some((_, true)) => {}
            Some((_, false)) => out.push(widening(
                key,
                format!(
                    "profile {} grants its jobs a Docker daemon: dind false -> true",
                    p.name
                ),
            )),
            None => out.push(widening(
                key,
                format!(
                    "new docker profile {} grants its jobs a Docker daemon (dind true)",
                    p.name
                ),
            )),
        }
    }

    // 1a. What runs beside a Docker daemon (#220): the runner image holds the
    // daemon's socket and the dind image is the privileged container itself, so
    // changing either on a profile that keeps (or gains) dind widens. A new
    // profile is already reported as a grant above.
    for p in new
        .profiles
        .iter()
        .filter(|p| p.backend == BackendKind::Docker && p.dind.unwrap_or(true))
    {
        let Some(prev) = old
            .profiles
            .iter()
            .find(|o| o.name == p.name && o.backend == BackendKind::Docker)
        else {
            continue;
        };
        for (field, before, after) in [
            ("image", &prev.image, &p.image),
            ("dind_image", &prev.dind_image, &p.dind_image),
        ] {
            if before != after {
                let shown = |v: &Option<String>| v.clone().unwrap_or_else(|| "unset".into());
                out.push(widening(
                    format!("profile.{}.{field}", p.name),
                    format!(
                        "profile {} {field} {} -> {}: it runs beside the job's Docker daemon",
                        p.name,
                        shown(before),
                        shown(after)
                    ),
                ));
            }
        }
    }

    // 1b. Routing. `match_profile` sends a job to the FIRST profile whose
    // labels are all on the job, so any change to the profiles at or ahead of
    // the last dind-effective docker profile can hand jobs to a Docker daemon.
    // Over-reports (a gained label narrows a profile); never under-reports.
    let route = |ps: &[crate::config::FileProfile], n: usize| {
        ps.iter()
            .take(n)
            .map(|p| {
                let labels: std::collections::BTreeSet<&str> =
                    p.labels.iter().map(String::as_str).collect();
                (
                    p.name.clone(),
                    p.backend,
                    labels.into_iter().map(String::from).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let last_dind = new
        .profiles
        .iter()
        .rposition(|p| p.backend == BackendKind::Docker && p.dind.unwrap_or(true));
    let ahead_of_dind =
        last_dind.filter(|&k| route(&old.profiles, k + 1) != route(&new.profiles, k + 1));
    if let Some(k) = ahead_of_dind {
        out.push(widening(
            "profiles.routing",
            format!(
                "profile routing changed ahead of dind profile {}: jobs may now reach a Docker daemon",
                new.profiles[k].name
            ),
        ));
    } else if let Some(dropping) = old.profiles.iter().find(|p| !p.drop_grants.is_empty()) {
        // While a profile drops grants (#221), any routing change can move its
        // jobs to a profile that drops fewer: a rename or a removal leaves the
        // per-name check in 1c nothing to compare, and a reorder, a relabel or
        // a new profile ahead of it takes its jobs without touching it at all.
        // Stricter than comparing grants per routing outcome, on purpose.
        if route(&old.profiles, usize::MAX) != route(&new.profiles, usize::MAX) {
            out.push(widening(
                "profiles.routing",
                format!(
                    "profile routing changed while profile {} drops grants: its jobs may now \
                     run where fewer grants are dropped",
                    dropping.name
                ),
            ));
        }
    }

    // 1c. The dind label rule turned off (true -> false or unstated): every job
    // on a granting profile gets a Docker daemon, whether or not it asks.
    if old.host.dind_label_rule().requires_label() && !new.host.dind_label_rule().requires_label() {
        out.push(widening(
            "host.dind_requires_label",
            "the dind label rule is turned off: every job on a granting profile gets a Docker daemon, whether or not it asks (dind_requires_label true -> false|unstated)",
        ));
    }

    // 1c. A dropped grant withdrawn: the next root render of the wall
    // (`sudo faber grant`) re-opens what root granted to this profile's jobs.
    for p in &new.profiles {
        let Some(prev) = old.profiles.iter().find(|o| o.name == p.name) else {
            continue;
        };
        let lost = gained(&p.drop_grants, &prev.drop_grants);
        if !lost.is_empty() {
            out.push(widening(
                format!("profile.{}.drop_grants", p.name),
                format!(
                    "profile {} drop_grants loses {}: the grant reopens at the next wall render",
                    p.name,
                    lost.join(", ")
                ),
            ));
        }
    }

    // 2. [gate], and the actor lists the gate reads.
    let events = gained(&old.gate.allowed_events, &new.gate.allowed_events);
    if !events.is_empty() {
        out.push(widening(
            "gate.allowed_events",
            format!("gate.allowed_events gains {}", events.join(", ")),
        ));
    }
    if !old.gate.allow_public_repos && new.gate.allow_public_repos {
        out.push(widening(
            "gate.allow_public_repos",
            "gate.allow_public_repos false -> true",
        ));
    }
    if !old.gate.allow_job_token_dispatches && new.gate.allow_job_token_dispatches {
        out.push(widening(
            "gate.allow_job_token_dispatches",
            "gate.allow_job_token_dispatches false -> true: a job's own token can dispatch the \
             code it pushed",
        ));
    }
    if old.gate.on_bypass == crate::gate::OnBypass::Enforce
        && new.gate.on_bypass == crate::gate::OnBypass::Record
    {
        out.push(widening(
            "gate.on_bypass",
            "gate.on_bypass enforce -> record",
        ));
    }
    if !old.trusted_actors.is_empty() {
        if new.trusted_actors.is_empty() {
            out.push(widening(
                "trusted_actors",
                format!(
                    "trusted_actors emptied ({} -> none): every actor is trusted",
                    old.trusted_actors.join(", ")
                ),
            ));
        } else {
            let actors = gained(&old.trusted_actors, &new.trusted_actors);
            if !actors.is_empty() {
                out.push(widening(
                    "trusted_actors",
                    format!("trusted_actors gains {}", actors.join(", ")),
                ));
            }
        }
    }
    let undenied = gained(&new.denied_actors, &old.denied_actors);
    if !undenied.is_empty() {
        out.push(widening(
            "denied_actors",
            format!("denied_actors loses {}", undenied.join(", ")),
        ));
    }

    // 4.-6. The update channel.
    let (u0, u1) = (&old.update, &new.update);
    if u0.repo != u1.repo {
        out.push(widening(
            "update.repo",
            format!("update.repo {} -> {}", u0.repo, u1.repo),
        ));
    }
    match (&u0.pin, &u1.pin) {
        (None, Some(to)) => out.push(widening("update.pin", format!("update.pin none -> {to}"))),
        (Some(from), Some(to)) if from != to => {
            out.push(widening("update.pin", format!("update.pin {from} -> {to}")))
        }
        _ => {}
    }
    // The wait actually applied, on both sides: a value under the floor is
    // applied as the default, so comparing raw numbers could miss a real drop.
    let (m0, m1) = (u0.effective_maturity_secs(), u1.effective_maturity_secs());
    if m1 < m0 {
        out.push(widening(
            "update.maturity_secs",
            format!("update.maturity_secs {m0} -> {m1}"),
        ));
    }
    if !u0.enabled && u1.enabled {
        out.push(widening(
            "update.enabled",
            "update.enabled false -> true: this host installs releases on its own",
        ));
    }

    // 7. Registry credentials (ADR-0028 addendum A): turning them on, and,
    // while they are on in the new config, pointing them anywhere else.
    let (r0, r1) = (&old.registry_auth, &new.registry_auth);
    if !r0.enabled && r1.enabled {
        out.push(widening(
            "registry_auth.enabled",
            format!(
                "registry_auth.enabled false -> true: jobs' images are pulled as {} on {} \
                 (password from ${})",
                if r1.username.is_empty() {
                    "the account in ~/.docker/config.json"
                } else {
                    r1.username.as_str()
                },
                r1.server,
                r1.password_env
            ),
        ));
    }
    if r1.enabled {
        for (field, before, after) in [
            ("password_env", &r0.password_env, &r1.password_env),
            ("server", &r0.server, &r1.server),
            ("username", &r0.username, &r1.username),
        ] {
            if before != after {
                out.push(widening(
                    format!("registry_auth.{field}"),
                    format!("registry_auth.{field} {before:?} -> {after:?}"),
                ));
            }
        }
    }

    // 8. An operator recipe for runner-image variants (ADR-0008) put in force:
    // set or moved while variants are built, or kept while building is switched
    // on. A variant is the image a job runs on, beside its Docker daemon when it
    // has one, so a recipe is the same grant as a profile's `image` (#220).
    // Back to faber's own template, or building off, is routine.
    let (i0, i1) = (&old.runner_image, &new.runner_image);
    if let Some(recipe) = i1.dockerfile.as_ref().filter(|_| i1.build.enabled()) {
        let before = i0.dockerfile.as_ref().filter(|_| i0.build.enabled());
        if before != Some(recipe) {
            out.push(widening(
                "runner_image.dockerfile",
                format!(
                    "runner_image.dockerfile {} -> {recipe}: runner-image variants are built from \
                     this recipe",
                    before.map_or("faber's own template", String::as_str)
                ),
            ));
        }
    }

    // 9. The `[needs]` ceiling (ADR-0010 §4, #238). A variant built for a
    // gained version runs as the job's image, beside its Docker daemon, and is
    // shared by later jobs. A new tool or a changed `versions` string widens;
    // a removed tool (it fails closed: `UnknownTool`) or an exact list that
    // only loses versions does not. A range change widens even when it
    // narrows: comparing ranges by containment is not worth the risk.
    //
    // An empty map is no ceiling at all, not an empty one: emptying it widens
    // to every version of every tool, and giving an empty map its first entry
    // only narrows.
    if !old.needs.is_empty() {
        if new.needs.is_empty() {
            out.push(widening(
                "needs",
                "[needs] emptied: no ceiling, so runner-image variants may be built for any \
                 version of any tool",
            ));
        }
        let mut tools: Vec<&String> = new.needs.keys().collect();
        tools.sort();
        for tool in tools {
            let after = &new.needs[tool].versions;
            let sentence = match old.needs.get(tool) {
                None => format!(
                    "[needs.{tool}] added (versions {after:?}): runner-image variants may be \
                     built for it"
                ),
                Some(b) if b.versions == *after => continue,
                Some(b) if crate::config::needs_list_only_narrows(&b.versions, after) => continue,
                Some(b) => format!(
                    "needs.{tool}.versions {:?} -> {after:?}: runner-image variants may be built \
                     for other versions",
                    b.versions
                ),
            };
            out.push(widening(format!("needs.{tool}.versions"), sentence));
        }
    }

    // 10. A profile's `env`, on every profile and backend (#238). Runner-level
    // variables act before and around the job's steps, and the job cannot
    // override them: `HTTPS_PROXY` routes the runner's credentialed traffic,
    // `ACTIONS_RUNNER_HOOK_*` and `LD_PRELOAD` run code ahead of the job. An
    // added or changed entry widens, on a new profile too; a removed one does
    // not. Names only in the sentence: a value may be a secret.
    for p in &new.profiles {
        let before = old
            .profiles
            .iter()
            .find(|o| o.name == p.name)
            .map(|o| &o.env);
        let changed: Vec<&str> = p
            .env
            .iter()
            .filter(|(k, v)| before.and_then(|b| b.get(*k)) != Some(*v))
            .map(|(k, _)| k.as_str())
            .collect();
        if !changed.is_empty() {
            let (who, what) = if before.is_some() {
                ("profile", "adds or changes")
            } else {
                ("new profile", "sets")
            };
            out.push(widening(
                format!("profile.{}.env", p.name),
                format!(
                    "{who} {} env {what} {}: runner-level variables act around every step of \
                     its jobs",
                    p.name,
                    changed.join(", ")
                ),
            ));
        }
    }

    // 11. `cache_volume` turned on (#238): the one state that crosses the job
    // boundary, so a pull-request job can seed what a later push job, which has
    // secrets, reads. A new profile with it on widens too; turning it off does
    // not.
    for p in new.profiles.iter().filter(|p| p.cache_volume) {
        let before = old.profiles.iter().find(|o| o.name == p.name);
        if before.is_some_and(|o| o.cache_volume) {
            continue;
        }
        let (who, change) = if before.is_some() {
            ("profile", "false -> true")
        } else {
            ("new profile", "true")
        };
        out.push(widening(
            format!("profile.{}.cache_volume", p.name),
            format!(
                "{who} {} cache_volume {change}: a build cache crosses the job boundary, so one \
                 job can seed what a later one reads",
                p.name
            ),
        ));
    }

    // 12. `[services.*]` (ADR-0046 §10). A service image runs on the network
    // of every job that declares it, and the shared Postgres's extensions are
    // installed by its superuser into the template every job's database is
    // made from. A table added, an `image` changed, an extension added or a
    // Postgres major added widens; a table removed, an entry dropped or a
    // `memory_mib` or `idle_stop_secs` change does not. A per-job table's
    // `args`, `env` and `health_cmd` are the code that runs on those networks
    // too (A4), so a change to any of them widens; `job_env` only sets what a
    // job could set itself, so it is routine. An `env` value may be a
    // credential: the sentence names the keys that changed, never a value.
    for (name, after) in &new.services {
        let Some(before) = old.services.get(name) else {
            out.push(widening(
                format!("services.{name}"),
                format!(
                    "[services.{name}] added (image {}): it runs on the network of every job \
                     that declares faber.{name}",
                    after.image
                ),
            ));
            continue;
        };
        if before.image != after.image {
            out.push(widening(
                format!("services.{name}.image"),
                format!(
                    "services.{name}.image {} -> {}: it runs on the network of every job that \
                     declares faber.{name}",
                    before.image, after.image
                ),
            ));
        }
        if before.args != after.args {
            out.push(widening(
                format!("services.{name}.args"),
                format!(
                    "services.{name}.args {:?} -> {:?}: the command it runs on the network of \
                     every job that declares faber.{name}",
                    before.args, after.args
                ),
            ));
        }
        if before.env != after.env {
            let changed: Vec<&str> = before
                .env
                .keys()
                .chain(after.env.keys())
                .filter(|k| before.env.get(*k) != after.env.get(*k))
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            out.push(widening(
                format!("services.{name}.env"),
                format!(
                    "services.{name}.env changes {}: the environment of a container on the \
                     network of every job that declares faber.{name}",
                    changed.join(", ")
                ),
            ));
        }
        if before.health_cmd != after.health_cmd {
            out.push(widening(
                format!("services.{name}.health_cmd"),
                format!(
                    "services.{name}.health_cmd {:?} -> {:?}: a command run inside the \
                     container of every job that declares faber.{name}",
                    before.health_cmd.as_deref().unwrap_or(""),
                    after.health_cmd.as_deref().unwrap_or("")
                ),
            ));
        }
        let gained: Vec<&str> = after
            .extensions
            .iter()
            .filter(|e| !before.extensions.contains(e))
            .map(String::as_str)
            .collect();
        if !gained.is_empty() {
            out.push(widening(
                format!("services.{name}.extensions"),
                format!(
                    "services.{name}.extensions adds {}: installed by the server's superuser \
                     into every job's database",
                    gained.join(", ")
                ),
            ));
        }
        if services_versions_gain(before.versions.as_deref(), after.versions.as_deref()) {
            out.push(widening(
                format!("services.{name}.versions"),
                format!(
                    "services.{name}.versions {:?} -> {:?}: another image may run for jobs \
                     that declare it",
                    before.versions.as_deref().unwrap_or(""),
                    after.versions.as_deref().unwrap_or("")
                ),
            ));
        }
    }

    out
}

/// Whether a `[services.<name>] versions` change `old` -> `new` adds a major
/// (ADR-0046 §10). Each added major runs another image. A string that does not
/// parse on either side is compared as written, which can only over-report.
fn services_versions_gain(old: Option<&str>, new: Option<&str>) -> bool {
    use crate::config::parse_service_versions;
    let Some(new) = new else {
        return false;
    };
    match (old.map(parse_service_versions), parse_service_versions(new)) {
        (Some(Ok(before)), Ok(after)) => after.iter().any(|v| !before.contains(v)),
        _ => old.map(str::trim) != Some(new.trim()),
    }
}

/// A digest of `cfg`, the base a dry run judged a config against (#221). `faber
/// config apply` sends it back with the real save, and the daemon refuses that
/// save with `409` when its base has moved, so root never applies a widening it
/// was not shown. Keys are sorted, so the digest is the content's, not a map's
/// iteration order.
pub fn config_digest(cfg: &FileConfig) -> serde_json::Result<String> {
    fn canonical(v: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::Object(m) => {
                let mut entries: Vec<_> = m.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k, canonical(v)))
                        .collect(),
                )
            }
            Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
            other => other,
        }
    }
    let bytes = serde_json::to_vec(&canonical(serde_json::to_value(cfg)?))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

/// The widening a repo-hold clear is (ADR-0028 addendum A): clearing `hold`
/// lets faber mint for its repo again.
pub fn repo_hold_cleared(hold: &crate::config::RepoHold) -> Widening {
    widening(
        format!("repo_hold.{}", hold.repo),
        format!(
            "minting hold cleared for {} (held since {}: {:?})",
            hold.repo,
            hold.since_utc(),
            capped(&hold.reason, 120)
        ),
    )
}

/// `text` cut to at most `max` chars, with an ellipsis when it was cut.
fn capped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max).collect();
        cut.push('…');
        cut
    }
}

/// The entries of `after` that `before` lacks.
fn gained<'a>(before: &[String], after: &'a [String]) -> Vec<&'a str> {
    after
        .iter()
        .filter(|e| !before.contains(e))
        .map(String::as_str)
        .collect()
}

// --- dashboard sessions -----------------------------------------------------

/// How long a one-time dashboard link stays redeemable.
pub const LINK_TTL: Duration = Duration::from_secs(60);
/// A session's absolute lifetime.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// At most this many live sessions; the oldest goes first.
pub const MAX_SESSIONS: usize = 16;
/// At most this many unredeemed links; the oldest goes first.
pub const MAX_LINKS: usize = 16;

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "faber_session";
/// The header carrying the session's key on every API call.
pub const SESSION_HEADER: &str = "x-faber-session";

/// Wall-clock milliseconds, injected so tests can move time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// 32 random bytes, injected so tests are deterministic.
pub type Entropy = Arc<dyn Fn() -> io::Result<[u8; 32]> + Send + Sync>;

type Digest32 = [u8; 32];

/// Dashboard sessions, in memory only: a daemon restart ends every one.
///
/// Holds SHA-256 digests of the secrets, never the secrets themselves.
pub struct Sessions {
    clock: Clock,
    entropy: Entropy,
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    links: VecDeque<Pending>,
    live: VecDeque<Live>,
}

struct Pending {
    token: Digest32,
    expires_ms: u64,
}

struct Live {
    cookie: Digest32,
    key: Digest32,
    expires_ms: u64,
}

impl SessionState {
    fn purge(&mut self, now_ms: u64) {
        self.links.retain(|l| l.expires_ms > now_ms);
        self.live.retain(|s| s.expires_ms > now_ms);
    }
}

/// A redeemed link: the cookie for the browser, and the key for its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub cookie: String,
    pub key: String,
}

/// What a cookie and key are worth. Ordered by how much they open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    /// No live session has this cookie.
    NoSession,
    /// A live session, but the key is missing or not its key.
    NoKey,
    /// A live session, and its key.
    Valid,
}

impl Sessions {
    pub fn new(clock: Clock, entropy: Entropy) -> Self {
        Self {
            clock,
            entropy,
            state: Mutex::new(SessionState::default()),
        }
    }

    /// The daemon's store: the wall clock, and `/dev/urandom`.
    pub fn system() -> Self {
        Self::new(Arc::new(crate::audit::now_ms), Arc::new(urandom))
    }

    fn secret(&self) -> io::Result<String> {
        Ok(hex(&(self.entropy)()?))
    }

    /// Mint a one-time link token: 32 random bytes, hex.
    pub fn mint_link(&self) -> io::Result<String> {
        let token = self.secret()?;
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        state.purge(now);
        state.links.push_back(Pending {
            token: digest(&token),
            expires_ms: now.saturating_add(LINK_TTL.as_millis() as u64),
        });
        while state.links.len() > MAX_LINKS {
            state.links.pop_front();
        }
        Ok(token)
    }

    /// Spend a link: `None` unless `token` is a live, unused one. A link is
    /// gone once tried with its token, whatever happens next.
    pub fn redeem(&self, token: &str) -> io::Result<Option<Redeemed>> {
        let now = (self.clock)();
        {
            let mut state = self.state.lock().unwrap();
            state.purge(now);
            let wanted = digest(token);
            let Some(at) = state.links.iter().position(|l| l.token == wanted) else {
                return Ok(None);
            };
            state.links.remove(at);
        }
        let redeemed = Redeemed {
            cookie: self.secret()?,
            key: self.secret()?,
        };
        let mut state = self.state.lock().unwrap();
        state.live.push_back(Live {
            cookie: digest(&redeemed.cookie),
            key: digest(&redeemed.key),
            expires_ms: now.saturating_add(SESSION_TTL.as_millis() as u64),
        });
        while state.live.len() > MAX_SESSIONS {
            state.live.pop_front();
        }
        Ok(Some(redeemed))
    }

    /// What `cookie` and `key` open.
    pub fn check(&self, cookie: &str, key: Option<&str>) -> Check {
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        state.purge(now);
        let cookie = digest(cookie);
        let Some(live) = state.live.iter().find(|s| s.cookie == cookie) else {
            return Check::NoSession;
        };
        match key {
            Some(key) if digest(key) == live.key => Check::Valid,
            _ => Check::NoKey,
        }
    }
}

fn digest(secret: &str) -> Digest32 {
    Sha256::digest(secret.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn urandom() -> io::Result<[u8; 32]> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

// --- the loopback guard -----------------------------------------------------

/// Is `host` exactly a loopback name with this port?
pub fn host_is_loopback(host: &str, port: u16) -> bool {
    host == format!("127.0.0.1:{port}")
        || host == format!("localhost:{port}")
        || host == format!("[::1]:{port}")
}

/// Is `origin` exactly this dashboard's origin, under one of its loopback names?
pub fn origin_is_exact(origin: Option<&str>, port: u16) -> bool {
    origin.is_some_and(|o| {
        o == format!("http://127.0.0.1:{port}")
            || o == format!("http://localhost:{port}")
            || o == format!("http://[::1]:{port}")
    })
}

/// Every `faber_session` cookie a request carries.
fn session_cookies(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// The pages a browser navigates to, admitted on the session cookie alone: a
/// navigation cannot carry the key, which lives in the page's `sessionStorage`.
/// Each is a view of the React shell (ADR-0028 addendum C, as amended 2026-10-22
/// and 2026-10-28; ADR-0054 § 5; ADR-0056 § 5). Matched exactly: `/settings/` is not one of them.
pub(crate) const NAVIGATED: [&str; 6] = [
    "/",
    "/activity",
    "/jobs",
    "/memory",
    "/maintenance",
    "/settings",
];

/// The prefix of the React bundle's hashed assets, served without a session to
/// `GET`/`HEAD` (ADR-0028 addendum C, decision 2).
const ASSETS_PREFIX: &str = "/assets/";

/// What the loopback guard does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Serve it without a session: `/session`, and a `GET`/`HEAD` under
    /// `/assets/`.
    Open,
    /// Serve it as [`Caller::Session`].
    Session,
    /// Answer this status and nothing else.
    Deny(StatusCode),
}

// --- the front door ---------------------------------------------------------

/// The socket's mode after bind: `0660` under SYSTEM (the operator group's,
/// [`socket_group`]), `0600` under LEGACY (the daemon is the operator).
pub fn socket_mode(layout: Layout) -> u32 {
    match layout {
        Layout::System => 0o660,
        Layout::Legacy => 0o600,
    }
}

/// The group the socket is given after bind: the operator group the setup
/// record names, which the operator reaches the daemon through. `None` under
/// LEGACY, and with a record from before the operator group, whose operator
/// reaches it through `_faber`'s group, which the socket takes from its
/// directory (macOS) or the daemon (Linux).
pub fn socket_group(resolution: &Resolution) -> Option<u32> {
    resolution.record().and_then(|r| r.operator_gid())
}

/// The daemon's two listeners and the sessions they share.
///
/// One per process, built at boot: standby's stub and the live dashboard both
/// serve through it, so a session minted in standby keeps working once the
/// daemon is up.
pub struct FrontDoor {
    port: u16,
    socket: Option<PathBuf>,
    socket_mode: u32,
    socket_group: Option<u32>,
    operator_uid: u32,
    sessions: Arc<Sessions>,
}

impl std::fmt::Debug for FrontDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontDoor")
            .field("port", &self.port)
            .field("socket", &self.socket)
            .field("socket_mode", &format_args!("{:o}", self.socket_mode))
            .field("socket_group", &self.socket_group)
            .field("operator_uid", &self.operator_uid)
            .finish_non_exhaustive()
    }
}

impl FrontDoor {
    /// `socket: None` serves loopback only, with no way to mint a session; the
    /// daemon has that only when it cannot find its state directory.
    /// `socket_group`, when set, is the group the socket is given after bind.
    pub fn new(
        port: u16,
        socket: Option<PathBuf>,
        socket_mode: u32,
        socket_group: Option<u32>,
        operator_uid: u32,
        sessions: Arc<Sessions>,
    ) -> Arc<Self> {
        Arc::new(Self {
            port,
            socket,
            socket_mode,
            socket_group,
            operator_uid,
            sessions,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// The loopback guard's decision, in the spec's order: a loopback `Host`;
    /// `/session` open; a `GET`/`HEAD` under `/assets/` open; a live session
    /// cookie; its key on everything but the pages in [`NAVIGATED`]; an exact
    /// `Origin` on anything but `GET`/`HEAD`.
    ///
    /// The key is required beyond `/api/*`: the pages are the only things a
    /// browser navigates to, and everything else is fetched by them.
    ///
    /// `/assets/` holds the React bundle's content-hashed build output, which
    /// carries no data (ADR-0028 addendum C, decision 2). Its exemption is
    /// decided on `path` as given, the raw path the router matches, never a
    /// decoded or normalised form of it, so the guard and the asset handler
    /// cannot disagree about which file a request names. A mutation there
    /// falls through to the session rules: `401`, `403`, then the router's
    /// `405`.
    pub fn admit(&self, method: &Method, path: &str, headers: &HeaderMap) -> Admit {
        let host = headers.get(header::HOST).map(|v| v.to_str().unwrap_or(""));
        match host {
            None => return Admit::Deny(StatusCode::BAD_REQUEST),
            Some(h) if !host_is_loopback(h, self.port) => {
                return Admit::Deny(StatusCode::MISDIRECTED_REQUEST)
            }
            Some(_) => {}
        }
        let read = *method == Method::GET || *method == Method::HEAD;
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if path == "/session" {
            if !read && !origin_is_exact(origin, self.port) {
                return Admit::Deny(StatusCode::FORBIDDEN);
            }
            return Admit::Open;
        }
        if read && path.starts_with(ASSETS_PREFIX) {
            return Admit::Open;
        }
        let key = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok());
        let check = session_cookies(headers)
            .map(|cookie| self.sessions.check(cookie, key))
            .max()
            .unwrap_or(Check::NoSession);
        match check {
            Check::NoSession => return Admit::Deny(StatusCode::UNAUTHORIZED),
            Check::NoKey if !NAVIGATED.contains(&path) => {
                return Admit::Deny(StatusCode::UNAUTHORIZED)
            }
            Check::NoKey | Check::Valid => {}
        }
        if !read && !origin_is_exact(origin, self.port) {
            return Admit::Deny(StatusCode::FORBIDDEN);
        }
        Admit::Session
    }

    /// `app` as the loopback listener serves it: behind the guard, plus
    /// `GET /session`, and with `/api/session` answering `404`.
    pub fn loopback_router(self: &Arc<Self>, app: Router) -> Router {
        let own = Router::new()
            .route("/session", get(redeem))
            .route("/api/session", any(not_found))
            .with_state(self.clone());
        app.merge(own)
            .layer(middleware::from_fn_with_state(self.clone(), loopback_guard))
    }

    /// `app` as the socket serves it to an admitted caller: plus
    /// `POST /api/session`, and with `/session` answering `404`.
    pub fn socket_router(self: &Arc<Self>, app: Router) -> Router {
        let own = Router::new()
            .route("/api/session", post(mint))
            .route("/session", any(not_found))
            .with_state(self.clone());
        app.merge(own)
    }

    /// Bind both listeners and serve `app` on them until [`Serving::stop`].
    ///
    /// Fails if either bind fails, so a daemon never comes up half-reachable
    /// without saying so.
    pub async fn start(self: &Arc<Self>, app: Router) -> io::Result<Serving> {
        let tcp = crate::web::bind_listener(
            SocketAddr::from(([127, 0, 0, 1], self.port)),
            crate::web::BIND_ATTEMPTS,
            crate::web::BIND_PAUSE,
        )
        .await?;
        let unix = match &self.socket {
            Some(path) => Some(
                bind_socket(path, self.socket_mode, self.socket_group).map_err(|e| {
                    io::Error::new(e.kind(), format!("control socket {}: {e}", path.display()))
                })?,
            ),
            None => None,
        };
        let addr = tcp.local_addr()?;
        let (stop, stopped) = tokio::sync::watch::channel(false);

        let loopback = self.loopback_router(app.clone());
        let mut tcp_stopped = stopped.clone();
        let tcp_task = tokio::spawn(async move {
            axum::serve(tcp, loopback)
                .with_graceful_shutdown(async move {
                    let _ = tcp_stopped.changed().await;
                })
                .await
        });
        let socket_task = unix.map(|listener| {
            tokio::spawn(serve_socket(
                listener,
                self.operator_uid,
                self.socket_router(app),
                stopped,
            ))
        });
        Ok(Serving {
            addr,
            stop,
            tcp: tcp_task,
            socket: socket_task,
        })
    }
}

/// Both listeners, running.
pub struct Serving {
    /// The loopback address served.
    pub addr: SocketAddr,
    stop: tokio::sync::watch::Sender<bool>,
    tcp: tokio::task::JoinHandle<io::Result<()>>,
    socket: Option<tokio::task::JoinHandle<()>>,
}

impl Serving {
    /// Serve until the loopback listener fails. The daemon's dashboard.
    pub async fn wait(self) -> io::Result<()> {
        let Serving {
            stop, tcp, socket, ..
        } = self;
        let served = tcp.await.unwrap_or_else(|e| Err(io::Error::other(e)));
        drop(stop);
        if let Some(socket) = socket {
            socket.abort();
        }
        served
    }

    /// Stop both listeners, letting requests in flight finish for up to
    /// `grace` each, and return once both are closed.
    pub async fn stop(self, grace: Duration) {
        let _ = self.stop.send(true);
        let mut tcp = self.tcp;
        if tokio::time::timeout(grace, &mut tcp).await.is_err() {
            tcp.abort();
            let _ = tcp.await;
        }
        if let Some(mut socket) = self.socket {
            if tokio::time::timeout(grace, &mut socket).await.is_err() {
                socket.abort();
                let _ = socket.await;
            }
        }
    }
}

/// Clear the way for binding `path`: nothing there is fine, a socket is a
/// stale one of ours and is removed, and anything else (a file, a directory, a
/// symlink) is an error and is left alone.
pub fn clear_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket; faber will not remove it",
                path.display()
            ),
        )),
    }
}

/// Bind the socket at `path` and give it `mode` and, when set, `group`: the
/// operator group, which the daemon is in and so may give its own socket. A
/// group it cannot give is said loudly and the socket served as it is, since
/// only the operator's way in is lost, and `sudo faber setup` is the fix; a
/// daemon that refused to start would lose the loopback dashboard too.
fn bind_socket(path: &Path, mode: u32, group: Option<u32>) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    clear_stale_socket(path)?;
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if let Some(gid) = group {
        if let Err(e) = std::os::unix::fs::chown(path, None, Some(gid)) {
            tracing::warn!(
                "control socket {}: could not give it the operator group (gid {gid}): {e}. The \
                 operator cannot reach the daemon through it until `sudo faber setup` puts \
                 {FABER_ACCOUNT} in {FABER_OPERATOR_GROUP}",
                path.display()
            );
        }
    }
    Ok(listener)
}

/// Accept socket connections until `stop` changes, classifying each by its
/// peer's uid. A refused peer is served [`REFUSED_BODY`] by a router with no
/// routes at all; a peer whose uid cannot be read is dropped.
async fn serve_socket(
    listener: tokio::net::UnixListener,
    operator_uid: u32,
    admitted: Router,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let refused = Router::new().fallback(|| async { (StatusCode::FORBIDDEN, REFUSED_BODY) });
    let mut conns = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        tracing::warn!("control socket: accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let service = match crate::platform::peer_uid(&stream) {
                    Ok(uid) => match class_for_uid(uid, operator_uid) {
                        Class::Refused => {
                            tracing::debug!("control socket: refused uid {uid}");
                            refused.clone()
                        }
                        class => admitted.clone().layer(Extension(Caller::Socket { uid, class })),
                    },
                    Err(e) => {
                        tracing::warn!("control socket: cannot read the peer's uid ({e}); dropped");
                        continue;
                    }
                };
                conns.spawn(serve_connection(stream, service, stop.clone()));
                while conns.try_join_next().is_some() {}
            }
            _ = stop.changed() => break,
        }
    }
    drop(listener);
    while conns.join_next().await.is_some() {}
}

/// One socket connection, shut down gracefully once `stop` changes.
async fn serve_connection(
    stream: tokio::net::UnixStream,
    service: Router,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .serve_connection(
            hyper_util::rt::TokioIo::new(stream),
            hyper_util::service::TowerToHyperService::new(service),
        );
    tokio::pin!(conn);
    let mut stopping = false;
    loop {
        tokio::select! {
            served = conn.as_mut() => {
                if let Err(e) = served {
                    tracing::debug!("control socket: connection ended: {e}");
                }
                break;
            }
            _ = stop.changed(), if !stopping => {
                stopping = true;
                conn.as_mut().graceful_shutdown();
            }
        }
    }
}

async fn loopback_guard(
    State(door): State<Arc<FrontDoor>>,
    mut req: Request,
    next: Next,
) -> Response {
    match door.admit(req.method(), req.uri().path(), req.headers()) {
        Admit::Open => next.run(req).await,
        Admit::Session => {
            req.extensions_mut().insert(Caller::Session);
            next.run(req).await
        }
        Admit::Deny(status) if status == StatusCode::UNAUTHORIZED => {
            (status, Html(SIGN_IN_HTML)).into_response()
        }
        Admit::Deny(status) => status.into_response(),
    }
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// `POST /api/session`'s answer.
#[derive(Debug, Serialize)]
struct Minted {
    url: String,
    expires_in_secs: u64,
}

/// `POST /api/session`, socket only: a one-time dashboard link.
async fn mint(State(door): State<Arc<FrontDoor>>, caller: Option<Extension<Caller>>) -> Response {
    let Some(Extension(caller)) = caller else {
        return (StatusCode::FORBIDDEN, REFUSED_BODY).into_response();
    };
    if caller.class() < Class::Operator {
        return (StatusCode::FORBIDDEN, REFUSED_BODY).into_response();
    }
    match door.sessions.mint_link() {
        Ok(token) => {
            tracing::info!(
                "dashboard link minted for uid {}",
                caller
                    .uid()
                    .map_or_else(|| "-".to_string(), |u| u.to_string())
            );
            Json(Minted {
                url: format!("http://127.0.0.1:{}/session?token={token}", door.port),
                expires_in_secs: LINK_TTL.as_secs(),
            })
            .into_response()
        }
        Err(e) => {
            tracing::warn!("cannot mint a dashboard link: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /session?token=…`, loopback only: spend a link, set the cookie, hand
/// the page its key.
///
/// A `next=settings` pair sends the page on to `/settings`; any other `next`,
/// or none, to `/`. The value is matched against that allowlist and never
/// echoed, so the page's script holds only a path written here.
async fn redeem(State(door): State<Arc<FrontDoor>>, RawQuery(query): RawQuery) -> Response {
    let param = |wanted: &str| {
        query.as_deref().and_then(|q| {
            q.split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(name, _)| *name == wanted)
                .map(|(_, value)| value)
        })
    };
    let token = param("token");
    let next = redeem_target(param("next"));
    let redeemed = match token.map(|t| door.sessions.redeem(t)) {
        Some(Ok(Some(redeemed))) => redeemed,
        Some(Err(e)) => {
            tracing::warn!("cannot open a dashboard session: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        _ => return (StatusCode::UNAUTHORIZED, Html(SIGN_IN_HTML)).into_response(),
    };
    let cookie = format!(
        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
        redeemed.cookie
    );
    let Ok(cookie) = HeaderValue::from_str(&cookie) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = Html(format!(
        "<!doctype html><meta charset=\"utf-8\"><title>faber</title>\
         <script>sessionStorage.setItem('faber-key', '{}'); \
         sessionStorage.setItem('faber-since', String(Date.now())); location.replace('{next}')</script>",
        redeemed.key
    ))
    .into_response();
    let headers = response.headers_mut();
    headers.insert(header::SET_COOKIE, cookie);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Where a redeemed link sends the page: `/settings` for `next=settings`, `/`
/// for anything else. Only these two literals ever reach the redeem page.
fn redeem_target(next: Option<&str>) -> &'static str {
    match next {
        Some("settings") => "/settings",
        _ => "/",
    }
}

/// What a request with no session gets: how to open the dashboard, and
/// nothing about this host.
pub const SIGN_IN_HTML: &str = concat!(
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>faber</title>
<style>
"#,
    include_str!("../desktop/src/tokens.css"),
    r#"  body { font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
         background: var(--bg); color: var(--fg); margin: 0; padding: 24px; }
</style>
</head>
<body>
  <p>Open the dashboard with <code>faber dashboard</code>, or from the menu-bar app.</p>
</body>
</html>
"#
);

// --- the client side ---------------------------------------------------------

/// Whether [`request`] may fall back to plain TCP when the socket has no daemon.
///
/// Only a daemon from before ADR-0028 can be reached that way, and it can exist
/// only on a LEGACY host; on a SYSTEM host a missing socket means no daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The socket or nothing.
    Never,
    /// Then `127.0.0.1:port`.
    Tcp(u16),
}

/// The fallback for a command run on a host with this `resolution`: the
/// daemon's `port` only where no setup record exists. A host with a record,
/// trusted or not, was set up, so its daemon is post-cut and has no TCP port.
pub fn fallback_for(resolution: &crate::platform::Resolution, port: u16) -> Fallback {
    match resolution {
        crate::platform::Resolution::Legacy => Fallback::Tcp(port),
        crate::platform::Resolution::System(_) | crate::platform::Resolution::Untrusted(_) => {
            Fallback::Never
        }
    }
}

/// [`fallback_for`] this process's resolution.
pub fn fallback_here(port: u16) -> Fallback {
    fallback_for(crate::platform::resolution(), port)
}

impl Fallback {
    /// How a message names the places a daemon was looked for, after the socket.
    pub fn or_tcp(self) -> String {
        match self {
            Fallback::Never => String::new(),
            Fallback::Tcp(port) => format!(" or 127.0.0.1:{port}"),
        }
    }
}

/// Why [`request`] got no usable answer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing answers on the socket (or on loopback, when that was allowed).
    #[error("no faber daemon is answering on {}{}", .socket.display(), .fallback.or_tcp())]
    NoDaemon { socket: PathBuf, fallback: Fallback },
    /// A daemon answers, but not to this account.
    #[error(
        "the faber daemon answers only its operator's account (or root) on {}",
        .socket.display()
    )]
    Refused { socket: PathBuf },
    /// Anything else between here and the daemon.
    #[error("talking to the faber daemon: {0}")]
    Transport(String),
}

/// What a refused caller is told. `uid` is this process's, `operator_uid` the
/// trusted setup record's, and `group` the one the socket admits the operator
/// through: the operator is refused only by a login session that predates
/// their membership of it.
pub fn refused_message(socket: &Path, uid: u32, operator_uid: Option<u32>, group: &str) -> String {
    if operator_uid == Some(uid) {
        format!(
            "you are faber's operator, but this login session predates your {group} group \
             membership, so the control socket {} refused it: log out and back in (or open a \
             new login session).",
            socket.display()
        )
    } else {
        format!(
            "the daemon's control socket {} refused this account: only the operator's account \
             or root may use it. If you were just added to group {group}, a fresh login is \
             needed before the membership applies.",
            socket.display()
        )
    }
}

/// [`refused_message`] for this process: the operator group, or `_faber`'s on a
/// host whose record is from before it.
pub fn refused_text(socket: &Path) -> String {
    let resolution = crate::platform::resolution();
    let operator = resolution.record().map(|r| r.operator.uid);
    let group = match socket_group(resolution) {
        Some(_) => FABER_OPERATOR_GROUP,
        None => FABER_ACCOUNT,
    };
    refused_message(socket, crate::standby::euid(), operator, group)
}

/// How long one control request may take.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// One request to the running daemon: over `socket`, or, when there is no
/// socket to connect to and `fallback` allows it (a daemon from before
/// ADR-0028), over plain TCP `127.0.0.1:port`.
///
/// A refused socket caller, a socket this account may not open, and a `401` over
/// the fallback (a daemon that has a socket this account could not use) are all
/// [`ClientError::Refused`]. Every other status, `403` from a refused settings
/// change included, comes back as the answer.
pub async fn request(
    socket: &Path,
    fallback: Fallback,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(reqwest::StatusCode, String), ClientError> {
    // Nothing at the path: a daemon from before the socket, or none at all.
    if std::fs::symlink_metadata(socket).is_ok() {
        let over_socket = reqwest::Client::builder()
            .unix_socket(socket.to_path_buf())
            .timeout(CLIENT_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let url = format!("http://localhost{path}");
        match send(&over_socket, method.clone(), url, body.as_ref()).await {
            Ok((status, text))
                if status == reqwest::StatusCode::FORBIDDEN && text == REFUSED_BODY =>
            {
                return Err(ClientError::Refused {
                    socket: socket.to_path_buf(),
                })
            }
            Ok(answer) => return Ok(answer),
            Err(SendError::TooLarge) => return Err(ClientError::Transport(TOO_LARGE.into())),
            Err(SendError::Http(e)) => match io_kind(&e) {
                // A socket file nobody listens on: try loopback, if allowed.
                Some(io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {}
                // The socket's mode shuts this account out.
                Some(io::ErrorKind::PermissionDenied) => {
                    return Err(ClientError::Refused {
                        socket: socket.to_path_buf(),
                    })
                }
                // A failed connect whose cause is not on record: loopback says
                // whether a daemon is there, and whether it takes this caller.
                None if e.is_connect() => {}
                _ => return Err(ClientError::Transport(e.to_string())),
            },
        }
    }

    let Fallback::Tcp(port) = fallback else {
        return Err(ClientError::NoDaemon {
            socket: socket.to_path_buf(),
            fallback,
        });
    };
    let over_tcp = reqwest::Client::builder()
        .no_proxy()
        .timeout(CLIENT_TIMEOUT)
        .build()
        .map_err(|e| ClientError::Transport(e.to_string()))?;
    let url = format!("http://127.0.0.1:{port}{path}");
    match send(&over_tcp, method, url, body.as_ref()).await {
        Ok((status, _)) if status == reqwest::StatusCode::UNAUTHORIZED => {
            Err(ClientError::Refused {
                socket: socket.to_path_buf(),
            })
        }
        Ok(answer) => Ok(answer),
        Err(SendError::TooLarge) => Err(ClientError::Transport(TOO_LARGE.into())),
        Err(SendError::Http(e)) if e.is_connect() => Err(ClientError::NoDaemon {
            socket: socket.to_path_buf(),
            fallback,
        }),
        Err(SendError::Http(e)) => Err(ClientError::Transport(e.to_string())),
    }
}

/// The largest reply [`request`] reads. Every real reply is a few KiB; a body
/// past this is refused rather than held, whoever is answering.
const REPLY_MAX: usize = 8 << 20;

/// Why [`send`] got no reply.
#[derive(Debug)]
enum SendError {
    Http(reqwest::Error),
    /// The body ran past [`REPLY_MAX`].
    TooLarge,
}

impl From<reqwest::Error> for SendError {
    fn from(e: reqwest::Error) -> Self {
        SendError::Http(e)
    }
}

async fn send(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    body: Option<&serde_json::Value>,
) -> Result<(reqwest::StatusCode, String), SendError> {
    let mut req = client.request(method, url);
    if let Some(body) = body {
        req = req.json(body);
    }
    let mut resp = req.send().await?;
    let status = resp.status();
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > REPLY_MAX {
            return Err(SendError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((status, String::from_utf8_lossy(&buf).into_owned()))
}

const TOO_LARGE: &str = "reply too large";

/// The kind of the first I/O error in `err`'s source chain.
fn io_kind(err: &(dyn std::error::Error + 'static)) -> Option<io::ErrorKind> {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<io::Error>() {
            return Some(io.kind());
        }
        cur = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use axum::body::Body;
    use tower::ServiceExt;

    use crate::config::FileProfile;
    use crate::gate::OnBypass;

    const OP: u32 = 501;
    const FABER: u32 = 451;

    // --- the client's choices -----------------------------------------------

    #[test]
    fn only_a_host_without_a_record_may_fall_back_to_tcp() {
        use crate::platform::record::{
            Account, Accounts, Installed, Operator, SetupRecord, TrustError, SCHEMA,
        };
        use crate::platform::Resolution;
        let record = SetupRecord {
            schema: SCHEMA,
            operator: Operator {
                uid: 501,
                name: "owner".into(),
            },
            accounts: Accounts {
                faber: Account { uid: 451, gid: 451 },
                faber_vm: Account { uid: 452, gid: 452 },
                faber_mac: None,
                operator_group: None,
            },
            installed: Installed {
                version: "0.4.0".into(),
            },
            migrated_from: None,
            artifacts: vec![],
        };
        assert_eq!(
            fallback_for(&Resolution::System(record), 8787),
            Fallback::Never
        );
        assert_eq!(
            fallback_for(&Resolution::Untrusted(TrustError::Symlink), 8787),
            Fallback::Never
        );
        assert_eq!(fallback_for(&Resolution::Legacy, 8787), Fallback::Tcp(8787));
    }

    #[test]
    fn a_refused_operator_message_has_no_stray_spaces() {
        let m = refused_message(Path::new("/s"), 501, Some(501), "_faber_operator");
        assert_eq!(
            m,
            "you are faber's operator, but this login session predates your _faber_operator \
             group membership, so the control socket /s refused it: log out and back in (or \
             open a new login session)."
        );
    }

    #[test]
    fn a_hold_reason_is_quoted_and_capped() {
        assert_eq!(capped("short", 120), "short");
        let long = "é".repeat(130);
        let c = capped(&long, 120);
        assert_eq!(c.chars().count(), 121);
        assert!(c.ends_with('…'));
    }

    #[test]
    fn no_daemon_names_the_places_it_looked() {
        let none = ClientError::NoDaemon {
            socket: "/run/f.sock".into(),
            fallback: Fallback::Never,
        };
        assert_eq!(
            none.to_string(),
            "no faber daemon is answering on /run/f.sock"
        );
        let tcp = ClientError::NoDaemon {
            socket: "/run/f.sock".into(),
            fallback: Fallback::Tcp(9000),
        };
        assert!(tcp.to_string().ends_with("/run/f.sock or 127.0.0.1:9000"));
    }

    #[test]
    fn the_operator_is_told_to_log_in_again_everyone_else_who_may_use_it() {
        let sock = Path::new("/var/lib/faber/control.sock");
        let op = refused_message(sock, OP, Some(OP), "_faber_operator");
        assert!(op.contains("you are faber's operator"), "{op}");
        assert!(op.contains("log out and back in"), "{op}");
        for (uid, operator) in [(OP + 1, Some(OP)), (OP, None), (OP, Some(OP + 1))] {
            let other = refused_message(sock, uid, operator, "_faber_operator");
            assert!(other.contains("operator's account or root"), "{other}");
            assert!(!other.contains("you are faber's operator"), "{other}");
        }
    }

    // --- classes ------------------------------------------------------------

    #[test]
    fn root_is_present_the_operator_is_operator_everyone_else_is_refused() {
        assert_eq!(class_for_uid(0, OP), Class::Present);
        assert_eq!(class_for_uid(OP, OP), Class::Operator);
        assert_eq!(class_for_uid(FABER, OP), Class::Refused);
        assert_eq!(class_for_uid(FABER + 1, OP), Class::Refused);
        assert_eq!(class_for_uid(OP + 1, OP), Class::Refused);
        assert!(Class::Refused < Class::Operator && Class::Operator < Class::Present);
    }

    #[test]
    fn a_session_is_an_operator() {
        assert_eq!(Caller::Session.class(), Class::Operator);
        assert_eq!(Caller::Session.uid(), None);
        let socket = Caller::Socket {
            uid: OP,
            class: Class::Operator,
        };
        assert_eq!(socket.class(), Class::Operator);
        assert_eq!(socket.uid(), Some(OP));
        assert_eq!((socket.via(), Caller::Session.via()), ("socket", "session"));
    }

    // --- authorize ----------------------------------------------------------

    fn one() -> Vec<Widening> {
        vec![widening("update.pin", "update.pin none -> 0.4.0")]
    }

    #[test]
    fn authorize_each_class_for_routine_and_widening_changes() {
        let refused = Verdict::Refuse(REFUSED_BODY.trim_end().to_string());
        assert_eq!(authorize(Class::Refused, &[]), refused);
        assert_eq!(authorize(Class::Refused, &one()), refused);
        assert_eq!(
            authorize(Class::Operator, &[]),
            Verdict::Allow { audit: false }
        );
        assert_eq!(
            authorize(Class::Operator, &one()),
            Verdict::Refuse(
                "update.pin widens trust and needs a present caller (sudo); this one is operator"
                    .into()
            )
        );
        assert_eq!(
            authorize(Class::Present, &[]),
            Verdict::Allow { audit: false }
        );
        assert_eq!(
            authorize(Class::Present, &one()),
            Verdict::Allow { audit: true }
        );
    }

    /// Issue #78: until phase 2, every widening takes `sudo`. An operator and a
    /// dashboard session are refused each one, alone or together, while their
    /// routine saves still pass; root is allowed and audited.
    #[test]
    fn widening_needs_a_present_caller() {
        assert_eq!(WIDENING_NEEDS, Class::Present);
        let mut wide = base();
        wide.profiles[0].dind = Some(true);
        wide.gate.allow_public_repos = true;
        wide.gate.on_bypass = OnBypass::Record;
        wide.trusted_actors.push("someone".into());
        wide.denied_actors.clear();
        wide.update.repo = "owner/demo".into();
        wide.update.pin = Some("0.4.1".into());
        wide.update.maturity_secs = 3600;
        wide.update.enabled = true;
        wide.registry_auth.enabled = true;
        let mut old = base();
        old.gate.on_bypass = OnBypass::Enforce;
        let all = trust_widening(&old, &wide);
        assert!(all.len() >= 10, "{all:?}");
        let session = Caller::Session.class();
        let operator = Caller::Socket {
            uid: OP,
            class: Class::Operator,
        }
        .class();
        for class in [session, operator] {
            for w in &all {
                let one = std::slice::from_ref(w);
                assert!(
                    matches!(authorize(class, one), Verdict::Refuse(_)),
                    "{class:?} {}",
                    w.key
                );
            }
            assert!(matches!(authorize(class, &all), Verdict::Refuse(_)));
            assert_eq!(authorize(class, &[]), Verdict::Allow { audit: false });
        }
        assert_eq!(
            authorize(Class::Present, &all),
            Verdict::Allow { audit: true }
        );
    }

    /// The threshold is a parameter: a lower one lets the operator widen, and
    /// a refusal names each key and the class it needs.
    #[test]
    fn the_threshold_decides_who_widens() {
        assert_eq!(
            authorize_with(Class::Operator, &one(), Class::Operator),
            Verdict::Allow { audit: true }
        );
        let two = vec![widening("update.pin", "x"), widening("update.enabled", "y")];
        match authorize_with(Class::Operator, &two, Class::Present) {
            Verdict::Refuse(why) => assert_eq!(
                why,
                "update.pin, update.enabled widen trust and need a present caller (sudo); \
                 this one is operator"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn needs_names_the_class_and_command_per_key() {
        let mut new = base();
        new.update.pin = Some("0.4.0".into());
        let n = needs(&trust_widening(&base(), &new), &base(), &new);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].key, "update.pin");
        assert_eq!(n[0].needs, WIDENING_NEEDS);
        assert_eq!(n[0].command, "sudo faber config set update.pin 0.4.0");
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json[0]["needs"], "present");
        assert_eq!(json[0]["command"], "sudo faber config set update.pin 0.4.0");
    }

    // --- the command per widening -------------------------------------------

    #[test]
    fn a_value_is_quoted_only_when_the_shell_would_read_it() {
        for plain in ["0.4.1", "owner/demo", "alice,bob", "a=b+c-d@e:f", "X_Y"] {
            assert_eq!(shell_quote(plain), plain);
        }
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("dependabot[bot]"), "'dependabot[bot]'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }

    /// The command for each widening key, from the config it was measured on.
    #[test]
    fn each_widening_key_has_its_command() {
        let w = |key: &str| widening(key, "");
        let old = base();
        let mut new = base();
        new.profiles[0].dind = Some(true);
        new.profiles[0].drop_grants =
            vec!["10.0.0.5/32 5432/tcp".into(), "10.0.0.6/32 22/tcp".into()];
        new.gate.allowed_events = vec!["push".into(), "release".into()];
        new.gate.allow_public_repos = true;
        new.gate.on_bypass = OnBypass::Record;
        new.trusted_actors = vec!["alice".into(), "bob".into()];
        new.denied_actors = vec!["renovate[bot]".into()];
        new.update.repo = "owner/demo".into();
        new.update.pin = Some("0.4.1".into());
        new.update.maturity_secs = 3600;
        new.update.enabled = true;
        new.registry_auth.enabled = true;
        new.registry_auth.server = "registry.example".into();
        new.registry_auth.username = "someone".into();
        new.registry_auth.password_env = "REG_SECRET".into();
        for (key, want) in [
            (
                "profile.linux-docker.dind",
                "sudo faber config set profile.linux-docker.dind true",
            ),
            (
                "gate.allowed_events",
                "sudo faber config set gate.allowed_events push,release",
            ),
            (
                "profile.linux-docker.drop_grants",
                "sudo faber config set profile.linux-docker.drop_grants \
                 '10.0.0.5/32 5432/tcp,10.0.0.6/32 22/tcp'",
            ),
            (
                "gate.allow_public_repos",
                "sudo faber config set gate.allow_public_repos true",
            ),
            (
                "gate.on_bypass",
                "sudo faber config set gate.on_bypass record",
            ),
            (
                "trusted_actors",
                "sudo faber config set trusted_actors alice,bob",
            ),
            (
                "denied_actors",
                "sudo faber config set denied_actors 'renovate[bot]'",
            ),
            (
                "update.repo",
                "sudo faber config set update.repo owner/demo",
            ),
            ("update.pin", "sudo faber config set update.pin 0.4.1"),
            (
                "update.maturity_secs",
                "sudo faber config set update.maturity_secs 3600",
            ),
            (
                "update.enabled",
                "sudo faber config set update.enabled true",
            ),
            (
                "registry_auth.enabled",
                "sudo faber config set registry_auth.enabled true",
            ),
            (
                "registry_auth.server",
                "sudo faber config set registry_auth.server registry.example",
            ),
            (
                "registry_auth.username",
                "sudo faber config set registry_auth.username someone",
            ),
            (
                "registry_auth.password_env",
                "sudo faber config set registry_auth.password_env REG_SECRET",
            ),
            (
                "profiles.routing",
                "sudo faber config apply <edited config.toml>",
            ),
            (
                "repo_hold.owner/demo",
                "sudo faber repo-hold clear owner/demo",
            ),
            (
                "something.new",
                "sudo faber config apply <edited config.toml>",
            ),
        ] {
            assert_eq!(sudo_command(&w(key), &old, &new), want, "{key}");
        }

        // Emptied lists and a blank username are unset, not set to nothing.
        let mut emptied = base();
        emptied.trusted_actors.clear();
        emptied.denied_actors.clear();
        emptied.registry_auth.enabled = true;
        for (key, want) in [
            (
                "profile.linux-docker.drop_grants",
                "sudo faber config unset profile.linux-docker.drop_grants",
            ),
            ("trusted_actors", "sudo faber config unset trusted_actors"),
            ("denied_actors", "sudo faber config unset denied_actors"),
            (
                "registry_auth.username",
                "sudo faber config unset registry_auth.username",
            ),
        ] {
            assert_eq!(sudo_command(&w(key), &old, &emptied), want, "{key}");
        }

        // A new docker profile is not one `set` away: it is applied whole.
        let mut grown = base();
        grown.profiles.push(docker("heavy job", None));
        assert_eq!(
            sudo_command(&w("profile.heavy job.dind"), &old, &grown),
            "sudo faber config apply <edited config.toml>"
        );
        // A name the shell would split is quoted.
        let mut odd = base();
        odd.profiles[0].name = "heavy job".into();
        let mut odd_new = odd.clone();
        odd_new.profiles[0].dind = None;
        assert_eq!(
            sudo_command(&w("profile.heavy job.dind"), &odd, &odd_new),
            "sudo faber config set 'profile.heavy job.dind' true"
        );
        // A repo with a quote in it stays one word.
        assert_eq!(
            sudo_command(&w("repo_hold.owner/it's"), &old, &new),
            "sudo faber repo-hold clear 'owner/it'\\''s'"
        );
    }

    // --- trust widening -----------------------------------------------------

    fn docker(name: &str, dind: Option<bool>) -> FileProfile {
        FileProfile {
            name: name.into(),
            labels: vec!["faber".into(), "docker".into()],
            backend: BackendKind::Docker,
            image: Some("ghcr.io/owner/my-runner:latest".into()),
            cpus: None,
            memory_mib: None,
            dind,
            dind_image: None,
            cache_volume: false,
            git_mirror: false,
            mirror: false,
            drop_grants: Vec::new(),
            env: Default::default(),
        }
    }

    fn keys(old: &FileConfig, new: &FileConfig) -> Vec<String> {
        trust_widening(old, new)
            .into_iter()
            .map(|w| w.key)
            .collect()
    }

    fn base() -> FileConfig {
        FileConfig {
            profiles: vec![docker("linux-docker", Some(false))],
            trusted_actors: vec!["owner".into()],
            ..FileConfig::default()
        }
    }

    #[test]
    fn an_unchanged_config_widens_nothing() {
        assert!(keys(&base(), &base()).is_empty());
        assert!(keys(&FileConfig::default(), &FileConfig::default()).is_empty());
    }

    #[test]
    fn granting_dind_widens() {
        let mut new = base();
        new.profiles[0].dind = Some(true);
        assert_eq!(keys(&base(), &new), ["profile.linux-docker.dind"]);
        // Unstating it is a grant too: absent reads as true.
        new.profiles[0].dind = None;
        assert_eq!(keys(&base(), &new), ["profile.linux-docker.dind"]);
    }

    #[test]
    fn withdrawing_dind_does_not_widen() {
        let mut old = base();
        old.profiles[0].dind = Some(true);
        assert!(keys(&old, &base()).is_empty());
        old.profiles[0].dind = None;
        assert!(keys(&old, &base()).is_empty());
    }

    #[test]
    fn turning_the_dind_label_rule_off_widens_and_on_does_not() {
        let with = |v: Option<bool>| {
            let mut c = base();
            c.host.dind_requires_label = v;
            c
        };
        assert_eq!(
            keys(&with(Some(true)), &with(Some(false))),
            ["host.dind_requires_label"]
        );
        assert_eq!(
            keys(&with(Some(true)), &with(None)),
            ["host.dind_requires_label"]
        );
        assert!(keys(&with(Some(false)), &with(Some(true))).is_empty());
        assert!(keys(&with(None), &with(Some(true))).is_empty());
        assert!(keys(&with(Some(true)), &with(Some(true))).is_empty());
    }

    #[test]
    fn a_new_docker_profile_with_dind_widens_and_without_does_not() {
        let mut new = base();
        new.profiles.push(docker("heavy", None));
        // A new dind profile is also a routing change ahead of it.
        assert_eq!(
            keys(&base(), &new),
            ["profile.heavy.dind", "profiles.routing"]
        );
        new.profiles[1].dind = Some(true);
        assert_eq!(
            keys(&base(), &new),
            ["profile.heavy.dind", "profiles.routing"]
        );
        new.profiles[1].dind = Some(false);
        assert!(keys(&base(), &new).is_empty());
    }

    fn routed() -> FileConfig {
        let mut a = docker("plain", Some(false));
        a.labels = vec!["faber".into(), "light".into()];
        let mut b = docker("heavy", Some(true));
        b.labels = vec!["faber".into(), "heavy".into()];
        let mut c = docker("tail", Some(false));
        c.labels = vec!["faber".into(), "tail".into()];
        FileConfig {
            profiles: vec![a, b, c],
            ..FileConfig::default()
        }
    }

    #[test]
    fn routing_changes_ahead_of_a_dind_profile_widen() {
        let old = routed();
        assert!(keys(&old, &old).is_empty());

        // The dind profile loses a label, or gains one (over-report is fine).
        let mut fewer = old.clone();
        fewer.profiles[1].labels.pop();
        assert_eq!(keys(&old, &fewer), ["profiles.routing"]);
        let mut more = old.clone();
        more.profiles[1].labels.push("extra".into());
        assert_eq!(keys(&old, &more), ["profiles.routing"]);

        // Labels swapped between a dind and a non-dind profile.
        let mut swapped = old.clone();
        swapped.profiles[0].labels = old.profiles[1].labels.clone();
        swapped.profiles[1].labels = old.profiles[0].labels.clone();
        assert_eq!(keys(&old, &swapped), ["profiles.routing"]);
        assert!(trust_widening(&old, &swapped)[0].sentence.contains("heavy"),);

        // The dind profile moves ahead of a non-dind one.
        let mut moved = old.clone();
        moved.profiles.swap(0, 1);
        assert_eq!(keys(&old, &moved), ["profiles.routing"]);

        // An earlier non-dind profile is relabelled.
        let mut relabelled = old.clone();
        relabelled.profiles[0].labels.push("extra".into());
        assert_eq!(keys(&old, &relabelled), ["profiles.routing"]);
    }

    #[test]
    fn routing_changes_after_the_last_dind_profile_do_not_widen() {
        let old = routed();
        let mut new = old.clone();
        new.profiles[2].labels.push("extra".into());
        assert!(keys(&old, &new).is_empty());
    }

    #[test]
    fn routing_changes_with_no_dind_profile_do_not_widen() {
        let mut old = routed();
        old.profiles[1].dind = Some(false);
        let mut new = old.clone();
        new.profiles[0].labels.push("extra".into());
        new.profiles.swap(0, 1);
        assert!(keys(&old, &new).is_empty());
    }

    /// #220: the runner image and the dind image run beside a Docker daemon,
    /// so changing either widens while dind is effective, and is routine
    /// without it.
    #[test]
    fn changing_an_image_beside_a_docker_daemon_widens() {
        let mut dind = base();
        dind.profiles[0].dind = Some(true);
        let mut image = dind.clone();
        image.profiles[0].image = Some("ghcr.io/owner/other-runner:latest".into());
        assert_eq!(keys(&dind, &image), ["profile.linux-docker.image"]);
        assert_eq!(keys(&image, &dind), ["profile.linux-docker.image"]);
        let mut dind_image = dind.clone();
        dind_image.profiles[0].dind_image = Some("docker:27-dind".into());
        assert_eq!(
            keys(&dind, &dind_image),
            ["profile.linux-docker.dind_image"]
        );
        // Unstated dind is effective dind.
        let (mut unstated, mut unstated_image) = (dind.clone(), image.clone());
        unstated.profiles[0].dind = None;
        unstated_image.profiles[0].dind = None;
        assert_eq!(
            keys(&unstated, &unstated_image),
            ["profile.linux-docker.image"]
        );

        // Without dind, the same changes are routine...
        let mut plain = base();
        plain.profiles[0].image = Some("ghcr.io/owner/other-runner:latest".into());
        plain.profiles[0].dind_image = Some("docker:27-dind".into());
        assert!(keys(&base(), &plain).is_empty());
        // ...and so is one made while dind is being withdrawn.
        assert!(keys(&image, &plain).is_empty());
        // Granting dind with a new image reports both.
        assert_eq!(
            keys(&base(), &image),
            ["profile.linux-docker.dind", "profile.linux-docker.image"]
        );
    }

    #[test]
    fn an_image_widening_is_one_config_set_or_unset() {
        let mut dind = base();
        dind.profiles[0].dind = Some(true);
        let mut new = dind.clone();
        new.profiles[0].image = Some("ghcr.io/owner/other-runner:2".into());
        new.profiles[0].dind_image = Some("docker:27-dind".into());
        let n = needs(&trust_widening(&dind, &new), &dind, &new);
        let commands: Vec<&str> = n.iter().map(|n| n.command.as_str()).collect();
        assert_eq!(
            commands,
            [
                "sudo faber config set profile.linux-docker.image ghcr.io/owner/other-runner:2",
                "sudo faber config set profile.linux-docker.dind_image docker:27-dind",
            ]
        );
        let n = needs(&trust_widening(&new, &dind), &new, &dind);
        assert_eq!(
            n[1].command,
            "sudo faber config unset profile.linux-docker.dind_image"
        );
    }

    /// Two plain profiles, the first dropping a grant: no dind anywhere, so
    /// only the drop makes routing matter.
    fn dropping() -> FileConfig {
        let mut a = docker("db", Some(false));
        a.labels = vec!["faber".into(), "db".into()];
        a.drop_grants = vec!["10.0.0.5/32 5432/tcp".into()];
        let mut b = docker("rest", Some(false));
        b.labels = vec!["faber".into()];
        FileConfig {
            profiles: vec![a, b],
            ..FileConfig::default()
        }
    }

    /// #221: while a profile drops grants, a rename, a removal, a reorder, a
    /// relabel or a new profile is a routing change that widens; with no drops
    /// the same edits are routine.
    #[test]
    fn routing_changes_widen_while_a_profile_drops_grants() {
        let old = dropping();
        assert!(keys(&old, &old).is_empty());
        let edits: Vec<(&str, Edit)> = vec![
            ("rename", |c| c.profiles[0].name = "db2".into()),
            ("remove", |c| {
                c.profiles.remove(0);
            }),
            ("remove the other", |c| {
                c.profiles.remove(1);
            }),
            ("reorder", |c| c.profiles.swap(0, 1)),
            ("relabel", |c| c.profiles[0].labels.push("extra".into())),
            ("new ahead", |c| {
                c.profiles.insert(0, docker("first", Some(false)))
            }),
            ("new behind", |c| {
                c.profiles.push(docker("last", Some(false)))
            }),
        ];
        for (what, edit) in edits {
            let mut new = old.clone();
            edit(&mut new);
            let found = trust_widening(&old, &new);
            let found_keys: Vec<&str> = found.iter().map(|w| w.key.as_str()).collect();
            assert_eq!(found_keys, ["profiles.routing"], "{what}");
            assert!(found[0].sentence.contains("db drops grants"), "{what}");

            let mut no_drops = old.clone();
            no_drops.profiles[0].drop_grants.clear();
            let mut new = no_drops.clone();
            edit(&mut new);
            assert!(keys(&no_drops, &new).is_empty(), "{what} without drops");
        }

        // Adding a drop, or another one, stays one routine call.
        let mut more = old.clone();
        more.profiles[1].drop_grants = vec!["10.0.0.6/32 22/tcp".into()];
        assert!(keys(&old, &more).is_empty());
        let mut none = old.clone();
        none.profiles[0].drop_grants.clear();
        assert!(keys(&none, &old).is_empty());
        // A routing change through `apply` is the command it needs.
        let mut renamed = old.clone();
        renamed.profiles[0].name = "db2".into();
        let n = needs(&trust_widening(&old, &renamed), &old, &renamed);
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
    }

    /// A routing change ahead of a dind profile is reported once, under the
    /// dind sentence, even while a profile drops grants.
    #[test]
    fn routing_is_reported_once() {
        let mut old = routed();
        old.profiles[2].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()];
        let mut new = old.clone();
        new.profiles.swap(0, 1);
        let found = trust_widening(&old, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].sentence.contains("dind profile heavy"));
    }

    /// An operator recipe for runner-image variants is the same grant as a
    /// profile's image: putting one in force widens, going back to faber's own
    /// template or turning building off does not.
    #[test]
    fn an_operator_recipe_in_force_widens() {
        use crate::config::ImageBuildMode;
        let with = |build: ImageBuildMode, recipe: Option<&str>| {
            let mut c = base();
            c.runner_image.build = build;
            c.runner_image.dockerfile = recipe.map(String::from);
            c
        };
        let (on, off) = (ImageBuildMode::OnDemand, ImageBuildMode::Off);
        let key = ["runner_image.dockerfile"];
        assert_eq!(keys(&with(on, None), &with(on, Some("/etc/faber/a"))), key);
        assert_eq!(
            keys(
                &with(on, Some("/etc/faber/a")),
                &with(on, Some("/etc/faber/b"))
            ),
            key
        );
        assert_eq!(
            keys(
                &with(off, Some("/etc/faber/a")),
                &with(on, Some("/etc/faber/a"))
            ),
            key
        );
        assert!(keys(&with(on, Some("/etc/faber/a")), &with(on, None)).is_empty());
        assert!(keys(
            &with(on, Some("/etc/faber/a")),
            &with(off, Some("/etc/faber/a"))
        )
        .is_empty());
        assert!(keys(&with(off, None), &with(off, Some("/etc/faber/a"))).is_empty());
        assert!(keys(&with(off, None), &with(on, None)).is_empty());

        let (old, new) = (with(on, None), with(on, Some("/etc/faber/a")));
        let n = needs(&trust_widening(&old, &new), &old, &new);
        assert_eq!(
            n[0].command,
            "sudo faber config set runner_image.dockerfile /etc/faber/a"
        );
        let (old, new) = (
            with(off, Some("/etc/faber/a")),
            with(on, Some("/etc/faber/a")),
        );
        let n = needs(&trust_widening(&old, &new), &old, &new);
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
    }

    // --- #238: [needs], env, cache_volume ------------------------------------

    fn with_needs(entries: &[(&str, &str)]) -> FileConfig {
        let mut c = base();
        for (tool, versions) in entries {
            c.needs.insert(
                (*tool).into(),
                crate::config::ToolNeeds {
                    versions: (*versions).into(),
                },
            );
        }
        c
    }

    fn with_services(tables: &str) -> FileConfig {
        let mut c = base();
        c.services = toml::from_str::<FileConfig>(tables).unwrap().services;
        c
    }

    const PG: &str = "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
                      extensions = [\"pg_trgm\"]\nmemory_mib = 2048\n";
    const REDIS: &str = "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 256\n";

    /// ADR-0046 §10: what runs on every service job's network widens.
    #[test]
    fn widening_the_services_widens() {
        let old = with_services(PG);
        // A table added, to an empty set or beside another.
        assert_eq!(keys(&base(), &old), ["services.postgres"]);
        let both = with_services(&format!("{PG}{REDIS}"));
        assert_eq!(keys(&old, &both), ["services.redis"]);
        // An image changed.
        let moved = with_services(&PG.replace("postgres:{version}", "ghcr.io/owner/pg:{version}"));
        assert_eq!(keys(&old, &moved), ["services.postgres.image"]);
        // An extension added, even while another goes.
        let ext = with_services(&PG.replace("[\"pg_trgm\"]", "[\"postgis\"]"));
        assert_eq!(keys(&old, &ext), ["services.postgres.extensions"]);
        // A major added, however it is spelled around the old one.
        for gained in ["16, 17", "17,16", "17"] {
            let v = with_services(&PG.replace("\"16\"", &format!("{gained:?}")));
            assert_eq!(keys(&old, &v), ["services.postgres.versions"], "{gained}");
        }
        // Each is applied whole: `[services]` is no settings field.
        let n = needs(&trust_widening(&old, &moved), &old, &moved);
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
    }

    /// ADR-0046 A4: what a per-job table runs, with and under, widens; what it
    /// hands the runner, or how much memory it may use, does not.
    #[test]
    fn a_per_job_table_widens_by_what_it_runs_not_by_what_it_exports() {
        const S3: &str = "[services.s3]\nimage = \"ghcr.io/owner/s3:1\"\nmemory_mib = 64\n\
                          args = [\"--port\", \":7070\"]\nenv = { KEY = \"firstvalue\", OTHER = \"x\" }\n\
                          job_env = { S3_ENDPOINT = \"http://s3:7070\" }\nhealth_cmd = \"true\"\n";
        let old = with_services(S3);
        assert_eq!(keys(&base(), &old), ["services.s3"]);
        for (from, to, key) in [
            (
                "ghcr.io/owner/s3:1",
                "ghcr.io/owner/s3:2",
                "services.s3.image",
            ),
            (":7070", ":7071", "services.s3.args"),
            ("\"--port\", ", "", "services.s3.args"),
            (
                "KEY = \"firstvalue\"",
                "KEY = \"secondvalue\"",
                "services.s3.env",
            ),
            (", OTHER = \"x\"", "", "services.s3.env"),
            (
                "OTHER = \"x\"",
                "OTHER = \"x\", NEW = \"y\"",
                "services.s3.env",
            ),
            (
                "health_cmd = \"true\"",
                "health_cmd = \"false\"",
                "services.s3.health_cmd",
            ),
            ("health_cmd = \"true\"\n", "", "services.s3.health_cmd"),
        ] {
            let new = with_services(&S3.replace(from, to));
            assert_eq!(keys(&old, &new), [key], "{from} -> {to}");
        }
        for (from, to) in [
            ("http://s3:7070", "http://s3:9000"),
            (
                "job_env = { S3_ENDPOINT = \"http://s3:7070\" }",
                "job_env = { OTHER_URL = \"u\" }",
            ),
            ("memory_mib = 64", "memory_mib = 512"),
        ] {
            let new = with_services(&S3.replace(from, to));
            assert!(keys(&old, &new).is_empty(), "{from} -> {to}");
        }
        // The sentence names the keys that changed and no value.
        let new = with_services(&S3.replace("KEY = \"firstvalue\"", "KEY = \"hunter2\""));
        let w = trust_widening(&old, &new);
        assert!(
            w[0].sentence.contains("KEY") && !w[0].sentence.contains("hunter2"),
            "{w:?}"
        );
        assert!(!w[0].sentence.contains("firstvalue"), "{w:?}");
        // …and a table added says no value either.
        let w = trust_widening(&base(), &old);
        assert!(!w[0].sentence.contains("firstvalue"), "{w:?}");
    }

    #[test]
    fn narrowing_the_services_does_not_widen() {
        let old = with_services(&format!(
            "{}{REDIS}",
            PG.replace("\"16\"", "\"16, 17\"")
                .replace("[\"pg_trgm\"]", "[\"pg_trgm\", \"postgis\"]")
        ));
        assert!(keys(&old, &old).is_empty());
        // A table removed, a major dropped or respelled, an extension dropped,
        // and the sizes changed either way.
        let narrowed = with_services(&PG.replace(
            "memory_mib = 2048",
            "memory_mib = 4096\nidle_stop_secs = 60",
        ));
        assert!(
            keys(&old, &narrowed).is_empty(),
            "{:?}",
            keys(&old, &narrowed)
        );
        let respelled = with_services(&format!(
            "{}{REDIS}",
            PG.replace("\"16\"", "\"17,16\"")
                .replace("[\"pg_trgm\"]", "[\"postgis\", \"pg_trgm\"]")
        ));
        assert!(keys(&old, &respelled).is_empty());
        assert!(keys(&old, &base()).is_empty());
    }

    /// A raised `[needs]` ceiling widens: a new tool, a changed range (even a
    /// narrower one) and an exact list that gains a version.
    #[test]
    fn raising_the_needs_ceiling_widens() {
        let old = with_needs(&[("playwright", "1.61.1, 1.62.1")]);
        // A new tool beside an existing one.
        let new = with_needs(&[("playwright", "1.61.1, 1.62.1"), ("node", "20.0.0")]);
        assert_eq!(keys(&old, &new), ["needs.node.versions"]);
        // An exact list gaining a version.
        let new = with_needs(&[("playwright", "1.61.1, 1.62.1, 1.63.0")]);
        assert_eq!(keys(&old, &new), ["needs.playwright.versions"]);
        // An exact list swapping one version for another.
        let new = with_needs(&[("playwright", "1.61.1, 1.70.0")]);
        assert_eq!(keys(&old, &new), ["needs.playwright.versions"]);
        // A list turned into a range.
        let new = with_needs(&[("playwright", ">=1.61, <1.63")]);
        assert_eq!(keys(&old, &new), ["needs.playwright.versions"]);
        // A range changed, wider or narrower: never compared by containment.
        let range = with_needs(&[("playwright", ">=1.55, <1.70")]);
        for changed in [">=1.55, <1.80", ">=1.60, <1.65", "1.61.1"] {
            let new = with_needs(&[("playwright", changed)]);
            assert_eq!(
                keys(&range, &new),
                ["needs.playwright.versions"],
                "{changed}"
            );
        }
        // Emptying the map lifts the ceiling altogether.
        assert_eq!(keys(&old, &base()), ["needs"]);
        // Each is applied whole: `[needs]` is no settings field.
        let new = with_needs(&[("playwright", "1.61.1, 1.62.1"), ("node", "20.0.0")]);
        let n = needs(&trust_widening(&old, &new), &old, &new);
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
        let n = needs(&trust_widening(&old, &base()), &old, &base());
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
    }

    /// Lowering the ceiling stays one routine call.
    #[test]
    fn lowering_the_needs_ceiling_does_not_widen() {
        let old = with_needs(&[("playwright", "1.61.1, 1.62.1"), ("node", "20.0.0")]);
        // A tool removed: it fails closed (`UnknownTool`).
        let new = with_needs(&[("playwright", "1.61.1, 1.62.1")]);
        assert!(keys(&old, &new).is_empty());
        // An exact list that only loses versions, respelled or reordered.
        for shrunk in [
            "1.61.1",
            "1.62.1,1.61.1",
            " 1.62.1 ",
            "1.62.1, not-a-version",
        ] {
            let new = with_needs(&[("playwright", shrunk), ("node", "20.0.0")]);
            assert!(keys(&old, &new).is_empty(), "{shrunk}");
        }
        // The same string, and a first ceiling where there was none.
        assert!(keys(&old, &old).is_empty());
        assert!(keys(&base(), &old).is_empty());
    }

    fn macos_vm(name: &str) -> FileProfile {
        FileProfile {
            backend: BackendKind::MacosVm,
            labels: vec!["faber".into(), "macos".into()],
            image: Some("golden-15".into()),
            dind: None,
            ..docker(name, None)
        }
    }

    /// A profile's `env` widens on every backend when an entry is added or
    /// changed, and is routine when one is removed.
    #[test]
    fn an_env_entry_added_or_changed_widens_on_every_profile() {
        let mut old = base();
        old.profiles.push(macos_vm("mac"));
        old.profiles[0].env.insert("KEEP".into(), "1".into());
        assert!(keys(&old, &old).is_empty());
        for (i, name) in [(0, "linux-docker"), (1, "mac")] {
            let key = format!("profile.{name}.env");
            // Added.
            let mut added = old.clone();
            added.profiles[i]
                .env
                .insert("HTTPS_PROXY".into(), "http://proxy.example".into());
            assert_eq!(keys(&old, &added), [key.as_str()], "{name}");
            let found = trust_widening(&old, &added);
            assert!(found[0].sentence.contains("HTTPS_PROXY"), "{found:?}");
            assert!(!found[0].sentence.contains("proxy.example"), "{found:?}");
            // Changed.
            let mut base_env = old.clone();
            base_env.profiles[i]
                .env
                .insert("LD_PRELOAD".into(), "/a.so".into());
            let mut changed = base_env.clone();
            changed.profiles[i]
                .env
                .insert("LD_PRELOAD".into(), "/b.so".into());
            assert_eq!(keys(&base_env, &changed), [key.as_str()], "{name}");
            // Removed: routine, and so is clearing the table.
            assert!(keys(&added, &old).is_empty(), "{name}");
            let mut cleared = added.clone();
            cleared.profiles[i].env.clear();
            assert!(keys(&added, &cleared).is_empty(), "{name}");
            // A table is no `config set` value: applied whole.
            let n = needs(&trust_widening(&old, &added), &old, &added);
            assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
        }
    }

    /// A new profile is judged against nothing: a non-empty `env` or
    /// `cache_volume` on widens; one with neither does not.
    #[test]
    fn a_new_profile_with_env_or_a_cache_volume_widens() {
        let mut plain = base();
        plain.profiles.push(docker("extra", Some(false)));
        assert!(keys(&base(), &plain).is_empty());

        let mut with_env = plain.clone();
        with_env.profiles[1]
            .env
            .insert("ACTIONS_RUNNER_HOOK_JOB_STARTED".into(), "/hook.sh".into());
        assert_eq!(keys(&base(), &with_env), ["profile.extra.env"]);
        assert!(trust_widening(&base(), &with_env)[0]
            .sentence
            .starts_with("new profile extra"));

        let mut vm = base();
        vm.profiles.push(macos_vm("mac"));
        vm.profiles[1].env.insert("K".into(), "v".into());
        assert_eq!(keys(&base(), &vm), ["profile.mac.env"]);

        let mut cached = plain.clone();
        cached.profiles[1].cache_volume = true;
        assert_eq!(keys(&base(), &cached), ["profile.extra.cache_volume"]);
        // Not one `set` away: `config set profile.<name>.…` edits a profile
        // that exists.
        let n = needs(&trust_widening(&base(), &cached), &base(), &cached);
        assert_eq!(n[0].command, "sudo faber config apply <edited config.toml>");
    }

    /// `cache_volume` false -> true widens; true -> false does not.
    #[test]
    fn turning_the_cache_volume_on_widens_and_off_does_not() {
        let mut on = base();
        on.profiles[0].cache_volume = true;
        assert_eq!(keys(&base(), &on), ["profile.linux-docker.cache_volume"]);
        assert!(keys(&on, &base()).is_empty());
        assert!(keys(&on, &on).is_empty());
        let n = needs(&trust_widening(&base(), &on), &base(), &on);
        assert_eq!(
            n[0].command,
            "sudo faber config set profile.linux-docker.cache_volume true"
        );
    }

    #[test]
    fn the_config_digest_is_the_contents() {
        let mut a = base();
        let mut b = base();
        // Maps built in different orders digest the same.
        for (cfg, order) in [(&mut a, ["x", "y", "z"]), (&mut b, ["z", "y", "x"])] {
            for tool in order {
                cfg.needs.insert(
                    tool.into(),
                    crate::config::ToolNeeds {
                        versions: "1.0".into(),
                    },
                );
            }
        }
        assert_eq!(config_digest(&a).unwrap(), config_digest(&b).unwrap());
        assert_eq!(config_digest(&a).unwrap().len(), 64);
        b.update.enabled = true;
        assert_ne!(config_digest(&a).unwrap(), config_digest(&b).unwrap());
    }

    #[test]
    fn a_profile_turned_docker_with_dind_widens() {
        let mut old = base();
        old.profiles[0].backend = BackendKind::MacosVm;
        let mut new = base();
        new.profiles[0].dind = Some(true);
        assert_eq!(
            keys(&old, &new),
            ["profile.linux-docker.dind", "profiles.routing"]
        );
    }

    #[test]
    fn gaining_an_event_widens_and_losing_one_does_not() {
        let mut new = base();
        new.gate.allowed_events.push("release".into());
        let found = trust_widening(&base(), &new);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "gate.allowed_events");
        assert!(
            found[0].sentence.contains("release"),
            "{}",
            found[0].sentence
        );

        let mut fewer = base();
        fewer.gate.allowed_events.pop();
        assert!(keys(&base(), &fewer).is_empty());
    }

    #[test]
    fn allowing_public_repos_widens_and_disallowing_does_not() {
        let mut open = base();
        open.gate.allow_public_repos = true;
        assert_eq!(keys(&base(), &open), ["gate.allow_public_repos"]);
        assert!(keys(&open, &base()).is_empty());
    }

    #[test]
    fn allowing_job_token_dispatches_widens_and_disallowing_does_not() {
        let mut open = base();
        open.gate.allow_job_token_dispatches = true;
        assert_eq!(keys(&base(), &open), ["gate.allow_job_token_dispatches"]);
        assert!(keys(&open, &base()).is_empty());
    }

    #[test]
    fn enforce_to_record_widens_and_record_to_enforce_does_not() {
        let mut enforce = base();
        enforce.gate.on_bypass = OnBypass::Enforce;
        let mut record = base();
        record.gate.on_bypass = OnBypass::Record;
        assert_eq!(keys(&enforce, &record), ["gate.on_bypass"]);
        assert!(keys(&record, &enforce).is_empty());
    }

    #[test]
    fn the_trusted_list_widens_when_emptied_or_grown() {
        let mut emptied = base();
        emptied.trusted_actors.clear();
        assert_eq!(keys(&base(), &emptied), ["trusted_actors"]);

        let mut grown = base();
        grown.trusted_actors.push("someone".into());
        assert_eq!(keys(&base(), &grown), ["trusted_actors"]);

        // Narrowing it, or starting one from empty, does not.
        assert!(keys(&grown, &base()).is_empty());
        assert!(keys(&emptied, &base()).is_empty());
        // An empty list gaining nothing is unchanged.
        assert!(keys(&emptied, &emptied).is_empty());
    }

    #[test]
    fn the_denied_list_widens_when_it_loses_an_entry() {
        let mut fewer = base();
        let lost = fewer.denied_actors.pop();
        assert!(lost.is_some(), "the default denies someone");
        assert_eq!(keys(&base(), &fewer), ["denied_actors"]);
        // Denying more does not.
        assert!(keys(&fewer, &base()).is_empty());
    }

    #[test]
    fn moving_the_update_repo_widens() {
        let mut new = base();
        new.update.repo = "owner/demo".into();
        assert_eq!(keys(&base(), &new), ["update.repo"]);
    }

    #[test]
    fn setting_or_changing_a_pin_widens_and_clearing_it_does_not() {
        let mut pinned = base();
        pinned.update.pin = Some("0.4.0".into());
        assert_eq!(keys(&base(), &pinned), ["update.pin"]);

        let mut repinned = base();
        repinned.update.pin = Some("0.4.1".into());
        assert_eq!(keys(&pinned, &repinned), ["update.pin"]);

        assert!(keys(&pinned, &pinned).is_empty());
        assert!(keys(&pinned, &base()).is_empty(), "clearing a pin");
    }

    #[test]
    fn lowering_maturity_widens_and_raising_it_does_not() {
        let mut lower = base();
        lower.update.maturity_secs = 2 * 3600;
        assert_eq!(keys(&base(), &lower), ["update.maturity_secs"]);
        assert!(keys(&lower, &base()).is_empty(), "raising it");

        let mut higher = base();
        higher.update.maturity_secs = 48 * 3600;
        assert!(keys(&base(), &higher).is_empty());
    }

    #[test]
    fn widenings_are_reported_together() {
        let mut new = base();
        new.profiles[0].dind = Some(true);
        new.update.pin = Some("0.4.0".into());
        new.gate.allow_public_repos = true;
        assert_eq!(
            keys(&base(), &new),
            [
                "profile.linux-docker.dind",
                "gate.allow_public_repos",
                "update.pin"
            ]
        );
    }

    #[test]
    fn enabling_updates_widens_and_disabling_them_does_not() {
        let mut on = base();
        on.update.enabled = true;
        assert_eq!(keys(&base(), &on), ["update.enabled"]);
        assert!(keys(&on, &base()).is_empty(), "true -> false");
        assert!(keys(&on, &on).is_empty());
    }

    fn registry_on() -> FileConfig {
        let mut c = base();
        c.registry_auth.enabled = true;
        c.registry_auth.username = "someone".into();
        c
    }

    #[test]
    fn enabling_registry_auth_widens_and_disabling_it_does_not() {
        // Turning it on with a username set also changes the username while on.
        assert_eq!(
            keys(&base(), &registry_on()),
            ["registry_auth.enabled", "registry_auth.username"]
        );
        assert!(keys(&registry_on(), &base()).is_empty(), "true -> false");
    }

    /// One registry field and how to change it.
    type FieldCase = (&'static str, fn(&mut FileConfig));

    #[test]
    fn each_registry_field_widens_while_registry_auth_is_on() {
        let cases: [FieldCase; 3] = [
            ("password_env", |c| {
                c.registry_auth.password_env = "FABER_REGISTRY_OTHER".into()
            }),
            ("server", |c| {
                c.registry_auth.server = "registry.example".into()
            }),
            ("username", |c| {
                c.registry_auth.username = "someone-else".into()
            }),
        ];
        for (field, set) in cases {
            let mut new = registry_on();
            set(&mut new);
            assert_eq!(
                keys(&registry_on(), &new),
                [format!("registry_auth.{field}")],
                "{field}"
            );

            // The same change while registry auth is off widens nothing...
            let mut off_new = base();
            set(&mut off_new);
            assert!(keys(&base(), &off_new).is_empty(), "{field} while off");

            // ...nor does it while registry auth is being switched off.
            let mut switched_off = new.clone();
            switched_off.registry_auth.enabled = false;
            assert!(
                keys(&registry_on(), &switched_off).is_empty(),
                "{field} while switching off"
            );
        }
    }

    #[test]
    fn a_repo_hold_clear_names_the_repo_since_and_reason() {
        let w = repo_hold_cleared(&crate::config::RepoHold {
            repo: "owner/demo".into(),
            reason: "gate bypassed".into(),
            by: "gate".into(),
            since_ms: 0,
        });
        assert_eq!(w.key, "repo_hold.owner/demo");
        assert!(
            w.sentence
                .starts_with("minting hold cleared for owner/demo (held since 1970-01-01"),
            "{}",
            w.sentence
        );
        assert!(
            w.sentence.ends_with(": \"gate bypassed\")"),
            "{}",
            w.sentence
        );
    }

    /// One change and its reverse.
    type Edit = fn(&mut FileConfig);

    /// Every widening key, both ways: `widen` turns `narrow` into the wider
    /// config and is reported under `key`; the way back is never reported.
    #[test]
    fn each_key_widens_one_way_only() {
        let cases: Vec<(&str, &str, Edit, Edit)> = vec![
            (
                "dind",
                "profile.linux-docker.dind",
                |c| c.profiles[0].dind = Some(false),
                |c| c.profiles[0].dind = Some(true),
            ),
            (
                "drop_grants lost",
                "profile.linux-docker.drop_grants",
                |c| {
                    c.profiles[0].drop_grants =
                        vec!["10.0.0.5/32 5432/tcp".into(), "10.0.0.6/32 22/tcp".into()]
                },
                |c| c.profiles[0].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()],
            ),
            (
                "drop_grants emptied",
                "profile.linux-docker.drop_grants",
                |c| c.profiles[0].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()],
                |c| c.profiles[0].drop_grants.clear(),
            ),
            (
                "events gained",
                "gate.allowed_events",
                |c| c.gate.allowed_events = vec!["push".into()],
                |c| c.gate.allowed_events = vec!["push".into(), "release".into()],
            ),
            // Empty refuses every event (gate::boundary), so emptying narrows.
            (
                "events from empty",
                "gate.allowed_events",
                |c| c.gate.allowed_events.clear(),
                |c| c.gate.allowed_events = vec!["push".into()],
            ),
            (
                "public repos",
                "gate.allow_public_repos",
                |c| c.gate.allow_public_repos = false,
                |c| c.gate.allow_public_repos = true,
            ),
            (
                "on_bypass",
                "gate.on_bypass",
                |c| c.gate.on_bypass = OnBypass::Enforce,
                |c| c.gate.on_bypass = OnBypass::Record,
            ),
            (
                "trusted gained",
                "trusted_actors",
                |c| c.trusted_actors = vec!["owner".into()],
                |c| c.trusted_actors = vec!["owner".into(), "someone".into()],
            ),
            // Empty trusts by provenance alone, so emptying widens.
            (
                "trusted emptied",
                "trusted_actors",
                |c| c.trusted_actors = vec!["owner".into()],
                |c| c.trusted_actors.clear(),
            ),
            (
                "denied lost",
                "denied_actors",
                |c| c.denied_actors = vec!["a[bot]".into(), "b[bot]".into()],
                |c| c.denied_actors = vec!["a[bot]".into()],
            ),
            // Empty denies nobody, so emptying is losing every entry.
            (
                "denied emptied",
                "denied_actors",
                |c| c.denied_actors = vec!["a[bot]".into()],
                |c| c.denied_actors.clear(),
            ),
            (
                "pin set",
                "update.pin",
                |c| c.update.pin = None,
                |c| c.update.pin = Some("0.4.1".into()),
            ),
            (
                "maturity",
                "update.maturity_secs",
                |c| c.update.maturity_secs = 86_400,
                |c| c.update.maturity_secs = 3_600,
            ),
            (
                "updates on",
                "update.enabled",
                |c| c.update.enabled = false,
                |c| c.update.enabled = true,
            ),
            (
                "registry on",
                "registry_auth.enabled",
                |c| c.registry_auth.enabled = false,
                |c| c.registry_auth.enabled = true,
            ),
        ];
        for (what, key, narrow, widen) in cases {
            let mut narrow_cfg = base();
            narrow(&mut narrow_cfg);
            let mut wide_cfg = narrow_cfg.clone();
            widen(&mut wide_cfg);
            assert_eq!(keys(&narrow_cfg, &wide_cfg), [key], "{what}: widen");
            assert!(keys(&wide_cfg, &narrow_cfg).is_empty(), "{what}: narrow");
        }

        // Both ways are reported for a key whose value only moves.
        let mut a = base();
        a.update.repo = "owner/demo".into();
        assert_eq!(keys(&base(), &a), ["update.repo"]);
        assert_eq!(keys(&a, &base()), ["update.repo"]);
        let (mut p1, mut p2) = (base(), base());
        p1.update.pin = Some("0.4.0".into());
        p2.update.pin = Some("0.4.1".into());
        assert_eq!(keys(&p1, &p2), ["update.pin"]);
        assert_eq!(keys(&p2, &p1), ["update.pin"]);
        // Repointing registry auth while it stays on widens; while off it is
        // routine.
        let mut on = base();
        on.registry_auth.enabled = true;
        let mut moved = on.clone();
        moved.registry_auth.server = "registry.example".into();
        assert_eq!(keys(&on, &moved), ["registry_auth.server"]);
        let mut off_moved = base();
        off_moved.registry_auth.server = "registry.example".into();
        assert!(keys(&base(), &off_moved).is_empty());
        // A repo-hold clear is always a widening, under its repo.
        let hold = crate::config::RepoHold {
            repo: "owner/demo".into(),
            reason: "r".into(),
            by: "gate".into(),
            since_ms: 0,
        };
        assert_eq!(repo_hold_cleared(&hold).key, "repo_hold.owner/demo");
    }

    /// Every key `trust_widening` can report, but routing, repo holds and
    /// `[needs]` (which has no settings field), is a settings field with a `sudo`
    /// command shown when it is read-only, or a held one: a new widening key
    /// without either fails here.
    #[test]
    fn every_widening_key_is_a_field_with_a_sudo_command() {
        let mut old = base();
        old.gate.on_bypass = OnBypass::Enforce;
        old.gate.allowed_events = vec!["push".into()];
        old.profiles[0].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()];
        let mut new = old.clone();
        new.profiles[0].drop_grants.clear();
        new.profiles[0].dind = Some(true);
        new.gate.allowed_events.push("release".into());
        new.gate.allow_public_repos = true;
        new.gate.on_bypass = OnBypass::Record;
        new.trusted_actors.push("someone".into());
        new.denied_actors.clear();
        old.host.dind_requires_label = Some(true);
        new.host.dind_requires_label = Some(false);
        new.update.repo = "owner/demo".into();
        new.update.pin = Some("0.4.1".into());
        new.update.maturity_secs = 3600;
        new.update.enabled = true;
        new.registry_auth.enabled = true;
        new.registry_auth.server = "registry.example".into();
        new.registry_auth.username = "someone".into();
        new.registry_auth.password_env = "REG_SECRET".into();
        new.profiles[0].image = Some("ghcr.io/owner/other-runner:latest".into());
        new.profiles[0].dind_image = Some("docker:27-dind".into());
        new.runner_image.build = crate::config::ImageBuildMode::OnDemand;
        new.runner_image.dockerfile = Some("/etc/faber/variant.Dockerfile".into());
        new.profiles[0].cache_volume = true;
        new.profiles[0]
            .env
            .insert("HTTPS_PROXY".into(), "http://proxy.example".into());
        let found = keys(&old, &new);
        assert_eq!(found.len(), 21, "{found:?}");
        let schema = crate::settings::FORM_SCHEMA;
        for key in found {
            let field = match key.strip_prefix("profile.") {
                Some(rest) => {
                    let (_, field) = rest.rsplit_once('.').unwrap();
                    schema.profile_fields.iter().find(|f| f.key == field)
                }
                None => schema
                    .sections
                    .iter()
                    .flat_map(|s| s.fields.iter())
                    .find(|f| f.key == key),
            };
            let field = field.unwrap_or_else(|| panic!("{key} is no settings field"));
            // A held field (`[registry_auth]`) offers no command: validation
            // refuses what it would turn on, so there is none to show.
            assert!(
                field.sudo.is_some() || field.held,
                "{key} has no sudo command"
            );
        }
    }

    // --- sessions -----------------------------------------------------------

    /// A store on a clock the test moves and entropy that counts.
    fn store() -> (Arc<Sessions>, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock = now.clone();
        let seq = Arc::new(AtomicUsize::new(1));
        let sessions = Sessions::new(
            Arc::new(move || clock.load(Ordering::SeqCst)),
            Arc::new(move || {
                let n = seq.fetch_add(1, Ordering::SeqCst);
                let mut bytes = [0u8; 32];
                bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
                Ok::<_, io::Error>(bytes)
            }),
        );
        (Arc::new(sessions), now)
    }

    fn advance(now: &AtomicU64, by: Duration) {
        now.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }

    #[test]
    fn a_link_redeems_once_into_a_session() {
        let (s, _) = store();
        let token = s.mint_link().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));

        let r = s.redeem(&token).unwrap().expect("a fresh link redeems");
        assert_eq!((r.cookie.len(), r.key.len()), (64, 64));
        assert_ne!(r.cookie, r.key);
        assert_eq!(s.check(&r.cookie, Some(&r.key)), Check::Valid);

        assert_eq!(s.redeem(&token).unwrap(), None, "single use");
    }

    #[test]
    fn a_link_expires_after_its_ttl() {
        let (s, now) = store();
        let token = s.mint_link().unwrap();
        advance(&now, LINK_TTL);
        assert_eq!(s.redeem(&token).unwrap(), None);

        let token = s.mint_link().unwrap();
        advance(&now, LINK_TTL - Duration::from_millis(1));
        assert!(s.redeem(&token).unwrap().is_some());
    }

    #[test]
    fn a_session_ends_after_twelve_hours_whatever_its_use() {
        let (s, now) = store();
        let r = s.redeem(&s.mint_link().unwrap()).unwrap().unwrap();
        advance(&now, SESSION_TTL - Duration::from_millis(1));
        assert_eq!(s.check(&r.cookie, Some(&r.key)), Check::Valid);
        advance(&now, Duration::from_millis(1));
        assert_eq!(s.check(&r.cookie, Some(&r.key)), Check::NoSession);
    }

    #[test]
    fn a_wrong_key_or_cookie_opens_nothing_more() {
        let (s, _) = store();
        let r = s.redeem(&s.mint_link().unwrap()).unwrap().unwrap();
        assert_eq!(s.check(&r.cookie, None), Check::NoKey);
        assert_eq!(s.check(&r.cookie, Some(&r.cookie)), Check::NoKey);
        assert_eq!(s.check(&r.key, Some(&r.key)), Check::NoSession);
        assert_eq!(s.check("", Some(&r.key)), Check::NoSession);
        // A raw secret is not its own digest's preimage of anything stored as-is.
        assert_eq!(s.redeem(&r.cookie).unwrap(), None);
    }

    #[test]
    fn the_oldest_link_and_session_are_evicted_past_the_cap() {
        let (s, _) = store();
        let links: Vec<String> = (0..=MAX_LINKS).map(|_| s.mint_link().unwrap()).collect();
        assert_eq!(s.redeem(&links[0]).unwrap(), None, "oldest link evicted");

        let first = s.redeem(&links[1]).unwrap().unwrap();
        for link in &links[2..] {
            assert!(s.redeem(link).unwrap().is_some());
        }
        // MAX_LINKS sessions so far; one more evicts the first.
        assert_eq!(s.check(&first.cookie, Some(&first.key)), Check::Valid);
        let last = s.redeem(&s.mint_link().unwrap()).unwrap().unwrap();
        assert_eq!(s.check(&first.cookie, Some(&first.key)), Check::NoSession);
        assert_eq!(s.check(&last.cookie, Some(&last.key)), Check::Valid);
    }

    #[test]
    fn failing_entropy_mints_nothing() {
        let s = Sessions::new(
            Arc::new(|| 0u64),
            Arc::new(|| Err::<[u8; 32], _>(io::Error::other("no randomness"))),
        );
        assert!(s.mint_link().is_err());
    }

    // --- Host and Origin ----------------------------------------------------

    #[test]
    fn only_exact_loopback_hosts_pass() {
        for ok in ["127.0.0.1:8787", "localhost:8787", "[::1]:8787"] {
            assert!(host_is_loopback(ok, 8787), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "127.0.0.1:8788",
            "localhost",
            "evil.example:8787",
            "127.0.0.1.evil.example:8787",
            "host.docker.internal:8787",
            "",
        ] {
            assert!(!host_is_loopback(bad, 8787), "{bad}");
        }
    }

    #[test]
    fn only_this_dashboards_origin_passes() {
        for ok in [
            "http://127.0.0.1:8787",
            "http://localhost:8787",
            "http://[::1]:8787",
        ] {
            assert!(origin_is_exact(Some(ok), 8787), "{ok}");
        }
        for bad in [
            None,
            Some("http://127.0.0.1:8788"),
            Some("https://127.0.0.1:8787"),
            Some("http://127.0.0.1"),
            Some("http://127.0.0.1:8787/"),
            Some("https://evil.example"),
            Some("null"),
        ] {
            assert!(!origin_is_exact(bad, 8787), "{bad:?}");
        }
    }

    // --- the guard, through the router --------------------------------------

    const PORT: u16 = 8787;

    fn door_with(
        sessions: Arc<Sessions>,
        socket: Option<PathBuf>,
        operator_uid: u32,
    ) -> Arc<FrontDoor> {
        FrontDoor::new(PORT, socket, 0o600, None, operator_uid, sessions)
    }

    /// A stand-in for the daemon's routes: a page, a read, a write, and one
    /// that says who called.
    fn app() -> Router {
        async fn whoami(caller: Option<Extension<Caller>>) -> String {
            match caller {
                Some(Extension(Caller::Socket { uid, class })) => {
                    format!("socket {uid} {}", class.as_str())
                }
                Some(Extension(Caller::Session)) => "session".to_string(),
                None => "nobody".to_string(),
            }
        }
        Router::new()
            .route("/", get(|| async { "page" }))
            .route("/api/state", get(|| async { "state" }))
            .route("/api/drain", post(|| async { "draining 0" }))
            .route("/api/whoami", get(whoami))
    }

    struct Req<'a> {
        method: Method,
        path: &'a str,
        host: Option<&'a str>,
        cookie: Option<&'a str>,
        key: Option<&'a str>,
        origin: Option<&'a str>,
    }

    fn get_req(path: &str) -> Req<'_> {
        Req {
            method: Method::GET,
            path,
            host: Some("127.0.0.1:8787"),
            cookie: None,
            key: None,
            origin: None,
        }
    }

    async fn call(router: &Router, r: Req<'_>) -> (StatusCode, HeaderMap, String) {
        let mut b = axum::http::Request::builder().method(r.method).uri(r.path);
        if let Some(h) = r.host {
            b = b.header(header::HOST, h);
        }
        if let Some(c) = r.cookie {
            b = b.header(header::COOKIE, format!("other=1; {SESSION_COOKIE}={c}"));
        }
        if let Some(k) = r.key {
            b = b.header(SESSION_HEADER, k);
        }
        if let Some(o) = r.origin {
            b = b.header(header::ORIGIN, o);
        }
        let resp = router
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// Open a session through the routes, as a browser does.
    async fn sign_in(door: &Arc<FrontDoor>, router: &Router) -> Redeemed {
        let token = door.sessions().mint_link().unwrap();
        let path = format!("/session?token={token}");
        let (status, headers, body) = call(router, get_req(&path)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let value = cookie
            .strip_prefix("faber_session=")
            .and_then(|rest| rest.strip_suffix("; HttpOnly; SameSite=Strict; Path=/"))
            .unwrap_or_else(|| panic!("{cookie}"))
            .to_string();
        let key = body
            .split("sessionStorage.setItem('faber-key', '")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap_or_else(|| panic!("{body}"))
            .to_string();
        assert!(body.contains("location.replace('/')"), "{body}");
        // ADR-0028 addendum C: when the session began, for the 401 line's reason.
        assert!(
            body.contains("sessionStorage.setItem('faber-since', String(Date.now()))"),
            "{body}"
        );
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        Redeemed { cookie: value, key }
    }

    #[tokio::test]
    async fn the_host_header_must_be_loopback_with_this_port() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        let mut no_host = get_req("/");
        no_host.host = None;
        assert_eq!(call(&router, no_host).await.0, StatusCode::BAD_REQUEST);
        for host in [
            "evil.example:8787",
            "127.0.0.1:9999",
            "host.docker.internal:8787",
        ] {
            let mut r = get_req("/session?token=00");
            r.host = Some(host);
            let (status, _, body) = call(&router, r).await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{host}");
            assert!(body.is_empty(), "no detail: {body}");
        }
    }

    #[tokio::test]
    async fn without_a_session_everything_is_the_static_page() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        for path in ["/", "/api/state", "/api/whoami", "/nowhere"] {
            let (status, _, body) = call(&router, get_req(path)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(body, SIGN_IN_HTML, "{path}");
        }
        let mut r = get_req("/");
        r.cookie = Some("deadbeef");
        assert_eq!(call(&router, r).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_bad_or_spent_link_is_refused() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        for path in [
            "/session",
            "/session?token=",
            "/session?token=00ff",
            "/session?x=1",
        ] {
            let (status, headers, body) = call(&router, get_req(path)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert!(headers.get(header::SET_COOKIE).is_none());
            assert_eq!(body, SIGN_IN_HTML);
        }
        let token = door.sessions().mint_link().unwrap();
        let path = format!("/session?token={token}");
        assert_eq!(call(&router, get_req(&path)).await.0, StatusCode::OK);
        assert_eq!(
            call(&router, get_req(&path)).await.0,
            StatusCode::UNAUTHORIZED,
            "a link is single use"
        );
    }

    #[tokio::test]
    async fn the_cookie_opens_the_page_and_the_key_opens_the_api() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        let r = sign_in(&door, &router).await;

        let mut page = get_req("/");
        page.cookie = Some(&r.cookie);
        let (status, _, body) = call(&router, page).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "page"));

        let mut cookie_only = get_req("/api/state");
        cookie_only.cookie = Some(&r.cookie);
        assert_eq!(call(&router, cookie_only).await.0, StatusCode::UNAUTHORIZED);

        let mut wrong_key = get_req("/api/state");
        wrong_key.cookie = Some(&r.cookie);
        wrong_key.key = Some(&r.cookie);
        assert_eq!(call(&router, wrong_key).await.0, StatusCode::UNAUTHORIZED);

        let mut full = get_req("/api/whoami");
        full.cookie = Some(&r.cookie);
        full.key = Some(&r.key);
        let (status, _, body) = call(&router, full).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "session"));

        // The key alone, with no cookie, is nothing.
        let mut key_only = get_req("/api/state");
        key_only.key = Some(&r.key);
        assert_eq!(call(&router, key_only).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_mutation_needs_this_dashboards_exact_origin() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        let r = sign_in(&door, &router).await;
        for (origin, expect) in [
            (None, StatusCode::FORBIDDEN),
            (Some("https://evil.example"), StatusCode::FORBIDDEN),
            (Some("http://127.0.0.1:9999"), StatusCode::FORBIDDEN),
            (Some("http://127.0.0.1:8787"), StatusCode::OK),
            (Some("http://localhost:8787"), StatusCode::OK),
        ] {
            let req = Req {
                method: Method::POST,
                path: "/api/drain",
                host: Some("127.0.0.1:8787"),
                cookie: Some(&r.cookie),
                key: Some(&r.key),
                origin,
            };
            assert_eq!(call(&router, req).await.0, expect, "{origin:?}");
        }
    }

    #[tokio::test]
    async fn minting_is_socket_only_and_redeeming_loopback_only() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let loopback = door.loopback_router(app());
        let r = sign_in(&door, &loopback).await;
        let req = Req {
            method: Method::POST,
            path: "/api/session",
            host: Some("127.0.0.1:8787"),
            cookie: Some(&r.cookie),
            key: Some(&r.key),
            origin: Some("http://127.0.0.1:8787"),
        };
        assert_eq!(call(&loopback, req).await.0, StatusCode::NOT_FOUND);

        let socket = door.socket_router(app()).layer(Extension(Caller::Socket {
            uid: OP,
            class: Class::Operator,
        }));
        let mint = Req {
            method: Method::POST,
            path: "/api/session",
            host: None,
            cookie: None,
            key: None,
            origin: None,
        };
        let (status, _, body) = call(&socket, mint).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["expires_in_secs"], 60);
        let url = json["url"].as_str().unwrap();
        let token = url
            .strip_prefix("http://127.0.0.1:8787/session?token=")
            .unwrap_or_else(|| panic!("{url}"));
        assert_eq!(token.len(), 64);
        // The minted link opens a session on loopback.
        let path = format!("/session?token={token}");
        assert_eq!(call(&loopback, get_req(&path)).await.0, StatusCode::OK);

        let (status, _, _) = call(&socket, get_req("/session?token=00")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The sessions belong to the door, not to whichever routes it serves: a
    /// session opened in front of standby's stub still opens the dashboard.
    #[tokio::test]
    async fn a_session_outlives_the_routes_it_was_opened_on() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let stub =
            door.loopback_router(Router::new().route("/api/state", get(|| async { "stub" })));
        let r = sign_in(&door, &stub).await;
        let live = door.loopback_router(app());
        let mut req = get_req("/api/state");
        req.cookie = Some(&r.cookie);
        req.key = Some(&r.key);
        let (status, _, body) = call(&live, req).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "state"));
    }

    // --- the React bundle (ADR-0028 addendum C) -------------------------------

    fn admit_headers(
        host: Option<&str>,
        session: Option<&Redeemed>,
        origin: Option<&str>,
    ) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(host) = host {
            h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        }
        if let Some(r) = session {
            let cookie = format!("{SESSION_COOKIE}={}", r.cookie);
            h.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
            h.insert(SESSION_HEADER, HeaderValue::from_str(&r.key).unwrap());
        }
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    /// Decision 2: a read under `/assets/` needs no session; a mutation there
    /// gets the session rules of any path, and only the router's `405` after.
    #[tokio::test]
    async fn assets_are_open_to_reads_and_nothing_else() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        let r = sign_in(&door, &router).await;
        let host = Some("127.0.0.1:8787");
        let exact = Some("http://127.0.0.1:8787");

        for method in [Method::GET, Method::HEAD] {
            for path in ["/assets/index-abc.js", "/assets/", "/assets/no-such.woff2"] {
                assert_eq!(
                    door.admit(&method, path, &admit_headers(host, None, None)),
                    Admit::Open,
                    "{method} {path}"
                );
            }
        }
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let path = "/assets/index-abc.js";
            assert_eq!(
                door.admit(&method, path, &admit_headers(host, None, exact)),
                Admit::Deny(StatusCode::UNAUTHORIZED),
                "{method} without a session"
            );
            for bad in [
                None,
                Some("https://evil.example"),
                Some("http://127.0.0.1:9"),
            ] {
                assert_eq!(
                    door.admit(&method, path, &admit_headers(host, Some(&r), bad)),
                    Admit::Deny(StatusCode::FORBIDDEN),
                    "{method} with a session and Origin {bad:?}"
                );
            }
            assert_eq!(
                door.admit(&method, path, &admit_headers(host, Some(&r), exact)),
                Admit::Session,
                "{method} with both runs no handler but the router's 405"
            );
        }

        // The Host check comes first, assets included.
        let asset = "/assets/index-abc.js";
        assert_eq!(
            door.admit(&Method::GET, asset, &admit_headers(None, None, None)),
            Admit::Deny(StatusCode::BAD_REQUEST)
        );
        for bad in [
            "evil.example:8787",
            "127.0.0.1:9999",
            "host.docker.internal:8787",
        ] {
            assert_eq!(
                door.admit(&Method::GET, asset, &admit_headers(Some(bad), None, None)),
                Admit::Deny(StatusCode::MISDIRECTED_REQUEST),
                "{bad}"
            );
        }

        // Only the `/assets/` prefix, exactly as the router sees it, is open.
        for path in [
            "/assets",
            "/Assets/x.js",
            "//assets/x.js",
            "/x/assets/y.js",
            "/api/state",
        ] {
            assert_eq!(
                door.admit(&Method::GET, path, &admit_headers(host, None, None)),
                Admit::Deny(StatusCode::UNAUTHORIZED),
                "{path}"
            );
        }
    }

    /// `/settings` is a page a browser navigates to, so it opens on the cookie
    /// alone, like `/`; nothing else near it does.
    #[tokio::test]
    async fn the_settings_page_opens_on_the_cookie_alone() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(
            app()
                .route("/settings", get(|| async { "settings" }))
                .route("/settings/", get(|| async { "nested" })),
        );
        let (status, _, body) = call(&router, get_req("/settings")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, SIGN_IN_HTML);

        let r = sign_in(&door, &router).await;
        let mut page = get_req("/settings");
        page.cookie = Some(&r.cookie);
        let (status, _, body) = call(&router, page).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "settings"));

        for path in ["/settings/", "/settings?x=1", "/api/settings"] {
            let mut cookie_only = get_req(path);
            cookie_only.cookie = Some(&r.cookie);
            let status = call(&router, cookie_only).await.0;
            // `/settings?x=1` is the page itself: the query is not the path.
            let want = if path == "/settings?x=1" {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(status, want, "{path}");
        }

        let mut post = get_req("/settings");
        post.method = Method::POST;
        post.cookie = Some(&r.cookie);
        assert_eq!(
            call(&router, post).await.0,
            StatusCode::FORBIDDEN,
            "a mutation still needs the exact Origin"
        );
    }

    /// The dashboard's views (ADR-0054 § 5) are navigated to, so each opens on
    /// the cookie alone; a trailing slash, a view not opened yet and the API do
    /// not.
    #[tokio::test]
    async fn each_view_opens_on_the_cookie_alone_and_nothing_near_it_does() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());
        let r = sign_in(&door, &router).await;
        let cookie_only = Redeemed {
            cookie: r.cookie.clone(),
            key: String::new(),
        };
        let host = Some("127.0.0.1:8787");
        for view in [
            "/",
            "/activity",
            "/jobs",
            "/memory",
            "/maintenance",
            "/settings",
        ] {
            let h = admit_headers(host, Some(&cookie_only), None);
            assert_eq!(door.admit(&Method::GET, view, &h), Admit::Session, "{view}");
            let none = admit_headers(host, None, None);
            assert_eq!(
                door.admit(&Method::GET, view, &none),
                Admit::Deny(StatusCode::UNAUTHORIZED),
                "{view}"
            );
        }
        for path in [
            "/activity/",
            "/memory/",
            "/maintenance/",
            "/repositories",
            "/Activity",
            "/activity/x",
            "/api/state",
        ] {
            let h = admit_headers(host, Some(&cookie_only), None);
            assert_eq!(
                door.admit(&Method::GET, path, &h),
                Admit::Deny(StatusCode::UNAUTHORIZED),
                "{path}"
            );
        }
    }

    /// Redeem a fresh link with `extra` after its token; the page's body.
    async fn redeem_with(door: &Arc<FrontDoor>, router: &Router, extra: &str) -> String {
        let token = door.sessions().mint_link().unwrap();
        let path = format!("/session?token={token}{extra}");
        let (status, _, body) = call(router, get_req(&path)).await;
        assert_eq!(status, StatusCode::OK, "{extra}: {body}");
        body
    }

    /// `next=settings` sends the page on to `/settings`; anything else to `/`,
    /// and the value itself never reaches the page.
    #[tokio::test]
    async fn a_redeemed_link_goes_on_only_where_the_allowlist_says() {
        let (s, _) = store();
        let door = door_with(s, None, OP);
        let router = door.loopback_router(app());

        let body = redeem_with(&door, &router, "&next=settings").await;
        assert!(body.contains("location.replace('/settings')"), "{body}");
        let body = redeem_with(&door, &router, "").await;
        assert!(body.contains("location.replace('/')"), "{body}");

        for hostile in [
            "settings2",
            "Settings",
            "settings%00",
            "%2Fsettings",
            "/settings",
            "//evil.example",
            "https://evil.example/",
            "javascript:alert(1)",
            "x');alert(1)//",
            "%27);alert(1)//",
            "",
        ] {
            let body = redeem_with(&door, &router, &format!("&next={hostile}")).await;
            assert!(body.contains("location.replace('/')"), "{hostile}: {body}");
            assert!(!body.contains("/settings"), "{hostile}: {body}");
            for echoed in ["evil", "alert", "javascript", "%2", "settings"] {
                assert!(!body.contains(echoed), "{hostile} echoed: {body}");
            }
        }
        // `next` before the token is the same pair.
        let token = door.sessions().mint_link().unwrap();
        let path = format!("/session?next=settings&token={token}");
        let (_, _, body) = call(&router, get_req(&path)).await;
        assert!(body.contains("location.replace('/settings')"), "{body}");
    }

    // --- the socket, for real -------------------------------------------------

    /// A short directory for one socket: `sun_path` holds about 100 bytes.
    struct SockDir(PathBuf);

    impl SockDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            // `/tmp`, not `temp_dir()`: a runner's `TMPDIR` can be deep enough
            // to push a socket path past `sun_path`'s 104 bytes on macOS.
            let dir = PathBuf::from("/tmp").join(format!(
                "fbr-ctl-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn sock(&self) -> PathBuf {
            self.0.join("control.sock")
        }
    }

    impl Drop for SockDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn free_port() -> u16 {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap().port()
    }

    /// The kernel's answer for a client in this very process is this euid, and
    /// it classifies as the operator exactly when the operator is this account.
    #[tokio::test]
    async fn a_same_process_peer_is_classified_by_its_real_uid() {
        let me = crate::standby::euid();
        for operator in [me, me.wrapping_add(1)] {
            let dir = SockDir::new();
            let (s, _) = store();
            let door = FrontDoor::new(
                free_port().await,
                Some(dir.sock()),
                0o600,
                None,
                operator,
                s,
            );
            let serving = door.start(app()).await.unwrap();

            let mode = {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(dir.sock()).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode, 0o600);

            let expected = class_for_uid(me, operator);
            let answer = request(
                &dir.sock(),
                Fallback::Tcp(door.port()),
                reqwest::Method::GET,
                "/api/whoami",
                None,
            )
            .await;
            match expected {
                Class::Refused => assert!(
                    matches!(answer, Err(ClientError::Refused { .. })),
                    "{answer:?}"
                ),
                class => {
                    let (status, body) = answer.unwrap();
                    assert_eq!(status, reqwest::StatusCode::OK);
                    assert_eq!(body, format!("socket {me} {}", class.as_str()));
                }
            }
            if me != 0 {
                let want = if operator == me {
                    Class::Operator
                } else {
                    Class::Refused
                };
                assert_eq!(expected, want);
            }
            serving.stop(Duration::from_secs(5)).await;
        }
    }

    /// Every gid this process holds, as `id -G` lists them.
    fn my_gids() -> Vec<u32> {
        let out = std::process::Command::new("/usr/bin/id")
            .arg("-G")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .collect()
    }

    /// The group and mode the socket at `path` has.
    fn group_and_mode(path: &Path) -> (u32, u32) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).unwrap();
        (meta.gid(), meta.mode() & 0o777)
    }

    /// S1: under SYSTEM the socket is given the operator group, whichever
    /// group it was created with, and keeps its mode; with no group named it
    /// keeps the one it was created with.
    #[tokio::test]
    async fn the_socket_gets_the_operator_group_and_its_mode() {
        let me = crate::standby::euid();
        let dir = SockDir::new();
        let (s, _) = store();
        let plain = FrontDoor::new(free_port().await, Some(dir.sock()), 0o660, None, me, s);
        let serving = plain.start(app()).await.unwrap();
        let (created, mode) = group_and_mode(&dir.sock());
        assert_eq!(mode, 0o660);
        serving.stop(Duration::from_secs(5)).await;

        // A group this process may give that differs from the one the socket
        // is created with; root may give any.
        let Some(other) = my_gids()
            .into_iter()
            .chain((me == 0).then_some(1))
            .find(|g| *g != created)
        else {
            return; // a single-group account: nothing to tell apart
        };
        let (s, _) = store();
        let door = FrontDoor::new(
            free_port().await,
            Some(dir.sock()),
            0o660,
            Some(other),
            me,
            s,
        );
        let serving = door.start(app()).await.unwrap();
        assert_eq!(group_and_mode(&dir.sock()), (other, 0o660));
        let (status, _) = request(
            &dir.sock(),
            Fallback::Never,
            reqwest::Method::GET,
            "/api/whoami",
            None,
        )
        .await
        .unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
        serving.stop(Duration::from_secs(5)).await;
    }

    /// A group the daemon cannot give is said and the socket served as it is:
    /// losing the operator's way in must not take the daemon down with it.
    #[tokio::test]
    async fn a_group_the_daemon_cannot_give_still_serves() {
        let me = crate::standby::euid();
        if me == 0 {
            return; // root may give any group
        }
        let held = my_gids();
        let foreign = (1..4096).find(|g| !held.contains(g)).unwrap();
        let dir = SockDir::new();
        let (s, _) = store();
        let door = FrontDoor::new(
            free_port().await,
            Some(dir.sock()),
            0o660,
            Some(foreign),
            me,
            s,
        );
        let serving = door.start(app()).await.unwrap();
        let (gid, mode) = group_and_mode(&dir.sock());
        assert_ne!(gid, foreign);
        assert_eq!(mode, 0o660);
        serving.stop(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn a_refused_peer_is_told_one_sentence_and_nothing_runs() {
        let me = crate::standby::euid();
        if me == 0 {
            return; // root is always present; nothing to refuse
        }
        let dir = SockDir::new();
        let (s, _) = store();
        let door = FrontDoor::new(
            free_port().await,
            Some(dir.sock()),
            0o600,
            None,
            me + 1,
            s.clone(),
        );
        let serving = door.start(app()).await.unwrap();
        let client = reqwest::Client::builder()
            .unix_socket(dir.sock())
            .build()
            .unwrap();
        for (method, path) in [
            ("POST", "/api/session"),
            ("GET", "/api/state"),
            ("GET", "/nowhere"),
        ] {
            let resp = client
                .request(
                    reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                    format!("http://localhost{path}"),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403, "{method} {path}");
            assert_eq!(resp.text().await.unwrap(), REFUSED_BODY);
        }
        serving.stop(Duration::from_secs(5)).await;
    }

    /// With no socket and nothing on the port, the client says there is no
    /// daemon, rather than that it was refused.
    #[tokio::test]
    async fn no_socket_and_no_listener_is_no_daemon() {
        let dir = SockDir::new();
        let answer = request(
            &dir.sock(),
            Fallback::Tcp(free_port().await),
            reqwest::Method::GET,
            "/api/state",
            None,
        )
        .await;
        assert!(
            matches!(answer, Err(ClientError::NoDaemon { .. })),
            "{answer:?}"
        );
    }

    /// A daemon from before ADR-0028 has no socket: the client falls back to
    /// loopback, and a `401` there means a daemon whose socket this account
    /// cannot use.
    #[tokio::test]
    async fn without_a_socket_the_client_falls_back_to_loopback() {
        let dir = SockDir::new();
        let (s, _) = store();
        let door = FrontDoor::new(free_port().await, None, 0o600, None, OP, s);
        let serving = door.start(app()).await.unwrap();
        let answer = request(
            &dir.sock(),
            Fallback::Tcp(door.port()),
            reqwest::Method::GET,
            "/api/state",
            None,
        )
        .await;
        assert!(
            matches!(answer, Err(ClientError::Refused { .. })),
            "{answer:?}"
        );
        serving.stop(Duration::from_secs(5)).await;
    }

    #[test]
    fn a_stale_socket_is_cleared_and_anything_else_is_left_alone() {
        let dir = SockDir::new();
        let path = dir.sock();
        clear_stale_socket(&path).expect("nothing there");

        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());
        clear_stale_socket(&path).unwrap();
        assert!(!path.exists(), "a stale socket is removed");

        std::fs::write(&path, "not a socket").unwrap();
        assert!(clear_stale_socket(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a socket");
        std::fs::remove_file(&path).unwrap();

        std::os::unix::fs::symlink(dir.0.join("elsewhere"), &path).unwrap();
        assert!(clear_stale_socket(&path).is_err());
        assert!(
            std::fs::symlink_metadata(&path).is_ok(),
            "the symlink stays"
        );
        std::fs::remove_file(&path).unwrap();

        std::fs::create_dir(&path).unwrap();
        assert!(clear_stale_socket(&path).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn the_socket_mode_follows_the_layout() {
        assert_eq!(socket_mode(Layout::System), 0o660);
        assert_eq!(socket_mode(Layout::Legacy), 0o600);
    }

    /// However much a daemon sends, `request` refuses past the bound rather
    /// than holding it, and a reply under the bound still arrives whole.
    #[tokio::test]
    async fn a_huge_reply_is_refused() {
        let app = axum::Router::new()
            .route(
                "/huge",
                axum::routing::get(|| async { "y".repeat(REPLY_MAX + 1) }),
            )
            .route(
                "/fits",
                axum::routing::get(|| async { "y".repeat(REPLY_MAX) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        // No socket at this path, so the request goes over loopback TCP.
        let socket = Path::new("/nonexistent/faber-test/control.sock");

        let err = request(
            socket,
            Fallback::Tcp(port),
            reqwest::Method::GET,
            "/huge",
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ClientError::Transport(m) if m == TOO_LARGE),
            "{err:?}"
        );
        let (status, body) = request(
            socket,
            Fallback::Tcp(port),
            reqwest::Method::GET,
            "/fits",
            None,
        )
        .await
        .unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(body.len(), REPLY_MAX);
    }
}
