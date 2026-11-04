//! `faber config` and `faber repo-hold` (issue #78): change the running
//! daemon's config, or clear a repo's minting hold, from a terminal.
//!
//! The daemon decides. These commands send the change over the control socket
//! (never TCP: they write) and print the answer: what widened trust, whether it
//! applied live or waits for a restart, and, when the daemon refused a
//! widening, the `sudo` command that makes it. Root is not required: a routine
//! or narrowing change works as the operator, and only a trust-widening one
//! needs `sudo` ([`crate::control::WIDENING_NEEDS`]).
//!
//! [`edit`] is the one-key edit both sides share: the daemon applies it to the
//! file as it is under its save lock (`POST /api/settings/key`), so the CLI
//! never sends a whole config it read earlier.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::Value;

use crate::config::FileConfig;
use crate::control::{self, Fallback};
use crate::settings::{FieldSpec, FORM_SCHEMA};

/// `faber config …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConfigCommand {
    /// Set one key, e.g. `update.maturity_secs 86400` or
    /// `profile.linux-docker.dind false`. Lists are comma-separated.
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Clear an optional key (it reads as unset) or empty a list.
    Unset { key: String },
    /// Replace the whole config with this TOML file, validated and judged
    /// like a dashboard save. As root, what it widens is shown and confirmed
    /// first.
    Apply {
        file: PathBuf,
        /// Apply a widening change without asking (as root). Without it, and
        /// without a terminal to ask on, a widening change is refused.
        #[arg(long)]
        yes: bool,
    },
}

/// `faber repo-hold …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RepoHoldCommand {
    /// Let faber mint for `owner/repo` again (ADR-0030). Widens trust: run it
    /// with sudo.
    Clear { repo: String },
}

// --- the one-key edit -------------------------------------------------------

/// What a key names: a field of a section, or a field of one profile.
enum Target<'k> {
    Top(&'static FieldSpec),
    Profile {
        name: &'k str,
        field: &'static FieldSpec,
    },
}

fn top_fields() -> impl Iterator<Item = &'static FieldSpec> {
    FORM_SCHEMA.sections.iter().flat_map(|s| s.fields.iter())
}

fn target(key: &str) -> Option<Target<'_>> {
    if let Some(field) = top_fields().find(|f| f.key == key) {
        return Some(Target::Top(field));
    }
    let (name, field) = key.strip_prefix("profile.")?.rsplit_once('.')?;
    let field = FORM_SCHEMA.profile_fields.iter().find(|f| f.key == field)?;
    (!name.is_empty()).then_some(Target::Profile { name, field })
}

/// Every key `edit` takes on `cfg`, for a suggestion.
fn known_keys(cfg: &FileConfig) -> Vec<String> {
    let mut keys: Vec<String> = top_fields().map(|f| f.key.to_string()).collect();
    for p in &cfg.profiles {
        keys.extend(
            FORM_SCHEMA
                .profile_fields
                .iter()
                .map(|f| format!("profile.{}.{}", p.name, f.key)),
        );
    }
    keys
}

/// Up to five keys that look like `key`: they share its last segment, or one
/// contains the other.
fn close_keys(cfg: &FileConfig, key: &str) -> Vec<String> {
    let last = key.rsplit('.').next().unwrap_or(key);
    known_keys(cfg)
        .into_iter()
        .filter(|k| {
            k.rsplit('.').next() == Some(last)
                || (key.len() >= 3 && (k.contains(key) || key.contains(k.as_str())))
        })
        .take(5)
        .collect()
}

/// `value` for `field`, as the JSON the config takes. `None` unsets it.
fn parse_value(key: &str, field: &FieldSpec, value: Option<&str>) -> Result<Value, String> {
    let list = matches!(field.kind, "csv" | "lines");
    if field.kind == "map" {
        return Err(format!(
            "{key} is a table of NAME=value entries: edit it with `faber config apply`"
        ));
    }
    let value = match value {
        Some("") if field.optional && !list => None,
        v => v,
    };
    let Some(v) = value else {
        return if list {
            Ok(Value::Array(Vec::new()))
        } else if field.optional {
            Ok(Value::Null)
        } else {
            Err(format!("{key} cannot be unset: set a value instead"))
        };
    };
    match field.kind {
        "bool" => match v {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("{key} takes true or false, not {v:?}")),
        },
        "int" => v
            .parse::<u64>()
            .map(Value::from)
            .map_err(|_| format!("{key} takes a whole number, not {v:?}")),
        "float" => v
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("{key} takes a number, not {v:?}")),
        "enum" if field.options.contains(&v) => Ok(Value::String(v.to_string())),
        "enum" => Err(format!(
            "{key} takes one of {}, not {v:?}",
            field.options.join(", ")
        )),
        "text" => Ok(Value::String(v.to_string())),
        "csv" | "lines" => Ok(Value::Array(
            v.split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(|e| Value::String(e.to_string()))
                .collect(),
        )),
        "ceiling" => match v {
            "auto" | "off" => Ok(Value::String(v.to_string())),
            n => n
                .parse::<u64>()
                .map(Value::from)
                .map_err(|_| format!("{key} takes auto, off or a whole number, not {v:?}")),
        },
        other => Err(format!("{key} is a {other} field this command cannot set")),
    }
}

/// `cfg` with one key changed: set to `value`, or unset (`None`).
///
/// Keys are the settings form's (`update.pin`, `gate.allowed_events`) and
/// `profile.<name>.<field>` for a profile that exists. The value is read by the
/// field's kind: `true`/`false`, a number, one of an enum's options, text, a
/// comma-separated list (empty: an empty list), `auto`/`off`/a number for the
/// hard ceiling. An empty value unsets an optional field. The result is parsed
/// back as strictly as a config file; it is not validated, which is the save's
/// job.
pub fn edit(cfg: &FileConfig, key: &str, value: Option<&str>) -> Result<FileConfig, String> {
    let Some(target) = target(key) else {
        let close = close_keys(cfg, key);
        return Err(if close.is_empty() {
            format!("{key} is not a config key this command knows")
        } else {
            format!(
                "{key} is not a config key this command knows; did you mean {}?",
                close.join(", ")
            )
        });
    };
    let field = match &target {
        Target::Top(f) | Target::Profile { field: f, .. } => *f,
    };
    let parsed = parse_value(key, field, value)?;
    let mut json = serde_json::to_value(cfg).map_err(|e| format!("reading the config: {e}"))?;
    let unset = parsed.is_null();
    put(&mut json, &target, cfg, parsed)?;
    match serde_json::from_value(json.clone()) {
        Ok(next) => Ok(next),
        // Text that is plain text in the config, not optional text, reads
        // unset as empty.
        Err(_) if unset && field.kind == "text" => {
            put(&mut json, &target, cfg, Value::String(String::new()))?;
            from_json(json, key)
        }
        Err(e) => Err(format!("{key}: {e}")),
    }
}

/// Put `value` where `target` points in `json`, the config as JSON.
fn put(
    json: &mut Value,
    target: &Target<'_>,
    cfg: &FileConfig,
    value: Value,
) -> Result<(), String> {
    match target {
        Target::Top(f) => {
            let mut at = json.as_object_mut().ok_or("the config is not a table")?;
            let mut parts: Vec<&str> = f.key.split('.').collect();
            let last = parts.pop().unwrap_or(f.key);
            for part in parts {
                at = at
                    .entry(part.to_string())
                    .or_insert_with(|| Value::Object(Default::default()))
                    .as_object_mut()
                    .ok_or_else(|| format!("{part} is not a table"))?;
            }
            at.insert(last.to_string(), value);
        }
        Target::Profile { name, field } => {
            let unknown = || {
                let names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
                if names.is_empty() {
                    format!("there is no profile named {name:?}: the config has none")
                } else {
                    format!(
                        "there is no profile named {name:?}; the profiles are {}",
                        names.join(", ")
                    )
                }
            };
            json.get_mut("profile")
                .and_then(Value::as_array_mut)
                .and_then(|ps| {
                    ps.iter_mut()
                        .find(|p| p.get("name").and_then(Value::as_str) == Some(*name))
                })
                .and_then(Value::as_object_mut)
                .ok_or_else(unknown)?
                .insert(field.key.to_string(), value);
        }
    }
    Ok(())
}

fn from_json(json: Value, key: &str) -> Result<FileConfig, String> {
    serde_json::from_value(json).map_err(|e| format!("{key}: {e}"))
}

// --- the commands -------------------------------------------------------------

/// The control socket, and root's binary check when this runs as root: root
/// never executes what faber's accounts can write (AGENTS.md).
async fn prepare() -> anyhow::Result<(PathBuf, bool)> {
    let socket = crate::platform::control_socket()
        .ok_or_else(|| anyhow::anyhow!("no HOME to find the daemon's control socket under"))?;
    let root = crate::standby::euid() == 0;
    if root {
        tokio::task::spawn_blocking(crate::setup::exe::trust_running_binary).await??;
    }
    Ok((socket, root))
}

/// One write to the daemon over its socket, never TCP.
async fn post(socket: &Path, path: &str, body: Value) -> anyhow::Result<(u16, String)> {
    let (status, text) = control::request(
        socket,
        Fallback::Never,
        reqwest::Method::POST,
        path,
        Some(body),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", crate::drain::client_error_message(&e)))?;
    Ok((status.as_u16(), text))
}

/// `faber config set|unset|apply`.
pub async fn run_config(action: ConfigCommand) -> anyhow::Result<()> {
    let (socket, root) = prepare().await?;
    let (path, body, key) = match &action {
        ConfigCommand::Set { key, value } => (
            "/api/settings/key".to_string(),
            serde_json::json!({ "key": key, "value": value }),
            Some(key.as_str()),
        ),
        ConfigCommand::Unset { key } => (
            "/api/settings/key".to_string(),
            serde_json::json!({ "key": key, "value": null }),
            Some(key.as_str()),
        ),
        ConfigCommand::Apply { file, yes } => {
            let shown = crate::term::sanitize(&file.display().to_string()).into_owned();
            let reader =
                std::fs::File::open(file).map_err(|e| anyhow::anyhow!("opening {shown}: {e}"))?;
            // The parse error quotes the file: sanitized, as it may reach
            // root's terminal.
            let cfg = FileConfig::load_from_reader(reader, &format!("from {shown}"))
                .map_err(|e| anyhow::anyhow!("{}", crate::term::sanitize(&format!("{e:#}"))))?;
            let body = apply_body(cfg)?;
            // As root, the save lands only on the config the dry run judged
            // (#221): another save in between is a 409, not a widening root
            // never saw.
            let mut path = "/api/settings".to_string();
            if root {
                let (status, text) = post(&socket, "/api/settings?dry_run=1", body.clone()).await?;
                let DryRun {
                    widened: widenings,
                    base,
                } = dry_run_report(status, &text)?;
                path = format!("/api/settings?base={base}");
                if !widenings.is_empty() {
                    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
                    match confirm_step(*yes, tty) {
                        Confirm::Go => {}
                        Confirm::Refuse => anyhow::bail!(
                            "this config widens trust:\n{}\nno terminal to confirm on: pass --yes",
                            bullets(&widenings)
                        ),
                        Confirm::Ask => {
                            println!("this config widens trust:\n{}", bullets(&widenings));
                            if !ask_yes("apply it? [y/N] ")? {
                                anyhow::bail!("not applied");
                            }
                        }
                    }
                }
            }
            (path, body, None)
        }
    };
    let (status, text) = post(&socket, &path, body).await?;
    match save_report(status, &text, key, root) {
        Ok(lines) => {
            println!("{lines}");
            Ok(())
        }
        Err(why) => anyhow::bail!("{why}"),
    }
}

/// What `apply` does about a widening change it is about to send as root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Send it: `--yes`.
    Go,
    /// Ask on the terminal.
    Ask,
    /// No `--yes` and no terminal: refuse.
    Refuse,
}

/// [`Confirm`] for `--yes` and whether stdin is a terminal.
pub fn confirm_step(yes: bool, tty: bool) -> Confirm {
    match (yes, tty) {
        (true, _) => Confirm::Go,
        (false, true) => Confirm::Ask,
        (false, false) => Confirm::Refuse,
    }
}

fn bullets(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| format!("  - {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ask `prompt` on the terminal: true only for `y` or `yes`.
fn ask_yes(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// What a dry run found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    /// The widening sentences, sanitized.
    pub widened: Vec<String>,
    /// The digest of the config it judged against, to send with the save.
    pub base: String,
}

/// The body `faber config apply` posts for a parsed file. The old
/// `prevent_sleep_while_busy` bool is never serialized, so it is moved to
/// `prevent_sleep` first, as the settings form does; otherwise applying a file
/// that still sets it would turn sleep prevention off.
fn apply_body(mut cfg: FileConfig) -> serde_json::Result<Value> {
    cfg.host.migrate_sleep_key();
    serde_json::to_value(&cfg)
}

/// A dry run's answer: what it found, or why the config cannot be applied at
/// all.
pub fn dry_run_report(status: u16, body: &str) -> anyhow::Result<DryRun> {
    let clean = |s: &str| crate::term::sanitize(s).into_owned();
    let json: Value = serde_json::from_str(body).map_err(|_| {
        anyhow::anyhow!(
            "the daemon answered the dry run with {status}: {}",
            crate::term::sanitize_bounded(body.trim(), 2048)
        )
    })?;
    if status != 200 {
        anyhow::bail!("the daemon answered the dry run with {status}");
    }
    let errors: Vec<String> = json["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(clean)
        .collect();
    if !errors.is_empty() {
        anyhow::bail!("the daemon refused the config:\n{}", bullets(&errors));
    }
    // Hex only, so it goes into the query as it is.
    let base = json["base"]
        .as_str()
        .filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("the daemon's dry run named no base config"))?
        .to_string();
    let widened = json["widened"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["sentence"].as_str())
        .map(clean)
        .collect();
    Ok(DryRun { widened, base })
}

/// What a save's answer means, as lines to print (`Ok`) or the refusal to
/// exit with (`Err`). `key` is the one key changed (`None`: a whole file),
/// `root` whether this ran as root. Every string from the daemon is
/// sanitized: this may print on root's terminal.
pub fn save_report(
    status: u16,
    body: &str,
    key: Option<&str>,
    root: bool,
) -> Result<String, String> {
    let clean = |s: &str| crate::term::sanitize(s).into_owned();
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return Err(format!(
            "the daemon answered {status}: {}",
            crate::term::sanitize_bounded(body.trim(), 2048)
        ));
    };
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(clean).collect())
            .unwrap_or_default()
    };
    let errors = strings(&json["errors"]);
    match status {
        200 => {
            let mut out: Vec<String> = json["widened"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|w| w["sentence"].as_str())
                .map(|s| format!("trust widened: {}", clean(s)))
                .collect();
            let pending = strings(&json["settings"]["pending_restart"]);
            out.push(match key {
                Some(k) if crate::settings::is_live_key(k) => "applied live".to_string(),
                Some(k) => {
                    let waiting: Vec<&String> = pending
                        .iter()
                        .filter(|e| {
                            k == e.as_str()
                                || k.strip_prefix(e.as_str())
                                    .is_some_and(|rest| rest.starts_with('.'))
                        })
                        .collect();
                    if waiting.is_empty() {
                        "saved; faber already runs this value".to_string()
                    } else {
                        format!(
                            "saved; restart faber to apply: {}",
                            waiting
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                }
                None if pending.is_empty() => "saved and applied live".to_string(),
                None => format!("saved; restart faber to apply: {}", pending.join(", ")),
            });
            Ok(out.join("\n"))
        }
        403 => {
            let refused = json["refused"].as_array().cloned().unwrap_or_default();
            let mut out = Vec::new();
            if refused.is_empty() {
                out.extend(errors);
            } else {
                out.push("refused: this change widens trust and needs root".to_string());
                for r in &refused {
                    out.push(format!(
                        "  {}: {}",
                        clean(r["key"].as_str().unwrap_or("?")),
                        clean(r["sentence"].as_str().unwrap_or(""))
                    ));
                    if let Some(cmd) = r["command"].as_str() {
                        out.push(format!("    {}", clean(cmd)));
                    }
                }
            }
            if !root {
                out.push("run it with sudo".to_string());
            }
            Err(out.join("\n"))
        }
        409 => Err(format!(
            "not applied: {}\nrun `sudo faber config apply` again to see what it widens now",
            if errors.is_empty() {
                "the config changed after the dry run".to_string()
            } else {
                errors.join("; ")
            }
        )),
        400 => Err(
            std::iter::once("the daemon refused the config:".to_string())
                .chain(errors.iter().map(|e| format!("  - {e}")))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => Err(format!(
            "the daemon answered {other}: {}",
            if errors.is_empty() {
                crate::term::sanitize_bounded(body.trim(), 2048).into_owned()
            } else {
                errors.join("; ")
            }
        )),
    }
}

/// `faber repo-hold clear <owner/repo>`.
pub async fn run_repo_hold(action: RepoHoldCommand) -> anyhow::Result<()> {
    let RepoHoldCommand::Clear { repo } = action;
    let repo = repo.trim().to_string();
    let (socket, root) = prepare().await?;
    let held_before = held(&socket, &repo).await;
    let (status, text) = post(
        &socket,
        "/api/repo-hold/clear",
        serde_json::json!({ "repo": repo }),
    )
    .await?;
    match hold_report(status, &text, &repo, held_before, root) {
        Ok(line) => {
            println!("{line}");
            Ok(())
        }
        Err(why) => anyhow::bail!("{why}"),
    }
}

/// Whether `/api/state` lists `repo` as held, or `None` when it cannot say.
async fn held(socket: &Path, repo: &str) -> Option<bool> {
    let (status, text) = control::request(
        socket,
        Fallback::Never,
        reqwest::Method::GET,
        "/api/state",
        None,
    )
    .await
    .ok()?;
    if !status.is_success() {
        return None;
    }
    let json: Value = serde_json::from_str(&text).ok()?;
    Some(
        json["repo_holds"]
            .as_array()?
            .iter()
            .any(|h| h["repo"].as_str() == Some(repo)),
    )
}

/// What a repo-hold clear's answer means, like [`save_report`].
pub fn hold_report(
    status: u16,
    body: &str,
    repo: &str,
    held_before: Option<bool>,
    root: bool,
) -> Result<String, String> {
    let shown = crate::term::sanitize(repo).into_owned();
    match status {
        200 => Ok(match held_before {
            Some(false) => format!("{shown} is not held; nothing to clear"),
            _ => format!("minting hold cleared for {shown}"),
        }),
        403 => {
            let mut out = vec![
                crate::term::sanitize_bounded(body.trim(), 2048).into_owned(),
                format!("  {}", crate::term::sanitize(&repo_hold_command(repo))),
            ];
            if !root {
                out.push("run it with sudo".to_string());
            }
            Err(out.join("\n"))
        }
        other => Err(format!(
            "the daemon answered {other}: {}",
            crate::term::sanitize_bounded(body.trim(), 2048)
        )),
    }
}

/// The command that clears `repo`'s hold as root.
fn repo_hold_command(repo: &str) -> String {
    let w = control::repo_hold_cleared(&crate::config::RepoHold {
        repo: repo.to_string(),
        reason: String::new(),
        by: String::new(),
        since_ms: 0,
    });
    control::sudo_command(&w, &FileConfig::default(), &FileConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendKind, FileProfile, HardCeiling};

    #[test]
    fn apply_carries_the_old_sleep_key_as_the_new_one() {
        let old = FileConfig::load_from_str("[host]\nprevent_sleep_while_busy = true\n", "test")
            .expect("parses");
        let body = apply_body(old).expect("serializes");
        assert_eq!(body["host"]["prevent_sleep"], "while_busy");

        let none = FileConfig::load_from_str("", "test").expect("parses");
        assert_eq!(
            apply_body(none).expect("serializes")["host"]["prevent_sleep"],
            "off"
        );
    }

    fn cfg() -> FileConfig {
        FileConfig {
            profiles: vec![FileProfile {
                name: "linux-docker".into(),
                labels: vec!["faber".into(), "docker".into()],
                backend: BackendKind::Docker,
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
            trusted_actors: vec!["owner".into()],
            ..FileConfig::default()
        }
    }

    /// `edit`'s result as JSON, with `key`'s JSON path set back to what `cfg`
    /// had: everything else must be untouched.
    fn json(c: &FileConfig) -> Value {
        serde_json::to_value(c).unwrap()
    }

    fn only_changed(before: &FileConfig, after: &FileConfig, path: &[&str]) {
        let (mut a, b) = (json(after), json(before));
        let mut want = b.clone();
        // Copy the edited leaf from `after` into the expectation.
        let mut src = &a;
        for p in path {
            src = if let Ok(i) = p.parse::<usize>() {
                &src[i]
            } else {
                &src[*p]
            };
        }
        let leaf = src.clone();
        let mut dst = &mut want;
        for p in &path[..path.len() - 1] {
            dst = if let Ok(i) = p.parse::<usize>() {
                &mut dst[i]
            } else {
                &mut dst[*p]
            };
        }
        dst.as_object_mut()
            .unwrap()
            .insert(path[path.len() - 1].to_string(), leaf);
        // Absent-when-default keys may vanish on the way back.
        strip_nulls(&mut a);
        strip_nulls(&mut want);
        assert_eq!(a, want, "{path:?}");
    }

    fn strip_nulls(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.retain(|_, v| !v.is_null());
                m.values_mut().for_each(strip_nulls);
            }
            Value::Array(a) => a.iter_mut().for_each(strip_nulls),
            _ => {}
        }
    }

    #[test]
    fn each_kind_is_read_and_set() {
        let c = cfg();
        let e = edit(&c, "update.enabled", Some("true")).unwrap();
        assert!(e.update.enabled);
        only_changed(&c, &e, &["update", "enabled"]);

        let e = edit(&c, "update.maturity_secs", Some("172800")).unwrap();
        assert_eq!(e.update.maturity_secs, 172_800);
        only_changed(&c, &e, &["update", "maturity_secs"]);

        let e = edit(&c, "autolimit.margin", Some("1.25")).unwrap();
        assert_eq!(e.autolimit.margin, 1.25);
        only_changed(&c, &e, &["autolimit", "margin"]);

        let e = edit(&c, "gate.on_bypass", Some("enforce")).unwrap();
        assert_eq!(e.gate.on_bypass, crate::gate::OnBypass::Enforce);
        only_changed(&c, &e, &["gate", "on_bypass"]);

        let e = edit(&c, "update.repo", Some("owner/demo")).unwrap();
        assert_eq!(e.update.repo, "owner/demo");
        only_changed(&c, &e, &["update", "repo"]);

        let e = edit(&c, "trusted_actors", Some("alice, bob,,")).unwrap();
        assert_eq!(e.trusted_actors, ["alice", "bob"]);
        only_changed(&c, &e, &["trusted_actors"]);
        let e = edit(&c, "trusted_actors", Some("")).unwrap();
        assert!(e.trusted_actors.is_empty());

        for (v, want) in [
            ("auto", HardCeiling::Auto),
            ("off", HardCeiling::Off),
            ("6", HardCeiling::Fixed(6)),
        ] {
            let e = edit(&c, "autolimit.hard_ceiling", Some(v)).unwrap();
            assert_eq!(e.autolimit.hard_ceiling, want, "{v}");
            only_changed(&c, &e, &["autolimit", "hard_ceiling"]);
        }

        let e = edit(&c, "profile.linux-docker.dind", Some("true")).unwrap();
        assert_eq!(e.profiles[0].dind, Some(true));
        only_changed(&c, &e, &["profile", "0", "dind"]);
        let e = edit(
            &c,
            "profile.linux-docker.labels",
            Some("faber,docker,heavy"),
        )
        .unwrap();
        assert_eq!(e.profiles[0].labels, ["faber", "docker", "heavy"]);
        only_changed(&c, &e, &["profile", "0", "labels"]);
    }

    #[test]
    fn unset_clears_optional_fields_and_empties_lists() {
        let mut c = cfg();
        c.update.pin = Some("0.4.1".into());
        c.profiles[0].dind = Some(true);
        c.registry_auth.username = "someone".into();
        let e = edit(&c, "update.pin", None).unwrap();
        assert_eq!(e.update.pin, None);
        // An empty value on an optional field is an unset too.
        assert_eq!(edit(&c, "update.pin", Some("")).unwrap().update.pin, None);
        assert_eq!(
            edit(&c, "profile.linux-docker.dind", None)
                .unwrap()
                .profiles[0]
                .dind,
            None
        );
        assert!(edit(&c, "denied_actors", None)
            .unwrap()
            .denied_actors
            .is_empty());
        // Optional text the config keeps as plain text unsets to empty.
        assert_eq!(
            edit(&c, "registry_auth.username", None)
                .unwrap()
                .registry_auth
                .username,
            ""
        );
    }

    #[test]
    fn bad_values_are_refused_with_the_reason() {
        let c = cfg();
        for (key, value, says) in [
            ("update.enabled", Some("yes"), "true or false"),
            ("update.maturity_secs", Some("1h"), "whole number"),
            ("update.maturity_secs", Some("-5"), "whole number"),
            ("autolimit.margin", Some("lots"), "a number"),
            ("autolimit.margin", Some("NaN"), "a number"),
            ("gate.on_bypass", Some("ignore"), "record, enforce"),
            ("autolimit.hard_ceiling", Some("max"), "auto, off or"),
            ("update.enabled", None, "cannot be unset"),
            ("update.repo", None, "cannot be unset"),
            (
                "profile.linux-docker.env",
                Some("A=1"),
                "faber config apply",
            ),
            (
                "profile.nope.dind",
                Some("true"),
                "no profile named \"nope\"",
            ),
            (
                "profile.nope.dind",
                Some("true"),
                "the profiles are linux-docker",
            ),
            ("update.nonsense", Some("1"), "not a config key"),
        ] {
            let err = edit(&c, key, value).unwrap_err();
            assert!(err.contains(says), "{key} {value:?}: {err}");
        }
        let err = edit(&c, "maturity_secs", Some("1")).unwrap_err();
        assert!(err.contains("did you mean update.maturity_secs"), "{err}");
        let err = edit(&FileConfig::default(), "profile.x.dind", Some("true")).unwrap_err();
        assert!(err.contains("the config has none"), "{err}");
    }

    /// The edit is parsed back as strictly as a file: a value the field's kind
    /// accepts but its type does not is refused, not dropped.
    #[test]
    fn the_result_is_parsed_strictly() {
        let err = edit(&cfg(), "poll_interval_secs", Some("99999999999999999999")).unwrap_err();
        assert!(err.contains("whole number"), "{err}");
        let err = edit(&cfg(), "profile.linux-docker.cpus", Some("99999999999")).unwrap_err();
        assert!(err.starts_with("profile.linux-docker.cpus:"), "{err}");
    }

    #[test]
    fn every_form_field_but_maps_is_a_key() {
        let c = cfg();
        for f in top_fields() {
            assert!(target(f.key).is_some(), "{}", f.key);
        }
        for f in FORM_SCHEMA.profile_fields {
            let key = format!("profile.linux-docker.{}", f.key);
            assert!(target(&key).is_some(), "{key}");
            if f.kind == "map" {
                assert!(edit(&c, &key, Some("")).is_err());
            }
        }
    }

    fn body(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn a_save_reports_what_widened_and_when_it_applies() {
        let ok = body(serde_json::json!({
            "saved": true, "errors": [],
            "widened": [{"key": "update.pin", "sentence": "update.pin none -> 0.4.1"}],
            "settings": {"pending_restart": ["gate", "profile"]},
        }));
        assert_eq!(
            save_report(200, &ok, Some("update.pin"), true).unwrap(),
            "trust widened: update.pin none -> 0.4.1\napplied live"
        );
        assert_eq!(
            save_report(200, &ok, Some("gate.on_bypass"), true).unwrap(),
            "trust widened: update.pin none -> 0.4.1\nsaved; restart faber to apply: gate"
        );
        assert_eq!(
            save_report(200, &ok, Some("profile.linux-docker.dind"), false).unwrap(),
            "trust widened: update.pin none -> 0.4.1\nsaved; restart faber to apply: profile"
        );
        assert!(save_report(200, &ok, Some("topic"), false)
            .unwrap()
            .ends_with("saved; faber already runs this value"));
        assert!(save_report(200, &ok, None, false)
            .unwrap()
            .ends_with("saved; restart faber to apply: gate, profile"));
        let none = body(serde_json::json!({"saved": true, "settings": {"pending_restart": []}}));
        assert_eq!(
            save_report(200, &none, None, false).unwrap(),
            "saved and applied live"
        );
    }

    #[test]
    fn a_refusal_prints_each_command_and_asks_for_sudo() {
        let refused = body(serde_json::json!({
            "saved": false,
            "errors": ["update.pin widens trust and needs a present caller (sudo); this one is operator"],
            "refused": [{"key": "update.pin", "sentence": "update.pin none -> 0.4.1",
                         "needs": "present",
                         "command": "sudo faber config set update.pin 0.4.1"}],
        }));
        let err = save_report(403, &refused, Some("update.pin"), false).unwrap_err();
        assert!(
            err.contains("update.pin: update.pin none -> 0.4.1"),
            "{err}"
        );
        assert!(
            err.contains("    sudo faber config set update.pin 0.4.1"),
            "{err}"
        );
        assert!(err.ends_with("run it with sudo"), "{err}");
        let as_root = save_report(403, &refused, Some("update.pin"), true).unwrap_err();
        assert!(!as_root.contains("run it with sudo"), "{as_root}");

        let invalid = body(serde_json::json!({"saved": false, "errors": ["a", "b"]}));
        assert_eq!(
            save_report(400, &invalid, None, false).unwrap_err(),
            "the daemon refused the config:\n  - a\n  - b"
        );
        let err = save_report(500, "boom\u{1b}[2J", None, false).unwrap_err();
        assert!(!err.contains('\u{1b}'), "{err:?}");
    }

    #[test]
    fn apply_confirms_a_widening_only_on_a_terminal_or_with_yes() {
        assert_eq!(confirm_step(true, false), Confirm::Go);
        assert_eq!(confirm_step(true, true), Confirm::Go);
        assert_eq!(confirm_step(false, true), Confirm::Ask);
        assert_eq!(confirm_step(false, false), Confirm::Refuse);
    }

    #[test]
    fn a_dry_run_report_lists_widenings_or_refuses() {
        let ok = body(serde_json::json!({
            "saved": false, "errors": [],
            "widened": [{"key": "update.pin", "sentence": "update.pin none -> 0.4.1\u{1b}[2J"}],
        }));
        let err = dry_run_report(200, &ok).unwrap_err().to_string();
        assert!(err.contains("no base"), "{err}");
        let ok = body(serde_json::json!({
            "saved": false, "errors": [], "base": "0a1b",
            "widened": [{"key": "update.pin", "sentence": "update.pin none -> 0.4.1\u{1b}[2J"}],
        }));
        let DryRun { widened: w, base } = dry_run_report(200, &ok).unwrap();
        assert_eq!(base, "0a1b");
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("update.pin none -> 0.4.1") && !w[0].contains('\u{1b}'));
        // A base that is not hex never reaches the query.
        let odd = body(serde_json::json!({"saved": false, "errors": [], "base": "a&dry_run=1"}));
        assert!(dry_run_report(200, &odd).is_err());
        let bad = body(serde_json::json!({"saved": false, "errors": ["no profile"]}));
        let err = dry_run_report(200, &bad).unwrap_err().to_string();
        assert!(err.contains("- no profile"), "{err}");
        assert!(dry_run_report(404, "nope").is_err());
    }

    /// #221: a save whose base moved after the dry run tells root to re-run.
    #[test]
    fn a_moved_base_tells_root_to_run_apply_again() {
        let moved = body(serde_json::json!({"saved": false, "errors": ["it moved\u{1b}[2J"]}));
        let err = save_report(409, &moved, None, true).unwrap_err();
        assert!(err.starts_with("not applied: it moved"), "{err}");
        assert!(err.ends_with("run `sudo faber config apply` again to see what it widens now"));
        assert!(!err.contains('\u{1b}'), "{err:?}");
    }

    #[test]
    fn a_hold_clear_reports_and_its_refusal_names_the_command() {
        assert_eq!(
            hold_report(200, "{}", "owner/demo", Some(true), true).unwrap(),
            "minting hold cleared for owner/demo"
        );
        assert_eq!(
            hold_report(200, "{}", "owner/demo", Some(false), false).unwrap(),
            "owner/demo is not held; nothing to clear"
        );
        let err = hold_report(403, "why\n", "owner/demo", Some(true), false).unwrap_err();
        assert_eq!(
            err,
            "why\n  sudo faber repo-hold clear owner/demo\nrun it with sudo"
        );
    }
}
