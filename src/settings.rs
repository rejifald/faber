//! The settings substrate behind the dashboard's Settings page: read the
//! config file, validate a proposed one, write it back, and say honestly which
//! changes the running daemon has actually picked up.
//!
//! faber reads `config.toml` exactly once, at startup, and folds it into an
//! immutable [`crate::reconcile::Config`]. Only the autolimiter keeps live
//! knobs (see [`crate::limiter::Limiter::set_tuning`]). So a settings page has
//! two jobs, and this module draws the line between them:
//!
//! * **Persist** the whole config, so the next start uses it — that part covers
//!   every key.
//! * **Report** which of the just-saved keys differ from the ones this process
//!   booted with, so the UI can say "restart to apply: topic, profile" instead
//!   of pretending the change already took.
//!
//! The boot snapshot is captured in `main` — from the file when there is one,
//! synthesized from the CLI flags when there isn't — so the diff is against what
//! the daemon is *running*, never against whatever the file happens to say now.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::config::FileConfig;

/// The autolimiter knobs the control panel applies to the live daemon
/// (`POST /api/config` → [`crate::limiter::Limiter::set_tuning`]). Changing one
/// of these through the settings page takes effect immediately, so it must not
/// be reported as needing a restart.
const LIVE_AUTOLIMIT_KEYS: &[&str] = &[
    "reserve_mib",
    "headroom_mib",
    "margin",
    "native_concurrency",
    "hard_ceiling",
];

/// Whether a dotted config key is one the running daemon takes without a
/// restart.
///
/// Two families qualify today. The autolimiter knobs above go straight into the
/// live limiter, and *every* `[update]` key does too: `POST /api/settings` hands
/// the new policy to the updater task, which re-reads it on its next tick. That
/// second one matters more than it looks — an autoupdate section whose own
/// switches only took effect after a restart would be a poor advertisement for
/// the feature.
pub(crate) fn is_live_key(key: &str) -> bool {
    if let Some(sub) = key.strip_prefix("autolimit.") {
        return LIVE_AUTOLIMIT_KEYS.contains(&sub);
    }
    key.starts_with("update.")
}

// --- the form schema -------------------------------------------------------
//
// The Settings page (`/settings`, built from `ui/`) renders from this, rather
// than hard-coding the key list. A new config key is then one entry here plus
// its field on [`FileConfig`].

/// One editable config key.
#[derive(Debug, Clone, Serialize)]
pub struct FieldSpec {
    /// Dotted path into the config object (`autolimit.margin`).
    pub key: &'static str,
    pub label: &'static str,
    /// `text` | `int` | `float` | `bool` | `enum` | `ceiling` | `csv` | `lines`
    /// | `map`.
    ///
    /// `ceiling` is `autolimit.hard_ceiling`'s own kind (ADR-0019): a select
    /// between `auto` / `off` / `number`, plus a number input that only matters
    /// — and only shows — in `number` mode. Renders `"auto"`, `"off"` or an
    /// integer; never `null` (a plain `int` field's "blank = unset" doesn't
    /// apply here, since the setting is never truly absent, only `off`).
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    /// Suffix shown after the input (`s`, `MiB`, `×`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<&'static str>,
    /// Choices for an `enum`. Also carries the three labels of an **optional
    /// `bool`**, in the order `[unset, true, false]` — an optional bool is a
    /// tri-state (`null` is not `false`), so it renders as a select, not a
    /// checkbox, and each state needs a word of its own.
    ///
    /// For an **optional `enum`** (`autolimit.cpu` and `autolimit.reserve`), the front end
    /// renders one extra blank "unstated" choice ahead of these, whose
    /// underlying value is the *last* entry here — so the field never appears
    /// to default to the first option, and a form saved while it's still blank
    /// writes that last (old/off-equivalent) value explicitly rather than
    /// leaving the key unset. Order this list accordingly: the safe/off-like
    /// choice goes last.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub options: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// True when a save pushes this straight into the running daemon; everything
    /// else waits for a restart.
    pub live: bool,
    /// True when an empty input means "unset" (`null`) rather than an empty value.
    /// On a `bool` this makes the field a tri-state — see [`FieldSpec::options`].
    pub optional: bool,
    /// For an **optional `int`** whose absence is a third state rather than a
    /// default (`host.disk_admission_floor_gib`, ADR-0024, and the machine's share,
    /// `host.keep_memory_gib` and `host.keep_cpus`, ADR-0021): the value a
    /// blank input saves as. The input reads "unstated" while the key is absent,
    /// and a form saved while it is still blank writes this number explicitly —
    /// the optional-enum rule above, for a number, and for the same reason
    /// (AGENTS.md: a tightening key must not stay silently unset once the form
    /// that would state it has been submitted). Absent from the JSON otherwise,
    /// which keeps an optional int's ordinary "blank = unset, faber's default".
    ///
    /// On an **optional `bool`** (`host.pull_unqualified_images`, #156) the same
    /// rule: a blank select saves `true` for a non-zero value and `false` for 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstated_saves: Option<f64>,
    /// For a field whose widening needs `sudo` (issue #78): the command a UI
    /// shows for the widening direction (the whole field, unless it has
    /// [`FieldSpec::narrows`]). `<profile>` stands for the profile's
    /// name, and a `<…>` word for the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sudo: Option<&'static str>,
    /// Read-only with no command at all: a held section (`[registry_auth]`) whose
    /// widening validation refuses outright, so there is no `sudo` command a UI
    /// could honestly offer. The value shows; [`FieldSpec::narrow`], when set, is
    /// still offered, since turning a held switch off is always accepted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    /// The one narrowing a UI offers on that read-only field without `sudo`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrow: Option<Narrow>,
    /// For a `sudo` list or map: the edit that narrows it, which the form may
    /// make without `sudo` (anatomy.md § 2, "Trust lists narrow in place").
    /// `"remove"`: removing an entry narrows. `"remove-keep-one"`: so does
    /// removing one, but removing the last widens (an empty `trusted_actors`
    /// trusts everyone). `"add"`: adding one narrows. Absent: the field stays
    /// read-only. Each is held to [`crate::control::trust_widening`] by a test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrows: Option<&'static str>,
    /// For a list the form adds to: what one entry must look like.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemSpec>,
}

/// What one entry of a list must look like, for a list the form adds to. The
/// form checks `pattern`; the daemon checks the same shape by hand
/// ([`item_ok`]), in `check-config` and the settings save, and a test keeps the
/// two in agreement.
#[derive(Debug, Clone, Serialize)]
pub struct ItemSpec {
    /// A regular expression (JavaScript syntax) the whole entry must match.
    pub pattern: &'static str,
    /// An entry that matches, shown as the input's placeholder.
    pub example: &'static str,
    /// The sentence shown under an entry that does not match.
    pub error: &'static str,
}

/// A GitHub login, a bot's included (`dependabot[bot]`). An underscore is
/// allowed: Enterprise Managed User logins carry one (`handle_shortcode`).
const LOGIN_ITEM: ItemSpec = ItemSpec {
    pattern: r"^[A-Za-z0-9_]+(-[A-Za-z0-9_]+)*(\[bot\])?$",
    example: "someone-else",
    error: "Not a GitHub login: letters, digits, underscores and single hyphens only.",
};

/// A `drop_grants` entry. The daemon's own check is the wall's parser
/// ([`crate::wall::grant_edit::parse_drop_spec`]), stricter than this pattern
/// (octet and port ranges, no leading zeros).
const GRANT_ITEM: ItemSpec = ItemSpec {
    pattern: r"^\d{1,3}(\.\d{1,3}){3}/\d{1,2} \d{1,5}/(tcp|udp)$",
    example: "10.0.0.5/32 5432/tcp",
    error: "Write it as ADDRESS/PREFIX PORT/PROTO, for example 10.0.0.5/32 5432/tcp.",
};

/// An `owner/repo` slug.
const REPO_ITEM: ItemSpec = ItemSpec {
    pattern: r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$",
    example: "owner/repo",
    error: "Write it as owner/repo.",
};

/// Whether `item` is a well-formed entry of the list at `key` (a profile
/// field by its bare key, `drop_grants`): the daemon's side of
/// [`FieldSpec::item`]. `true` for a key without an `item`.
pub(crate) fn item_ok(key: &str, item: &str) -> bool {
    let word = |s: &str, extra: &[u8]| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
    };
    match key {
        "denied_actors" => {
            let login = item.strip_suffix("[bot]").unwrap_or(item);
            !login.is_empty() && login.split('-').all(|part| word(part, b"_"))
        }
        "drop_grants" => crate::wall::grant_edit::parse_drop_spec(item).is_ok(),
        "autolimit.priority_repos" => item
            .split_once('/')
            .is_some_and(|(owner, repo)| word(owner, b"_.-") && word(repo, b"_.-")),
        _ => true,
    }
}

/// Every list entry in `cfg` that fails its field's [`FieldSpec::item`], as a
/// sentence naming the key. `check-config` reports these as warnings, never as
/// errors: a file the daemon already runs on is not refused over an entry the
/// form would not have let in. The settings save refuses only the entries it
/// adds ([`new_item_errors`]). `drop_grants` is not here:
/// `validate_structural` already parses each one with the wall's own parser.
pub fn item_errors(cfg: &FileConfig) -> Vec<String> {
    item_errors_where(cfg, |_, _| true)
}

/// [`item_errors`] for the entries of `next` that `old` does not already hold:
/// what the settings save refuses. An entry already in the file is kept as it
/// is, so a save that narrows or changes something else is never held back
/// by it, and removing it (a widening, for `denied_actors`) is never the only
/// way to save.
pub(crate) fn new_item_errors(old: &FileConfig, next: &FileConfig) -> Vec<String> {
    item_errors_where(next, |key, item| match key {
        "denied_actors" => !old.denied_actors.iter().any(|a| a == item),
        _ => !old
            .autolimit
            .priority_repos
            .iter()
            .any(|r| r.trim() == item.trim()),
    })
}

fn item_errors_where(cfg: &FileConfig, judged: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut errs = Vec::new();
    for (i, a) in cfg.denied_actors.iter().enumerate() {
        if judged("denied_actors", a) && !item_ok("denied_actors", a) {
            errs.push(format!(
                "denied_actors[{i}]: {a:?} is not a GitHub login: letters, digits, \
                 underscores and single hyphens only"
            ));
        }
    }
    for (i, r) in cfg.autolimit.priority_repos.iter().enumerate() {
        if judged("autolimit.priority_repos", r) && !item_ok("autolimit.priority_repos", r.trim()) {
            // The sentence `validate_structural` gives a malformed one, so the
            // two checks read as one.
            errs.push(format!(
                "autolimit.priority_repos[{i}] must be written owner/repo"
            ));
        }
    }
    errs
}

/// A narrowing button on a read-only field: its label and the value it saves.
#[derive(Debug, Clone, Serialize)]
pub struct Narrow {
    pub label: &'static str,
    /// The value, as a JSON literal (`false`, `"enforce"`, `[]`, `null`).
    pub value: &'static str,
}

const fn narrow(label: &'static str, value: &'static str) -> Option<Narrow> {
    Some(Narrow { label, value })
}

/// A titled group of fields.
#[derive(Debug, Clone, Serialize)]
pub struct SectionSpec {
    pub title: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    pub fields: &'static [FieldSpec],
}

/// The whole form: the single-value sections plus the per-profile fields the UI
/// repeats for each `[[profile]]`.
#[derive(Debug, Clone, Serialize)]
pub struct FormSchema {
    pub sections: &'static [SectionSpec],
    pub profile_fields: &'static [FieldSpec],
}

/// A `FieldSpec` with everything optional left out.
const fn f(key: &'static str, label: &'static str, kind: &'static str) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        hint: None,
        unit: None,
        options: &[],
        min: None,
        step: None,
        live: false,
        optional: false,
        unstated_saves: None,
        sudo: None,
        held: false,
        narrow: None,
        narrows: None,
        item: None,
    }
}

const DAEMON_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        hint: Some("The GitHub repo topic that opts a repo into faber."),
        ..f("topic", "Topic", "text")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(1.0),
        ..f("poll_interval_secs", "Poll interval", "int")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(0.0),
        hint: Some("Janitor cadence for leaked Docker resources. 0 disables it."),
        ..f("sweep_interval_secs", "Sweep interval", "int")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(0.0),
        optional: true,
        hint: Some("Lifetime cap on a runner container. Blank = 3h; 0 = uncapped."),
        ..f("runner_timeout_secs", "Runner timeout", "int")
    },
    FieldSpec {
        options: &["per-job", "demand"],
        hint: Some("demand mints only the shortfall vs. runners already on GitHub, so several faber instances can share one pool."),
        ..f("provisioning", "Provisioning", "enum")
    },
];

const TRUST_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        sudo: Some("sudo faber config set gate.allowed_events <events>"),
        narrows: Some("remove"),
        hint: Some(
            "Trigger events provisioned for. Narrow-only: anything outside faber's own list \
             (push, pull_request, workflow_dispatch, schedule, merge_group, repository_dispatch, release) \
             is refused whatever you put here. repository_dispatch and release are off unless listed. \
             A repo that locks branch creation gains exposure from them. \
             Empty = refuse everything.",
        ),
        ..f("gate.allowed_events", "Events", "lines")
    },
    FieldSpec {
        sudo: Some("sudo faber config set gate.allow_public_repos true"),
        narrow: narrow("Turn off", "false"),
        hint: Some(
            "Off = private repos only, refused before any other check. On, anyone can queue \
             jobs against a watched public repo — fork PRs are still refused, but this widens \
             what the machine will run.",
        ),
        ..f("gate.allow_public_repos", "Service public repos", "bool")
    },
    FieldSpec {
        sudo: Some("sudo faber config set gate.allow_job_token_dispatches true"),
        narrow: narrow("Turn off", "false"),
        hint: Some(
            "Off = a workflow_dispatch or repository_dispatch made with a job's own token \
             (actor github-actions[bot]) is refused, whatever the actor lists say. On, it runs: \
             whatever code a job pushed with that token, under the dispatched workflow's labels \
             and secrets (ADR-0059).",
        ),
        ..f(
            "gate.allow_job_token_dispatches",
            "Run job-token dispatches",
            "bool",
        )
    },
    FieldSpec {
        sudo: Some("sudo faber config set gate.on_bypass record"),
        narrow: narrow("Enforce", "\"enforce\""),
        options: &["record", "enforce"],
        hint: Some(
            "What happens when a job the gate refuses runs anyway, on a runner faber minted \
             for another job (ADR-0030). record (the default) records it on the dashboard and \
             in the audit log. enforce, for a fork, untrusted-actor, public-repo or \
             never-provisioned event: kills that runner, deletes the repo's cache volume, holds \
             the repo until you clear it, and pauses its minting while such a job is queued. \
             The repo's own events you narrowed away are only recorded either way.",
        ),
        ..f("gate.on_bypass", "On a gate bypass", "enum")
    },
    FieldSpec {
        // A shorter search lets more jobs reach the hook's deadline outcome,
        // which on a private repo under `record` is to run unverified.
        sudo: Some("sudo faber config set gate.job_start_search_secs <seconds>"),
        unit: Some("s"),
        min: Some(1.0),
        hint: Some(
            "How long faber looks for, and judges, the job a runner was handed once it starts \
             (ADR-0059). At least one poll interval plus 60 s, and 30 s short of the hook's \
             deadline.",
        ),
        ..f("gate.job_start_search_secs", "Job-start search", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set gate.job_start_deadline_secs <seconds>"),
        unit: Some("s"),
        min: Some(1.0),
        hint: Some(
            "How long a runner's job-started hook waits for faber's verdict. Then it runs the \
             job unverified on a private repo under record, and stops it otherwise.",
        ),
        ..f("gate.job_start_deadline_secs", "Job-start deadline", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set trusted_actors <logins>"),
        narrows: Some("remove-keep-one"),
        hint: Some("One login per line. Empty = trust by provenance alone."),
        ..f("trusted_actors", "Trusted actors", "lines")
    },
    FieldSpec {
        sudo: Some("sudo faber config set denied_actors <logins>"),
        narrows: Some("add"),
        item: Some(LOGIN_ITEM),
        hint: Some("Never provisioned for, whatever the provenance."),
        ..f("denied_actors", "Denied actors", "lines")
    },
];

const AUTOLIMIT_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        unit: Some("MiB"),
        min: Some(0.0),
        live: true,
        hint: Some(
            "Held back from VM memory: budget = MemTotal − reserve. With the machine's memory \
             share declared, the floor: Gate 1 holds back the larger of this and the 24 h peak \
             of what the VM's other tenants use (ADR-0021 §3).",
        ),
        ..f("autolimit.reserve_mib", "Reserve", "int")
    },
    FieldSpec {
        unit: Some("MiB"),
        min: Some(1.0),
        live: true,
        hint: Some("Gate 2 slack — never admit down to the last byte."),
        ..f("autolimit.headroom_mib", "Headroom", "int")
    },
    FieldSpec {
        unit: Some("×"),
        min: Some(1.0),
        step: Some(0.05),
        live: true,
        hint: Some("Learned cap = rolling-max(peaks) × margin."),
        ..f("autolimit.margin", "Margin", "float")
    },
    FieldSpec {
        min: Some(1.0),
        live: true,
        hint: Some("Limits nothing since ADR-0052: the native jobs it counted are gone."),
        ..f("autolimit.native_concurrency", "Native concurrency", "int")
    },
    FieldSpec {
        min: Some(1.0),
        live: true,
        hint: Some(
            "Cap on concurrent Docker jobs, on top of the budget. Auto = faber's own \
             pick (today: 4); off = pure budget. To stop Docker admission, use Hold \
             Docker… on the Overview tab.",
        ),
        ..f("autolimit.hard_ceiling", "Hard ceiling", "ceiling")
    },
    FieldSpec {
        options: &["p75", "max"],
        optional: true,
        hint: Some(
            "ADR-0033. What a Docker admission debits. max (blank, the default) debits each \
             class's cap. p75 debits its typical peak, min(cap, p75 × margin), while the \
             container limit stays the cap; it freezes the newest job when VM memory falls \
             below half of headroom, and falls back to max for a day or more on any sign of \
             overrun. Applies on restart.",
        ),
        ..f("autolimit.reserve", "Reservation", "enum")
    },
    FieldSpec {
        optional: true,
        hint: Some(
            "ADR-0036. One owner/repo per line, matched case-insensitively. A parked claim \
             from one of these repos queues ahead of every other repo's, first-come-first-served \
             within each group; every gate still applies and nothing running is preempted. \
             Meant for small, low-volume repos: a steady stream from a listed repo can delay \
             the rest. Empty = strict first-come-first-served. Applies on restart.",
        ),
        item: Some(REPO_ITEM),
        ..f("autolimit.priority_repos", "Priority repos", "lines")
    },
    FieldSpec {
        unit: Some("MiB"),
        min: Some(0.0),
        hint: Some("Anti-flap band above headroom before admitting resumes."),
        ..f("autolimit.band_mib", "Band", "int")
    },
    FieldSpec {
        unit: Some("MiB"),
        min: Some(1.0),
        hint: Some("Per-container ceiling for a class with no learned cap yet."),
        ..f("autolimit.profiling_cap_mib", "Profiling cap", "int")
    },
    FieldSpec {
        min: Some(0.0),
        step: Some(0.05),
        hint: Some(
            "A learned reservation above this fraction of budget warns instead of applying.",
        ),
        ..f("autolimit.escalation_frac", "Escalation fraction", "float")
    },
    FieldSpec {
        min: Some(1.0),
        hint: Some("How many recent per-class peaks the rolling window keeps."),
        ..f("autolimit.window", "Learning window", "int")
    },
    FieldSpec {
        // faber runs it on the engine that holds every job.
        sudo: Some("sudo faber config set autolimit.probe_image <image>"),
        hint: Some("Tiny image used for the live memory probe."),
        ..f("autolimit.probe_image", "Probe image", "text")
    },
    FieldSpec {
        options: &["auto", "off"],
        optional: true,
        hint: Some(
            "CPU-aware admission (ADR-0019): reserve, enforce (--cpus) and learn per-class \
             CPU shares, and pause on VM CPU pressure. Blank = unstated, which behaves as off.",
        ),
        ..f("autolimit.cpu", "CPU admission", "enum")
    },
    FieldSpec {
        unit: Some("×"),
        min: Some(0.1),
        step: Some(0.05),
        hint: Some("CPU budget = VM cores × overcommit − reserved cores."),
        ..f("autolimit.cpu_overcommit", "CPU overcommit", "float")
    },
    FieldSpec {
        unit: Some("cores"),
        min: Some(0.0),
        step: Some(0.5),
        hint: Some("Cores held back from the CPU budget for Docker itself and dev stacks."),
        ..f("autolimit.reserve_cpus", "Reserved cores", "float")
    },
    FieldSpec {
        unit: Some("cores"),
        min: Some(0.5),
        step: Some(0.5),
        hint: Some("Runner share for a class with no learned share yet, and the most a learned one may grow to."),
        ..f("autolimit.profiling_cpus", "Profiling cores", "float")
    },
    FieldSpec {
        unit: Some("%"),
        min: Some(1.0),
        step: Some(5.0),
        hint: Some("Pause admission while VM CPU pressure (PSI some avg60) is at or above this."),
        ..f("autolimit.cpu_pause_pressure", "CPU pause pressure", "float")
    },
    FieldSpec {
        unit: Some("%"),
        min: Some(0.0),
        step: Some(5.0),
        hint: Some("Resume admission once VM CPU pressure has fallen to this."),
        ..f("autolimit.cpu_resume_pressure", "CPU resume pressure", "float")
    },
];

const DRIFT_FIELDS: &[FieldSpec] = &[FieldSpec {
    options: &["off", "surface", "gate"],
    hint: Some("surface posts a failure commit status; gate is Phase 2 (detect-only today)."),
    ..f("playwright_drift.mode", "Mode", "enum")
}];

const RUNNER_IMAGE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        options: &["off", "on-demand"],
        hint: Some(
            "on-demand builds a per-version variant of a profile's image when a job pins a \
             version the image doesn't bake.",
        ),
        ..f("runner_image.build", "Build", "enum")
    },
    FieldSpec {
        min: Some(1.0),
        hint: Some("How many most-recently-used variants stay warm."),
        ..f("runner_image.keep", "Keep warm", "int")
    },
    FieldSpec {
        unit: Some("h"),
        min: Some(0.0),
        hint: Some("Evict a variant untouched this long whatever its rank. 0 = no TTL."),
        ..f("runner_image.idle_ttl_hours", "Idle TTL", "int")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(60.0),
        hint: Some("Cap on one variant build — a browser set is a gigabyte over this host's link."),
        ..f("runner_image.build_timeout_secs", "Build timeout", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set runner_image.dockerfile <path>"),
        narrow: narrow("Use faber's template", "null"),
        optional: true,
        hint: Some("Your own derivation recipe. Blank = faber's own template."),
        ..f("runner_image.dockerfile", "Recipe", "text")
    },
];

const HOST_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        hint: Some("Restart a wedged Docker daemon instead of only refusing to use it."),
        ..f("host.revive_docker", "Revive Docker", "bool")
    },
    FieldSpec {
        hint: Some("Restart a macOS VM stuck before its runner console, once per job."),
        ..f("host.revive_vms", "Revive VMs", "bool")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(60.0),
        hint: Some("Unresponsive this long counts as a wedge, not a hiccup under load."),
        ..f("host.revive_after_secs", "Revive after", "int")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(60.0),
        hint: Some("Minimum gap between revival attempts — what stops a restart loop."),
        ..f("host.revive_cooldown_secs", "Revive cooldown", "int")
    },
    FieldSpec {
        unit: Some("GiB"),
        min: Some(0.0),
        hint: Some("Reclaim CI-created images and build cache below this free space. 0 = never."),
        ..f("host.disk_floor_gib", "Disk floor", "int")
    },
    FieldSpec {
        unit: Some("GiB"),
        min: Some(0.0),
        optional: true,
        unstated_saves: Some(0.0),
        hint: Some(
            "Start a Docker job only if the Docker volume keeps at least this free after its \
             own charge (10 GiB with dind, 3 GiB without) and those of the jobs started in the \
             last 10 minutes — and don't restart a wedged Docker below it either (ADR-0024, \
             ADR-0032). A stated floor also freezes the newest Docker job below 3 GiB free, \
             thawing above 6 GiB. 0 = off. At most the disk floor above, so reclamation \
             starts first. Blank = unstated: no gate, warned at every boot; saving the form \
             writes 0.",
        ),
        ..f(
            "host.disk_admission_floor_gib",
            "Admission disk floor",
            "int",
        )
    },
    FieldSpec {
        unit: Some("GiB"),
        min: Some(0.0),
        optional: true,
        unstated_saves: Some(0.0),
        hint: Some(
            "RAM the Docker VM and CI must leave to this machine (ADR-0021) — about half on a \
             workstation used alongside CI. CI is planned within the rest, faber warns while the \
             Docker VM is larger than the rest (the VM never gives memory back), and nothing new \
             starts, macOS VM jobs included, while the machine reports memory pressure or its swap \
             grows by 1 GiB in 5 minutes (ADR-0032). Gate 1 then also holds back the 24 h peak of \
             what the VM's other tenants use, when that is more than the reserve. 0 = no share. Blank = unstated: as before, \
             warned at every boot; saving the form writes 0. Must be less than the machine's RAM.",
        ),
        ..f("host.keep_memory_gib", "Machine keeps (memory)", "int")
    },
    FieldSpec {
        min: Some(0.0),
        optional: true,
        unstated_saves: Some(0.0),
        hint: Some(
            "Cores CI must leave idle for this machine (ADR-0021). The CPU budget is built on \
             the smaller of the Docker VM's cores and the machine's cores less these. 0 = none. \
             Blank = unstated: as before, warned at every boot; saving the form writes 0. Must \
             be less than the machine's cores.",
        ),
        ..f("host.keep_cpus", "Machine keeps (cores)", "int")
    },
    FieldSpec {
        optional: true,
        hint: Some(
            "An IPv4 block faber carves each job's private network from, e.g. 10.63.0.0/16. \
             Empty = let Docker allocate from its default pools, which the WHOLE machine \
             shares — 31 subnets total, so a compose-heavy host can starve CI with \
             'all predefined address pools have been fully subnetted'. Pick a private range \
             nothing here or on your VPNs already routes.",
        ),
        ..f("host.network_pool", "Job network pool", "text")
    },
    FieldSpec {
        optional: true,
        hint: Some(
            "The name that ends every runner this host mints (faber-<job>-<ts>-<name>) \
             and its faber-host-<name> label, so a shared pool says which host ran a job. \
             A lowercase letter, then up to 15 lowercase letters or digits. Empty = a short \
             hash of the hostname, never the hostname itself.",
        ),
        ..f("host.instance", "Instance name", "text")
    },
    FieldSpec {
        // No -> yes or unstated widens and the daemon refuses it without
        // sudo, but the field stays editable: a `sudo` field round-trips as
        // loaded, and this one must state itself on the first save (#156).
        optional: true,
        unstated_saves: Some(1.0),
        options: &["not stated — pulls them, warned", "yes", "no"],
        hint: Some(
            "Whether faber may pull an absent profile image whose name has no registry \
             host (owner/x, ubuntu:24.04). Runners start only from a local image, so this is \
             the only pull. owner/x comes from Docker Hub's owner namespace: if it was a \
             local-only tag that went missing, the pull fetches whatever that namespace holds. \
             No = never pull one: it is missing at once. ghcr.io/… and digest-pinned names are \
             always pulled. Blank = unstated: pulled, warned at every boot; saving the form \
             writes yes.",
        ),
        ..f(
            "host.pull_unqualified_images",
            "Pull unqualified images",
            "bool",
        )
    },
    FieldSpec {
        sudo: Some("sudo faber config set host.dind_requires_label false"),
        narrow: narrow("Turn on", "true"),
        optional: true,
        unstated_saves: Some(0.0),
        options: &["not stated — every job gets one, warned", "yes", "no"],
        hint: Some(
            "ADR-0026. Give a Docker daemon only to jobs whose runs-on includes dind, on a \
             profile that grants one; other jobs get none, and a job asking for dind on a \
             profile with dind = false is refused. No = every job on a granting profile gets a \
             daemon, as before. Add dind to the runs-on of every workflow that runs docker \
             before saying yes. Blank = unstated: as no, warned at every boot; saving the form \
             writes no. The cut sets yes.",
        ),
        ..f(
            "host.dind_requires_label",
            "Docker daemon only for dind jobs",
            "bool",
        )
    },
    FieldSpec {
        min: Some(8.0),
        hint: Some(
            "Prefix length of each job's subnet inside the pool. 24 gives 254 addresses per \
             job (a runner and a dind need two) and turns a /16 pool into 256 concurrent \
             slots. Ignored when no pool is set.",
        ),
        ..f("host.network_subnet_size", "Job subnet size", "int")
    },
    FieldSpec {
        unit: Some("days"),
        min: Some(0.0),
        optional: true,
        hint: Some(
            "ADR-0024. Delete a cache volume's top-level entries that nothing has read, \
             written or changed for this many days — whole entries only, re-checked just \
             before deletion. Blank or 0 = no age limit. With this and the size limit both \
             blank, faber decides: nothing on a healthy disk, the least recently used \
             entries under disk pressure (ADR-0055). At least 2: the Docker VM's disk is mounted relatime, so a read refreshes an \
             entry's age at most once a day.",
        ),
        ..f("host.cache_max_age_days", "Cache max age", "int")
    },
    FieldSpec {
        unit: Some("GiB"),
        min: Some(0.0),
        optional: true,
        hint: Some(
            "ADR-0055. A cache volume holding more than this loses its least recently used \
             top-level entries, oldest first, until it fits. Blank or 0 = no size limit. \
             An entry used in the last 2 hours is never deleted for size. If either this or \
             the max age is set, only the limits that are set apply; with both blank faber \
             frees cache only under disk pressure.",
        ),
        ..f("host.cache_max_gib", "Cache max size", "int")
    },
    FieldSpec {
        options: &["off", "while_busy", "always"],
        hint: Some(
            "Hold off idle sleep; a sleeping host loses every job it is running. \
             off = faber leaves sleep to pmset. while_busy = awake while any job's runner is up, \
             asleep-able 3 minutes after the last one ends. always = awake for as long as the \
             daemon runs, jobs or not. Idle sleep only: a closed laptop lid still sleeps. \
             macOS only; elsewhere it does nothing and boot warns. Needs a restart.",
        ),
        ..f("host.prevent_sleep", "Prevent sleep", "enum")
    },
];

const LINUX_VM_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        min: Some(1.0),
        optional: true,
        hint: Some("vCPUs for the Linux VM that runs Docker. Blank = 6."),
        ..f("linux_vm.cpus", "Linux VM vCPUs", "int")
    },
    FieldSpec {
        unit: Some("MiB"),
        min: Some(1024.0),
        optional: true,
        hint: Some(
            "Memory for the Linux VM that runs Docker. It keeps what it takes while it runs, \
             so this comes out of what the machine share leaves CI, beside two macOS VMs. \
             Blank = 16384.",
        ),
        ..f("linux_vm.memory_mib", "Linux VM memory", "int")
    },
    FieldSpec {
        unit: Some("GiB"),
        min: Some(64.0),
        optional: true,
        hint: Some(
            "The size the Linux VM's disk may grow to, 32 GiB at a time, when cleaning cannot \
             keep room for jobs. It never shrinks: the Mac keeps losing what it grows into \
             until the VM is rebuilt. 64 turns growth off. Blank = 128.",
        ),
        ..f("linux_vm.disk_max_gib", "Linux VM disk ceiling", "int")
    },
];

const RETRY_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        hint: Some("Re-run jobs faber's own runner killed or lost. Never a genuine step failure."),
        ..f("retry.enabled", "Enabled", "bool")
    },
    FieldSpec {
        min: Some(2.0),
        hint: Some("Highest run_attempt faber will create. 2 = one automatic retry."),
        ..f("retry.max_attempts", "Max attempts", "int")
    },
    FieldSpec {
        unit: Some("s"),
        min: Some(1.0),
        hint: Some("Only re-run runs updated within this window."),
        ..f("retry.lookback_secs", "Lookback", "int")
    },
];

/// `[registry_auth]` is held ([`crate::docker::RegistryAuth`]): every field is
/// read-only and offers no `sudo` command, since validation refuses the section
/// on. Only turning it off is offered, which a hand-edited config may need before
/// any other save is accepted.
const REGISTRY_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        held: true,
        narrow: narrow("Turn off", "false"),
        hint: Some(
            "Held: a credential mounted into a job can be read by the job's code. Returns when faber pulls images for the job; until then pulls are anonymous.",
        ),
        ..f("registry_auth.enabled", "Enabled", "bool")
    },
    FieldSpec {
        held: true,
        hint: Some("The registry as keyed in a docker config.json."),
        ..f("registry_auth.server", "Server", "text")
    },
    FieldSpec {
        held: true,
        optional: true,
        ..f("registry_auth.username", "Username", "text")
    },
    FieldSpec {
        held: true,
        hint: Some(
            "Name of the environment variable that would hold the secret; must start with FABER_REGISTRY_.",
        ),
        ..f("registry_auth.password_env", "Password variable", "text")
    },
];

const UPDATE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        sudo: Some("sudo faber config set update.enabled true"),
        narrow: narrow("Turn off", "false"),
        live: true,
        hint: Some(
            "Let this host install releases on its own. The machine releases are cut on leaves this off — it is already at the version it just published.",
        ),
        ..f("update.enabled", "Enabled", "bool")
    },
    FieldSpec {
        sudo: Some("sudo faber config set update.repo <owner/repo>"),
        live: true,
        hint: Some("owner/repo the releases are read from."),
        ..f("update.repo", "Release repo", "text")
    },
    FieldSpec {
        live: true,
        unit: Some("s"),
        min: Some(60.0),
        hint: Some(
            "Between release checks. Conditional GETs, so this is cheap — but noticing a release sooner than the host can go idle to install it buys nothing.",
        ),
        ..f("update.poll_interval_secs", "Check interval", "int")
    },
    FieldSpec {
        live: true,
        unit: Some("s"),
        min: Some(0.0),
        hint: Some(
            "How long a ready update waits for a natural idle window before it starts refusing new jobs to get one. 0 waits forever.",
        ),
        ..f("update.escalate_after_secs", "Escalate after", "int")
    },
    FieldSpec {
        live: true,
        unit: Some("s"),
        min: Some(10.0),
        hint: Some(
            "How long a freshly-installed binary must stay up before the update counts as good. Boot twice inside this window and faber rolls back.",
        ),
        ..f("update.probation_secs", "Probation", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set update.maturity_secs <seconds>"),
        narrow: narrow("Raise to 24 h", "86400"),
        live: true,
        unit: Some("s"),
        min: Some(3600.0),
        hint: Some(
            "How long a release waits before it installs: from the later of its publication and this host first seeing it. Minimum 3600 (1 h), default 86400 (24 h). Lowering it is trust-widening (ADR-0028).",
        ),
        ..f("update.maturity_secs", "Maturity wait", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set update.pin <version>"),
        narrow: narrow("Clear pin", "null"),
        live: true,
        optional: true,
        hint: Some("Hold this host at one version — the manual way to keep a machine back while a release proves itself elsewhere. Blank = track the latest."),
        ..f("update.pin", "Pin to version", "text")
    },
];

const PROFILE_FIELDS: &[FieldSpec] = &[
    f("name", "Name", "text"),
    FieldSpec {
        sudo: Some("sudo faber config set profile.<profile>.labels <labels>"),
        hint: Some("The job's runs-on set, comma-separated. Lead with `faber`."),
        ..f("labels", "Labels", "csv")
    },
    FieldSpec {
        // Moving a profile to or from Docker changes what its image means.
        sudo: Some("sudo faber config set profile.<profile>.backend <backend>"),
        options: &["docker", "macos-vm"],
        ..f("backend", "Backend", "enum")
    },
    FieldSpec {
        // Widens on every docker profile (#220): the job, its secrets and a
        // git-mirror refresh run in it. Read-only on every profile, as `labels`
        // is.
        sudo: Some("sudo faber config set profile.<profile>.image <image>"),
        optional: true,
        hint: Some(
            "Required for a docker backend (the runner image) and a macos-vm backend (the \
             golden macOS image's name in the image store).",
        ),
        ..f("image", "Runner image", "text")
    },
    FieldSpec {
        optional: true,
        hint: Some("macos-vm only. vCPUs per VM. Blank = 4."),
        ..f("cpus", "VM vCPUs", "int")
    },
    FieldSpec {
        optional: true,
        hint: Some("macos-vm only. Memory per VM in MiB. Blank = 8192."),
        ..f("memory_mib", "VM memory (MiB)", "int")
    },
    FieldSpec {
        sudo: Some("sudo faber config set profile.<profile>.dind true"),
        narrow: narrow("Revoke", "false"),
        optional: true,
        options: &["not stated — grants it", "yes", "no"],
        hint: Some(
            "Gives the job a --privileged Docker daemon on its own network. That is host \
             root: say no unless the job actually runs `docker`. On Linux this has no account \
             wall around it yet (ADR-0025): granting it, or leaving it unstated, is rejected \
             right here and by `faber check-config` — a daemon already running on an older \
             config only warns about it. A later, release-noted version will refuse it \
             everywhere.",
        ),
        ..f("dind", "Docker access", "bool")
    },
    FieldSpec {
        sudo: Some("sudo faber config set profile.<profile>.dind_image <image>"),
        optional: true,
        hint: Some("Blank = docker:dind. Unread without Docker access."),
        ..f("dind_image", "dind image", "text")
    },
    FieldSpec {
        // Turning it on widens (#238): a pull-request job can seed what a later
        // push job, which has secrets, reads.
        sudo: Some("sudo faber config set profile.<profile>.cache_volume true"),
        narrow: narrow("Turn off", "false"),
        hint: Some("Mount a per-repo build cache — the only state that crosses the job boundary."),
        ..f("cache_volume", "Cache volume", "bool")
    },
    FieldSpec {
        // Turning it on widens: the refresh runs with faber's own credential.
        sudo: Some("sudo faber config set profile.<profile>.git_mirror true"),
        narrow: narrow("Turn off", "false"),
        hint: Some(
            "Docker only. Keep a git mirror of each watched repo that only faber writes, mounted \
             read-only into jobs. The workflow needs one step and `clean: false` (ADR-0035).",
        ),
        ..f("git_mirror", "Git mirror", "bool")
    },
    FieldSpec {
        sudo: Some("sudo faber config set profile.<profile>.mirror true"),
        narrow: narrow("Turn off", "false"),
        hint: Some(
            "Docker only. Point jobs at the pull-through mirror while it is healthy: it joins \
             the job's network as faber-mirror and the job gets $FABER_MIRROR. Needs a [mirror] \
             section in config.toml (docs/mirrors.md).",
        ),
        ..f("mirror", "Pull-through mirror", "bool")
    },
    FieldSpec {
        sudo: Some("sudo faber config set profile.<profile>.drop_grants <grants>"),
        narrows: Some("add"),
        item: Some(GRANT_ITEM),
        optional: true,
        hint: Some(
            "Grants this profile's jobs must not have, comma-separated, each ADDRESS/PREFIX \
             PORT/PROTO (10.0.0.5/32 5432/tcp). It only narrows what root granted with `sudo \
             faber grant add`; nothing here can open anything. Applied at the next `sudo faber \
             grant` change, or at once with `sudo faber grant apply` (ADR-0026).",
        ),
        ..f("drop_grants", "Dropped grants", "csv")
    },
    FieldSpec {
        // An added or changed entry widens, on every backend (#238): runner-level
        // variables act around the job's steps, and the job cannot override them.
        // A table is no `config set` value, so the command is `config apply`.
        sudo: Some("sudo faber config apply <edited config.toml>"),
        narrow: narrow("Clear", "{}"),
        narrows: Some("remove"),
        hint: Some(
            "NAME=value entries passed into the runner, ahead of every step of its jobs \
             (HTTPS_PROXY, ACTIONS_RUNNER_HOOK_*, LD_PRELOAD act before the job can). \
             RUNNER_JITCONFIG and ACTIONS_RUNNER_HOOK_JOB_STARTED are faber's own and are \
             refused (ADR-0059).",
        ),
        ..f("env", "Environment", "map")
    },
];

const SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        title: "Daemon",
        hint: Some("Discovery and the reconcile loop."),
        fields: DAEMON_FIELDS,
    },
    SectionSpec {
        title: "Trust",
        hint: Some("The boundary this host enforces, behind runs-on routing. It narrows only: faber never provisions for a fork PR, or for an event outside its own list, whatever these say."),
        fields: TRUST_FIELDS,
    },
    SectionSpec {
        title: "Autolimiter",
        hint: Some("Memory-budget admission. Concurrency is emergent from the budget, not a knob."),
        fields: AUTOLIMIT_FIELDS,
    },
    SectionSpec {
        title: "Runner-image drift",
        hint: Some("ADR-0001. A watched repo may narrow this with its own .faber/config.toml."),
        fields: DRIFT_FIELDS,
    },
    SectionSpec {
        title: "Runner images",
        hint: Some("ADR-0008. Off by default: with it off the image is a static property of the profile, which is what faber always did."),
        fields: RUNNER_IMAGE_FIELDS,
    },
    SectionSpec {
        title: "Auto-retry",
        hint: Some("ADR-0003. Off by default: enabling it grants faber the outward-facing action of re-running a workflow."),
        fields: RETRY_FIELDS,
    },
    SectionSpec {
        title: "Docker host",
        hint: Some("ADR-0007. faber is the Docker manager on this box, so keeping the daemon alive and its disk clear is faber's job, not the operator's."),
        fields: HOST_FIELDS,
    },
    SectionSpec {
        title: "Linux VM",
        hint: Some("ADR-0026. On a set-up macOS host faber's Docker runs in a Linux VM of its own, not in Docker Desktop. Applies on restart."),
        fields: LINUX_VM_FIELDS,
    },
    SectionSpec {
        title: "Updates",
        hint: Some("ADR-0006. Off by default: installing faber must not also mean letting it replace its own binary and restart itself."),
        fields: UPDATE_FIELDS,
    },
    SectionSpec {
        title: "Registry auth",
        hint: Some("Held. Credentials for the images a job pulls would be readable by the job's code, so faber mounts none and pulls anonymously. The section returns when faber pulls images for the job."),
        fields: REGISTRY_FIELDS,
    },
];

/// The form both settings UIs render.
pub const FORM_SCHEMA: FormSchema = FormSchema {
    sections: SECTIONS,
    profile_fields: PROFILE_FIELDS,
};

/// What the daemon knows about its own config file.
pub struct SettingsStore {
    /// Where the config lives — the `--config` path, or the default app-support
    /// location. Saving creates it if it isn't there yet.
    path: PathBuf,
    /// The config this process is actually running, captured at startup.
    boot: FileConfig,
    /// Whether that snapshot came from a file. `false` = the flags-only path, so
    /// the UI can say that saving will create a file which then *supersedes* the
    /// CLI flags.
    boot_from_file: bool,
}

/// The config as it is on disk right now, plus how that read went.
pub struct CurrentConfig {
    pub config: FileConfig,
    /// Whether a config file exists at [`SettingsStore::path`].
    pub exists: bool,
    /// Set when the file exists but doesn't parse — the returned `config` is then
    /// the running one, not the file's, and saving will overwrite the broken file.
    pub error: Option<String>,
}

impl SettingsStore {
    pub fn new(path: PathBuf, mut boot: FileConfig, boot_from_file: bool) -> Self {
        // The form's copy of the boot config speaks the new sleep key only, so a
        // host on the old bool is not offered a spurious "restart to apply".
        boot.host.migrate_sleep_key();
        Self {
            path,
            boot,
            boot_from_file,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn boot_from_file(&self) -> bool {
        self.boot_from_file
    }

    /// The running config — what this process booted with, whatever the file says
    /// now.
    pub fn boot(&self) -> &FileConfig {
        &self.boot
    }

    /// Read the config file. A missing file yields the running config with
    /// `exists: false` (the settings page then shows what faber is doing, and
    /// saving writes the first file); an unparseable one yields the running
    /// config plus the parse error.
    pub fn current(&self) -> CurrentConfig {
        self.current_and_text().0
    }

    /// [`Self::current`] and [`Self::read_text`] from one read of the file, so
    /// the config a save judges and the bytes its base digest is taken of are
    /// the same file, not two reads a write could land between.
    pub fn current_and_text(&self) -> (CurrentConfig, anyhow::Result<String>) {
        let broken = |error: String| CurrentConfig {
            config: self.boot.clone(),
            exists: true,
            error: Some(error),
        };
        let text = match std::fs::read(&self.path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    let why = format!("{} is not UTF-8", self.path.display());
                    return (broken(why.clone()), Err(anyhow::anyhow!(why)));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    CurrentConfig {
                        config: self.boot.clone(),
                        exists: false,
                        error: None,
                    },
                    Ok(String::new()),
                );
            }
            Err(e) => {
                let why = e.to_string();
                let err = anyhow::Error::new(e).context(format!("reading {}", self.path.display()));
                return (broken(why), Err(err));
            }
        };
        let current = match toml::from_str::<FileConfig>(&text) {
            Ok(mut config) => {
                config.host.migrate_sleep_key();
                // As `FileConfig::load_from_str` does: a `[mirror.lfs]` with
                // no `[mirror]` of its own is refused at the save.
                if let Some(m) = config.mirror.as_mut() {
                    m.implicit = crate::config::mirror_table_is_implicit(&text);
                }
                CurrentConfig {
                    config,
                    exists: true,
                    error: None,
                }
            }
            Err(e) => broken(e.to_string()),
        };
        (current, Ok(text))
    }

    /// Validate and write `cfg` to [`Self::path`].
    pub fn save(&self, cfg: &FileConfig) -> anyhow::Result<()> {
        cfg.save_to(&self.path)
    }

    /// Validate `cfg` and write `text`, which parses to it, verbatim: the raw
    /// editor's save ([`FileConfig::save_text_to`]).
    pub fn save_text(&self, cfg: &FileConfig, text: &str) -> anyhow::Result<()> {
        cfg.save_text_to(&self.path, text)
    }

    /// The config file as text, for the raw editor: empty when there is no
    /// file yet. A file that is not UTF-8 is an error, not a lossy copy a save
    /// would then write back.
    pub fn read_text(&self) -> anyhow::Result<String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("{} is not UTF-8", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("reading {}", self.path.display())))
            }
        }
    }

    /// Which config keys differ from the ones this daemon booted with — i.e. what
    /// a restart would change. The live autolimiter knobs are excluded: they are
    /// applied to the running limiter as part of the same save.
    pub fn pending_restart(&self, cfg: &FileConfig) -> Vec<String> {
        pending_restart(&self.boot, cfg)
    }
}

/// The changed-key diff, as config-file key paths (`topic`, `autolimit.window`,
/// `profile`, …). Compared through JSON so every nested shape is covered without
/// each type having to derive `PartialEq`.
fn pending_restart(boot: &FileConfig, next: &FileConfig) -> Vec<String> {
    let (Ok(b), Ok(n)) = (
        serde_json::to_value(boot).map(sorted),
        serde_json::to_value(next).map(sorted),
    ) else {
        // Serialization of a plain config can't realistically fail; if it somehow
        // did, claim no pending changes rather than inventing a restart prompt.
        return Vec::new();
    };
    let (Value::Object(b), Value::Object(n)) = (b, n) else {
        return Vec::new();
    };

    let mut keys: Vec<&String> = b.keys().chain(n.keys()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut out = Vec::new();
    for key in keys {
        let (bv, nv) = (b.get(key), n.get(key));
        if bv == nv {
            continue;
        }
        match key.as_str() {
            "autolimit" => out.extend(nested_diff("autolimit", bv, nv)),
            "update" => out.extend(nested_diff("update", bv, nv)),
            _ => out.push(key.clone()),
        }
    }
    out
}

/// The sub-keys of one config table that changed *and* need a restart — the live
/// knobs (see [`is_live_key`]) are filtered out.
///
/// A table whose keys are *all* live therefore contributes nothing here, which
/// is the point: changing only `[update]` settings must not put a "restart to
/// apply" banner on the settings page.
fn nested_diff(table: &str, boot: Option<&Value>, next: Option<&Value>) -> Vec<String> {
    let (Some(Value::Object(b)), Some(Value::Object(n))) = (boot, next) else {
        return vec![table.to_string()];
    };
    let mut keys: Vec<&String> = b.keys().chain(n.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .map(|k| format!("{table}.{k}"))
        .filter(|path| !is_live_key(path))
        .filter(|path| {
            let k = path.rsplit('.').next().unwrap_or(path);
            b.get(k) != n.get(k)
        })
        .collect()
}

/// Normalize a JSON value so equality is about content, not key order or how a
/// number was spelled (`1.0` from TOML vs `1` from a JSON form post).
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, sorted(v))).collect()),
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        Value::Number(n) => n
            .as_f64()
            .map(|f| Value::String(format!("{f:?}")))
            .unwrap_or(Value::Number(n)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendKind, FileProfile};

    fn cfg() -> FileConfig {
        FileConfig {
            profiles: vec![FileProfile {
                name: "n".into(),
                labels: vec!["faber".into(), "docker".into()],
                backend: BackendKind::Docker,
                image: Some("ghcr.io/owner/my-runner:latest".into()),
                cpus: None,
                memory_mib: None,
                dind: Some(true),
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

    #[test]
    fn an_unchanged_config_needs_no_restart() {
        assert!(pending_restart(&cfg(), &cfg()).is_empty());
    }

    /// A config with every optional field populated, so a schema key can't pass
    /// the sync check below merely by being absent from a default config.
    fn fully_populated() -> FileConfig {
        let mut c = cfg();
        c.runner_timeout_secs = Some(10800);
        c.autolimit.hard_ceiling = crate::config::HardCeiling::Fixed(5);
        c.autolimit.cpu = crate::config::CpuMode::Auto;
        c.autolimit.reserve = crate::config::ReserveSetting::P75;
        c.autolimit.priority_repos = vec!["acme/tooling".into()];
        c.profiles[0].dind_image = Some("docker:dind".into());
        c.profiles[0].cpus = Some(4);
        c.profiles[0].memory_mib = Some(8192);
        c.profiles[0].env.insert("K".into(), "v".into());
        // `git_mirror` is skipped when false (ADR-0035 §8).
        c.profiles[0].git_mirror = true;
        // `mirror` likewise, and `[mirror]` is skipped when absent.
        c.profiles[0].mirror = true;
        c.mirror = Some(crate::config::MirrorFile::default());
        // `drop_grants` is skipped when empty (ADR-0026).
        c.profiles[0].drop_grants = vec!["10.0.0.5/32 5432/tcp".into()];
        // `update.pin` is skipped when None, so the schema-vs-config tests would
        // never see it unless it is set here.
        c.update.pin = Some("9.9.9".into());
        c.runner_image.dockerfile = Some("/etc/faber/variant.Dockerfile".into());
        c.host.disk_admission_floor_gib = Some(10);
        c.host.keep_memory_gib = Some(16);
        c.host.keep_cpus = Some(2);
        c.host.cache_max_age_days = Some(14);
        c.host.cache_max_gib = Some(20);
        c.host.instance = "builder".into();
        c.host.pull_unqualified_images = Some(false);
        c.host.dind_requires_label = Some(true);
        c.host.prevent_sleep = Some(crate::config::SleepPolicy::Always);
        c.linux_vm.cpus = Some(6);
        c.linux_vm.memory_mib = Some(16384);
        c.linux_vm.disk_max_gib = Some(128);
        c
    }

    fn walk<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
        path.split('.').try_fold(v, |acc, seg| acc.get(seg))
    }

    #[test]
    fn the_cache_size_limit_is_an_optional_gib_field() {
        let field = FORM_SCHEMA
            .sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|f| f.key == "host.cache_max_gib")
            .expect("host.cache_max_gib is in the schema");
        assert_eq!(field.label, "Cache max size");
        assert_eq!(field.unit, Some("GiB"));
        assert!(field.optional);
    }

    #[test]
    fn every_schema_field_names_a_real_config_key() {
        // The whole point of serving the schema is that the two front-ends stop
        // hard-coding key lists. That only holds if the schema itself can't drift
        // from `FileConfig` — a renamed field has to fail here, loudly.
        let cfg = serde_json::to_value(fully_populated()).unwrap();
        for section in FORM_SCHEMA.sections {
            for field in section.fields {
                assert!(
                    walk(&cfg, field.key).is_some(),
                    "schema field {:?} has no matching config key",
                    field.key
                );
            }
        }
        let profile = &cfg["profile"][0];
        for field in FORM_SCHEMA.profile_fields {
            assert!(
                walk(profile, field.key).is_some(),
                "schema profile field {:?} has no matching key",
                field.key
            );
        }
    }

    #[test]
    fn every_editable_config_key_is_in_the_schema() {
        // The other direction: a config key with no field is a key the settings
        // page silently can't reach, which is exactly what this feature is for.
        let cfg = serde_json::to_value(fully_populated()).unwrap();
        let known: Vec<&str> = FORM_SCHEMA
            .sections
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.key))
            .collect();
        // Keys still parsed but ignored since ADR-0026 (faber warns when they
        // are set, see `config::recreatable_ignored_warning` and
        // `HostPolicy::ignored_keys_warning`). They stay out of the form on
        // purpose: offering a control that does nothing would mislead.
        const PARSED_BUT_IGNORED: &[(&str, &str)] = &[
            (
                "host.recreatable_projects",
                "Docker Desktop route removed, ADR-0026",
            ),
            (
                "host.recreatable_volumes",
                "Docker Desktop route removed, ADR-0026",
            ),
            ("host.revive_unable_to_start_secs", "ignored since ADR-0026"),
        ];
        // Sections whose presence is their switch, edited in config.toml by hand
        // (docs/mirrors.md). A form field for one of their keys would create the
        // section, and so turn the feature on, the first time any form is saved
        // with that field blank. A save keeps them as they are: the form starts
        // from the whole config.
        const HAND_EDITED_SECTIONS: &[&str] = &["mirror"];
        for (key, value) in cfg.as_object().unwrap() {
            // Profiles are edited through their own repeated form, not a field.
            if key == "profile" {
                continue;
            }
            if HAND_EDITED_SECTIONS.contains(&key.as_str()) {
                assert!(
                    !known.iter().any(|k| k.starts_with(&format!("{key}."))),
                    "[{key}] is hand-edited, so it must not be in the form"
                );
                continue;
            }
            match value {
                // A table's leaves are the editable keys, not the table itself.
                Value::Object(inner) => {
                    for sub in inner.keys() {
                        let path = format!("{key}.{sub}");
                        if PARSED_BUT_IGNORED.iter().any(|(k, _)| *k == path) {
                            assert!(
                                !known.contains(&path.as_str()),
                                "{path} is ignored, so it must not be in the form"
                            );
                            continue;
                        }
                        assert!(
                            known.contains(&path.as_str()),
                            "config key {path} is unreachable from the settings page"
                        );
                    }
                }
                _ => assert!(
                    known.contains(&key.as_str()),
                    "config key {key} is unreachable from the settings page"
                ),
            }
        }
        let profile_keys: Vec<&str> = FORM_SCHEMA.profile_fields.iter().map(|f| f.key).collect();
        for key in cfg["profile"][0].as_object().unwrap().keys() {
            assert!(
                profile_keys.contains(&key.as_str()),
                "profile key {key} is unreachable from the settings page"
            );
        }
    }

    #[test]
    fn the_live_flag_matches_what_the_daemon_can_actually_apply() {
        for section in FORM_SCHEMA.sections {
            for field in section.fields {
                assert_eq!(
                    field.live,
                    is_live_key(field.key),
                    "field {:?} claims live = {}",
                    field.key,
                    field.live
                );
            }
        }
    }

    #[test]
    fn update_knobs_do_not_ask_for_a_restart() {
        // Every `[update]` field is live, so an autoupdate-only edit must leave
        // the settings page with nothing to warn about. If this fails, either a
        // new key was added without wiring it into `UpdateHandle::set_policy`,
        // or `is_live_key` stopped covering the table.
        let mut next = cfg();
        next.update.enabled = true;
        next.update.poll_interval_secs += 600;
        next.update.pin = Some("1.2.3".into());
        assert!(pending_restart(&cfg(), &next).is_empty());
    }

    #[test]
    fn live_autolimit_knobs_do_not_ask_for_a_restart() {
        // These are pushed straight into the running limiter by the same save.
        let mut next = cfg();
        next.autolimit.reserve_mib += 1024;
        next.autolimit.headroom_mib += 256;
        next.autolimit.margin += 0.1;
        next.autolimit.native_concurrency += 1;
        next.autolimit.hard_ceiling = crate::config::HardCeiling::Fixed(4);
        assert!(pending_restart(&cfg(), &next).is_empty());
    }

    #[test]
    fn the_reserve_mode_is_an_optional_enum_applied_on_restart() {
        // ADR-0033 §2: blank reads "unstated" and a first save writes "max", the
        // last option, explicitly; a change waits for a restart, like `cpu`.
        let field = FORM_SCHEMA
            .sections
            .iter()
            .flat_map(|s| s.fields)
            .find(|f| f.key == "autolimit.reserve")
            .expect("reachable from the settings page");
        assert_eq!(
            (field.kind, field.optional, field.live),
            ("enum", true, false)
        );
        assert_eq!(field.options, ["p75", "max"]);
        // dind denied on both sides: `cfg()`'s granted dind would trip the Linux
        // dind wall in `validate()` (ADR-0025 §8), which this test is not about.
        let mut baseline = cfg();
        baseline.profiles[0].dind = Some(false);
        let mut next = baseline.clone();
        next.autolimit.reserve = crate::config::ReserveSetting::P75;
        assert!(next.validate().is_empty(), "{:?}", next.validate());
        assert_eq!(pending_restart(&baseline, &next), vec!["autolimit.reserve"]);
        // The save refuses a value the release does not know.
        next.autolimit.reserve = crate::config::ReserveSetting::Unknown("p50".into());
        assert_eq!(next.validate().len(), 1);
    }

    #[test]
    fn startup_only_autolimit_keys_do() {
        let mut next = cfg();
        next.autolimit.window += 5;
        next.autolimit.probe_image = "alpine".into();
        assert_eq!(
            pending_restart(&cfg(), &next),
            vec!["autolimit.probe_image", "autolimit.window"]
        );
    }

    #[test]
    fn scalar_and_nested_changes_are_reported_by_key() {
        let mut next = cfg();
        next.topic = "other".into();
        next.retry.enabled = true;
        next.profiles[0].image = Some("ghcr.io/owner/other:latest".into());
        let pending = pending_restart(&cfg(), &next);
        assert_eq!(pending, vec!["profile", "retry", "topic"]);
    }

    #[test]
    fn a_float_written_as_an_integer_is_not_a_change() {
        // A JSON form post sends `margin: 1` where TOML parsed `1.0`; that is the
        // same number, and must not light up the restart banner.
        let mut boot = cfg();
        boot.autolimit.escalation_frac = 1.0;
        let next = serde_json::from_str::<FileConfig>(
            &serde_json::to_string(&boot)
                .unwrap()
                .replace("\"escalation_frac\":1.0", "\"escalation_frac\":1"),
        )
        .unwrap();
        assert!(pending_restart(&boot, &next).is_empty());
    }

    /// #238: `cache_volume` and `env` widen, so both are read-only with the
    /// command that changes them, and each offers its one narrowing. The
    /// narrowing value is one the config takes.
    #[test]
    fn cache_volume_and_env_need_sudo_and_narrow_in_one_click() {
        let field = |key: &str| PROFILE_FIELDS.iter().find(|f| f.key == key).unwrap();
        let cache = field("cache_volume");
        assert_eq!(
            cache.sudo,
            Some("sudo faber config set profile.<profile>.cache_volume true")
        );
        let n = cache.narrow.as_ref().unwrap();
        assert_eq!((n.label, n.value), ("Turn off", "false"));
        let env = field("env");
        // A table is no `config set` value (`config_cli::parse_value`).
        assert_eq!(
            env.sudo,
            Some("sudo faber config apply <edited config.toml>")
        );
        let n = env.narrow.as_ref().unwrap();
        assert_eq!((n.label, n.value), ("Clear", "{}"));

        let mut c = cfg();
        c.profiles[0].cache_volume = true;
        c.profiles[0].env.insert("K".into(), "v".into());
        let mut v = serde_json::to_value(&c).unwrap();
        v["profile"][0]["cache_volume"] =
            serde_json::from_str(cache.narrow.as_ref().unwrap().value).unwrap();
        v["profile"][0]["env"] = serde_json::from_str(env.narrow.as_ref().unwrap().value).unwrap();
        let narrowed: FileConfig = serde_json::from_value(v).unwrap();
        assert!(!narrowed.profiles[0].cache_volume);
        assert!(narrowed.profiles[0].env.is_empty());
        assert!(crate::control::trust_widening(&c, &narrowed).is_empty());
    }

    /// `[registry_auth]` is held: every field is read-only and offers no `sudo`
    /// command (validation would refuse what it turned on), and the one thing
    /// offered, turning it off, is a value the daemon takes without `sudo`.
    #[test]
    fn registry_auth_fields_are_held_read_only_with_no_command() {
        assert_eq!(REGISTRY_FIELDS.len(), 4);
        for field in REGISTRY_FIELDS {
            assert!(field.key.starts_with("registry_auth."), "{}", field.key);
            assert!(field.held, "{} is not held", field.key);
            assert_eq!(field.sudo, None, "{} offers a command", field.key);
            assert!(field.narrows.is_none(), "{}", field.key);
            let json = serde_json::to_value(field).unwrap();
            assert_eq!(json["held"], Value::Bool(true), "{}", field.key);
            assert!(json.get("sudo").is_none(), "{}", field.key);
        }
        // Everything else is unchanged: no other field is held.
        for f in SECTIONS.iter().flat_map(|s| s.fields).chain(PROFILE_FIELDS) {
            if !f.key.starts_with("registry_auth.") {
                assert!(!f.held, "{} is held", f.key);
                assert!(serde_json::to_value(f).unwrap().get("held").is_none());
            }
        }
        let enabled = &REGISTRY_FIELDS[0];
        assert_eq!(enabled.key, "registry_auth.enabled");
        assert!(enabled.hint.unwrap().contains("Held"));
        let n = enabled.narrow.as_ref().unwrap();
        assert_eq!((n.label, n.value), ("Turn off", "false"));
        let mut on = cfg();
        on.registry_auth.enabled = true;
        // dind denied: `cfg()`'s granted dind would trip the Linux dind wall in
        // `validate()` (ADR-0025 §8), which this test is not about.
        on.profiles[0].dind = Some(false);
        let mut off = on.clone();
        off.registry_auth.enabled = serde_json::from_str(n.value).unwrap();
        assert!(crate::control::trust_widening(&on, &off).is_empty());
        assert!(off.validate().is_empty(), "{:?}", off.validate());
    }

    /// Every field with `narrows`, from the trust section and a profile.
    fn narrowing_fields() -> Vec<(&'static FieldSpec, bool)> {
        let top = SECTIONS.iter().flat_map(|s| s.fields).map(|f| (f, false));
        let profile = PROFILE_FIELDS.iter().map(|f| (f, true));
        top.chain(profile)
            .filter(|(f, _)| f.narrows.is_some())
            .collect()
    }

    /// The anatomy's list (§ 5, "FieldSpec.narrows"), so a new one is a
    /// decision, not a slip.
    #[test]
    fn the_narrowing_lists_are_the_decided_ones() {
        let got: Vec<(&str, &str)> = narrowing_fields()
            .iter()
            .map(|(f, _)| (f.key, f.narrows.unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                ("gate.allowed_events", "remove"),
                ("trusted_actors", "remove-keep-one"),
                ("denied_actors", "add"),
                ("drop_grants", "add"),
                ("env", "remove"),
            ]
        );
        for (f, _) in narrowing_fields() {
            assert!(f.sudo.is_some(), "{}: narrows only a sudo field", f.key);
            assert!(matches!(f.kind, "lines" | "csv" | "map"), "{}", f.key);
        }
    }

    /// One edit of a list or map: add an entry, or remove `n` of them.
    enum Edit {
        Add,
        Remove(usize),
    }

    /// `base` with `field` edited, through JSON as the form sends it.
    fn edited(base: &FileConfig, field: &FieldSpec, in_profile: bool, edit: &Edit) -> FileConfig {
        let mut v = serde_json::to_value(base).unwrap();
        let mut at = if in_profile {
            &mut v["profile"][0]
        } else {
            &mut v
        };
        for part in field.key.split('.') {
            at = &mut at[part];
        }
        let added = field.item.as_ref().map_or(
            if field.key == "gate.allowed_events" {
                "workflow_dispatch"
            } else {
                "someone-new"
            },
            |i| i.example,
        );
        match (at, edit) {
            (Value::Array(a), Edit::Add) => a.push(Value::from(added)),
            (Value::Array(a), Edit::Remove(n)) => a.truncate(a.len() - n),
            (Value::Object(m), Edit::Add) => {
                m.insert("ADDED".into(), Value::from("1"));
            }
            (Value::Object(m), Edit::Remove(n)) => {
                let keys: Vec<String> = m.keys().take(*n).cloned().collect();
                for k in keys {
                    m.remove(&k);
                }
            }
            (other, _) => panic!("{}: not a list or map: {other}", field.key),
        }
        serde_json::from_value(v).unwrap()
    }

    /// anatomy.md § 2, "Trust lists narrow in place": the form may make the
    /// edit `narrows` names because `control::trust_widening` (what the daemon
    /// refuses without sudo) agrees it narrows, and the other direction is
    /// refused because it widens.
    #[test]
    fn each_narrowing_direction_is_one_the_daemon_takes_without_sudo() {
        let mut base = cfg();
        base.gate.allowed_events = vec!["push".into(), "pull_request".into()];
        base.trusted_actors = vec!["owner".into(), "release-bot".into()];
        base.denied_actors = vec!["someone-else".into()];
        base.profiles[0].drop_grants = vec!["10.0.0.9/32 6379/tcp".into()];
        base.profiles[0]
            .env
            .insert("HTTPS_PROXY".into(), "http://10.0.0.2:3128".into());
        base.profiles[0].env.insert("CI_DEBUG".into(), "1".into());
        let widens = |f: &FieldSpec, p: bool, e: Edit| {
            let next = edited(&base, f, p, &e);
            !crate::control::trust_widening(&base, &next).is_empty()
        };
        for (f, p) in narrowing_fields() {
            match f.narrows.unwrap() {
                "remove" => {
                    assert!(
                        !widens(f, p, Edit::Remove(1)),
                        "{}: removing narrows",
                        f.key
                    );
                    assert!(
                        !widens(f, p, Edit::Remove(2)),
                        "{}: removing all narrows",
                        f.key
                    );
                    assert!(widens(f, p, Edit::Add), "{}: adding widens", f.key);
                }
                "remove-keep-one" => {
                    assert!(
                        !widens(f, p, Edit::Remove(1)),
                        "{}: removing one narrows",
                        f.key
                    );
                    assert!(
                        widens(f, p, Edit::Remove(2)),
                        "{}: removing the last widens",
                        f.key
                    );
                    assert!(widens(f, p, Edit::Add), "{}: adding widens", f.key);
                }
                "add" => {
                    assert!(!widens(f, p, Edit::Add), "{}: adding narrows", f.key);
                    assert!(widens(f, p, Edit::Remove(1)), "{}: removing widens", f.key);
                }
                other => panic!("{}: unknown narrows {other:?}", f.key),
            }
        }
    }

    /// Entries each `item` pattern takes and refuses. The daemon's check
    /// ([`item_ok`]) must agree with the pattern the form runs on every one.
    const ITEM_CASES: &[(&str, &[&str], &[&str])] = &[
        (
            "denied_actors",
            &[
                "someone-else",
                "owner",
                "a",
                "dependabot[bot]",
                "Release-Bot-2",
                "handle_shortcode",
                "a_b",
            ],
            &[
                "some one",
                "-lead",
                "trail-",
                "two--hyphens",
                "a/b",
                "[bot]",
                "x[bot]y",
                "",
            ],
        ),
        (
            "drop_grants",
            &[
                "10.0.0.5/32 5432/tcp",
                "100.64.0.0/10 22/udp",
                "0.0.0.0/0 1/tcp",
            ],
            &[
                "10.0.0.5 5432",
                "10.0.0.5/32",
                "db.lan/32 5432/tcp",
                "10.0.0.5/32 5432/icmp",
                "10.0.0.5/32 5432",
                "10.0.0/24 22/tcp",
                "",
            ],
        ),
        (
            "autolimit.priority_repos",
            &["owner/repo", "owner/demo", "Acme-Co/my.repo_2"],
            &["demo", "owner/", "/repo", "owner/repo/x", "own er/repo", ""],
        ),
    ];

    /// The daemon's check and the pattern the form runs give the same verdict
    /// on every case. The crate has no regex engine, so the pattern side is
    /// [`js_like_match`], a literal reading of each pattern string.
    #[test]
    fn the_item_check_agrees_with_the_documented_pattern() {
        let all: Vec<&FieldSpec> = SECTIONS
            .iter()
            .flat_map(|s| s.fields)
            .chain(PROFILE_FIELDS)
            .filter(|f| f.item.is_some())
            .collect();
        let keys: Vec<&str> = all.iter().map(|f| f.key).collect();
        assert_eq!(
            keys,
            ["denied_actors", "autolimit.priority_repos", "drop_grants"]
        );
        for (key, good, bad) in ITEM_CASES {
            let f = all.iter().find(|f| f.key == *key).unwrap();
            let item = f.item.as_ref().unwrap();
            assert!(item_ok(key, item.example), "{key}: its own example");
            assert!(item.pattern.starts_with('^') && item.pattern.ends_with('$'));
            for g in *good {
                assert!(item_ok(key, g), "{key}: {g:?} is well-formed");
                assert!(
                    js_like_match(item.pattern, g),
                    "{key}: pattern refuses {g:?}"
                );
            }
            for b in *bad {
                assert!(!item_ok(key, b), "{key}: {b:?} is malformed");
                assert!(
                    !js_like_match(item.pattern, b),
                    "{key}: pattern takes {b:?}"
                );
            }
        }
    }

    /// The three item patterns, compiled by hand into the shapes they
    /// describe, character class by character class. Not a regex engine: a
    /// literal reading of each pattern string, so a change to a pattern that
    /// this does not recognise fails the test instead of passing silently.
    fn js_like_match(pattern: &str, s: &str) -> bool {
        let all = |t: &str, ok: fn(u8) -> bool| !t.is_empty() && t.bytes().all(ok);
        let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let slug = |b: u8| b.is_ascii_alphanumeric() || b"_.-".contains(&b);
        let digits = |t: &str, max: usize| all(t, |b| b.is_ascii_digit()) && t.len() <= max;
        match pattern {
            r"^[A-Za-z0-9_]+(-[A-Za-z0-9_]+)*(\[bot\])?$" => {
                let login = s.strip_suffix("[bot]").unwrap_or(s);
                login.split('-').all(|p| all(p, word))
            }
            r"^\d{1,3}(\.\d{1,3}){3}/\d{1,2} \d{1,5}/(tcp|udp)$" => {
                let Some((net, port)) = s.split_once(' ') else {
                    return false;
                };
                let Some((addr, prefix)) = net.split_once('/') else {
                    return false;
                };
                let Some((n, proto)) = port.split_once('/') else {
                    return false;
                };
                let octets: Vec<&str> = addr.split('.').collect();
                octets.len() == 4
                    && octets.iter().all(|o| digits(o, 3))
                    && digits(prefix, 2)
                    && digits(n, 5)
                    && matches!(proto, "tcp" | "udp")
            }
            r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$" => s
                .split_once('/')
                .is_some_and(|(o, r)| all(o, slug) && all(r, slug)),
            other => panic!("a pattern this test cannot read: {other}"),
        }
    }

    /// `check-config` reports what the form refuses, by key, as warnings
    /// ([`item_errors`]); `validate` itself never blocks on one, so a file the
    /// daemon runs on is never refused over an entry already in it.
    #[test]
    fn a_malformed_list_entry_is_reported_by_key_not_refused_by_validate() {
        let mut c = cfg();
        c.profiles[0].dind = Some(false);
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        c.denied_actors = vec!["dependabot[bot]".into(), "some one".into()];
        c.autolimit.priority_repos = vec!["owner/demo".into(), "demo".into(), "own er/x".into()];
        assert!(
            !c.validate().iter().any(|e| e.starts_with("denied_actors")),
            "{:?}",
            c.validate()
        );
        let errs = item_errors(&c);
        assert!(
            errs.iter().any(|e| e.starts_with("denied_actors[1]")),
            "{errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.starts_with("denied_actors[0]")),
            "{errs:?}"
        );
        let repo: Vec<&String> = errs
            .iter()
            .filter(|e| e.contains("priority_repos"))
            .collect();
        assert_eq!(repo.len(), 2, "{errs:?}");
        assert!(
            repo[0].starts_with("autolimit.priority_repos[1]"),
            "{errs:?}"
        );
        assert!(
            repo[1].starts_with("autolimit.priority_repos[2]"),
            "{errs:?}"
        );
    }

    /// The save judges only the entries it adds: one already in the file
    /// never blocks a save, and a new malformed one is refused.
    #[test]
    fn a_save_refuses_only_the_list_entries_it_adds() {
        let mut old = cfg();
        old.denied_actors = vec!["some one".into(), "x y".into()];
        old.autolimit.priority_repos = vec!["own er/x".into()];
        // Narrowing (dropping one), and keeping the rest as they are.
        let mut next = old.clone();
        next.denied_actors.remove(1);
        assert!(new_item_errors(&old, &next).is_empty());
        assert!(new_item_errors(&old, &old).is_empty());
        next.denied_actors.push("new one".into());
        next.autolimit.priority_repos.push("demo".into());
        let errs = new_item_errors(&old, &next);
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].starts_with("denied_actors[1]"), "{errs:?}");
        assert!(
            errs[1].starts_with("autolimit.priority_repos[1]"),
            "{errs:?}"
        );
    }

    #[test]
    fn a_settings_save_keeps_the_profiles_dropped_grants() {
        // Losing this key on a save would widen what a job reaches at the next
        // root render, so the form must carry it: a csv field, before `env`,
        // and the whole save path keeps it.
        let keys: Vec<&str> = PROFILE_FIELDS.iter().map(|f| f.key).collect();
        let at = keys.iter().position(|k| *k == "drop_grants").unwrap();
        assert_eq!(keys[at + 1], "env");
        let field = &PROFILE_FIELDS[at];
        assert_eq!((field.kind, field.optional), ("csv", true));
        assert!(field.hint.unwrap().contains("only narrows"));

        let mut c = cfg();
        c.profiles[0].dind = Some(false);
        c.profiles[0].drop_grants =
            vec!["10.0.0.5/32 5432/tcp".into(), "100.64.0.0/10 22/udp".into()];
        // The form posts JSON; the store writes TOML; the next read is TOML.
        let posted: FileConfig = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "faber-settings-drop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SettingsStore::new(dir.join("config.toml"), c.clone(), true);
        store.save(&posted).unwrap();
        let read = store.current();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read.error.is_none(), "{:?}", read.error);
        assert_eq!(
            read.config.profiles[0].drop_grants,
            c.profiles[0].drop_grants
        );
        // And a malformed one is refused by the save, not written.
        let mut bad = c.clone();
        bad.profiles[0].drop_grants = vec!["db.lan 5432/tcp".into()];
        assert!(bad.validate().iter().any(|e| e.contains("drop_grants")));
    }

    #[test]
    fn the_admission_disk_floor_states_itself_on_the_first_save() {
        // ADR-0024 copies ADR-0013's three states: a blank field reads "unstated",
        // and saving the form writes the stated-off value instead of leaving the
        // key absent. The front ends do the writing; this pins what tells them to.
        let host = SECTIONS.iter().find(|s| s.title == "Docker host").unwrap();
        let keys: Vec<&str> = host.fields.iter().map(|f| f.key).collect();
        let at = keys
            .iter()
            .position(|k| *k == "host.disk_admission_floor_gib")
            .unwrap();
        assert_eq!(
            keys[at - 1],
            "host.disk_floor_gib",
            "beside the floor it may not exceed"
        );
        let field = &host.fields[at];
        assert_eq!(
            (field.kind, field.optional, field.unstated_saves),
            ("int", true, Some(0.0))
        );
        assert_eq!(serde_json::to_value(field).unwrap()["unstated_saves"], 0.0);
        // Every other field keeps an optional int's ordinary "blank = unset",
        // except the machine's share, which states itself the same way (below),
        // and the optional bools that do (#156, ADR-0026).
        let share = [
            "host.keep_memory_gib",
            "host.keep_cpus",
            "host.pull_unqualified_images",
            "host.dind_requires_label",
        ];
        for f in FORM_SCHEMA
            .sections
            .iter()
            .flat_map(|s| s.fields)
            .chain(FORM_SCHEMA.profile_fields)
            .filter(|f| f.key != field.key && !share.contains(&f.key))
        {
            assert!(f.unstated_saves.is_none(), "{}", f.key);
            assert!(serde_json::to_value(f)
                .unwrap()
                .get("unstated_saves")
                .is_none());
        }
        // What that first save writes is a config the save accepts, and a change
        // the running daemon picks up on restart, like the rest of [host].
        //
        // `dind` is denied on both sides of the comparison, not left at `cfg()`'s
        // default `Some(true)`: an Unstated *or* Granted grant would trip the
        // Linux dind-wall policy (ADR-0025 §8; `config::dind_needs_wall`) in
        // `validate()` on the `ubuntu-latest` CI leg only, and this test has
        // nothing to do with dind — only with [host] round-tripping cleanly.
        let mut baseline = cfg();
        baseline.profiles[0].dind = Some(false);
        let mut saved = baseline.clone();
        saved.host.disk_admission_floor_gib = Some(0);
        assert!(saved.validate().is_empty(), "{:?}", saved.validate());
        assert_eq!(pending_restart(&baseline, &saved), vec!["host"]);
    }

    #[test]
    fn the_machine_share_states_itself_on_the_first_save() {
        // ADR-0021 copies the same three states: blank reads "unstated", and the
        // first save writes 0 — no share, the behaviour from before — rather than
        // leaving the keys absent.
        let host = SECTIONS.iter().find(|s| s.title == "Docker host").unwrap();
        for key in ["host.keep_memory_gib", "host.keep_cpus"] {
            let field = host.fields.iter().find(|f| f.key == key).unwrap();
            assert_eq!(
                (field.kind, field.optional, field.unstated_saves, field.live),
                ("int", true, Some(0.0), false),
                "{key}"
            );
        }
        // dind denied on both sides: `cfg()`'s granted dind would trip the Linux
        // dind wall in `validate()` (ADR-0025 §8), which this test is not about.
        let mut baseline = cfg();
        baseline.profiles[0].dind = Some(false);
        let mut saved = baseline.clone();
        saved.host.keep_memory_gib = Some(0);
        saved.host.keep_cpus = Some(0);
        assert!(saved.validate().is_empty(), "{:?}", saved.validate());
        assert_eq!(pending_restart(&baseline, &saved), vec!["host"]);
        // What it writes is read back as stated, and changes nothing.
        let share = saved.host.machine_share();
        assert_eq!(share.as_str(), "declared");
        assert!(!share.yields_to_pressure());
    }

    #[test]
    fn pull_unqualified_images_states_itself_on_the_first_save() {
        // #156: blank keeps the implicit pull and reads "not stated"; the first
        // save writes `true`, the old behaviour, explicitly.
        let host = SECTIONS.iter().find(|s| s.title == "Docker host").unwrap();
        let field = host
            .fields
            .iter()
            .find(|f| f.key == "host.pull_unqualified_images")
            .unwrap();
        assert_eq!(
            (field.kind, field.optional, field.unstated_saves, field.live),
            ("bool", true, Some(1.0), false)
        );
        assert_eq!(field.options.len(), 3, "[unset, true, false] labels");
        // dind denied on both sides: `cfg()`'s granted dind would trip the Linux
        // dind wall in `validate()` (ADR-0025 §8), which this test is not about.
        let mut baseline = cfg();
        baseline.profiles[0].dind = Some(false);
        let mut saved = baseline.clone();
        saved.host.pull_unqualified_images = Some(true);
        assert!(saved.validate().is_empty(), "{:?}", saved.validate());
        assert_eq!(pending_restart(&baseline, &saved), vec!["host"]);
    }

    #[test]
    fn the_dind_label_rule_states_itself_on_the_first_save() {
        // ADR-0026: blank keeps every job on a granting profile its daemon and
        // reads "not stated"; the first save writes `false`, that same behaviour,
        // explicitly — never `true`, which the cut's host sitting sets.
        let host = SECTIONS.iter().find(|s| s.title == "Docker host").unwrap();
        let field = host
            .fields
            .iter()
            .find(|f| f.key == "host.dind_requires_label")
            .unwrap();
        assert_eq!(
            (field.kind, field.optional, field.unstated_saves, field.live),
            ("bool", true, Some(0.0), false)
        );
        assert_eq!(field.options.len(), 3, "[unset, true, false] labels");
        // dind denied on both sides: `cfg()`'s granted dind would trip the Linux
        // dind wall in `validate()` (ADR-0025 §8), which this test is not about.
        let mut baseline = cfg();
        baseline.profiles[0].dind = Some(false);
        let mut saved = baseline.clone();
        saved.host.dind_requires_label = Some(false);
        assert!(saved.validate().is_empty(), "{:?}", saved.validate());
        assert_eq!(pending_restart(&baseline, &saved), vec!["host"]);
        assert_eq!(
            saved.host.dind_label_rule(),
            crate::config::DindLabelRule::Off
        );
    }

    #[test]
    fn the_ignored_concurrency_key_never_shows_up() {
        // It is `skip_serializing`, so a legacy file's value can't be mistaken for
        // a pending change to a key faber no longer honours.
        let mut next = cfg();
        next.concurrency += 7;
        assert!(pending_restart(&cfg(), &next).is_empty());
    }
}
