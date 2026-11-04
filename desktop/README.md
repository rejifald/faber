# faber — menu-bar app

A native macOS menu-bar client for the faber daemon. It shows faber's live status
at a glance in the menu bar and gives you the dashboard + controls without opening a
browser.

The menu-bar icon is a monochrome template glyph (auto-tints to light/dark) whose
shape encodes state, with the jobs as a stacked fraction beside it:

```
mark 3/10  mark + fraction   → 3 jobs running of 10 claimed (7 waiting)
mark –   mark + dash         → idle (no job claimed)
mark ‖   mark + two bars     → paused, draining, or held
mark !   mark + alert        → unhealthy
mark     faded, struck through → daemon offline
```

The fraction is jobs whose steps are executing over every job faber holds a claim on;
the rest wait (queued, starting, or frozen — the hover tooltip splits them). Under
demand provisioning every host claims every queued job it sees, so the waiting part is
the shared GitHub queue, which the peer may take. The row that carries the news is
drawn at full strength: running while anything runs, else the total.

The mark is a cube whose front-corner core is cut free of the shell; its masters live in
`design/mark/` and the app icons in `src-tauri/icons/` are rendered from them.

Clicking the icon opens the popover (built from `ui/`, see
[docs/design/popover.md](../docs/design/popover.md)); its ⋯ menu has Open panel and Quit. "Open
panel" (⌘⇧F) opens the daemon's dashboard in an app window (ADR-0041): every open mints a
fresh one-time session link on the control socket and hands it straight to the window, which
has a browser tab's rights and no Tauri IPC (ADR-0028 addendum C, decision 1). A link to
another origin opens in the default browser. Settings, the whole `config.toml`, is the
dashboard's view at `/settings`, which the app can also open in the browser through a fresh
one-time link (the `open_settings` command), as it opens the dashboard itself. The page is rendered
from a schema the daemon serves with the config (`faber::settings::FORM_SCHEMA`), so a
new config key appears there without a change to this app; the daemon validates and
writes the file.

## How it works

The [faber daemon](../README.md) already serves a control panel on
its Unix control socket (`faber/src/web.rs`) with a JSON state endpoint:

```
GET  /api/state       → StateView (owner, profiles, counts, inflight, recent, …)
POST /api/reconcile   → force a reconcile now
POST /api/pause       → start nothing new (claim nothing, start no parked claim)
POST /api/resume      → resume + reconcile
POST /api/hold        → hold Docker admission, with a reason ({"reason": "…"}); persists
POST /api/unhold      → release the hold
GET  /api/settings    → the config file + the schema to render it with
POST /api/settings    → validate and write the config file
```

This app is a thin native client over that contract. The Rust side (`src-tauri`)
polls `/api/state` every 2s, renders the result into the tray (title + menu) and
`emit`s it to the popover, by label. Controls POST to `/api/*`. The popover never talks
to the daemon directly (it's cross-origin with no CORS headers) — all its HTTP goes
through Rust, so nothing new is exposed on the network. The panel window is the
dashboard itself, on its loopback session, and gets no event and no command.

If the daemon is down, the app shows an explicit **offline** state (which the
localhost dashboard itself can't, since the page fails to load).

## Configuration

- `FABER_SOCKET` — override the daemon's control-socket path. Default: the system
  path (`/Library/Application Support/faber/control.sock` on macOS,
  `/var/lib/faber/control.sock` on Linux) when `/etc/faber/setup.json` exists, else the
  per-user legacy path. `FABER_URL` is no longer read.

## Develop

```sh
(cd ui && npm ci)     # the popover is built from ui/ (ADR-0042); needs Node 22.12+
cd desktop
npm install
npm run tauri dev     # run against your live daemon
```

`tauri dev` builds `ui/` and assembles `desktop/dist` once (`scripts/assemble.mjs`), then
serves that: there is no hot reload. Restart it to see a change, or work on the popover in
Storybook (`npm run storybook` in `ui/`), where every state has a story.

Requires the faber daemon running (`cargo run -- run`, or the installed
service — see the root [README](../README.md) and
[deploy/README.md](../deploy/README.md)).

## Install (launch at login)

`sudo faber setup` installs it: it takes `faber-app-aarch64-apple-darwin.tar.gz` (a
release's asset, or a source build's per [docs/manual-update.md](../docs/manual-update.md))
from the directory the launcher and guest agent are in, puts it at `/Applications/faber.app`
as yours, and opens it. Every later setup run replaces an app whose `FaberBuild` stamp is not
its own binary's, so the app moves with the daemon; between runs it updates itself from
releases (ADR-0034, amended 2026-10-28 and 2026-10-31).

For a local build, `deploy/install-app.sh` does all of this. By hand:

```sh
(cd ui && npm ci)
cd desktop
npm run tauri build                                   # builds ui/, then faber.app
cp -R src-tauri/target/release/bundle/macos/faber.app /Applications/
open /Applications/faber.app
```

On first launch of the real bundle the app **registers itself as a login item**, so it
comes back after a reboot. **Start at login** in the popover's ⋯ menu toggles this off/on.
`tauri dev` never registers a login item (it would point at the throwaway dev binary).

## Status / follow-ups

- **Running vs waiting** reads the daemon's per-job `phase` (`queued` | `starting` |
  `running` | `frozen`). An older daemon without it can only say `admitted`, so a booting
  runner counts as running there. A job that a sibling host's runner took stays
  `starting` here: `started` latches only when the job's runner is one this host minted.
- Left-click opens the popover; what it shows, and why, is in
  [docs/design/popover.md](../docs/design/popover.md).
