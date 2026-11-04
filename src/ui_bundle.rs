//! The React dashboard, embedded at build time (ADR-0042 § 7).
//!
//! `build.rs` turns every file under `ui/dist/dashboard` into an entry of
//! [`FILES`]. When `ui/` has not been built the table is empty, and [`asset`]
//! answers the index with [`NOT_BUILT_HTML`] instead, so a bare `cargo build`
//! still yields a daemon that says why there is nothing to show.
//!
//! Two kinds of route serve it (ADR-0028 addendum C, as amended 2026-10-22 and
//! 2026-10-28), both in `src/web.rs`. `GET` on each view (`/`, `/activity`,
//! `/memory`, `/settings`, ADR-0054 § 5) answers the shell, `index.html`, on the
//! session cookie alone. `GET`/`HEAD /assets/*` answers one content-hashed bundle file
//! without a session (decision 2): a browser fetching a script or a font sends
//! only the cookie, and these files carry nothing sensitive, as the data stays
//! behind `/api/*`. The front door exempts exactly that path, and the handler
//! looks the raw path up whole in [`FILES`], reading nothing from disk. The
//! sign-in page is still the body of every `401`, so anything it shows, the fonts
//! included, is inlined in it rather than fetched.

include!(concat!(env!("OUT_DIR"), "/ui_dashboard.rs"));

/// The page served in place of the dashboard when `ui/` was not built.
pub const NOT_BUILT_HTML: &str = r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>faber</title></head>
<body>
<h1>The dashboard was not built</h1>
<p>This faber was compiled without its web UI. Build it with
<code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code>, then rebuild faber.</p>
</body>
</html>
"#;

/// Whether this binary carries the built dashboard.
pub fn built() -> bool {
    !FILES.is_empty()
}

/// The embedded file at `path` (relative to the bundle root, `""` meaning the
/// index), with its content type, or `None` when there is no such file.
pub fn asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    let path = match path.trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    if !built() {
        return (path == "index.html").then_some((NOT_BUILT_HTML.as_bytes(), content_type(path)));
    }
    FILES
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, bytes)| (*bytes, content_type(path)))
}

/// The `Content-Type` of a bundle file, from its extension.
pub fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_always_there() {
        let (body, kind) = asset("/").expect("an index, built or not");
        assert_eq!(kind, "text/html; charset=utf-8");
        assert!(!body.is_empty());
        assert_eq!(asset("index.html"), asset(""));
    }

    #[test]
    fn an_unbuilt_daemon_says_so_and_serves_nothing_else() {
        if built() {
            return;
        }
        assert_eq!(asset("/").map(|(b, _)| b), Some(NOT_BUILT_HTML.as_bytes()));
        assert_eq!(asset("assets/index.js"), None);
    }

    #[test]
    fn a_built_bundle_carries_its_fonts_and_their_licences() {
        if !built() {
            return;
        }
        let names: Vec<&str> = FILES.iter().map(|(n, _)| *n).collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("assets/Inter-Regular") && n.ends_with(".woff2")),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("assets/IBMPlexMono-Regular")),
            "{names:?}"
        );
        for licence in [
            "licenses/OFL-Inter.txt",
            "licenses/OFL-IBMPlexMono.txt",
            "licenses/lucide.LICENSE.txt",
        ] {
            assert!(
                asset(licence).is_some(),
                "{licence} missing from the bundle"
            );
        }
        let index = String::from_utf8_lossy(asset("/").expect("index").0).into_owned();
        assert!(
            index.contains("./assets/"),
            "asset URLs must stay relative: {index}"
        );
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("assets/a.woff2"), "font/woff2");
        assert_eq!(
            content_type("assets/a.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("noext"), "application/octet-stream");
    }
}
