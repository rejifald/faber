//! `sudo faber grant`: the one way a VM job is given LAN or tailnet access
//! (ADR-0026, amended 2026-10-17).
//!
//! ```text
//! sudo faber grant add    --profile P --to CIDR --port N/PROTO [--overlaps-host]
//! sudo faber grant remove --profile P --to CIDR --port N/PROTO
//! sudo faber grant dns    IP... | --default
//! sudo faber grant apply
//!      faber grant list
//! ```
//!
//! A change edits the root-owned grants file ([`super::paths::GRANTS`]),
//! renders the rules from the network plan and the grants, stores them root-owned
//! ([`super::paths::STORED`]) and reloads the anchor at once, so the checker,
//! which only ever enforces a stored set, has the new rules on its next tick
//! too. `apply` does the same without a change: for after a profile's
//! `drop_grants` in `config.toml` is edited.
//!
//! # Order, and why
//!
//! 1. Refuse to run from a binary `_faber` or `_faber_vm` could have replaced
//!    ([`crate::setup::exe::trust_running_binary`]), then refuse anyone but
//!    root, then any host not set up for faber's accounts.
//! 2. Read the grants file only if root alone could have written it and every
//!    directory above it ([`super::trust::root_only`]); absent is no grants.
//! 3. Read `config.toml` as data. It is `_faber`'s file, so it is opened
//!    without following a symlink and only if `_faber` owns it, size-capped,
//!    and parsed and nothing more. It supplies two things: the profile names
//!    an add may name, and each profile's drops. Neither can widen anything:
//!    a name only lets an add the operator typed through, and a drop only
//!    removes. The worst a rewritten `config.toml` can do is undo its own
//!    narrowing, back to what root granted.
//! 4. Read the plan, under the same root rule as the grants; absent is no VM
//!    networks yet.
//! 5. Decide ([`super::grant_edit`]), narrow ([`super::grant_edit::effective`])
//!    and render in memory. Any failure so far has written nothing.
//! 6. Write the grants file, read it back, write the stored set (only with a
//!    plan), reload (only on macOS, only with a plan), and append an audit
//!    line. A failed audit after a made change is a loud warning, not a
//!    rollback: the change is the operator's, and it stands.
//!
//! [`GrantHost`] is the seam: [`apply_change`] runs the steps above against it,
//! and the tests drive it with a fake that needs no root.

use std::net::Ipv4Addr;

use anyhow::{anyhow, bail, Context, Result};

use super::grant_edit::{
    effective, grant_line, validate_add, validate_remove, AddContext, GrantRequest,
};
use super::grants::{cidr, Grant, Grants, GrantsError, DEFAULT_RESOLVERS};
use super::model::NetworkPlan;
use super::paths;
use super::render;
use super::stored::StoredWall;
use crate::audit::Record;
use crate::config::FileConfig;

/// `faber grant …`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum GrantCommand {
    /// Let a profile's VM jobs reach one IPv4 network on one port. Root only.
    Add {
        /// The profile whose jobs get the grant, as config.toml names it.
        #[arg(long)]
        profile: String,
        /// The destination, ADDRESS/PREFIX (10.0.0.5/32). Never a hostname.
        #[arg(long)]
        to: String,
        /// PORT/PROTO: 5432/tcp or 53000/udp.
        #[arg(long)]
        port: String,
        /// The destination holds one of this host's own addresses, and that is
        /// meant. The host itself stays blocked whatever a grant says.
        #[arg(long)]
        overlaps_host: bool,
    },
    /// Withdraw a grant, named exactly as `list` prints it. Root only.
    Remove {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        port: String,
    },
    /// Print the grants, the resolvers, and what config.toml drops.
    List,
    /// Replace the resolvers VM guests may use. Root only.
    Dns {
        /// The resolvers, IPv4 addresses.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        resolvers: Vec<String>,
        /// Return to the default pair, Cloudflare's 1.1.1.1 and 1.0.0.1.
        #[arg(long)]
        default: bool,
    },
    /// Re-render and reload with no change, for after a profile's
    /// `drop_grants` is edited. Root only.
    Apply,
}

/// What `config.toml` supplies: the profile names it defines, each
/// profile's drops, and which profiles run in which kind of VM.
///
/// The last two are what `sudo faber setup` lists in the network plan's
/// slots ([`crate::setup::vm_runtime`]), so a grant reaches a profile's VMs.
/// They widen nothing either: a slot listing a profile is how that profile's
/// root-granted access applies at all, and a rewritten `config.toml` can at
/// most move a profile's grants between the VMs it already runs in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFacts {
    pub profiles: Vec<String>,
    pub drops: Vec<(String, Vec<String>)>,
    /// The `backend = "macos-vm"` profiles, in config order.
    pub vm_profiles: Vec<String>,
    /// The `backend = "docker"` profiles, in config order: every one runs in
    /// the one Linux VM.
    pub docker_profiles: Vec<String>,
}

impl ConfigFacts {
    pub fn from_config(cfg: &FileConfig) -> Self {
        use crate::config::BackendKind;
        let of = |kind: BackendKind| {
            cfg.profiles
                .iter()
                .filter(|p| p.backend == kind)
                .map(|p| p.name.clone())
                .collect()
        };
        Self {
            profiles: cfg.profiles.iter().map(|p| p.name.clone()).collect(),
            drops: cfg
                .profiles
                .iter()
                .filter(|p| !p.drop_grants.is_empty())
                .map(|p| (p.name.clone(), p.drop_grants.clone()))
                .collect(),
            vm_profiles: of(BackendKind::MacosVm),
            docker_profiles: of(BackendKind::Docker),
        }
    }
}

/// What rendering the wall for a grants file came to: the stored set (none
/// without a plan) and what the operator should hear about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub stored: Option<StoredWall>,
    /// The notes `config.toml`'s drops gave, then each profile whose grants
    /// render no rule because no slot lists it.
    pub warnings: Vec<String>,
}

/// The one way root renders what it stores: `grants` narrowed by `config`'s
/// drops ([`effective`]), rendered against `plan` ([`render::render`]).
/// `sudo faber grant` and `sudo faber setup` both store exactly this, so the
/// two never disagree about what a grants file means.
pub(crate) fn render_wall(
    grants: &Grants,
    config: &ConfigFacts,
    plan: Option<&NetworkPlan>,
) -> Result<Rendered> {
    let (eff, notes) = effective(grants, &config.drops)?;
    let stored = plan
        .map(|p| render::render(p, &eff))
        .transpose()
        .context("rendering the wall")?;
    let mut warnings: Vec<String> = notes.iter().map(ToString::to_string).collect();
    if let Some(p) = plan {
        for profile in render::unmatched_profiles(p, &eff) {
            warnings.push(format!(
                "no VM slot runs profile {}, so its grants render no rule",
                profile.escape_debug()
            ));
        }
    }
    Ok(Rendered { stored, warnings })
}

/// What reloading the anchor came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    Green,
    /// Red, with every reason the checker gave.
    Red(Vec<String>),
    /// Nothing was loaded, and why.
    NotLoaded(String),
}

/// Everything `sudo faber grant` does to the host.
pub(crate) trait GrantHost {
    /// The grants file's bytes, read only if root alone could have written
    /// it and every directory above it. `None` when there is none.
    fn read_grants(&mut self) -> Result<Option<String>>;
    /// `config.toml`, read as data.
    fn read_config(&mut self) -> Result<ConfigFacts>;
    /// The network plan, under the same rule as the grants. `None` before
    /// setup writes one.
    fn read_plan(&mut self) -> Result<Option<NetworkPlan>>;
    /// The host's own IPv4 addresses now.
    fn host_addrs(&mut self) -> Result<Vec<Ipv4Addr>>;
    /// Replace the grants file, root-owned, 0644.
    fn write_grants(&mut self, text: &str) -> Result<()>;
    /// Replace the stored set, root-owned, 0644.
    fn write_stored(&mut self, stored: &StoredWall) -> Result<()>;
    /// Load `stored` into pf now.
    fn reload(&mut self, stored: &StoredWall) -> Reload;
    /// Append one line to `_faber`'s audit log.
    fn append_audit(&mut self, record: &Record) -> Result<()>;
    /// Who asked: `SUDO_USER`, validated ([`actor_from`]).
    fn actor(&self) -> String;
}

/// One change, as the operator asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Add {
        req: GrantRequest,
        overlaps_host: bool,
    },
    Remove(GrantRequest),
    /// The new resolvers; `None` is the default pair.
    Dns(Option<Vec<String>>),
    Apply,
}

impl Change {
    /// The change a mutating subcommand asks for. `None` for `list`.
    pub fn from_command(cmd: GrantCommand) -> Option<Self> {
        Some(match cmd {
            GrantCommand::Add {
                profile,
                to,
                port,
                overlaps_host,
            } => Change::Add {
                req: GrantRequest { profile, to, port },
                overlaps_host,
            },
            GrantCommand::Remove { profile, to, port } => {
                Change::Remove(GrantRequest { profile, to, port })
            }
            GrantCommand::Dns { resolvers, default } => {
                Change::Dns((!default).then_some(resolvers))
            }
            GrantCommand::Apply => Change::Apply,
            GrantCommand::List => return None,
        })
    }

    fn verb(&self) -> &'static str {
        match self {
            Change::Add { .. } => "add",
            Change::Remove(_) => "remove",
            Change::Dns(_) => "dns",
            Change::Apply => "apply",
        }
    }
}

/// What a change did, for the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// What changed and what was stored, one line each.
    pub lines: Vec<String>,
    /// Worth the operator's attention, but not a failure.
    pub warnings: Vec<String>,
    pub reload: Reload,
}

/// Parse `dns`'s arguments. The rules after that are the file's
/// ([`Grants::with_resolvers`]).
pub fn parse_resolver_args(args: &[String]) -> Result<Vec<Ipv4Addr>, GrantsError> {
    args.iter()
        .map(|s| {
            s.parse()
                .map_err(|_| GrantsError::ResolverShape(s.escape_debug().to_string()))
        })
        .collect()
}

/// The steps in the module docs, from step 2, against `host`. Nothing is
/// written unless every check and the render succeed.
pub(crate) fn apply_change(host: &mut dyn GrantHost, change: &Change) -> Result<Done> {
    let current = match host.read_grants()? {
        None => Grants::default(),
        Some(text) => Grants::parse(&text).with_context(|| format!("reading {}", paths::GRANTS))?,
    };
    let config = host.read_config()?;
    let plan = host.read_plan()?;
    if let Some(p) = &plan {
        p.validate()
            .with_context(|| format!("the network plan {} is invalid", paths::PLAN))?;
    }
    let actor = host.actor();

    let (next, record, said) = match change {
        Change::Add { req, overlaps_host } => {
            let ctx = AddContext {
                known_profiles: config.profiles.clone(),
                host_addrs: host.host_addrs()?,
                vm_subnets: plan
                    .iter()
                    .flat_map(|p| p.networks.iter().map(|n| n.subnet))
                    .collect(),
                confirm_host_overlap: *overlaps_host,
            };
            let grant = validate_add(req, &ctx, &current)?;
            let next = current.with_grant(grant.clone())?;
            let record = grant_record("add", &grant, &actor);
            (next, record, format!("granted: {}", grant_line(&grant)))
        }
        Change::Remove(req) => {
            let grant = validate_remove(req, &current)?;
            let next = current.without_grant(&grant)?;
            let record = grant_record("remove", &grant, &actor);
            (next, record, format!("removed: {}", grant_line(&grant)))
        }
        Change::Dns(list) => {
            let resolvers = match list {
                None => DEFAULT_RESOLVERS.to_vec(),
                Some(args) => parse_resolver_args(args)?,
            };
            let next = current.with_resolvers(&resolvers)?;
            let shown = resolvers_line(&next);
            let record = Record::grant("dns", None, Some(shown.clone()), None, &actor);
            (next, record, format!("resolvers: {shown}"))
        }
        Change::Apply => {
            let record = Record::grant("apply", None, None, None, &actor);
            let said = format!("re-rendered from {}", paths::GRANTS);
            (current.clone(), record, said)
        }
    };

    let text = next.to_toml();
    if Grants::parse(&text).as_ref() != Ok(&next) {
        bail!("the grants this would write do not read back as themselves; nothing was written");
    }
    let Rendered {
        stored,
        warnings: mut notes,
    } = render_wall(&next, &config, plan.as_ref())?;

    // Everything is decided: from here on, write.
    if *change != Change::Apply {
        host.write_grants(&text)
            .with_context(|| format!("writing {}", paths::GRANTS))?;
        let back = host
            .read_grants()?
            .ok_or_else(|| anyhow!("{} is gone right after it was written", paths::GRANTS))?;
        if Grants::parse(&back).as_ref() != Ok(&next) {
            bail!(
                "{} does not read back as written; the stored rules were not changed",
                paths::GRANTS
            );
        }
    }
    let mut lines = vec![said];
    let reload = match &stored {
        Some(s) => {
            host.write_stored(s)
                .with_context(|| format!("writing {}", paths::STORED))?;
            lines.push(format!(
                "stored the rules in {} (digest {})",
                paths::STORED,
                &s.digest[..12.min(s.digest.len())]
            ));
            host.reload(s)
        }
        None => Reload::NotLoaded(format!(
            "there is no network plan yet ({}, which setup writes), so there are no VM \
             networks to load rules for",
            paths::PLAN
        )),
    };

    if let Err(e) = host.append_audit(&record) {
        notes.push(format!(
            "the change is made, but the audit log was not written: {e:#}"
        ));
    }
    Ok(Done {
        lines,
        warnings: notes,
        reload,
    })
}

fn grant_record(action: &str, g: &Grant, actor: &str) -> Record {
    Record::grant(
        action,
        Some(&g.profile),
        Some(cidr(&g.to)),
        Some(format!("{}/{}", g.port, g.proto)),
        actor,
    )
}

fn resolvers_line(g: &Grants) -> String {
    let list: Vec<String> = g.resolvers().iter().map(Ipv4Addr::to_string).collect();
    list.join(", ")
}

/// `SUDO_USER` as the audit line's actor: a conservative account-name shape
/// (ASCII letters, digits, `_`, `.`, `-`, not starting with `-`, at most 32
/// bytes), else `unknown`. It is the environment, so it is only a label.
pub fn actor_from(sudo_user: Option<std::ffi::OsString>) -> String {
    sudo_user
        .and_then(|v| v.into_string().ok())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 32
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// `ip -4 -o addr show` on Linux, reduced to the host's IPv4 addresses,
/// sorted. Each line is `N: IFNAME    inet ADDR[/PREFIX] …`. A line it cannot
/// read is an error, never skipped: a missed address is one an add would not
/// be warned about.
pub fn parse_ip_addr(out: &str) -> Result<Vec<Ipv4Addr>, String> {
    let mut addrs = Vec::new();
    for (i, line) in out.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || format!("line {}: not `N: IFNAME inet ADDR/PREFIX …`", i + 1);
        let words: Vec<&str> = line.split_whitespace().collect();
        let index_ok = words
            .first()
            .and_then(|w| w.strip_suffix(':'))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !index_ok || words.get(2) != Some(&"inet") {
            return Err(bad());
        }
        let addr = words
            .get(3)
            .map(|w| w.split_once('/').map_or(*w, |(a, _)| a))
            .and_then(|a| a.parse::<Ipv4Addr>().ok())
            .ok_or_else(bad)?;
        addrs.push(addr);
    }
    addrs.sort();
    addrs.dedup();
    Ok(addrs)
}

/// What `list` prints: the resolvers, each grant (marked when `config.toml`
/// drops it), and what the operator should know about the rest.
pub fn list_lines(
    grants: &Grants,
    config: Option<&ConfigFacts>,
    plan: Option<&NetworkPlan>,
) -> Vec<String> {
    let mut out = Vec::new();
    let default = if grants.resolvers() == DEFAULT_RESOLVERS {
        " (the default)"
    } else {
        ""
    };
    out.push(format!("resolvers: {}{default}", resolvers_line(grants)));
    let mut notes = Vec::new();
    let eff = match config.map(|c| effective(grants, &c.drops)) {
        Some(Ok((eff, drop_notes))) => {
            notes.extend(drop_notes.iter().map(ToString::to_string));
            Some(eff)
        }
        Some(Err(e)) => {
            notes.push(format!(
                "{e}; `sudo faber grant` refuses to render until it is fixed"
            ));
            None
        }
        None => None,
    };
    if grants.grants().is_empty() {
        out.push("no grants: VM jobs reach the public internet only".into());
    }
    for g in grants.grants() {
        let dropped = eff.as_ref().is_some_and(|e| !e.grants().contains(g));
        let mark = if dropped {
            "  (dropped by config.toml)"
        } else {
            ""
        };
        out.push(format!("{}{mark}", grant_line(g)));
    }
    out.extend(notes);
    match plan {
        Some(p) => out.extend(
            render::unmatched_profiles(p, grants)
                .into_iter()
                .map(|name| {
                    format!(
                        "no VM slot runs profile {}, so its grants render no rule",
                        name.escape_debug()
                    )
                }),
        ),
        None => out.push(format!(
            "no network plan yet ({}): setup writes it, and until then no grant is loaded",
            paths::PLAN
        )),
    }
    out
}

/// Run a `faber grant` command. Blocking: call it from `spawn_blocking`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn run(cmd: GrantCommand) -> Result<()> {
    use crate::platform::layout::Layout;
    use crate::platform::record::current_euid;
    use crate::term::sanitize;

    let Some(change) = Change::from_command(cmd) else {
        return list();
    };
    crate::setup::exe::trust_running_binary()?;
    if current_euid() != 0 {
        bail!("run with sudo: sudo faber grant {}", change.verb());
    }
    if crate::platform::layout() != Layout::System {
        bail!(
            "this host is not set up for faber's own accounts (no trusted /etc/faber/setup.json); \
             run `sudo faber setup` first: grants apply to the VMs it creates"
        );
    }
    let done = apply_change(&mut system::SystemHost, &change)?;
    for line in &done.lines {
        println!("{}", sanitize(line));
    }
    for w in &done.warnings {
        eprintln!("faber grant: warning: {}", sanitize(w));
    }
    match done.reload {
        Reload::Green => {
            println!("loaded: the wall is green");
            Ok(())
        }
        Reload::Red(reasons) => bail!(
            "the change is stored, but the wall is red: {}",
            sanitize(&reasons.join("; "))
        ),
        Reload::NotLoaded(why) => {
            println!("stored; nothing was loaded: {}", sanitize(&why));
            Ok(())
        }
    }
}

/// `faber grant list`: no root needed. Each input that cannot be read is
/// named rather than fatal, except the grants file itself.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn list() -> Result<()> {
    use crate::term::sanitize;

    let mut host = system::SystemHost;
    let grants = match host.read_grants()? {
        None => Grants::default(),
        Some(text) => Grants::parse(&text).with_context(|| format!("reading {}", paths::GRANTS))?,
    };
    let mut lines = Vec::new();
    let config = match host.read_config() {
        Ok(c) => Some(c),
        Err(e) => {
            lines.push(format!(
                "config.toml not read ({e:#}), so drops are not shown"
            ));
            None
        }
    };
    let plan = match host.read_plan() {
        Ok(p) => p,
        Err(e) => {
            lines.push(format!("the network plan was not read: {e:#}"));
            None
        }
    };
    for line in list_lines(&grants, config.as_ref(), plan.as_ref())
        .iter()
        .chain(&lines)
    {
        println!("{}", sanitize(line));
    }
    Ok(())
}

/// The real host: root's files under `/etc/faber`, `_faber`'s `config.toml`
/// and audit log, and pf on macOS.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) mod system {
    use std::io::Read;
    use std::net::Ipv4Addr;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use anyhow::{anyhow, bail, Context, Result};

    use super::super::model::NetworkPlan;
    use super::super::paths;
    use super::super::stored::StoredWall;
    use super::super::trust::root_only;
    use super::{ConfigFacts, GrantHost, Reload};
    use crate::audit::Record;
    use crate::config::FileConfig;
    use crate::platform::layout::{Os, SystemLayout, ETC_DIR};
    use crate::setup::exe::chain_of;
    use crate::setup::fsx::{self, Account, Dir, SourceRefusal, ROOT};

    pub(super) struct SystemHost;

    /// `/etc/faber`, opened through `/etc` (a symlink on macOS, root's own)
    /// and refused unless it is a root-owned directory nobody else can write.
    fn open_etc() -> Result<Dir> {
        let etc =
            fsx::open_leaf_dir(Path::new(ETC_DIR)).with_context(|| format!("opening {ETC_DIR}"))?;
        root_dir(&etc)?;
        Ok(etc)
    }

    fn root_dir(dir: &Dir) -> Result<()> {
        let meta = dir.metadata()?;
        if let Some(problem) =
            fsx::root_dir_problem(dir.path(), meta.is_dir(), meta.uid(), meta.mode())
        {
            bail!("{problem}, so root does not trust what is in it");
        }
        Ok(())
    }

    /// The root-owned file `name` in `dir` (under `/etc/faber`), whose full
    /// path is `full`: `None` when absent. Read only when root alone could
    /// have written it and every directory above it, then again judged by its
    /// own descriptor, opened without following a symlink, regular, and at
    /// most [`paths::STORED_READ_CAP`] bytes for the stored rules and
    /// [`paths::READ_CAP`] for anything else.
    pub(crate) fn read_root_file(
        subdir: Option<&str>,
        name: &str,
        full: &str,
    ) -> Result<Option<Vec<u8>>> {
        match chain_of(Path::new(full)) {
            Ok(chain) => root_only(&chain).map_err(|why| anyhow!("refusing {full}: {why}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("judging {full}")),
        }
        let etc = open_etc()?;
        let dir = match subdir {
            None => etc,
            Some(sub) => {
                let d = etc.subdir(sub).with_context(|| format!("opening {full}"))?;
                root_dir(&d)?;
                d
            }
        };
        let file = match dir.open_file(name) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("opening {full}")),
        };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            bail!("{full} is not a regular file");
        }
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            bail!("refusing {full}: it is not root's alone");
        }
        // The stored rules grow with the plan's networks; the grants and the
        // plan stay small.
        let cap = if name == paths::STORED_NAME {
            paths::STORED_READ_CAP
        } else {
            paths::READ_CAP
        };
        let mut bytes = Vec::new();
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {full}"))?;
        if bytes.len() as u64 > cap {
            bail!("{full} is larger than {cap} bytes; refusing it");
        }
        Ok(Some(bytes))
    }

    fn faber_account() -> Result<Account> {
        let record = crate::platform::setup_record()
            .ok_or_else(|| anyhow!("there is no trusted setup record, so no _faber account"))?;
        Ok(Account {
            uid: record.faber_uid(),
            gid: record.faber_gid(),
        })
    }

    /// `config.toml` as data, read only if `faber_uid` owns it: opened
    /// without following a symlink, size-capped, parsed and nothing more.
    /// Absent is an empty config. The one way root reads it, for `sudo faber
    /// grant` and for `sudo faber setup`, which knows `_faber`'s uid before
    /// any record says it.
    pub(crate) fn read_config_as(faber_uid: u32) -> Result<ConfigFacts> {
        let path = SystemLayout::for_os(Os::HOST).config_path();
        let shown = path.display().to_string();
        let (parent, name) = fsx::split(&path)?;
        let dir = match fsx::open_leaf_dir(parent) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConfigFacts::default()),
            Err(e) => return Err(e).with_context(|| format!("opening {}", parent.display())),
        };
        let (file, meta) = match fsx::open_source(&dir, name, faber_uid) {
            Ok(opened) => opened,
            Err(SourceRefusal::Missing) => return Ok(ConfigFacts::default()),
            Err(why) => bail!("{shown} {why}"),
        };
        let cap = FileConfig::READ_CAP as u64;
        if meta.len() > cap {
            bail!("{shown} is larger than 1 MiB; refusing it");
        }
        let mut bytes = Vec::new();
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {shown}"))?;
        if bytes.len() as u64 > cap {
            bail!("{shown} is larger than 1 MiB; refusing it");
        }
        let text = String::from_utf8(bytes).map_err(|_| anyhow!("{shown} is not UTF-8"))?;
        let cfg = FileConfig::load_from_str(&text, &shown)?;
        Ok(ConfigFacts::from_config(&cfg))
    }

    /// The host's own IPv4 addresses now, from `ifconfig -a` as the checker
    /// reads it, sorted.
    #[cfg(target_os = "macos")]
    pub(crate) fn host_addrs() -> Result<Vec<Ipv4Addr>> {
        use super::super::sys::{parse_ifconfig, MacHost, WallHost};
        let out = MacHost.ifconfig().context("running ifconfig -a")?;
        let ifs = parse_ifconfig(&out).map_err(|e| anyhow!("reading ifconfig -a: {e}"))?;
        let mut addrs: Vec<Ipv4Addr> = ifs
            .iter()
            .flat_map(|i| i.addrs.iter().map(|&(a, _)| a))
            .collect();
        addrs.sort();
        addrs.dedup();
        Ok(addrs)
    }

    /// `/etc/faber/wall`, created root's 0755 if missing, and refused unless
    /// it is root's alone.
    fn wall_dir() -> Result<Dir> {
        let etc = open_etc()?;
        let (wall, _) = fsx::ensure_dir(&etc, paths::WALL_DIR_NAME, ROOT, 0o755, &[0])
            .with_context(|| format!("creating {}", paths::WALL_DIR))?;
        root_dir(&wall)?;
        Ok(wall)
    }

    /// Replace the stored set, root-owned 0644, in `/etc/faber/wall`.
    pub(crate) fn store_stored(stored: &StoredWall) -> Result<()> {
        let bytes = stored.to_json();
        let mut src = &bytes[..];
        wall_dir()?.write_atomic(paths::STORED_NAME, &mut src, ROOT, 0o644)?;
        Ok(())
    }

    /// Replace the network plan, root-owned 0644, in `/etc/faber/wall`.
    #[cfg(target_os = "macos")]
    pub(crate) fn store_plan(bytes: &[u8]) -> Result<()> {
        let mut src = bytes;
        wall_dir()?.write_atomic(paths::PLAN_NAME, &mut src, ROOT, 0o644)?;
        Ok(())
    }

    impl GrantHost for SystemHost {
        fn read_grants(&mut self) -> Result<Option<String>> {
            read_root_file(None, paths::GRANTS_NAME, paths::GRANTS)?
                .map(|b| {
                    String::from_utf8(b).map_err(|_| anyhow!("{} is not UTF-8", paths::GRANTS))
                })
                .transpose()
        }

        /// `_faber`'s file, as data: see the module docs for why that is safe.
        fn read_config(&mut self) -> Result<ConfigFacts> {
            read_config_as(faber_account()?.uid)
        }

        fn read_plan(&mut self) -> Result<Option<NetworkPlan>> {
            read_root_file(Some(paths::WALL_DIR_NAME), paths::PLAN_NAME, paths::PLAN)?
                .map(|b| {
                    NetworkPlan::from_json(&b).with_context(|| format!("parsing {}", paths::PLAN))
                })
                .transpose()
        }

        #[cfg(target_os = "macos")]
        fn host_addrs(&mut self) -> Result<Vec<Ipv4Addr>> {
            host_addrs()
        }

        /// `ip -4 -o addr show`, by absolute path, with no shell, bounded.
        #[cfg(target_os = "linux")]
        fn host_addrs(&mut self) -> Result<Vec<Ipv4Addr>> {
            let ip = ["/usr/sbin/ip", "/sbin/ip"]
                .into_iter()
                .find(|p| Path::new(p).is_file())
                .ok_or_else(|| anyhow!("neither /usr/sbin/ip nor /sbin/ip is there"))?;
            let out = super::super::sys::run_bounded(
                ip,
                &["-4", "-o", "addr", "show"],
                None,
                std::time::Duration::from_secs(10),
            )
            .with_context(|| format!("running {ip}"))?;
            if !out.status_ok {
                bail!(
                    "{ip} -4 -o addr show failed: {}",
                    out.stderr.trim().escape_debug()
                );
            }
            super::parse_ip_addr(&out.stdout).map_err(|e| anyhow!("reading {ip}'s output: {e}"))
        }

        fn write_grants(&mut self, text: &str) -> Result<()> {
            let etc = open_etc()?;
            let mut src = text.as_bytes();
            etc.write_atomic(paths::GRANTS_NAME, &mut src, ROOT, 0o644)?;
            Ok(())
        }

        fn write_stored(&mut self, stored: &StoredWall) -> Result<()> {
            store_stored(stored)
        }

        /// One tick of the checker, now: it loads the stored set and kills
        /// the states it newly forbids. The verdict file stays the checker
        /// daemon's to write.
        #[cfg(target_os = "macos")]
        fn reload(&mut self, stored: &StoredWall) -> Reload {
            use super::super::sys::{tick, CheckerState, MacHost};
            let mut state = CheckerState::start(&MacHost);
            let out = tick(&MacHost, stored, &mut state);
            if out.verdict.is_green() {
                Reload::Green
            } else {
                Reload::Red(out.file.reasons)
            }
        }

        #[cfg(target_os = "linux")]
        fn reload(&mut self, _stored: &StoredWall) -> Reload {
            Reload::NotLoaded(
                "the VM wall is enforced by pf, on macOS hosts only (ADR-0026); this Linux host \
                 has no checker to load it"
                    .into(),
            )
        }

        /// `_faber`'s log, in `_faber`'s directory: appended to only as a
        /// regular file `_faber` owns with one link ([`Dir::append_owned`]).
        fn append_audit(&mut self, record: &Record) -> Result<()> {
            let faber = faber_account()?;
            let log_dir = SystemLayout::for_os(Os::HOST).log_dir;
            let dir = fsx::open_leaf_dir(&log_dir)
                .with_context(|| format!("opening {}", log_dir.display()))?;
            let mut line = serde_json::to_string(record)?;
            line.push('\n');
            dir.append_owned("audit.jsonl", line.as_bytes(), faber, 0o644)
                .with_context(|| format!("appending to {}/audit.jsonl", log_dir.display()))
        }

        fn actor(&self) -> String {
            super::actor_from(std::env::var_os("SUDO_USER"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::grants::Proto;
    use super::super::model::tests::plan;
    use super::*;

    #[derive(Default)]
    struct Fake {
        grants: Option<String>,
        config: ConfigFacts,
        plan: Option<NetworkPlan>,
        addrs: Vec<Ipv4Addr>,
        /// Every write and reload, in order.
        events: Vec<&'static str>,
        stored: Option<StoredWall>,
        audit: Vec<serde_json::Value>,
        fail_audit: bool,
        /// What the next reload says.
        red: bool,
    }

    impl GrantHost for Fake {
        fn read_grants(&mut self) -> Result<Option<String>> {
            Ok(self.grants.clone())
        }
        fn read_config(&mut self) -> Result<ConfigFacts> {
            Ok(self.config.clone())
        }
        fn read_plan(&mut self) -> Result<Option<NetworkPlan>> {
            Ok(self.plan.clone())
        }
        fn host_addrs(&mut self) -> Result<Vec<Ipv4Addr>> {
            Ok(self.addrs.clone())
        }
        fn write_grants(&mut self, text: &str) -> Result<()> {
            self.events.push("grants");
            self.grants = Some(text.to_string());
            Ok(())
        }
        fn write_stored(&mut self, stored: &StoredWall) -> Result<()> {
            self.events.push("stored");
            self.stored = Some(stored.clone());
            Ok(())
        }
        fn reload(&mut self, _stored: &StoredWall) -> Reload {
            self.events.push("reload");
            if self.red {
                Reload::Red(vec!["pf is disabled".into()])
            } else {
                Reload::Green
            }
        }
        fn append_audit(&mut self, record: &Record) -> Result<()> {
            if self.fail_audit {
                bail!("the log is not _faber's");
            }
            self.events.push("audit");
            self.audit.push(serde_json::to_value(record)?);
            Ok(())
        }
        fn actor(&self) -> String {
            "operator".into()
        }
    }

    fn fake() -> Fake {
        Fake {
            config: ConfigFacts {
                profiles: vec!["macos-vm".into(), "docker".into(), "docker-big".into()],
                drops: vec![],
                ..ConfigFacts::default()
            },
            plan: Some(plan()),
            addrs: vec![
                "127.0.0.1".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
                "192.168.64.1".parse().unwrap(),
            ],
            ..Fake::default()
        }
    }

    fn add(profile: &str, to: &str, port: &str) -> Change {
        Change::Add {
            req: GrantRequest {
                profile: profile.into(),
                to: to.into(),
                port: port.into(),
            },
            overlaps_host: false,
        }
    }

    fn written(f: &Fake) -> Grants {
        Grants::parse(f.grants.as_deref().unwrap_or("")).unwrap()
    }

    #[test]
    fn an_add_writes_grants_then_stored_then_reloads_then_audits() {
        let mut f = fake();
        let done = apply_change(&mut f, &add("macos-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        assert_eq!(f.events, ["grants", "stored", "reload", "audit"]);
        let g = written(&f);
        assert_eq!(g.grants().len(), 1);
        assert_eq!(g.grants()[0].proto, Proto::Tcp);
        // What was stored is exactly the render of the plan and the grants.
        let want = render::render(&plan(), &g).unwrap();
        assert_eq!(f.stored.as_ref().unwrap().digest, want.digest);
        assert!(f.stored.as_ref().unwrap().networks[1]
            .lines
            .iter()
            .any(|l| l.contains("to 10.0.0.5 port = 5432")));
        assert_eq!(done.reload, Reload::Green);
        assert_eq!(done.lines[0], "granted: macos-vm  10.0.0.5/32  5432/tcp");
        let a = &f.audit[0];
        assert_eq!(
            (
                &a["kind"],
                &a["action"],
                &a["profile"],
                &a["to"],
                &a["port"],
                &a["actor"]
            ),
            (
                &"grant".into(),
                &"add".into(),
                &"macos-vm".into(),
                &"10.0.0.5/32".into(),
                &"5432/tcp".into(),
                &"operator".into()
            )
        );
    }

    #[test]
    fn nothing_is_written_when_any_check_fails() {
        let base = fake();
        let mut bad_drop = fake();
        bad_drop.config.drops = vec![("docker".into(), vec!["db.lan 22/tcp".into()])];
        let mut bad_plan = fake();
        bad_plan.plan.as_mut().unwrap().networks[0].slots.clear();
        let mut bad_file = fake();
        bad_file.grants = Some("[grants]\n".into());
        let cases: Vec<(Fake, Change)> = vec![
            (fake(), add("linux", "10.0.0.5/32", "5432/tcp")),
            (fake(), add("macos-vm", "db.lan", "5432/tcp")),
            (fake(), add("macos-vm", "10.0.0.5", "5432/tcp")),
            (fake(), add("macos-vm", "10.0.0.5/32", "5432")),
            (fake(), add("macos-vm", "192.168.64.9/32", "22/tcp")),
            (fake(), add("macos-vm", "192.0.2.0/24", "22/tcp")),
            (
                fake(),
                Change::Remove(GrantRequest {
                    profile: "macos-vm".into(),
                    to: "10.0.0.5/32".into(),
                    port: "5432/tcp".into(),
                }),
            ),
            (fake(), Change::Dns(Some(vec!["one.one.one.one".into()]))),
            (fake(), Change::Dns(Some(vec![]))),
            (fake(), Change::Dns(Some(vec!["224.0.0.251".into()]))),
            (bad_drop, add("macos-vm", "10.0.0.5/32", "5432/tcp")),
            (bad_plan, add("macos-vm", "10.0.0.5/32", "5432/tcp")),
            (bad_file, Change::Apply),
        ];
        for (mut f, change) in cases {
            let before = f.grants.clone();
            assert!(apply_change(&mut f, &change).is_err(), "{change:?}");
            assert!(f.events.is_empty(), "{change:?}: {:?}", f.events);
            assert_eq!(f.grants, before);
        }
        // A duplicate, after one good add.
        let mut f = base;
        apply_change(&mut f, &add("docker", "10.0.0.5/32", "5432/tcp")).unwrap();
        f.events.clear();
        assert!(apply_change(&mut f, &add("docker", "10.0.0.5/32", "5432/tcp")).is_err());
        assert!(f.events.is_empty());
    }

    #[test]
    fn a_config_drop_removes_the_rule_but_not_the_grant() {
        let mut f = fake();
        f.config.drops = vec![("macos-vm".into(), vec!["10.0.0.5/32 5432/tcp".into()])];
        apply_change(&mut f, &add("macos-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        // The grant is root's and stays in the file …
        assert_eq!(written(&f).grants().len(), 1);
        // … but config narrowed it out of the rules.
        let stored = f.stored.clone().unwrap();
        assert_eq!(
            stored.digest,
            render::render(&plan(), &Grants::default()).unwrap().digest
        );
        assert!(!stored
            .networks
            .iter()
            .flat_map(|n| &n.lines)
            .any(|l| l.contains("5432")));
        // Clearing the drop and applying brings the rule back.
        f.config.drops.clear();
        f.events.clear();
        apply_change(&mut f, &Change::Apply).unwrap();
        assert_eq!(f.events, ["stored", "reload", "audit"]);
        assert_eq!(
            f.stored.unwrap().digest,
            render::render(&plan(), &written_grants_of(f.grants.as_deref()))
                .unwrap()
                .digest
        );
        assert_eq!(f.audit.last().unwrap()["action"], "apply");
    }

    fn written_grants_of(text: Option<&str>) -> Grants {
        Grants::parse(text.unwrap_or("")).unwrap()
    }

    #[test]
    fn with_no_plan_grants_are_stored_and_nothing_is_rendered_or_loaded() {
        let mut f = fake();
        f.plan = None;
        let done = apply_change(&mut f, &add("macos-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        assert_eq!(f.events, ["grants", "audit"]);
        assert!(f.stored.is_none());
        assert!(matches!(&done.reload, Reload::NotLoaded(why) if why.contains("no network plan")));
        // With no plan there is no VM subnet to be inert in, but the fixed
        // fence still holds.
        assert!(apply_change(&mut f, &add("macos-vm", "127.0.0.1/32", "22/tcp")).is_err());
    }

    #[test]
    fn remove_dns_and_dns_default_round_trip() {
        let mut f = fake();
        apply_change(&mut f, &add("docker", "10.0.0.0/24", "22/tcp")).unwrap();
        apply_change(
            &mut f,
            &Change::Dns(Some(vec!["9.9.9.9".into(), "149.112.112.112".into()])),
        )
        .unwrap();
        let g = written(&f);
        assert_eq!(g.resolvers().len(), 2);
        assert_eq!(g.grants().len(), 1);
        assert_eq!(f.audit.last().unwrap()["to"], "9.9.9.9, 149.112.112.112");
        let removed = apply_change(
            &mut f,
            &Change::Remove(GrantRequest {
                profile: "docker".into(),
                to: "10.0.0.0/24".into(),
                port: "22/tcp".into(),
            }),
        )
        .unwrap();
        assert_eq!(removed.lines[0], "removed: docker  10.0.0.0/24  22/tcp");
        apply_change(&mut f, &Change::Dns(None)).unwrap();
        assert_eq!(written(&f), Grants::default());
        assert_eq!(
            f.stored.unwrap().digest,
            render::render(&plan(), &Grants::default()).unwrap().digest
        );
        let actions: Vec<&str> = f
            .audit
            .iter()
            .map(|a| a["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, ["add", "dns", "remove", "dns"]);
    }

    #[test]
    fn a_failed_audit_is_a_warning_and_the_change_stands() {
        let mut f = fake();
        f.fail_audit = true;
        let done = apply_change(&mut f, &add("macos-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        assert_eq!(written(&f).grants().len(), 1);
        assert!(done
            .warnings
            .iter()
            .any(|w| w.contains("audit log was not written")));
    }

    #[test]
    fn a_red_reload_is_reported_with_its_reasons() {
        let mut f = fake();
        f.red = true;
        let done = apply_change(&mut f, &add("macos-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        assert_eq!(done.reload, Reload::Red(vec!["pf is disabled".into()]));
        assert!(f.stored.is_some());
    }

    #[test]
    fn a_grant_for_a_profile_no_slot_runs_is_stored_with_a_warning() {
        let mut f = fake();
        f.config.profiles.push("linux-vm".into());
        let done = apply_change(&mut f, &add("linux-vm", "10.0.0.5/32", "5432/tcp")).unwrap();
        assert!(done.warnings.iter().any(|w| w.contains("linux-vm")));
        assert_eq!(
            f.stored.unwrap().digest,
            render::render(&plan(), &Grants::default()).unwrap().digest
        );
    }

    #[test]
    fn the_host_overlap_can_be_confirmed() {
        let mut f = fake();
        let change = Change::Add {
            req: GrantRequest {
                profile: "docker".into(),
                to: "192.0.2.0/24".into(),
                port: "22/tcp".into(),
            },
            overlaps_host: true,
        };
        apply_change(&mut f, &change).unwrap();
        assert_eq!(written(&f).grants().len(), 1);
    }

    #[test]
    fn the_actor_is_a_plain_account_name_or_unknown() {
        let of = |s: &str| actor_from(Some(s.into()));
        assert_eq!(of("operator"), "operator");
        assert_eq!(of("first.last-2_x"), "first.last-2_x");
        for bad in ["", "-rf", "a b", "a\nb", "\u{1b}[31m", "é", &"a".repeat(33)] {
            assert_eq!(of(bad), "unknown", "{bad:?}");
        }
        assert_eq!(actor_from(None), "unknown");
    }

    #[test]
    fn ip_addr_output_is_reduced_to_ipv4_addresses() {
        let out = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.0.2.10/24 brd 192.0.2.255 scope global dynamic eth0\\       valid_lft 86000sec preferred_lft 86000sec
2: eth0    inet 192.0.2.11/24 scope global secondary eth0\\       valid_lft forever preferred_lft forever
5: tailscale0    inet 100.64.1.2/32 scope global tailscale0\\       valid_lft forever preferred_lft forever
6: wg0    inet 10.9.0.1 peer 10.9.0.2/32 scope global wg0\\       valid_lft forever preferred_lft forever

";
        let want: Vec<Ipv4Addr> = [
            "10.9.0.1",
            "100.64.1.2",
            "127.0.0.1",
            "192.0.2.10",
            "192.0.2.11",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        assert_eq!(parse_ip_addr(out), Ok(want));
        assert_eq!(parse_ip_addr(""), Ok(vec![]));
        for bad in [
            "lo inet 127.0.0.1/8",
            "1: lo inet6 ::1/128",
            "1: lo inet nonsense/8",
            "x: lo inet 127.0.0.1/8",
            "1: lo",
        ] {
            assert!(parse_ip_addr(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolver_args_are_addresses_and_escaped_when_not() {
        assert_eq!(
            parse_resolver_args(&["9.9.9.9".into()]),
            Ok(vec!["9.9.9.9".parse().unwrap()])
        );
        let e = parse_resolver_args(&["x\u{1b}".into()])
            .unwrap_err()
            .to_string();
        assert!(!e.chars().any(char::is_control), "{e:?}");
    }

    #[test]
    fn list_marks_drops_and_names_what_renders_nothing() {
        let g = Grants::default()
            .with_grant(Grant {
                profile: "macos-vm".into(),
                to: "10.0.0.5/32".parse().unwrap(),
                port: 5432,
                proto: Proto::Tcp,
            })
            .unwrap()
            .with_grant(Grant {
                profile: "nobody".into(),
                to: "10.0.0.0/8".parse().unwrap(),
                port: 22,
                proto: Proto::Tcp,
            })
            .unwrap();
        let config = ConfigFacts {
            profiles: vec![],
            drops: vec![("macos-vm".into(), vec!["10.0.0.5/32 5432/tcp".into()])],
            ..ConfigFacts::default()
        };
        let lines = list_lines(&g, Some(&config), Some(&plan()));
        assert_eq!(
            lines,
            [
                "resolvers: 1.1.1.1, 1.0.0.1 (the default)",
                "macos-vm  10.0.0.5/32  5432/tcp  (dropped by config.toml)",
                "nobody  10.0.0.0/8  22/tcp",
                "no VM slot runs profile nobody, so its grants render no rule",
            ]
        );
        let empty = list_lines(&Grants::default(), None, None);
        assert_eq!(
            empty[1],
            "no grants: VM jobs reach the public internet only"
        );
        assert!(empty[2].contains("no network plan yet"));
        let bad = ConfigFacts {
            profiles: vec![],
            drops: vec![("macos-vm".into(), vec!["db.lan 1/tcp".into()])],
            ..ConfigFacts::default()
        };
        let lines = list_lines(&g, Some(&bad), Some(&plan()));
        assert!(lines.iter().any(|l| l.contains("refuses to render")));
        assert!(!lines.iter().any(|l| l.contains("dropped by")));
    }

    /// Which profiles run where is read from the backend each declares.
    #[test]
    fn config_facts_split_profiles_by_backend() {
        let cfg = FileConfig::load_from_str(
            "topic = \"faber\"\n\
             [[profile]]\nname = \"docker\"\nbackend = \"docker\"\nimage = \"r:1\"\nlabels = [\"a\"]\n\
             [[profile]]\nname = \"mac\"\nbackend = \"macos-vm\"\nimage = \"golden\"\nlabels = [\"b\"]\n",
            "test",
        );
        let cfg = cfg.unwrap();
        let f = ConfigFacts::from_config(&cfg);
        assert_eq!(f.profiles, ["docker", "mac"]);
        assert_eq!(f.vm_profiles, ["mac"]);
        assert_eq!(f.docker_profiles, ["docker"]);
    }

    /// `render_wall` is what both root writers store: with a plan, exactly
    /// the render of the narrowed grants; without one, nothing.
    #[test]
    fn render_wall_is_the_narrowed_render() {
        let g = Grants::default()
            .with_grant(Grant {
                profile: "macos-vm".into(),
                to: "10.0.0.5/32".parse().unwrap(),
                port: 5432,
                proto: Proto::Tcp,
            })
            .unwrap();
        let config = ConfigFacts {
            drops: vec![("macos-vm".into(), vec!["10.0.0.5/32 5432/tcp".into()])],
            ..ConfigFacts::default()
        };
        let r = render_wall(&g, &config, Some(&plan())).unwrap();
        assert_eq!(
            r.stored.unwrap().digest,
            render::render(&plan(), &Grants::default()).unwrap().digest
        );
        let none = render_wall(&g, &ConfigFacts::default(), None).unwrap();
        assert_eq!(none.stored, None);
    }

    #[test]
    fn list_is_the_only_command_that_is_not_a_change() {
        assert_eq!(Change::from_command(GrantCommand::List), None);
        assert_eq!(
            Change::from_command(GrantCommand::Dns {
                resolvers: vec![],
                default: true
            }),
            Some(Change::Dns(None))
        );
        assert_eq!(
            Change::from_command(GrantCommand::Apply),
            Some(Change::Apply)
        );
    }
}
