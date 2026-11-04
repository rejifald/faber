//! `faber dashboard` — open the dashboard through a one-time link (ADR-0028).
//!
//! The browser dashboard on loopback accepts no caller without a session. A
//! session starts from a link the daemon mints for a caller it has already
//! identified on the control socket: this command asks for one, prints it, and
//! hands it to the default browser.
//!
//! The link never goes into a process's argv: every local account can read
//! argv (`ps`, `/proc/*/cmdline`) and could spend the link first. The browser
//! is handed a small file instead, written where only this account can look,
//! that redirects to the link.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::control;
use crate::drain::client_error_message;

/// Every dashboard link starts with this; anything else is not one.
const LINK_PREFIX: &str = "http://127.0.0.1:";

/// The file the browser is handed, in [`private_dir`].
const PAGE_NAME: &str = "faber-dashboard.html";

/// The link in a `POST /api/session` reply, if it is a loopback dashboard link
/// on `port`, the port this command asked through.
///
/// The reply is the daemon's to write, and the URL goes to a browser: only a
/// plain `http://127.0.0.1:<port>/…` URL with no whitespace or control
/// characters is accepted.
pub(crate) fn parse_link(body: &str, port: u16) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let url = v.get("url")?.as_str()?;
    let rest = url.strip_prefix(LINK_PREFIX)?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || !rest[digits..].starts_with('/') {
        return None;
    }
    if rest[..digits].parse::<u16>().ok()? != port {
        return None;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(url.to_string())
}

/// What a `404` from `POST /api/session` means.
pub(crate) fn predates_message(port: u16) -> String {
    format!(
        "this daemon predates the dashboard link (it has no /api/session). \
         Open http://127.0.0.1:{port} directly."
    )
}

/// The program that opens a file in the default browser, on this OS.
fn opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// A directory only this account can enter, for the redirect page: on macOS the
/// per-user temp dir (`$TMPDIR`, mode 0700), on Linux `$XDG_RUNTIME_DIR` and
/// nothing else (`/tmp` is shared). `None` means there is no such place.
fn private_dir(
    is_macos: bool,
    temp_dir: PathBuf,
    xdg_runtime: Option<OsString>,
) -> Option<PathBuf> {
    let dir = if is_macos {
        temp_dir
    } else {
        PathBuf::from(xdg_runtime.filter(|d| !d.is_empty())?)
    };
    dir.is_absolute().then_some(dir)
}

/// Is `dir` a directory no other account can enter? A `$TMPDIR` pointing at
/// `/tmp` would otherwise pass for private.
fn is_private(dir: &Path) -> bool {
    std::fs::metadata(dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o077 == 0)
}

/// `text` safe inside an HTML attribute or element.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The redirect page for `url`. The script reads the URL back from the meta
/// tag's escaped attribute, so the URL is never spliced into script text.
pub(crate) fn redirect_page(url: &str) -> String {
    let url = html_escape(url);
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta id=\"go\" http-equiv=\"refresh\" content=\"0;url={url}\">\n\
         <title>faber</title>\n\
         <script>location.replace(document.getElementById('go').content.slice(6));</script>\n\
         </head><body><a href=\"{url}\">Open the faber dashboard</a></body></html>\n"
    )
}

/// Write the redirect page for `url` into `dir` as [`PAGE_NAME`], mode 0600:
/// to a fresh name first (`create_new`, so nothing planted there is reused),
/// then renamed over the old page in one step.
pub(crate) fn write_redirect(dir: &Path, url: &str) -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{PAGE_NAME}.{}.{nanos}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = file
        .write_all(redirect_page(url).as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let path = dir.join(PAGE_NAME);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(path)
}

/// Ask the daemon for a dashboard link, print it and open it.
pub async fn run(socket: &Path, port: u16) -> anyhow::Result<()> {
    let fallback = control::fallback_here(port);
    let (status, body) = control::request(
        socket,
        fallback,
        reqwest::Method::POST,
        "/api/session",
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", client_error_message(&e)))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("{}", predates_message(port));
    }
    if !status.is_success() {
        anyhow::bail!("the daemon refused to mint a dashboard link ({status}). Check its log.");
    }
    let Some(url) = parse_link(&body, port) else {
        anyhow::bail!("the daemon's reply held no usable dashboard link");
    };
    println!("{url}");
    let paste = "open the link above yourself — it works once, for about a minute.";
    let Some(dir) = private_dir(
        cfg!(target_os = "macos"),
        std::env::temp_dir(),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .filter(|d| is_private(d)) else {
        eprintln!("no private directory to hand the browser the link through; {paste}");
        return Ok(());
    };
    let page = match write_redirect(&dir, &url) {
        Ok(page) => page,
        Err(e) => {
            eprintln!(
                "could not write {}: {e}; {paste}",
                dir.join(PAGE_NAME).display()
            );
            return Ok(());
        }
    };
    // The file path is one argument, never a shell string, and holds no token.
    match Command::new(opener()).arg(&page).status() {
        Ok(s) if s.success() => {}
        _ => eprintln!("could not open a browser ({}); {paste}", opener()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_link_is_accepted() {
        let body = r#"{"url":"http://127.0.0.1:8787/session?token=ab12","expires_in_secs":60}"#;
        assert_eq!(
            parse_link(body, 8787).as_deref(),
            Some("http://127.0.0.1:8787/session?token=ab12")
        );
    }

    #[test]
    fn anything_else_is_not_a_dashboard_link() {
        for url in [
            "https://127.0.0.1:8787/session?token=a",
            "http://evil.example/session?token=a",
            "http://127.0.0.1.evil.example/x",
            "http://127.0.0.1:/x",
            "http://127.0.0.1:80@evil.example/",
            "http://127.0.0.1:8787/a b",
            "http://127.0.0.1:8787/a\nb",
            "file:///etc/passwd",
            "",
        ] {
            let body = serde_json::json!({ "url": url }).to_string();
            assert_eq!(parse_link(&body, 8787), None, "{url:?}");
        }
        assert_eq!(parse_link("not json", 8787), None);
        assert_eq!(parse_link("{}", 8787), None);
    }

    #[test]
    fn a_link_on_another_port_is_refused() {
        let body = r#"{"url":"http://127.0.0.1:9999/session?token=ab12"}"#;
        assert_eq!(parse_link(body, 8787), None);
        assert!(parse_link(body, 9999).is_some());
        let huge = r#"{"url":"http://127.0.0.1:99999/session?token=ab12"}"#;
        assert_eq!(parse_link(huge, 34463), None, "no wrap past u16");
    }

    #[test]
    fn the_page_goes_only_where_this_account_alone_can_look() {
        let xdg = Some(OsString::from("/run/user/1000"));
        assert_eq!(
            private_dir(true, "/var/folders/ab/T/".into(), xdg.clone()),
            Some(PathBuf::from("/var/folders/ab/T/"))
        );
        assert_eq!(
            private_dir(false, "/tmp".into(), xdg),
            Some(PathBuf::from("/run/user/1000"))
        );
        // Linux without XDG_RUNTIME_DIR: never /tmp.
        assert_eq!(private_dir(false, "/tmp".into(), None), None);
        assert_eq!(
            private_dir(false, "/tmp".into(), Some(OsString::new())),
            None
        );
        assert_eq!(private_dir(false, "/tmp".into(), Some("run".into())), None);
    }

    #[test]
    fn a_shared_directory_is_not_private() {
        let dir = std::env::temp_dir().join(format!("faber-dash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_private(&dir));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(!is_private(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!is_private(&dir), "a missing dir is not private");
    }

    #[test]
    fn the_redirect_page_escapes_the_url() {
        let page = redirect_page("http://127.0.0.1:8787/session?token=ab&x=\"><script>");
        assert!(
            page.contains("token=ab&amp;x=&quot;&gt;&lt;script&gt;"),
            "{page}"
        );
        assert!(!page.contains("\"><script>"), "{page}");
        assert!(page.contains("http-equiv=\"refresh\""), "{page}");
        assert!(page.contains("location.replace"), "{page}");
    }

    #[test]
    fn the_page_is_written_owner_only_and_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("faber-dash-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = "http://127.0.0.1:8787/session?token=ab12";
        let path = write_redirect(&dir, url).unwrap();
        assert_eq!(path, dir.join(PAGE_NAME));
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), redirect_page(url));
        // A second write replaces it, and leaves no temp file behind.
        let url2 = "http://127.0.0.1:8787/session?token=cd34";
        write_redirect(&dir, url2).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("cd34"));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from(PAGE_NAME)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_daemon_is_pointed_at_its_port() {
        let msg = predates_message(9001);
        assert!(msg.contains("http://127.0.0.1:9001"), "{msg}");
        assert!(msg.contains("predates"), "{msg}");
    }
}
