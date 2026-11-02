//! Keeps the privacy gate's denylist current, so that nobody keeps a list.
//!
//! The names come from what the owner's machine can already see, read at run
//! time and never written down by this tool:
//!
//! - the private repositories and container packages of the account `gh` is
//!   signed in to, and the organisations it belongs to;
//! - this machine's host names.
//!
//! Two local files, never committed, adjust that set. Each holds one name per
//! line; a line starting with `#` is a comment.
//!
//! - `$HOME/.config/faber-privacy/extra-names`: names no source shows, such as
//!   another machine's host names or a private domain.
//! - `$HOME/.config/faber-privacy/not-private`: names the sources find that are
//!   public anyway, or ordinary words that would only raise false hits. A name
//!   comes off whole: the part before a dot and the windows of a long name too.
//!   A first word it shares with another private repository stays listed: if
//!   that word is public too, list it in `not-private` itself.
//!
//! With `FABER_PRIVACY_KEY` exported (ADR-0058):
//!
//! ```sh
//! cargo run --example privacy_denylist              # exits 1 when DENIED is out of date
//! cargo run --example privacy_denylist -- --write   # brings DENIED up to date
//! ```
//!
//! It prints counts, never names. `--list` prints the names themselves, for a
//! local terminal only, and needs no key. A name stays listed after its source
//! is gone: one that was private once stays worth catching. Only `not-private`
//! takes a name off. After `--write`, run the tree gate with the key: a hit on an
//! ordinary word means that word belongs in `not-private`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use faber::privacy::{key_from_env, name_token, words, DENIED, MAX_JOIN, MIN_TOKEN};

fn main() {
    let mut write = false;
    let mut list = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--write" => write = true,
            "--list" => list = true,
            _ => fail(&format!(
                "unknown argument {arg:?}: expected --write or --list"
            )),
        }
    }
    let derived = derive().unwrap_or_else(|e| fail(&e));
    if list {
        for token in &derived.deny {
            println!("{token}");
        }
        return;
    }
    let key = match key_from_env() {
        Ok(Some(key)) => key,
        Ok(None) => fail(
            "export FABER_PRIVACY_KEY first (ADR-0058): a digest made without it matches nothing",
        ),
        Err(e) => fail(e.describe()),
    };

    let current: BTreeSet<String> = DENIED.iter().map(|d| d.to_string()).collect();
    let never: BTreeSet<String> = derived.never.iter().map(|t| key.digest(t)).collect();
    let wanted: BTreeSet<String> = current
        .iter()
        .cloned()
        .chain(derived.deny.iter().map(|t| key.digest(t)))
        .filter(|d| !never.contains(d))
        .collect();
    let added = wanted.difference(&current).count();
    let removed = current.difference(&wanted).count();
    println!(
        "{} name(s) found; DENIED holds {}; {added} to add, {removed} to take off",
        derived.deny.len(),
        current.len()
    );
    if added == 0 && removed == 0 {
        return;
    }
    if !write {
        eprintln!("Run again with --write to update DENIED in src/privacy.rs.");
        std::process::exit(1);
    }

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/privacy.rs");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fail(&format!("cannot read src/privacy.rs: {e}")));
    let text = with_denied(&text, &wanted).unwrap_or_else(|e| fail(&e));
    std::fs::write(&path, text)
        .unwrap_or_else(|e| fail(&format!("cannot write src/privacy.rs: {e}")));
    println!(
        "DENIED now holds {} digest(s). Run the tree gate with the key next.",
        wanted.len()
    );
}

fn fail(message: &str) -> ! {
    eprintln!("privacy_denylist: {message}");
    std::process::exit(1);
}

/// Reads every source. A source that fails stops the run: a partial list would
/// pass for a complete one.
fn derive() -> Result<Derived, String> {
    let login = gh(&["api", "user", "--jq", ".login"])?.trim().to_owned();
    let display = gh(&["api", "user", "--jq", ".name // empty"])?
        .trim()
        .to_owned();
    let repos = lines(&gh(&[
        "repo",
        "list",
        login.as_str(),
        "--visibility",
        "private",
        "--limit",
        "1000",
        "--json",
        "name",
        "--jq",
        ".[].name",
    ])?);
    let orgs = lines(&gh(&[
        "api",
        "--paginate",
        "user/orgs",
        "--jq",
        ".[].login",
    ])?);
    let packages = lines(&gh(&[
        "api",
        "--paginate",
        "user/packages?package_type=container",
        "--jq",
        ".[].name",
    ])?);
    let dir = config_dir()?;
    let mut others = orgs;
    others.extend(packages);
    others.extend(host_names());
    others.extend(read_lines(&dir.join("extra-names"))?);
    Ok(candidates(&Sources {
        repos,
        others,
        not_private: read_lines(&dir.join("not-private"))?,
        own: vec![login, display],
    }))
}

fn gh(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run gh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// This machine's names, as its shell prompt and the network show them. A
/// command this platform lacks (`scutil` off macOS) is skipped.
fn host_names() -> Vec<String> {
    let commands: [(&str, &[&str]); 3] = [
        ("hostname", &["-s"]),
        ("scutil", &["--get", "LocalHostName"]),
        ("scutil", &["--get", "ComputerName"]),
    ];
    let mut names = Vec::new();
    for (command, args) in commands {
        if let Ok(out) = Command::new(command).args(args).output() {
            if out.status.success() {
                names.extend(lines(&String::from_utf8_lossy(&out.stdout)));
            }
        }
    }
    names
}

/// `$XDG_CONFIG_HOME/faber-privacy`, else `$HOME/.config/faber-privacy`: outside
/// every checkout and worktree, so one copy serves them all.
fn config_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("faber-privacy"));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("faber-privacy"))
}

/// The lines of a local file. A missing file is an empty list.
fn read_lines(path: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(lines(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!(
            "cannot read a file in the faber-privacy config: {e}"
        )),
    }
}

/// Trimmed, non-empty lines, without `#` comments.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// What [`derive`] read, kept apart from the reading so the rules in
/// [`candidates`] can be tested.
struct Sources {
    /// Private repository names.
    repos: Vec<String>,
    /// Organisations, packages, host names and the extra names.
    others: Vec<String>,
    /// Names that are not private after all.
    not_private: Vec<String>,
    /// The account's login and display name: public, and in every commit.
    own: Vec<String>,
}

/// Tokens ([`name_token`]) to list, and tokens never to list.
struct Derived {
    deny: BTreeSet<String>,
    never: BTreeSet<String>,
}

/// The tokens to list:
///
/// - every name from the sources;
/// - the part of a dotted name before its first dot (`widgetco.example` →
///   `widgetco`), so the name is caught with or without its domain;
/// - for a name of more than [`MAX_JOIN`] words, each run of `MAX_JOIN`
///   adjacent words (`my-app-v2-api` → `myappv2`, `appv2api`): the scan joins
///   no more words than that, so written with separators the whole name never
///   forms one token;
/// - a first word that two or more repository names share (`gizmo-api` and
///   `gizmo-web` → `gizmo`), so a project is caught by its own name, not only
///   by its repositories'.
///
/// Never listed:
///
/// - `not_private`, and every token such a name expands to ([`listed_tokens`]:
///   its own, its dotted head, its windows), even one that another private name
///   shares: declaring a name public takes off all it put on by itself. A first
///   word that another private repository shares stays listed until that word
///   is in `not_private` itself;
/// - the account's login and display name and each word of them;
/// - tokens shorter than [`MIN_TOKEN`], which the scan never digests.
///
/// faber's own repositories are not read at all.
fn candidates(sources: &Sources) -> Derived {
    let mut names: Vec<&str> = Vec::new();
    let mut firsts: HashMap<String, usize> = HashMap::new();
    for repo in &sources.repos {
        if first_word(repo).eq_ignore_ascii_case("faber") {
            continue;
        }
        names.push(repo);
        *firsts.entry(name_token(first_word(repo))).or_default() += 1;
    }
    names.extend(sources.others.iter().map(String::as_str));

    let mut deny = BTreeSet::new();
    for name in names {
        deny.extend(listed_tokens(name));
    }
    deny.extend(
        firsts
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .map(|(word, _)| word),
    );

    let mut never: BTreeSet<String> = sources
        .not_private
        .iter()
        .flat_map(|n| listed_tokens(n))
        .collect();
    for own in &sources.own {
        never.insert(name_token(own));
        never.extend(own.split_whitespace().map(name_token));
    }
    never.remove("");
    deny.retain(|token| token.len() >= MIN_TOKEN && !never.contains(token));
    Derived { deny, never }
}

/// Every token one name stands for: its own ([`name_token`]); for a dotted name,
/// the part before its first dot; and each run of [`MAX_JOIN`] adjacent words.
/// Not yet filtered: it may hold an empty or short token, and repeats.
///
/// [`candidates`] lists these for a source name and takes the same ones off for
/// a `not_private` name, so the two sides cannot drift apart.
fn listed_tokens(name: &str) -> Vec<String> {
    let mut tokens = vec![name_token(name)];
    if let Some((head, _)) = name.split_once('.') {
        tokens.push(name_token(head));
    }
    // Joined and lowercased as the scan joins them. A name of exactly MAX_JOIN
    // words yields its own token again, and a shorter one nothing.
    let spans = words(name);
    for window in spans.windows(MAX_JOIN) {
        tokens.push(
            window
                .iter()
                .map(|w| name[w.clone()].to_ascii_lowercase())
                .collect(),
        );
    }
    tokens
}

/// A repository name's first word: up to its first `-`, `_` or `.`.
fn first_word(name: &str) -> &str {
    name.split(['-', '_', '.']).next().unwrap_or(name)
}

/// `text` (src/privacy.rs) with `DENIED` holding exactly `digests`, one per line
/// and sorted, as rustfmt lays it out.
fn with_denied(text: &str, digests: &BTreeSet<String>) -> Result<String, String> {
    const OPEN: &str = "pub const DENIED: &[&str] = &[";
    let start = text
        .find(OPEN)
        .ok_or("src/privacy.rs has no `pub const DENIED` array")?
        + OPEN.len();
    let end = start
        + text[start..]
            .find("];")
            .ok_or("the DENIED array in src/privacy.rs is not closed")?;
    let mut body: String = digests.iter().map(|d| format!("\n    \"{d}\",")).collect();
    if !body.is_empty() {
        body.push('\n');
    }
    Ok(format!("{}{body}{}", &text[..start], &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(repos: &[&str], others: &[&str], not_private: &[&str]) -> Sources {
        let owned = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
        Sources {
            repos: owned(repos),
            others: owned(others),
            not_private: owned(not_private),
            own: vec!["octocat".into(), "Ada Lovelace".into()],
        }
    }

    fn denied(sources: &Sources) -> Vec<String> {
        candidates(sources).deny.into_iter().collect()
    }

    #[test]
    fn every_source_name_is_listed_as_its_token() {
        let s = sources(&["Widget-Co"], &["widgetco-runner", "Build-Host-7"], &[]);
        assert_eq!(denied(&s), ["buildhost7", "widgetco", "widgetcorunner"]);
    }

    #[test]
    fn a_dotted_name_is_listed_with_and_without_its_domain() {
        let s = sources(&["widgetco.example"], &[], &[]);
        assert_eq!(denied(&s), ["widgetco", "widgetcoexample"]);
    }

    #[test]
    fn a_name_longer_than_a_join_is_listed_by_its_windows_too() {
        use faber::privacy::tokens;

        let name = "my-app-v2-api";
        assert_eq!(words(name).len(), MAX_JOIN + 1, "my, app, v, 2, api");
        let d = denied(&sources(&[], &[name], &[]));
        assert_eq!(d, ["appv2api", "myappv2", "myappv2api"]);
        // Written with separators the whole name never forms a token, which is
        // why the windows are listed: the scan meets them instead.
        let scanned: Vec<String> = tokens(&format!("deploy {name} now"))
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert!(!scanned.contains(&name_token(name)));
        for window in ["myappv2", "appv2api"] {
            assert!(scanned.contains(&window.to_string()), "{window:?}");
        }
    }

    #[test]
    fn a_first_word_two_repositories_share_is_listed() {
        let d = denied(&sources(&["gizmo-api", "gizmo-web", "solo-app"], &[], &[]));
        assert!(d.contains(&"gizmo".to_string()));
        assert!(
            !d.contains(&"solo".to_string()),
            "one repository's first word is not a project name"
        );

        // Declaring one of them public takes off its own name, not the word the
        // other still shares: that word still names a private project.
        let d = denied(&sources(&["gizmo-api", "gizmo-web"], &[], &["gizmo-api"]));
        assert!(d.contains(&"gizmo".to_string()));
        assert!(!d.contains(&"gizmoapi".to_string()));
        // Only the word itself takes it off.
        let d = denied(&sources(&["gizmo-api", "gizmo-web"], &[], &["gizmo"]));
        assert!(!d.contains(&"gizmo".to_string()));
    }

    #[test]
    fn faber_the_account_and_short_names_are_never_listed() {
        let s = sources(
            &["faber", "faber-sandbox", "xy", "app-api", "app-web"],
            &["octocat", "Lovelace-Lab"],
            &[],
        );
        let derived = candidates(&s);
        assert!(!derived.deny.iter().any(|t| t.starts_with("faber")));
        assert!(!derived.deny.contains("xy"));
        assert!(
            !derived.deny.contains("app"),
            "three letters are never digested"
        );
        assert!(!derived.deny.contains("octocat"));
        assert!(
            derived.deny.contains("lovelacelab"),
            "a name that merely contains a word of the owner's name is still listed"
        );
        for own in ["octocat", "adalovelace", "lovelace"] {
            assert!(derived.never.contains(own), "{own}");
        }
    }

    #[test]
    fn a_not_private_name_is_never_listed() {
        let derived = candidates(&sources(&["gizmo-api"], &["PublicOrg"], &["public-org"]));
        assert!(!derived.deny.contains("publicorg"));
        assert!(derived.never.contains("publicorg"));
        assert!(derived.deny.contains("gizmoapi"));
    }

    #[test]
    fn a_not_private_name_takes_off_everything_listing_it_would_put_on() {
        // More than MAX_JOIN words and a domain: its own token, the part before
        // the dot, and three windows of MAX_JOIN words.
        let name = "my-app-v2-api.example";
        assert_eq!(
            words(name).len(),
            MAX_JOIN + 2,
            "my, app, v, 2, api, example"
        );
        let derived = candidates(&sources(&[], &[name], &[name]));
        assert!(derived.deny.is_empty(), "{:?}", derived.deny);
        for token in ["myappv2", "appv2api", "v2apiexample", "myappv2api"] {
            assert!(derived.never.contains(token), "{token}");
        }

        // The dotted head alone, which a name of up to MAX_JOIN words has too.
        let name = "widgetco.example";
        let derived = candidates(&sources(&[], &[name], &[name]));
        assert!(derived.deny.is_empty(), "{:?}", derived.deny);
        assert!(derived.never.contains("widgetco"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_names() {
        assert_eq!(lines("# a note\n\n  widgetco  \n"), ["widgetco"]);
    }

    #[test]
    fn denied_is_rewritten_in_place() {
        let before = "a\npub const DENIED: &[&str] = &[];\nb\n";
        let digest = "ab".repeat(32);
        let one = BTreeSet::from([digest.clone()]);
        let after = with_denied(before, &one).unwrap();
        assert_eq!(
            after,
            format!("a\npub const DENIED: &[&str] = &[\n    \"{digest}\",\n];\nb\n")
        );
        assert_eq!(with_denied(&after, &BTreeSet::new()).unwrap(), before);
    }
}
