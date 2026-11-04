# ui

faber's popover and dashboard as one React app
([ADR-0042](../docs/adrs/0042-the-ui-is-react-with-base-ui-and-tailwind.md)): React, Base UI for
behaviour, Tailwind for styling, lucide-react for icons, built by Vite. The tray popover lives
here; the dashboard entry still renders a placeholder with one of every kit part.

```sh
npm ci                  # exactly package-lock.json
npm run dev             # the dashboard entry on a dev server (dev:popover for the other)
npm run build           # type-check, then dist/dashboard and dist/popover
npm run lint            # the design rules below
npm test                # lint rules, kit (needs a build first), stories and catalogue script
npm run catalog         # regenerate docs/design/catalog.md (catalog:check in CI)
npm run storybook       # the story browser on http://localhost:6006 (storybook:build in CI)
```

| Path | What |
| --- | --- |
| `dashboard/`, `popover/` | one Vite entry each: its `index.html` and `main.tsx` |
| `src/theme/` | the Tailwind theme: ADR-0037 colours (imported from `desktop/src/tokens.css`), ADR-0038 scales, ADR-0039 ladder sizes, ADR-0040 fonts |
| `src/kit/` | every interactive part, wrapping Base UI once; sized by `size` (`sm`/`md`/`lg`) |
| `src/entries/` | what the entries share (mounting, the placeholder) and `PopoverApp`, the popover's link to Tauri |
| `src/screens/` | whole surfaces, composed from blocks and the kit: `Popover` |
| `src/blocks/` | the popover's rows and marks: GaugeBar, PipStrip, JobGroup, CompactGroup, AttentionRow, PeerRow, Pipe |
| `src/popover/` | the popover's data model, pure TS: attention rows, rings, job groups and pips, peers, formatting, story fixtures |
| `public/licenses/` | the font and icon licences, copied into every build |
| `src/{kit,blocks,screens}/*.stories.tsx` | one story per state of each part, block and screen (ADR-0043) |
| `.storybook/` | the story browser: its Vite config, the Theme and Text size switches, forced hover/pressed/focus states (`force.ts`) and play helpers |
| `eslint-rules/`, `scripts/` | the design-token lint rules and the catalogue generator |

**The daemon** embeds `dist/dashboard` at compile time when it exists (`build.rs`,
`src/ui_bundle.rs`), and a plain "not built" page when it does not, so `cargo build` needs no
Node. Release builds set `FABER_REQUIRE_UI=1` and fail without the bundle.

**The rules** (`eslint.config.mjs`): the theme clears Tailwind's defaults, so only faber's steps
exist; arbitrary values (`p-[7px]`, `h-(--x)`) and raw colours are rejected outside
`src/theme/`; Base UI is imported, and `<button>`, `<input>`, `<select>` rendered, only in
`src/kit/`, and only the kit takes a `style` prop. Versions are pinned exactly and install
scripts are off (`.npmrc`).

**The menu-bar app** loads `dist/popover`. Its Tauri build runs `npm run build` here, then
`desktop/scripts/assemble.mjs` copies the bundle beside the panel into `desktop/dist`, so build
the app only after `npm ci` here.
