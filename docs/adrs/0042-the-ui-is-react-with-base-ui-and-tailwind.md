---
title: The UI is React with Base UI and Tailwind
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0042: The UI is React with Base UI and Tailwind

## Status

accepted (2026-10-18). Built in `ui/` by #280; the vanilla surfaces are pending. It amends
[ADR-0039](0039-every-control-sizes-from-one-ladder.md) § 4 and
[ADR-0040](0040-icons-are-vendored-lucide-fonts-are-bundled-inter-and-plex-mono.md) § 1,
as stated below. ADR-0037, ADR-0038, ADR-0040's fonts and
[ADR-0041](0041-the-dashboard-is-the-one-full-ui.md) carry over unchanged.

## Context

faber's UI is hand-written DOM: the popover (`desktop/src/popover.js`, 1,214 lines), the
panel (`main.js` and `settings.js`, about 1,040 lines) and the dashboard (an inline
`<script>` in `src/web.rs`). The JS toolchain is the Tauri CLI and nothing else.
Components are functions that build DOM, with nothing shared between surfaces. ADR-0037
to ADR-0041 decided one token source, scales, a control ladder, icons, fonts, and one full
UI. All of them still had to be built twice, by hand, in vanilla CSS.

The owner decided to build the UI with React and Base UI, and to rework the in-flight
vanilla-CSS builds (ADR-0038, ADR-0039, ADR-0040) on Tailwind.

Origins were checked at the registries. React 19.3.1 (MIT, US). `@base-ui/react` 1.9.0
(MIT, MUI, with maintainers in France, Poland and Australia). The old
`@base-ui-components/react` name is its pre-1.0 package. Tailwind CSS 4.3.3 (MIT, Tailwind
Labs, Canada). `lucide-react` 1.59.0 (ISC, maintainers in the Netherlands, Denmark and
the UK). Vite 8.3.4 (MIT). None is of Russian origin.

The scope of that check, as the owner set it on 2026-10-18: the no-Russian-origin rule covers
direct dependencies and anything shipped in a build. Transitive build-time tools, which run
while building or linting and ship nothing, are allowed. At the time of the first build those
are `postcss` and `nanoid` (through Vite) and `browserslist` and `update-browserslist-db`
(through `@babel/core`, which `eslint-plugin-react-hooks` pulls in).

## Decision

1. **The popover and the dashboard are one React app** in a `ui/` workspace, with two Vite
   entries: `popover` for the Tauri bundle, and `dashboard` for the daemon to serve. The
   panel window shows the dashboard (ADR-0041). The vanilla `desktop/src/*.js` and the
   inline dashboard script are retired once their React replacements land.
2. **Base UI provides the behaviour,** and faber's kit provides the look. Every
   interactive part (button, menu, tabs, dialog, popover, field, select, checkbox,
   tooltip) is a Base UI part wrapped once in `ui/src/kit/`. Screens use the kit, never
   Base UI directly, and never a raw `<button>`.
3. **Tailwind is the styling layer.** Its v4 `@theme` holds the tokens: the colour roles of
   ADR-0037 (light and dark, following the system), the spacing, radius and type scales of
   ADR-0038, and the fonts of ADR-0040. The scale *is* the theme. Tailwind's default
   palette, spacing and sizes are cleared, so only faber's steps exist. **No arbitrary
   values** (`p-[7px]`, `bg-[#fff]`) outside `ui/src/theme/`.
4. **The control ladder is the kit's `size` prop** (`sm`, `md`, `lg`, plus `square`). Each
   step's height, padding, label, icon, gap and radius are as ADR-0039 § 1 says. This
   replaces ADR-0039 § 4's "one class per step": a screen picks a size, and never sets a
   control's dimensions.
5. **Icons come from `lucide-react`.** This replaces ADR-0040 § 1's vendored sprite. Lucide
   stays the set at 1.59.0, and its size comes from the control's ladder step. faber's own
   marks (the aperture logo, the offline glyph) are kit components.
6. **Fonts stay bundled** (ADR-0040 § 5 to 7): Inter and IBM Plex Mono WOFF2 in `ui/`,
   shipped in both builds.
7. **The daemon embeds the built dashboard if it exists.** CI and the release workflow run
   `npm ci && npm run build` in `ui/` before cargo. `build.rs` embeds `ui/dist/dashboard`
   when it is present, and a plain page saying the UI was not built when it is not, so a
   bare `cargo build` and the Linux test leg work without Node. Release builds fail if the
   bundle is missing.
8. **Enforcement:** ESLint rules from a design-token lint pack, adapted to Tailwind.
   They reject arbitrary values and raw colours in class names and styles, Base UI
   imports outside `ui/src/kit/`, and `style` props in screens. The kit catalogue is
   generated from docstrings and checked in CI. Story coverage, and the runner that
   provides it, is a separate decision, because it is a dependency: Storybook, in
   [ADR-0043](0043-stories-run-in-storybook.md).
9. **The in-flight vanilla builds stop.** The ADR-0038, ADR-0039 and ADR-0040 build sessions
   were stood down on 2026-10-18. Their decisions are built into the theme and the kit.

## Consequences

- faber gains a JS build (React, Base UI, Tailwind, Vite, lucide-react, ESLint) and the
  CI time to run it. Dependabot covers `ui/package-lock.json` as it does
  `desktop/package-lock.json`.
- The dashboard's markup leaves `src/web.rs`. The daemon serves static assets, so its
  session gate (ADR-0028) has to cover them, or they must carry nothing sensitive. The
  data stays behind the session-checked `/api/*`.
- The panel's retirement (ADR-0041) and the dashboard's rewrite become one piece of work.
- The design-review tooling now fits as intended: lint rules, a catalogue, and
  stories (ADR-0043).

## Considered and not adopted

- **Dashboard only in React, popover vanilla:** a smaller step, but two ways of building
  UI would stay in the repo.
- **Finishing the vanilla builds first:** the current UI would improve sooner, but every
  rule would be rewritten soon after.
- **Committing `ui/dist`:** cargo would never need Node, but every UI change would carry a
  generated diff that can go stale.
- **build.rs running npm:** always fresh, but every cargo build would need Node and the
  network.
- **A vendored icon sprite:** with React, the per-icon components of `lucide-react` are
  simpler, and only the icons used are bundled.
