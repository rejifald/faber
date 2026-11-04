//! The panel window: the dashboard in an app window, with a browser tab's rights
//! and no more (ADR-0028 addendum C, decision 1; ADR-0041).
//!
//! Every open loads a fresh one-time link the caller minted on the control
//! socket. The link goes straight to the webview, never into an opener's argv or
//! a file. The window holds no IPC: no capability names it, and none lists the
//! dashboard's origin under `remote`, so Tauri rejects every command, plugin call
//! and event listener a page in it sends. It stays on the dashboard's origin: any
//! other `http`/`https` URL goes to the default browser, any other scheme nowhere.

use std::sync::Mutex;

use tauri::{
    webview::NewWindowResponse, AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_opener::OpenerExt;

/// The window's label. Kept from the old panel, so the updater's "is the panel
/// open" check needs no change.
pub(crate) const LABEL: &str = "main";

/// The origin the window may stay on: the one of the link it last loaded. A
/// restarted daemon may answer on another port, so each open sets it anew.
static DASHBOARD: Mutex<Option<Url>> = Mutex::new(None);

/// `withGlobalTauri` injects `window.__TAURI__` into every webview, and the
/// popover reads it (`ui/src/entries/PopoverApp.tsx`), so it stays on for the
/// app. This runs after the injected scripts, in this window only, and blanks
/// the two names the bundle defines: `__TAURI__` and the `var __TAURI_IIFE__`
/// behind it (a `var` cannot be deleted, so it is overwritten). It is tidiness,
/// not the guard: Tauri's own `__TAURI_INTERNALS__` stays whatever the setting,
/// and what stops a page here is the ACL, which rejects every call from a
/// remote origin no capability names.
const DROP_GLOBAL: &str =
    "try { delete window.__TAURI__; window.__TAURI_IIFE__ = undefined; } catch (_) {}";

/// What the window does with a navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nav {
    /// Load it: it is on the dashboard's origin.
    Stay,
    /// Hand it to the default browser and load nothing.
    Browser,
    /// Load nothing and open nothing.
    Drop,
}

/// Where a navigation to `to` goes, for a window on `dashboard`'s origin.
pub(crate) fn judge(dashboard: Option<&Url>, to: &Url) -> Nav {
    if dashboard.is_some_and(|d| d.origin() == to.origin()) {
        return Nav::Stay;
    }
    match to.scheme() {
        "http" | "https" => Nav::Browser,
        _ => Nav::Drop,
    }
}

/// Apply [`judge`] to a navigation or a new-window request: `true` to load it.
fn guard(app: &AppHandle, to: &Url) -> bool {
    let dashboard = DASHBOARD.lock().unwrap().clone();
    match judge(dashboard.as_ref(), to) {
        Nav::Stay => true,
        Nav::Browser => {
            let _ = app.opener().open_url(to.as_str(), None::<&str>);
            false
        }
        Nav::Drop => false,
    }
}

/// Show and focus the open panel without touching its page: an open panel is
/// focused, not re-minted. **Main thread only** (AppKit).
pub(crate) fn focus(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Open the panel on `link`, a one-time link the caller minted and checked with
/// [`crate::dashboard_link::is_session_url`]: build the window, or point the
/// open one at it, then show and focus it. **Main thread only** (AppKit).
pub(crate) fn open(app: &AppHandle, link: &str) -> Result<(), String> {
    let url = Url::parse(link).map_err(|e| e.to_string())?;
    *DASHBOARD.lock().unwrap() = Some(url.clone());
    let win = match app.get_webview_window(LABEL) {
        Some(win) => {
            win.navigate(url).map_err(|e| e.to_string())?;
            win
        }
        None => {
            let nav = app.clone();
            let popup = app.clone();
            WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(url))
                .title("faber")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .resizable(true)
                .skip_taskbar(true)
                .initialization_script(DROP_GLOBAL)
                .on_navigation(move |to| guard(&nav, to))
                // A link meant for a new tab gets the same rule, and never a
                // second window of this app.
                .on_new_window(move |to, _| {
                    // A same-origin link (a modified click on a view) loads in
                    // this window, since a second one would hold no session.
                    if guard(&popup, &to) {
                        if let Some(win) = popup.get_webview_window(LABEL) {
                            let _ = win.navigate(to);
                        }
                    }
                    NewWindowResponse::Deny
                })
                .build()
                .map_err(|e| e.to_string())?
        }
    };
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn the_window_stays_on_the_dashboards_origin_only() {
        let d = url(
            "http://127.0.0.1:8787/session?token=0000000000000000000000000000000000000000000000000000000000000000",
        );
        let d = Some(&d);
        for on in [
            "http://127.0.0.1:8787/",
            "http://127.0.0.1:8787/activity",
            "http://127.0.0.1:8787/settings?x=1#y",
        ] {
            assert_eq!(judge(d, &url(on)), Nav::Stay, "{on}");
        }
        for off in [
            "http://127.0.0.1:8788/",
            "http://localhost:8787/",
            "https://127.0.0.1:8787/",
            "https://github.com/owner/demo/actions/runs/1",
            "http://example.com/",
        ] {
            assert_eq!(judge(d, &url(off)), Nav::Browser, "{off}");
        }
        for dropped in [
            "file:///etc/passwd",
            "tauri://localhost/popover/index.html",
            "javascript:alert(1)",
            "data:text/html,hi",
            "x-apple.systempreferences:com.apple.preference",
            "about:blank",
        ] {
            assert_eq!(judge(d, &url(dropped)), Nav::Drop, "{dropped}");
        }
    }

    #[test]
    fn with_no_dashboard_nothing_stays() {
        assert_eq!(judge(None, &url("http://127.0.0.1:8787/")), Nav::Browser);
        assert_eq!(judge(None, &url("file:///tmp/x")), Nav::Drop);
    }
}
