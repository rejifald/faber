---
title: Charts are kit blocks on visx
type: adr
status: accepted
date: 2026-10-19
owner: rejifald
---

# ADR-0045: Charts are kit blocks on visx

## Status

accepted (2026-10-19). Not built yet. It builds on the kit and theme of
[ADR-0042](0042-the-ui-is-react-with-base-ui-and-tailwind.md) and the stories of
[ADR-0043](0043-stories-run-in-storybook.md).

## Context

faber draws no charts with a library today. Its visuals are hand-drawn SVG: the
popover's ring gauges (`ui/src/blocks/RingGauge.tsx`), its job pips
(`ui/src/blocks/PipStrip.tsx`) and the panel's budget bar. The dashboard's Capacity
and History groups ([ADR-0041](0041-the-dashboard-is-the-one-full-ui.md)) will want a
few time series: memory reserved against the budget, running and waiting jobs, and
runs per hour by result. The daemon keeps only one series today, the app's rolling
running-count window, so the data is separate daemon work.

The owner asked to consider Tremor, the React chart library Vercel acquired. Checked at
the registries on 2026-10-19:

| Option | Licence, origin | Last release | React 19 | Size (gzip) | Fit |
| --- | --- | --- | --- | --- | --- |
| Tremor `@tremor/react` 3.18.7 | Apache-2.0, US | npm frozen 2025-01; the copy-paste repo last pushed 2025-10 | no (peer `^18`) | 217 KB | Headless UI or Radix and Recharts 2 beside the Base UI kit |
| Recharts 3.10.1 | MIT; maintainers in the US and Australia | 2026-10 | yes | 147 KB | declarative; its own tooltip and legend to override |
| visx 4.0.0 (Airbnb) | MIT, US | 2026-06 | yes | about 10 to 50 KB, per primitive used | SVG primitives the kit wraps |
| uPlot 1.6.32 | MIT, US | 2025-03 | not React | 21 KB | canvas, styled from JS, accessibility left to the caller |
| ECharts 6.1.0 | Apache-2.0 | 2026-05 | wrapper | 359 KB | far too heavy to embed |

Every chart ships twice, in the Tauri bundle and in the daemon binary, and the whole UI
bundle is about 650 KB with its fonts.

## Decision

1. **Charts are kit blocks built on visx primitives** (`@visx/scale`, `@visx/shape`,
   `@visx/axis`, and others only when a block needs them), pinned exactly. The blocks
   live in `ui/src/kit/` beside the controls. A screen uses a chart block, never visx
   directly, the same rule as Base UI (ADR-0042 § 2), enforced by the same import lint.
2. **Charts are styled only through the theme.** Series, axes, grid and labels use
   semantic roles, so light and dark follow the system. Text is held to 4.5:1, and marks
   and gridlines that carry meaning to 3:1, by the contrast test. No chart sets its own
   colours, sizes or fonts.
3. **Every chart block has a story per state** (data, empty, one point, loading, error,
   older daemon without the series) and an accessible name and text summary.
4. **Tremor is a visual reference, not a dependency.** Its legend placement and tooltip
   anatomy may inform the blocks' design, which is decided on rendered stories.
5. **The existing hand-drawn blocks stay.** `RingGauge` and `PipStrip` are already kit
   blocks; they move onto visx only if that makes them simpler.

## Consequences

- The first chart PR adds the visx packages, after an origin check of their
  maintainers at the registry, and a sparkline or time-series block with its stories.
- The data the dashboard's charts need is a separate daemon decision: which series,
  how long kept, and at what cost to the reconcile loop.
- Charts take more code than a declarative library would, in exchange for blocks that
  follow the kit's rules without overrides.

## Considered and not adopted

- **Tremor:** the npm package has been frozen since the acquisition, peers on React 18,
  and brings a second headless library and Recharts 2.
- **Recharts 3:** maintained and declarative, but 147 KB in each build, with its own
  tooltip and legend to override.
- **uPlot:** the smallest and fastest, but canvas: styled from JS rather than tokens,
  and accessibility would be ours to build.
- **ECharts:** 359 KB.
