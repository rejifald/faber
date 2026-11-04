//! `faber auth link` and `sudo faber auth set` (ADR-0027): how the operator
//! hands the daemon its GitHub token, explicitly.
//!
//! `link` runs as the operator and touches no token. It prints the GitHub page
//! that pre-fills a fine-grained token for this host: its name, the owner, the
//! lifetime, and exactly the permissions the enabled features need. The
//! operator picks the repositories there; that choice is the grant.
//!
//! `set` runs as root, because replacing the token widens trust: a substituted
//! token can point this host at someone else's repositories, whose jobs would
//! then run here. `sudo` is the proof the operator is present. It refuses to
//! run from a binary faber's own accounts could have replaced, exactly as
//! setup does ([`crate::setup::exe`]), reads the token from a hidden prompt or
//! from stdin and never from an argument, checks it with GitHub, says which
//! repositories it reaches, stores it ([`crate::credential::Store::save`]), and
//! waits for the running daemon to pick it up.
//!
//! The token is never printed, logged, put on a command line or in a child's
//! environment. The one child that sees it is `pbpaste`, whose output is read
//! back through a pipe to compare with it.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::FileConfig;
use crate::credential::{self, Feature, Need, Permission, Secret};
use crate::github::{Github, GithubError, GrantedRepo};

/// The most of stdin `auth set` reads for a token. A fine-grained token is 93
/// characters; this only bounds a mistaken pipe.
const TOKEN_CAP: u64 = 4096;

/// How long `auth set` waits for the daemon to report the new token.
const PICKUP_WAIT: Duration = Duration::from_secs(90);

/// The most repositories `auth set` checks one by one.
const CHECK_REPOS: usize = 100;

/// Options for `faber auth link`.
#[derive(Debug, Clone)]
pub struct LinkOpts {
    /// The account whose repositories the token reaches. Asked of the running
    /// daemon when not given.
    pub owner: Option<String>,
    /// The token's lifetime in days, 1 to 366.
    pub days: u32,
    /// The config whose features decide the permissions; the host's own by
    /// default.
    pub config: Option<PathBuf>,
    /// The control panel to ask for the owner.
    pub port: u16,
}

/// Options for `sudo faber auth set`.
#[derive(Debug, Clone)]
pub struct SetOpts {
    /// The expiry date GitHub's token page showed, `YYYY-MM-DD`: the override
    /// for when GitHub's answer does not carry one. Ignored when it does.
    pub expires: Option<String>,
    /// The control panel to watch for the daemon picking the token up.
    pub port: u16,
}

/// This host's name as a token name carries it: up to the first dot.
pub fn short_host(raw: &str) -> String {
    let short = raw.trim().split('.').next().unwrap_or_default();
    if short.is_empty() {
        "host".to_string()
    } else {
        short.to_string()
    }
}

/// `needs`, one line per permission, with the features that need it.
fn permission_lines(needs: &[Need]) -> Vec<String> {
    let mut perms: Vec<Permission> = needs.iter().map(|n| n.permission).collect();
    perms.sort();
    perms.dedup();
    perms
        .into_iter()
        .filter(|p| p.grantable())
        .map(|p| {
            let features: Vec<&str> = credential::features_needing(needs, p)
                .into_iter()
                .map(Feature::label)
                .collect();
            format!("    {:<26} {}", p.label(), features.join(", "))
        })
        .collect()
}

/// What `faber auth link` prints.
pub fn link_text(host: &str, owner: Option<&str>, days: u32, needs: &[Need]) -> String {
    let url = credential::link_url(host, owner, days, needs);
    let mut out = format!(
        "Create this host's GitHub token on GitHub's own page:\n\n  {url}\n\n\
         The page is pre-filled with:\n    name         faber-{host}\n"
    );
    match owner {
        Some(owner) => out.push_str(&format!("    owner        {owner}\n")),
        None => out.push_str(
            "    owner        (pick the account whose repositories this host runs; \
             --owner fills it in)\n",
        ),
    }
    out.push_str(&format!(
        "    expires in   {days} day{}\n    permissions, each for what this host's config turns on:\n",
        if days == 1 { "" } else { "s" }
    ));
    for line in permission_lines(needs) {
        out.push_str(&line);
        out.push('\n');
    }
    if needs.iter().any(|n| !n.permission.grantable()) {
        let features: Vec<&str> = credential::features_needing(needs, Permission::ChecksRead)
            .into_iter()
            .map(Feature::label)
            .collect();
        out.push_str(&format!(
            "\nNot asked for: Checks: read, which {} need. Fine-grained tokens cannot \
             currently be granted it, so these stay off on private repositories.\n",
            features.join(" and ")
        ));
    }
    out.push_str(
        "\nThen:\n  \
         1. Under \"Repository access\", choose \"Only select repositories\" and pick exactly \
         the repositories this host should run jobs for. Each also needs the `faber` topic: \
         the token is the ceiling, the topic is each repository's own opt-in.\n  \
         2. Generate the token and copy it.\n  \
         3. Run `sudo faber auth set` and paste it at the prompt. It is not shown, and never \
         passed as an argument.\n",
    );
    out
}

/// `faber auth link`.
pub async fn link(opts: LinkOpts) -> Result<()> {
    if !credential::DAYS.contains(&opts.days) {
        bail!(
            "--days must be from {} to {}, GitHub's range. A token that never expires is \
             refused: expiry is the backstop for a leak nobody noticed (ADR-0027)",
            credential::DAYS.start(),
            credential::DAYS.end()
        );
    }
    if let Some(owner) = &opts.owner {
        if !credential::valid_login(owner) {
            bail!("--owner {owner:?} is not a GitHub login");
        }
    }
    let config = match FileConfig::load(opts.config.as_deref()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("faber: no config file here; asking for what a default host turns on");
            FileConfig::default()
        }
        Err(e) => bail!(
            "{e:#}. The link asks for what this host's config turns on, so it has to read it: \
             pass --config, or use a terminal opened after `sudo faber setup` added you to \
             group {}",
            crate::platform::layout::FABER_OPERATOR_GROUP
        ),
    };
    let owner = match opts.owner.clone() {
        Some(owner) => Some(owner),
        None => daemon_owner(opts.port).await,
    };
    let host = short_host(&crate::instance::hostname());
    let text = link_text(
        &host,
        owner.as_deref(),
        opts.days,
        &credential::required(&config),
    );
    print!("{}", crate::term::sanitize(&text));
    Ok(())
}

/// The repository topic the running daemon discovers by, if one answers with
/// a topic GitHub would accept: up to 50 lowercase letters, digits and hyphens.
async fn daemon_topic(port: u16) -> Option<String> {
    let state = api_state(port).await?;
    state["topic"]
        .as_str()
        .filter(|t| {
            (1..=50).contains(&t.len())
                && t.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .map(str::to_string)
}

/// The owner the running daemon reports, if one answers and knows it.
async fn daemon_owner(port: u16) -> Option<String> {
    let state = api_state(port).await?;
    fn login(v: &serde_json::Value) -> Option<&str> {
        v.as_str().filter(|o| credential::valid_login(o))
    }
    login(&state["credential"]["owner"])
        .or_else(|| login(&state["owner"]))
        .map(str::to_string)
}

/// `GET /api/state` from the running daemon, if it answers: over its control
/// socket (ADR-0028), or over loopback only on a LEGACY host whose daemon
/// predates the socket. Loopback answers nothing without a dashboard session.
async fn api_state(port: u16) -> Option<serde_json::Value> {
    let socket = crate::platform::control_socket()?;
    let (status, body) = crate::control::request(
        &socket,
        crate::control::fallback_here(port),
        reqwest::Method::GET,
        "/api/state",
        None,
    )
    .await
    .ok()?;
    if !status.is_success() {
        return None;
    }
    serde_json::from_str(&body).ok()
}

/// `sudo faber auth set`.
pub async fn set(opts: SetOpts) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        set_unix(opts).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = opts;
        bail!("faber auth set supports macOS and Linux only")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn set_unix(opts: SetOpts) -> Result<()> {
    use crate::platform::record::current_euid;

    if current_euid() != 0 {
        bail!(
            "`faber auth set` must run as root: `sudo faber auth set`. Replacing the token \
             widens what runs on this host, so it needs proof that you are here (ADR-0027)"
        );
    }
    let resolution = crate::platform::resolution();
    if let Some(warning) = resolution.warning() {
        bail!("{warning}");
    }
    let record = resolution.record().ok_or_else(|| {
        anyhow!(
            "this host is not set up: run `sudo faber setup` first. The token is kept for {}, \
             which setup creates",
            crate::platform::layout::FABER_ACCOUNT
        )
    })?;
    check_binary(record)?;
    let date =
        match opts.expires.as_deref() {
            Some(text) => Some(credential::parse_date(text).ok_or_else(|| {
                anyhow!("--expires {text:?} is not a date in the form YYYY-MM-DD")
            })?),
            None => None,
        };

    let token = read_token().await?;
    credential::check_shape(token.expose()).map_err(|why| anyhow!("{why}"))?;

    let gh = Github::with_token(token.clone()).context("building the GitHub client")?;
    let (login, header) = gh.token_owner().await.map_err(|e| match e {
        GithubError::Status { status: 401, .. } => anyhow!(
            "GitHub rejected this token (401): check it was copied whole, and has not expired \
             or been revoked"
        ),
        other => anyhow!("asking GitHub who this token acts as: {other}"),
    })?;
    if !credential::valid_login(&login) {
        bail!("GitHub answered with a login faber cannot use as an owner");
    }
    let now = crate::audit::now_ms();
    let expiry = expiry_of(header.as_deref(), date)?;
    let expires_at_ms = expiry.ms();
    check_expiry(expires_at_ms, now)?;
    println!(
        "Token for {login}, expiring {}.",
        credential::date_of(expires_at_ms)
    );
    match expiry {
        Expiry::Github(_) if date.is_some() => {
            println!("--expires ignored: GitHub itself says when this token expires.");
        }
        Expiry::Github(_) => {}
        Expiry::Given(_) => eprintln!(
            "faber: WARNING: GitHub did not confirm when this token expires. {} is taken from \
             --expires as given, unchecked. If the token in fact never expires, revoke it on \
             GitHub and create one that does with `faber auth link`.",
            credential::date_of(expires_at_ms)
        ),
    }

    // The topic comes from the running daemon, never from config.toml: root
    // does not read a file faber's own accounts can write. It only labels the
    // report below, so any failure falls back to the default.
    let topic = daemon_topic(opts.port)
        .await
        .unwrap_or_else(|| crate::config::DEFAULT_TOPIC.to_string());
    cancellable(report_repos(&gh, &login, &topic), "nothing was stored").await;

    let layout = crate::platform::system();
    let faber = crate::setup::fsx::Account {
        uid: record.faber_uid(),
        gid: record.faber_gid(),
    };
    let store = credential::Store::new(layout.credentials_dir.clone(), faber);
    let removed = store
        .save(&login, &token, expires_at_ms, now)
        .with_context(|| format!("storing the token in {}", store.dir().display()))?;
    println!(
        "Stored in {}.",
        store.dir().join(credential::file_name(&login)).display()
    );
    for other in removed {
        println!(
            "Removed the token stored for {other}: faber serves one owner. It is still valid \
             on GitHub until you revoke it there."
        );
    }
    exclude_from_backups(store.dir());
    clear_clipboard(&token);
    cancellable(
        await_pickup(opts.port, now),
        "the token is already stored, and the daemon picks it up on its own",
    )
    .await;
    Ok(())
}

/// Run `work`, leaving with exit 130 on Ctrl-C. The hidden prompt's
/// [`tokio::signal::ctrl_c`] replaced SIGINT's default for the rest of the
/// process, so without this Ctrl-C would do nothing during the slow phases.
/// `state` says what was already done.
async fn cancellable<T>(work: impl std::future::Future<Output = T>, state: &str) -> T {
    tokio::select! {
        out = work => out,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nfaber: cancelled; {state}");
            std::process::exit(130);
        }
    }
}

/// Refuse to act as root from a binary `_faber` or `_faber_vm` could have
/// replaced, as setup and uninstall do (ADR-0025's root invariant).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn check_binary(record: &crate::platform::SetupRecord) -> Result<()> {
    use crate::platform::layout::{Os, FABER_ACCOUNT, FABER_VM_ACCOUNT};
    use crate::setup::exe::{self, Untrusted};
    use crate::setup::tools::SystemRunner;

    let exe_path = std::env::current_exe().context("finding this binary")?;
    let chain = exe::chain_of(&exe_path)
        .with_context(|| format!("reading the owners of {}", exe_path.display()))?;
    // The operator group too: `_faber` is in it, and on Linux runs as it.
    let mut untrusted = Untrusted {
        uids: vec![record.faber_uid(), record.faber_vm_uid()],
        gids: [record.faber_gid(), record.faber_vm_gid()]
            .into_iter()
            .chain(record.operator_gid())
            .collect(),
    };
    let mut runner = SystemRunner;
    for name in [FABER_ACCOUNT, FABER_VM_ACCOUNT] {
        untrusted
            .gids
            .extend(crate::setup::accounts::probe_groups_of(&mut runner, name));
    }
    if Os::HOST == Os::Linux {
        if let Ok(Some(docker)) =
            crate::setup::accounts::probe_group(Os::HOST, &mut runner, "docker")
        {
            if docker.has_member(FABER_ACCOUNT) {
                untrusted.gids.push(docker.gid);
            }
        }
    }
    exe::verdict(&chain, &untrusted).map_err(|reason| anyhow!(exe::refusal(&reason)))
}

/// The token, from a hidden prompt when stdin is a terminal, else from stdin
/// to its end. Never from an argument.
async fn read_token() -> Result<Secret> {
    if !std::io::stdin().is_terminal() {
        return tokio::task::spawn_blocking(read_stdin)
            .await
            .map_err(|e| anyhow!("reading stdin: {e}"))?;
    }
    let reading = tokio::task::spawn_blocking(|| {
        prompt_hidden("Paste the GitHub token (it is not shown), then press Return: ")
    });
    tokio::select! {
        read = reading => read.map_err(|e| anyhow!("reading the token: {e}"))?,
        _ = tokio::signal::ctrl_c() => {
            // The read is still blocked in its own thread, and the runtime
            // would wait for it on the way out: put the echo back and leave.
            echo(true);
            eprintln!("\nfaber: cancelled; nothing was stored");
            std::process::exit(130);
        }
    }
}

/// Read stdin to its end, at most [`TOKEN_CAP`] bytes, trimmed.
fn read_stdin() -> Result<Secret> {
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(TOKEN_CAP + 1)
        .read_to_end(&mut buf)
        .context("reading the token from stdin")?;
    trimmed(buf, "stdin")
}

/// `bytes` as a trimmed token, the untrimmed copy overwritten. Refused when it
/// is longer than any token or not text.
fn trimmed(mut bytes: Vec<u8>, from: &str) -> Result<Secret> {
    if bytes.len() as u64 > TOKEN_CAP {
        bytes.fill(0);
        bail!("more than {TOKEN_CAP} bytes arrived on {from}, which no token is");
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => Secret::new(text),
        Err(e) => {
            e.into_bytes().fill(0);
            bail!("what arrived on {from} is not text");
        }
    };
    Ok(Secret::new(text.expose().trim().to_string()))
}

/// Run `/bin/stty` on the controlling terminal: echo on or off. Best effort
/// for turning it back on, which is all a guard on the way out can do.
fn echo(on: bool) -> bool {
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        return false;
    };
    std::process::Command::new("/bin/stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::from(tty))
        .env_clear()
        .env("PATH", crate::setup::tools::SAFE_PATH)
        .status()
        .is_ok_and(|s| s.success())
}

/// Turns the terminal's echo back on when dropped, on every way out of the
/// prompt, a panic's unwinding included.
struct EchoOff;

impl Drop for EchoOff {
    fn drop(&mut self) {
        echo(true);
    }
}

/// Ask on `/dev/tty` with echo off, and read one line, at most [`TOKEN_CAP`]
/// bytes.
fn prompt_hidden(prompt: &str) -> Result<Secret> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("opening the terminal")?;
    if !echo(false) {
        bail!("could not turn the terminal's echo off; pipe the token in instead");
    }
    let _restore = EchoOff;
    tty.write_all(prompt.as_bytes())?;
    tty.flush()?;
    let mut line = Vec::new();
    std::io::BufReader::new(&tty)
        .take(TOKEN_CAP + 1)
        .read_until(b'\n', &mut line)
        .context("reading the token")?;
    let _ = tty.write_all(b"\n");
    trimmed(line, "the terminal")
}

/// Where the expiry `auth set` stores came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expiry {
    /// GitHub's own `github-authentication-token-expiration` header.
    Github(u64),
    /// `--expires`, which GitHub did not confirm.
    Given(u64),
}

impl Expiry {
    fn ms(self) -> u64 {
        match self {
            Expiry::Github(ms) | Expiry::Given(ms) => ms,
        }
    }
}

/// When the token expires: GitHub's header, else `--expires`, else a refusal.
/// GitHub sends the header only for a token that expires, so its absence most
/// likely means one that never does, which ADR-0027 refuses. `--expires` is
/// the operator's word against that, for when GitHub's page shows a date.
fn expiry_of(header: Option<&str>, given: Option<u64>) -> Result<Expiry> {
    if let Some(ms) = header.and_then(credential::parse_expiry_header) {
        return Ok(Expiry::Github(ms));
    }
    if let Some(ms) = given {
        return Ok(Expiry::Given(ms));
    }
    let said = if header.is_some() {
        "GitHub sent an expiry for this token that faber could not read"
    } else {
        "GitHub reported no expiry for this token, which usually means it never expires, and \
         faber refuses those (ADR-0027): create one that expires with `faber auth link`"
    };
    bail!(
        "{said}. If GitHub's token page does show an expiry date, run this again with \
         --expires YYYY-MM-DD"
    )
}

/// Refuse a token already expired, or one whose expiry is further out than
/// GitHub allows, which is what a token that never expires looks like when its
/// date is guessed.
fn check_expiry(expires_at_ms: u64, now_ms: u64) -> Result<()> {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    if expires_at_ms <= now_ms {
        bail!(
            "this token expired on {}: regenerate it on GitHub, then run this again",
            credential::date_of(expires_at_ms)
        );
    }
    if expires_at_ms > now_ms + (u64::from(*credential::DAYS.end()) + 1) * DAY_MS {
        bail!(
            "{} is more than {} days away, longer than GitHub lets a token live. A token that \
             never expires is refused (ADR-0027): create one with `faber auth link`",
            credential::date_of(expires_at_ms),
            credential::DAYS.end()
        );
    }
    Ok(())
}

/// One repository the token reaches, as `auth set` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reach {
    repo: String,
    administer: bool,
    /// Whether the token can read the repository's pull requests, which the
    /// gate needs to learn whose code a job runs (ADR-0059).
    pulls: bool,
    opted_in: bool,
    private: bool,
}

/// List what the token reaches, check it can administer each (the read
/// stands in for write, which cannot be probed without side effects), and
/// flag ADR-0027's two mismatches. Warns, never refuses: the grant is the
/// operator's to make.
async fn report_repos(gh: &Github, login: &str, topic: &str) {
    let granted: Vec<GrantedRepo> = match gh.granted_repos().await {
        Ok(repos) => repos,
        Err(e) => {
            println!("Could not list the repositories this token reaches ({e}).");
            return;
        }
    };
    let mut reach = Vec::new();
    for repo in granted
        .iter()
        .filter(|r| {
            r.full_name
                .split_once('/')
                .is_some_and(|(o, _)| o.eq_ignore_ascii_case(login))
        })
        .take(CHECK_REPOS)
    {
        let administer = gh
            .probe(&repo.full_name, Permission::Administration)
            .await
            .is_ok();
        let pulls = gh
            .probe(&repo.full_name, Permission::PullRequestsRead)
            .await
            .is_ok();
        reach.push(Reach {
            repo: repo.full_name.clone(),
            administer,
            pulls,
            opted_in: repo.topics.iter().any(|t| t == topic),
            private: repo.private,
        });
    }
    let opted: Vec<String> = match gh.repos_with_topic(login, topic).await {
        Ok(repos) => repos,
        Err(_) => reach
            .iter()
            .filter(|r| r.opted_in)
            .map(|r| r.repo.clone())
            .collect(),
    };
    print!(
        "{}",
        crate::term::sanitize(&reach_text(&reach, &opted, login, topic))
    );
}

/// The table and the mismatches `auth set` prints.
fn reach_text(reach: &[Reach], opted: &[String], login: &str, topic: &str) -> String {
    let yes = |b: bool| if b { "yes" } else { "NO" };
    let mut out = String::new();
    if reach.is_empty() {
        out.push_str(&format!(
            "\nWARNING: this token reaches none of {login}'s repositories, so this host will run \
             nothing. Edit the token on GitHub and pick the repositories under \"Repository \
             access\". Storing it anyway.\n"
        ));
    } else {
        let topic_column = format!("topic `{topic}`");
        out.push_str(&format!(
            "\nRepositories this token reaches:\n  {:<40} {:<15} {:<16} {}\n",
            "repository", "administration", topic_column, "private"
        ));
        for r in reach {
            out.push_str(&format!(
                "  {:<40} {:<15} {:<16} {}\n",
                r.repo,
                yes(r.administer),
                yes(r.opted_in),
                yes(r.private)
            ));
        }
    }
    let granted: Vec<String> = reach.iter().map(|r| r.repo.clone()).collect();
    let found = credential::mismatches(login, &granted, opted, |name| {
        reach.iter().any(|r| r.repo == name && !r.administer)
    });
    for repo in &found.granted_not_opted_in {
        out.push_str(&format!(
            "  granted, but not opted in: {repo}. Remove it from the token on GitHub: faber \
             cannot read a token's grant, so this is the only way an over-broad one shows.\n"
        ));
    }
    for repo in &found.opted_in_not_granted {
        out.push_str(&format!(
            "  opted in, but not granted: {repo}. The token cannot administer it, so none of its \
             jobs run here: add it to the token on GitHub, with Administration: write.\n"
        ));
    }
    for r in reach.iter().filter(|r| r.private && r.opted_in && !r.pulls) {
        out.push_str(&format!(
            "  lacks Pull requests: read: {}. faber cannot read who wrote a pull request there, \
             so it judges that repository's jobs as it did before ADR-0059: tick Pull \
             requests: read on the token's page on GitHub.\n",
            r.repo
        ));
    }
    for r in reach.iter().filter(|r| !r.private && r.opted_in) {
        out.push_str(&format!(
            "  public: {}. faber never runs a public repository's jobs.\n",
            r.repo
        ));
    }
    out
}

/// Keep the store out of Time Machine (macOS): `tmutil addexclusion` marks the
/// directory itself, and the mark stays with it. Best effort, and only the
/// path is on its command line.
fn exclude_from_backups(dir: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("/usr/bin/tmutil");
        cmd.arg("addexclusion")
            .arg(dir)
            .env_clear()
            .env("PATH", crate::setup::tools::SAFE_PATH)
            .stdin(std::process::Stdio::null());
        match crate::setup::tools::output_within(cmd, Duration::from_secs(20)) {
            Ok(out) if out.success => println!("Excluded {} from Time Machine.", dir.display()),
            Ok(out) => eprintln!(
                "faber: warning: could not exclude {} from Time Machine: {}",
                dir.display(),
                out.sanitized(400)
            ),
            Err(e) => eprintln!(
                "faber: warning: could not exclude {} from Time Machine: {e:#}",
                dir.display()
            ),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
    }
}

/// Clear the operator's clipboard if it still holds the token (macOS): read it
/// as them, through `launchctl asuser`, compare, and empty it only on a match.
/// Anything else is left alone, and the operator is told.
fn clear_clipboard(token: &Secret) {
    #[cfg(target_os = "macos")]
    {
        let uid = std::env::var("SUDO_UID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|uid| *uid != 0);
        if let Some(uid) = uid {
            if clipboard_holds(uid, token) && empty_clipboard(uid) {
                println!("Cleared the token from your clipboard.");
                return;
            }
        }
    }
    let _ = token;
    println!("If the token is still on your clipboard, clear it now.");
}

/// `launchctl asuser <uid> sudo -u #<uid> <tool>`: run a clipboard tool in the
/// operator's own session, as them.
#[cfg(target_os = "macos")]
fn as_operator(uid: u32, tool: &str) -> std::process::Command {
    let uid_arg = uid.to_string();
    let mut cmd = std::process::Command::new("/bin/launchctl");
    cmd.args(["asuser", uid_arg.as_str(), "/usr/bin/sudo", "-u"])
        .arg(format!("#{uid}"))
        .arg(tool)
        .env_clear()
        .env("PATH", crate::setup::tools::SAFE_PATH)
        .stdin(std::process::Stdio::null());
    cmd
}

#[cfg(target_os = "macos")]
fn clipboard_holds(uid: u32, token: &Secret) -> bool {
    match crate::setup::tools::output_within(
        as_operator(uid, "/usr/bin/pbpaste"),
        Duration::from_secs(5),
    ) {
        Ok(out) if out.success => {
            let pasted = Secret::new(out.stdout);
            pasted.expose().trim() == token.expose()
        }
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn empty_clipboard(uid: u32) -> bool {
    crate::setup::tools::output_within(as_operator(uid, "/usr/bin/pbcopy"), Duration::from_secs(5))
        .is_ok_and(|out| out.success)
}

/// Wait for the running daemon to report the token stored at `stored_at_ms`.
async fn await_pickup(port: u16, stored_at_ms: u64) {
    let deadline = tokio::time::Instant::now() + PICKUP_WAIT;
    println!(
        "Waiting up to {}s for the daemon to pick it up…",
        PICKUP_WAIT.as_secs()
    );
    let mut last_reason = None;
    loop {
        if let Some(state) = api_state(port).await {
            let c = &state["credential"];
            last_reason = c["reason"].as_str().map(str::to_string);
            if c["stored_at_ms"].as_u64() == Some(stored_at_ms) {
                let status = c["status"].as_str().unwrap_or("?");
                println!("The daemon picked it up: credential {status}.");
                if let Some(reason) = c["reason"].as_str() {
                    println!("  {}", crate::term::sanitize(reason));
                }
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if let Some(reason) = last_reason {
        println!("The daemon says: {}", crate::term::sanitize(&reason));
    }
    println!(
        "Not picked up yet. The daemon re-reads the store every cycle, and every minute while \
         it waits in standby; check with:\n  curl -s --unix-socket \"{}\" http://faber/api/state | jq \
         .credential",
        crate::platform::control_socket()
            .map_or_else(|| "control.sock".to_string(), |p| p.display().to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_name_is_cut_at_the_first_dot() {
        assert_eq!(short_host("demo-mac.local\n"), "demo-mac");
        assert_eq!(short_host("builder"), "builder");
        assert_eq!(short_host(""), "host");
    }

    /// The link text names this host and asks only for what the config needs;
    /// what cannot be granted is said, not silently dropped.
    #[test]
    fn the_link_text_names_the_host_the_permissions_and_the_next_steps() {
        let mut cfg = FileConfig::default();
        cfg.retry.enabled = true;
        let text = link_text("demo-mac", Some("owner"), 90, &credential::required(&cfg));
        assert!(text.contains("name=faber-demo-mac"), "{text}");
        assert!(text.contains("    owner        owner\n"), "{text}");
        assert!(text.contains("expires in   90 days"), "{text}");
        assert!(text.contains("Actions: write"), "{text}");
        assert!(text.contains("auto-retry (ADR-0003)"), "{text}");
        assert!(text.contains("Not asked for: Checks: read"), "{text}");
        assert!(text.contains("`sudo faber auth set`"), "{text}");
        assert!(!text.contains("checks="), "{text}");
    }

    #[test]
    fn an_expiry_too_soon_or_too_far_is_refused() {
        const DAY: u64 = 24 * 60 * 60 * 1000;
        let now = 20_000 * DAY;
        assert!(check_expiry(now, now).is_err());
        assert!(check_expiry(now + DAY, now).is_ok());
        assert!(check_expiry(now + 366 * DAY, now).is_ok());
        let never = check_expiry(now + 400 * DAY, now).unwrap_err().to_string();
        assert!(never.contains("never expires"), "{never}");
    }

    #[test]
    fn the_expiry_comes_from_github_first() {
        let header = "2026-12-31 23:59:59 UTC";
        let want = credential::parse_expiry_header(header).unwrap();
        assert_eq!(
            expiry_of(Some(header), Some(1)).unwrap(),
            Expiry::Github(want)
        );
        assert_eq!(expiry_of(Some(header), None).unwrap(), Expiry::Github(want));
        assert_eq!(expiry_of(None, Some(7)).unwrap(), Expiry::Given(7));
        assert_eq!(
            expiry_of(Some("garbled"), Some(7)).unwrap(),
            Expiry::Given(7)
        );
    }

    /// No header most likely means a token that never expires: refused, and
    /// the refusal names the override rather than asking for a date.
    #[test]
    fn no_expiry_from_github_is_refused_without_the_override() {
        let none = expiry_of(None, None).unwrap_err().to_string();
        assert!(none.contains("reported no expiry"), "{none}");
        assert!(none.contains("never expires"), "{none}");
        assert!(none.contains("--expires YYYY-MM-DD"), "{none}");
        let garbled = expiry_of(Some("garbled"), None).unwrap_err().to_string();
        assert!(garbled.contains("could not read"), "{garbled}");
        assert!(garbled.contains("--expires YYYY-MM-DD"), "{garbled}");
    }

    #[test]
    fn a_pasted_token_is_trimmed_and_bounded() {
        let fake = format!("{}{}", credential::FINE_GRAINED_PREFIX, "A".repeat(40));
        let got = trimmed(format!("  {fake}\n").into_bytes(), "stdin").unwrap();
        assert_eq!(got.expose(), fake);
        let big = vec![b'a'; TOKEN_CAP as usize + 1];
        assert!(trimmed(big, "stdin").is_err());
        assert!(trimmed(vec![0xff, 0xfe], "stdin").is_err());
    }

    #[test]
    fn the_reach_report_flags_both_mismatches_and_an_empty_grant() {
        let reach = vec![
            Reach {
                repo: "owner/both".into(),
                administer: true,
                pulls: true,
                opted_in: true,
                private: true,
            },
            Reach {
                repo: "owner/extra".into(),
                administer: true,
                pulls: true,
                opted_in: false,
                private: true,
            },
            Reach {
                repo: "owner/no-admin".into(),
                administer: false,
                pulls: true,
                opted_in: true,
                private: true,
            },
            Reach {
                repo: "owner/no-pulls".into(),
                administer: true,
                pulls: false,
                opted_in: true,
                private: true,
            },
        ];
        let opted = vec![
            "owner/both".to_string(),
            "owner/no-admin".to_string(),
            "owner/no-pulls".to_string(),
            "owner/missing".to_string(),
        ];
        let text = reach_text(&reach, &opted, "owner", "faber");
        // ADR-0059: the box the gate's read of a pull request's author needs.
        assert!(
            text.contains("lacks Pull requests: read: owner/no-pulls."),
            "{text}"
        );
        assert!(
            !text.contains("lacks Pull requests: read: owner/both"),
            "{text}"
        );
        assert!(
            text.contains("granted, but not opted in: owner/extra."),
            "{text}"
        );
        assert!(
            text.contains("opted in, but not granted: owner/missing."),
            "{text}"
        );
        assert!(
            text.contains("opted in, but not granted: owner/no-admin."),
            "{text}"
        );
        assert!(!text.contains("WARNING"), "{text}");
        let empty = reach_text(&[], &[], "owner", "faber");
        assert!(
            empty.contains("WARNING: this token reaches none"),
            "{empty}"
        );
    }
}
