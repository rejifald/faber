//! Handing the browser a one-time dashboard link (ADR-0028) without putting it
//! in any process's argv: every local account can read argv (`ps`), and could
//! spend the link first. The browser is opened on a small redirect page instead,
//! written where only this account can look.
//!
//! The daemon CLI's `src/dashboard.rs` does the same; the two crates share no
//! code, so this is a copy kept as small as that one.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The file the browser is handed, in [`private_dir`].
pub(crate) const PAGE_NAME: &str = "faber-dashboard.html";

/// The one suffix the app may add to a minted link: the redeem page then goes
/// on to `/settings` instead of `/` (ADR-0028 addendum C, as amended
/// 2026-10-22).
pub(crate) const NEXT_SETTINGS: &str = "&next=settings";

/// Is `url` a dashboard link as the daemon mints them:
/// `http://127.0.0.1:<port>/session?token=` and 64 lowercase hex digits,
/// optionally followed by exactly one [`NEXT_SETTINGS`], nothing else?
pub(crate) fn is_session_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || rest[..digits].parse::<u16>().is_err() {
        return false;
    }
    let Some(token) = rest[digits..].strip_prefix("/session?token=") else {
        return false;
    };
    let token = token.strip_suffix(NEXT_SETTINGS).unwrap_or(token);
    token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A directory only this account can enter: on macOS the per-user temp dir
/// (`$TMPDIR`, mode 0700), on Linux `$XDG_RUNTIME_DIR` and nothing else.
pub(crate) fn private_dir(
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

/// Is `dir` a directory no other account can enter?
pub(crate) fn is_private(dir: &Path) -> bool {
    std::fs::metadata(dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o077 == 0)
}

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

/// The redirect page for `url`; the script reads the URL back from the meta
/// tag's escaped attribute, never from script text.
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

/// Write the redirect page into `dir` as [`PAGE_NAME`], mode 0600: to a fresh
/// `create_new` name first, then renamed into place in one step.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_minted_link_is_a_session_url() {
        let token = "0123456789abcdef".repeat(4);
        assert!(is_session_url(&format!(
            "http://127.0.0.1:8787/session?token={token}"
        )));
        assert!(is_session_url(&format!(
            "http://127.0.0.1:8787/session?token={token}&next=settings"
        )));
        for bad in [
            format!("http://127.0.0.1:8787/session?token={token}&next=settings&next=settings"),
            format!("http://127.0.0.1:8787/session?token={token}&next=Settings"),
            format!("http://127.0.0.1:8787/session?token={token}&next=settings2"),
            format!("http://127.0.0.1:8787/session?token={token}&next=/evil"),
            format!("http://127.0.0.1:8787/session?token={token}&next="),
            format!("http://127.0.0.1:8787/session?token={token}&next=settings#x"),
            format!("http://127.0.0.1:8787/session?next=settings&token={token}"),
            format!(
                "http://127.0.0.1:8787/session?token={}&next=settings",
                &token[1..]
            ),
            "http://127.0.0.1:8787/session?token=&next=settings".to_string(),
            format!("https://127.0.0.1:8787/session?token={token}"),
            format!("http://127.0.0.1.evil.example:8787/session?token={token}"),
            format!("http://127.0.0.1:/session?token={token}"),
            format!("http://127.0.0.1:99999/session?token={token}"),
            format!("http://127.0.0.1:8787/elsewhere?token={token}"),
            format!("http://127.0.0.1:8787/session?token={token}&x=1"),
            format!(
                "http://127.0.0.1:8787/session?token={}",
                token.to_uppercase()
            ),
            format!("http://127.0.0.1:8787/session?token={}", &token[1..]),
            "http://127.0.0.1:8787/session?token=".to_string(),
            String::new(),
        ] {
            assert!(!is_session_url(&bad), "{bad}");
        }
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
        assert_eq!(private_dir(false, "/tmp".into(), None), None);
        assert_eq!(
            private_dir(false, "/tmp".into(), Some(OsString::new())),
            None
        );
    }

    #[test]
    fn the_redirect_page_escapes_the_url() {
        let page = redirect_page("http://127.0.0.1:8787/session?token=ab&x=\"><script>");
        assert!(
            page.contains("token=ab&amp;x=&quot;&gt;&lt;script&gt;"),
            "{page}"
        );
        assert!(!page.contains("\"><script>"), "{page}");
    }

    #[test]
    fn the_page_is_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("faber-menubar-dash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_private(&dir));
        let path = write_redirect(&dir, "http://127.0.0.1:8787/session?token=ab").unwrap();
        assert_eq!(path, dir.join(PAGE_NAME));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from(PAGE_NAME)]);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(!is_private(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
