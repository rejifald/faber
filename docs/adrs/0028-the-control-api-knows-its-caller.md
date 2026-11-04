---
title: The control API knows its caller
type: adr
status: accepted
date: 2026-09-28
owner: rejifald
---

# ADR-0028: The control API knows its caller

## Status

**Extended 2026-10-18 by addendum C** (below): the app's panel window shows the
dashboard with no Tauri IPC, and the React dashboard's hashed static assets are
served without a session.

**Amended 2026-10-09 by
[ADR-0031](0031-releases-are-signed-by-github-from-a-fresh-public-repository.md).**
The update channel no longer has accepted signers. Its trust root is a compiled-in
policy that no setting can change.

In phase 2's list of trust-widening changes, "accepted signers" gives way to two
changes: lowering `[update] maturity_secs`, and setting or changing
`[update] pin`. Both apply live through `/api/settings`, and either can let a
release in sooner than the waiting period allows.

accepted (2026-09-28). Phase 1 closes
[#74](https://github.com/rejifald/faber-legacy/issues/74) and ships in the same release
as ADR-0025, ADR-0026 and ADR-0027. Phase 2 follows that release.

## Context

The dashboard API listens on `127.0.0.1:8787` (`src/web.rs:82`). Its mutating
routes are guarded only by an `Origin` check (`src/web.rs:101`). That check
deliberately lets through a request with no `Origin` header, so that `curl` and
scripts work. The loopback bind was being treated as the boundary. It is not one:

- **Every Docker job reaches it.** On 2026-09-25 an unprivileged `busybox`
  container on the builder read `GET /api/state` through
  `http://host.docker.internal:8787` (#74). It ran on the default bridge network,
  without dind. Docker Desktop forwards that name to the host's loopback.
- **Every native job and every local process reaches it**, under any account.
  Loopback is not per-user.
- **What it accepts is faber's trust boundary.** `POST /api/settings` rewrites
  `config.toml`. The autolimiter and `[update]` keys apply live. Everything else
  applies at the next restart, including dind grants (ADR-0013), `[gate]`
  (ADR-0014) and `[host] recreatable_*` (ADR-0016). `/api/drain`, `/api/pause`,
  `/api/hold`, `/api/pin` and `/api/update/*` are open too.
- **Even its reads leak.** `GET /api/state` names the owner and every repository
  faber serves, to a job from any one of them.

After ADR-0025, faber runs as `_faber`, and every legitimate caller runs as the
operator: the browser, the tray app, `faber drain` and the deploy scripts. After
ADR-0026, jobs run in a VM that must not reach the host's loopback at all.
Neither makes the API safe on its own. The VM rule is a network property that a
misconfiguration can undo, and ADR-0025 ranks processes running as the operator
second only to jobs.

## Decision

There are two phases, because the second needs a presence mechanism the first
does not.

### Phase 1: the API authenticates by account

- **The API moves to a Unix socket.** faber reads the connecting process's user
  ID from the socket (`getpeereid` on macOS, `SO_PEERCRED` on Linux) and accepts
  only the operator's account, as recorded by `sudo faber setup`. The CLI, the
  tray app and the deploy scripts use the socket; scripts use `curl
  --unix-socket`. The socket is never forwarded into a VM.
- **The browser gets a session through the socket.** A browser cannot speak to a
  Unix socket, so the loopback listener stays, for the dashboard only. `faber
  dashboard`, or the tray, asks the socket for a one-time link. Opening it sets an
  `HttpOnly`, `SameSite=Strict` session cookie.
  - Every request on the loopback listener needs that session, reads included.
  - The `Origin` check stays for mutations.
  - The `Host` header is checked against loopback, so a rebound DNS name cannot
    ride along.

A job that reaches the loopback listener anyway gets `401` and learns nothing,
not even which repositories exist.

### Phase 2: widening trust needs the operator present

An account check stops jobs and other accounts. It does not stop a compromised
process running as the operator, which can use the socket as easily as the tray
can. The changes such a process would want are few and specific, so phase 2
guards exactly those with proof of presence: Touch ID or the operator's password
through the tray, or `sudo` on the CLI.

- granting dind (ADR-0013), or widening `[gate]` (ADR-0014)
- declaring projects or volumes recreatable (ADR-0016)
- replacing a credential (ADR-0027)
- changing the update channel: repository, pinned version or accepted signers
  (ADR-0006, ADR-0015)
- ~~turning `require_topic` off (ADR-0027)~~. The key was dropped on
  2026-10-18 (#188): the topic is always required.

Everything else stays one call: state, pause, resume, drain, hold, pin,
reconcile.

**Until phase 2 lands, those changes need `sudo`** (owner, 2026-10-18,
[#78](https://github.com/rejifald/faber-legacy/issues/78); ships in v0.4.0).
`WIDENING_NEEDS` in `src/control.rs` is `Present`, and in phase 1 only a root
caller on the socket is `Present`. An operator caller or a dashboard session
gets `403`, and each refused change comes back with the exact command that
makes it. Every attempt, allowed or refused, leaves a `control` line in the
audit log. The paths:

- `sudo faber config set <key> <value>` and `sudo faber config unset <key>`
  for one key, and `sudo faber config apply <file>` for what one key cannot
  say (a new docker profile, profile routing ahead of a dind profile, any
  routing change while a profile drops grants). The
  daemon does the work for every caller, root included: the same validation as
  a settings save, an atomic write as its own account, live apply or a restart
  notice, and the audit line. Root never writes into a directory `_faber` owns.
- `sudo faber repo-hold clear <owner/repo>` for a minting hold (addendum A).

The list also covers what runs beside a Docker daemon (#220): a profile's
`image` or `dind_image` while it has Docker access, and `[runner_image]
dockerfile` while variants are built. While any profile has `drop_grants`,
every routing change widens (#221): a rename, a removal, a reorder, a relabel
or a new profile can move that profile's jobs where fewer grants are dropped.
`sudo faber config apply` sends the digest of the config its dry run judged
with the save, and the daemon answers `409` if another save moved it in
between, so root never applies a widening it was not shown.

The test for any other field (#238, owner, 2026-10-18): does it widen what
runs next to a privileged container, a Docker socket or a secret, such as the
runner's JIT credential or the job's token? If it does, it is on the list, and
its narrowing direction stays one call.

- `[needs]` (ADR-0010 §4). A variant built for a gained version runs as the
  job's image beside its Docker daemon and is shared by later jobs. A new
  `[needs.<tool>]` entry or a changed `versions` string widens (key
  `needs.<tool>.versions`), and so does a range change that narrows: ranges
  are not compared by containment, and the check may only over-report.
  Removing a tool (it fails closed, `UnknownTool`) or an exact list that only
  loses versions is routine. Because an empty `[needs]` is no ceiling at all,
  emptying it widens (key `needs`) and giving an empty one its first entry
  is routine. `[needs]` has no settings field: the command is `sudo faber
  config apply`.
- A profile's `env`, on every profile and backend, not only with dind.
  Runner-level variables act before and around the job's steps and the job
  cannot override them: `HTTPS_PROXY` can route the runner's credentialed
  traffic through anyone's proxy, and `ACTIONS_RUNNER_HOOK_*` and `LD_PRELOAD`
  run code ahead of the job. An added or changed entry widens (key
  `profile.<name>.env`), on a new profile too; removing one is routine, and
  the form's narrowing is "Clear". A table is no `config set` value, so the
  command is `sudo faber config apply`.
- `cache_volume` false -> true (key `profile.<name>.cache_volume`), and a new
  profile with it on. It is the one state that crosses the job boundary: a
  pull-request job can seed what a later push job, which has secrets, reads.
  The command is `sudo faber config set profile.<name>.cache_volume true`;
  turning it off is routine.
- A macos-vm profile's `image` and `tools` stay routine: the VM is the
  boundary.

**The recipe file is fixed by ownership, not by a digest** (#238). Making
`[runner_image] dockerfile` widen does not watch the file it names: whoever can
write that path changes every variant without touching the config. So the
recipe must be a file only root could have written: the file and every
directory above it, by the same `wall::trust::root_only` rule as the wall's
stored files (any owner but root, or a group- or world-writable entry, a
sticky directory excepted). The build context is the recipe's directory, so
that a recipe's `COPY` still works, and every entry in it is held to the same
rule (a symlink by its owner only). The sticky exception holds only above the
context: the recipe's directory and every directory inside it are refused when
group- or world-writable, sticky or not, since any account could add an entry
there before the build reads it. A directory of more than 4096 entries is
refused rather than walked.

- `faber check-config` and a settings or `config set` save refuse anything
  else, with the reason.
- The daemon warns at boot and refuses each variant build from such a recipe;
  it never refuses to boot over one. It reads the recipe once, after the
  check, and feeds those bytes to `docker build --file -`.
- The operator needs no new layout: `sudo install -o root -m 0644 <recipe>
  /etc/faber/` is enough.
- Rejected: a digest of the recipe in the config. Every edit to the recipe
  would need another `sudo`, and a digest judges the file only at the moment
  of the check. Ownership removes the writer instead.

- `sudo faber grant` for LAN and tailnet access (ADR-0026), and
  `sudo faber auth set` for the credential (ADR-0027), as before.

The settings form and the menu-bar app show every widening field read-only,
with its command. Narrowing stays one click and needs no `sudo`: raising
`maturity_secs`, clearing a pin, revoking dind, turning a grant off. The same
`faber config set` works as the operator for any change that widens nothing.
`require_topic` was dropped (#188), so it is no longer on the list. Phase 2
replaces `sudo` with Touch ID or the password through the tray by changing
what counts as `Present`; the threshold stays where it is.

On a LEGACY host the operator owns `config.toml`, so this stops a change
through the API, not an edit of the file followed by a restart. The cut moves
every host to SYSTEM, where the file is `_faber`'s.

## Consequences

- Phase 1 closes #74 for jobs and other accounts.
- In the same release, three callers move to the socket: the tray app, `faber
  drain` (`src/drain.rs`, which today speaks HTTP to a port), and every deploy
  script that calls the API.
- Opening the dashboard gains one step: run `faber dashboard` or use the tray,
  instead of typing the URL.
- The settings form keeps working for routine changes. Until phase 2 its
  trust-widening fields are read-only and name their `sudo faber …` command;
  in phase 2 they ask for Touch ID or the password when saved.
- What faber still cannot stop: root on the host.

## Alternatives considered

**A random API token in a file the operator can read.** It stops jobs and other
accounts as well as the socket does. But it is a secret to create, store and
rotate, and anything running as the operator can read the file anyway. The
socket gets the same result from the kernel, with nothing to leak.

**Keep TCP and require the token only on mutating routes.** That leaves `GET
/api/state` naming every served repository to every job.

**Presence for every change.** This puts pause and drain behind Touch ID, which
turns routine operations into prompts and teaches the operator to approve them
without reading. Presence is kept for the changes that widen trust.

## Addendum C (2026-10-18): the panel window and the dashboard's assets

Owner decisions of 2026-10-18, recorded before any code, as
[ADR-0041](0041-the-dashboard-is-the-one-full-ui.md) requires. ADR-0041 makes the
panel window a view of the dashboard. [ADR-0042](0042-the-ui-is-react-with-base-ui-and-tailwind.md)
moves the dashboard to a React bundle that `build.rs` embeds (`src/ui_bundle.rs`),
which a browser loads as an HTML shell plus static files. (Addendum A, registry
credentials and repo holds, is cited in phase 2 above and in `src/control.rs`.
Addendum B, the trust-change notice, is cited in `src/audit.rs` and `src/web.rs`.
Neither was written into this file, so this one is lettered C.)

### Context

Two things in phase 1 do not fit the new dashboard.

- **The front door wants the session key on every path but `/`.** The key lives
  in the page's `sessionStorage` and goes out in `X-Faber-Session` on the page's
  own `fetch` calls. A browser loading `<script src>`, a stylesheet or a font
  sends only the cookie. Under today's `FrontDoor::admit`, every file of the
  React bundle would answer `401`.
- **The sign-in page is the body of every `401`.** With no session it can fetch
  nothing, so today it has to inline whatever it shows.

And the panel window has to show the dashboard without becoming a second, more
privileged way into the daemon.

### Decision 1: the panel window is a browser tab in an app window

- ⌘⇧F and *Open panel* ask the control socket for a one-time link
  (`POST /api/session`), exactly as *Open dashboard* does, and open it in a Tauri
  window.
- **The window's capability grants no IPC.** No capability names the window, and
  none lists the dashboard's origin under `remote`. The page gets no Tauri
  command, no plugin and no event, so it has exactly a browser tab's rights.
- **The page gets no `window.__TAURI__` either.** `withGlobalTauri: true` in
  `tauri.conf.json` injects it into every webview today. The build PR scopes or
  disables that injection for this window, or says in the PR why it stays, with
  every `invoke` from the page rejected all the same.
- **The app sends the window no event.** `SNAPSHOT_EVENT` and every other event
  go to the popover by label (`emit_to`), never to every window with `app.emit`,
  as `desktop/src-tauri/src/lib.rs` does for the snapshot today.
- **The session rules apply unchanged.** The link redeems at `/session`, the
  cookie is `HttpOnly; SameSite=Strict`, the key lives in that window's
  `sessionStorage`. The `Host` check and the `Origin` check on mutations are the
  same code a browser tab passes through. The daemon cannot tell the two apart,
  and does not need to.
- **Every open mints a fresh link.** The window never keeps a link, and an
  expired or restarted session shows the sign-in page, as in a browser, until the
  next open.
- **The link never leaves the app's process.** The app hands the URL straight to
  the webview. It never goes into an opener's argv or a file, which is why
  *Open dashboard* routes through a `0600` redirect page today.
- **The window stays on the dashboard.** A navigation to any other origin is
  handed to the default browser, not loaded in the window. Only an `http` or
  `https` URL is handed over. Any other scheme (`file:`, a custom app scheme) is
  dropped, and opens nothing.
- The panel's entry (`"main"`) is removed from
  `desktop/src-tauri/capabilities/default.json` in the PR that builds this. The
  popover stays the only window in that capability.

**Why no IPC.** The app's commands speak to the control socket, as the operator,
and the socket is gated by account, not by session or `Origin`. A page holding
IPC would reach that socket through the app. A script injected into the
dashboard, for example through a repository name rendered unsafely, could mint
links, call any route as the operator, and open URLs or paths on the host. A page
with no IPC holds only its session, which is what the same script would hold in a
browser tab.

### Decision 2: the bundle's hashed assets need no session

- **`GET`/`HEAD /assets/*` is served without a session.** That covers the
  bundle's JS, CSS and fonts. They hold no data: they are the build output of
  this public repository, identical on every host that runs the same version.
- **The HTML shell keeps requiring the session**, at `/` as today: a cookie, and
  no key, since the browser navigates to it.
- **Amended 2026-10-22 (owner decision, [anatomy](../design/settings/anatomy.md)):**
  the HTML shell answers `/` and `/settings`, both on the cookie alone.
- **Amended 2026-10-28 ([ADR-0054](0054-the-dashboard-is-views-in-a-sidebar.md)):** the
  shell also answers `/activity`, `/memory` and `/maintenance`; `/` serves the shell, and the inline page is
  gone.
- **Every `/api/*` route keeps requiring the cookie and the key.** So does every
  other path that is not an asset.
- **The `Host` check stays on everything**, assets included, and the `Origin`
  check on every mutation. An asset route accepts no mutating method: without a
  session a mutation under `/assets/` answers `401`, as any path does; with one
  it answers `403` unless its `Origin` is exact, and `405` when it is.
- **The sign-in page, the body of every `401`, may load its fonts from
  `/assets/`** like any other page, instead of inlining them. Its font URLs are
  absolute (`/assets/<name>`), so they resolve the same under any path that
  answered `401`.
- **Assets are served only from the embedded table, by exact name.**
  - The handler takes the raw request path minus its leading `/` and looks it up
    as a whole `assets/<name>` key of `ui_bundle::FILES`. Anything else is `404`.
  - It never goes through `ui_bundle::asset`, whose index mapping turns `""` into
    `index.html`. So no `/assets/` path returns the shell, and an unbuilt
    binary's empty table answers `404`, not the not-built page.
  - The path is not decoded or normalised. Nothing is read from disk, nothing is
    listed, and nothing falls back to the shell.
  - The exemption from the session is decided on the same raw path the router
    matches, so the guard and the handler can never disagree about which file a
    request names.
  - Only Vite's content-hashed output lands under `assets/`. `ui/public/` has no
    `assets/` directory, and its `licenses/` keeps the session like any other
    path.
  - The handler takes no daemon state, so no data can reach it.
- **The bundle carries nothing about the host.** It inlines no build-time
  environment (`VITE_*`) and no host data. What a host knows reaches the page only
  through `/api/*`.
- Asset responses carry `X-Content-Type-Options: nosniff` and
  `Cache-Control: public, max-age=31536000, immutable`, and no
  `Access-Control-Allow-*` header. A font fetched cross-origin stays unreadable to
  another origin, as a browser requires of fonts.

This supersedes the earlier sketch in `src/ui_bundle.rs` of admitting assets on
the cookie alone. A cookie-gated asset would still fail the sign-in page, and it
would protect nothing that is not already public.

**Not secret, and not relied on as secret: the asset names.** Their hashes follow
from the public source. A caller who reaches the listener can tell which faber
version runs there by fetching a guessed name, or by reading the sign-in page's
font links. That version is public on the release channel, which followers
install unattended. The threat model protects the host's data and the routes that
change it, not the version number.

### The threat model, checked

The callers phase 1 was built against, each against both decisions.

**A job that reaches the host's loopback** (through Docker Desktop's
`host.docker.internal`, as in #74, or through a VM misconfiguration that undoes
ADR-0026). It can forge any `Host` header it likes, so the `Host` check does not
stop it. It now gets the public build output under `/assets/`, and only that. `/`
and every `/api/*` route still answer `401` with the sign-in page. That page
names no repository, owner or host, so the job still learns nothing about what
faber serves. It cannot mint a session: that needs the socket, which is never
forwarded into a VM and refuses every account but the operator's. The panel
window changes nothing here, because it is reached from the app, not from the
network.

**DNS rebinding.** A page on a name that rebinds to `127.0.0.1` sends that name in
`Host` and gets `421` on every path, assets included. A page on any origin can
still embed `http://127.0.0.1:8787/assets/…` as a script or an image, because
then `Host` is loopback. What it embeds is public code running in its own origin.
It cannot read a font or a stylesheet's rules without CORS, and assets send none.
Its calls to `/api/*` go out cross-site, so the `SameSite=Strict` cookie stays
behind, and they carry no key, so they get `401`. Inside the panel window there
is no other origin to rebind from, since navigation away leaves the window.

**A second local account.** It can connect to loopback and fetch the assets,
which it could equally build from the public source. It cannot mint a link,
because the socket refuses its uid. It cannot read one either. The browser path
already keeps the link out of argv and in a directory only the operator can
enter. The panel window keeps it inside the app's process, which is stronger
still. The window's cookie and key sit in the operator's webview data, under the
operator's home, out of another account's reach.

**A compromised process running as the operator.** Unchanged: it already has the
socket, and phase 2's presence check is what stands in its way. A dashboard page
without IPC adds no path to the socket that the operator's own browser tab lacks.

### What the build PR must test

- **An asset route serves no data path.** Each `assets/` entry of the embedded
  table answers `200` without a cookie, byte for byte as built. The asset handler
  takes no daemon state. Every route that is not an asset still answers `401`
  without a session: `/`, `/api/state`, `/api/settings`, an unknown path.
- **No directory listing, and no traversal.** `/assets/` answers `404` without a
  session. `/assets`, which is not under `/assets/`, answers `401` like any
  unknown path. `/assets/../index.html` and `/assets/../api/state`, their
  percent-encoded forms (`%2e%2e`, `%2f`, a doubled `%252e`), a backslash form
  and `/assets//etc/passwd` answer `404`, or `400` where hyper refuses the URI,
  and never `200`. None of them answers with the shell or reads a file. The
  shell is not reachable under any `/assets/` name, and an unbuilt binary answers
  `404` under `/assets/`.
- **The shell still `401`s without a session**, and still loads with the cookie
  and no key. `/api/*` still needs both.
- **The `Host` and `Origin` rules hold on assets.** A non-loopback `Host` gets
  `421`. A missing `Host` gets `400`. A `POST`, `PUT` or `DELETE` under
  `/assets/` runs no handler: `401` without a session, `403` with one and no exact
  `Origin`, `405` with both. An asset
  response carries `X-Content-Type-Options: nosniff`, the immutable
  `Cache-Control`, and no `Access-Control-Allow-Origin`.
- **The sign-in page loads its fonts from `/assets/` with no session**, both
  when it is the `401` of `/` and when it is the `401` of a nested path such as
  `/api/state`. Its font URLs are absolute `/assets/<name>`.
- **The panel window has no IPC.** From the dashboard page loaded in the window,
  an `invoke` of a plugin command and of one of the app's own commands
  (`open_dashboard`, say) is rejected. No capability file names the window. The
  app's own commands are not left allowed for every window by default. The page
  has no `window.__TAURI__`, or the PR says why it stays.
- **The panel window gets no event.** Every event the app sends goes to the
  popover by label; a listener in the window hears nothing, the snapshot
  included.
- **Navigation leaves the window where it was.** A navigation to another
  `http`/`https` origin opens in the browser. A navigation to `file:` or a custom
  scheme opens nothing.

`FrontDoor::admit` stays the pure function it is, so the rules on paths and
headers are unit-tested there without a network. The window's IPC test needs a
built app, and runs where the desktop app's tests run.

### Consequences

- The panel's own HTML, JS and capability entry go, as ADR-0041 says. The window
  becomes a loader for a link.
- The sign-in page stops inlining fonts. It keeps inlining its tokens, which are
  the CSS it needs to render at all.
- Anyone who reaches the listener learns that faber runs and which version, as
  they could already from the sign-in page's distinctive `401`. Nothing else is
  newly reachable without a session.

### Considered and not adopted

- **Admit assets on the cookie alone.** It protects files that are already
  public, and leaves the sign-in page unable to load its fonts.
- **Inline the whole bundle into the shell.** One gated response and nothing to
  open, but it loses caching and the bundle's split into files. It also makes
  every font a base64 blob in an HTML page that is served on every load.
- **Give the panel window a narrow IPC capability** (hide, show, open a run).
  Any IPC exposed to the page is reachable by a script in it, and the window
  needs none of it: the window's chrome belongs to the app, not to the page.
