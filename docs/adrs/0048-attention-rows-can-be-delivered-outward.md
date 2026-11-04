---
title: Attention rows can be delivered outward
type: adr
status: proposed
date: 2026-10-21
owner: rejifald
---

# ADR-0048: Attention rows can be delivered outward

## Status

proposed (2026-10-21). The owner decided the shape on 2026-10-21, in the same
design session as `jobs-fetch-through-a-mirror-only-faber-fills`: outward delivery
is a general faber feature, and its two channels are named below. The details
listed under *Open questions* are not decided, and this ADR stays `proposed` until
they are.

Amends [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)'s
permission table: the GitHub-issues channel adds Issues: write, asked for only while
that channel is on.

## Context

An attention row is how faber says something needs a human: a paused host, a
frozen runner, a refused job, an update waiting, and, with
`jobs-fetch-through-a-mirror-only-faber-fills`, a mirror that is down or passing
through. Rows carry a severity (bad, warn, info) and appear in the tray popover and
on the dashboard (ADR-0041).

Both surfaces are pull: a row is seen only when somebody opens one. faber runs on
machines nobody is watching (AGENTS.md: a bad release propagates "to a machine
nobody is watching"), so a bad row can sit unseen for hours while jobs fail or
queue. The mirror made this concrete, because "Mirror down" names jobs it affected,
and nobody may look until the next red PR. It is not specific to the mirror: every
bad row has the same problem.

Two constraints shape where a delivery can come from:

- **The daemon cannot reach the user's session.** After the cut the daemon runs as
  the hidden `_faber` account (ADR-0025). A macOS notification is posted into a
  logged-in user's session, which `_faber` does not have. The menu-bar app runs in
  the user's own session (ADR-0034), so it can.
- **The rows are derived in the app today.** The popover builds its attention list
  from daemon state (`ui/src/popover/attention.ts`). A channel that must work while
  no app is running, such as a GitHub issue, needs the daemon to know the same
  incidents.

## Decision

### 1. Outward delivery is a faber feature, not a mirror feature

Any attention row can be delivered outward. A feature that adds a row, as the
mirror does, gets delivery without deciding anything of its own.

### 2. Two channels

- **macOS notifications, posted by the menu-bar app.** The app already reads the
  daemon's state; it posts a notification for a row that qualifies. The daemon
  posts nothing itself (§ Context).
- **GitHub issues.** faber opens an issue for an incident. That needs Issues: write,
  which joins ADR-0027's table as a new row, asked for only while the channel is on.

### 3. Each channel is configured on its own

Each channel can be turned on or off separately, and each has its own choice of
which severities it delivers. Both ship off: absent configuration delivers nothing,
which is today's behaviour.

## Open questions

Not decided; each needs an owner decision before implementation:

- **Target repo for issues.** Which repository the issues go to (a fixed operator
  repo, the repo the incident affected, or something else), and how that interacts
  with the fine-grained grant's repo list.
- **Dedup and resolution.** The intent is one notification or issue per incident,
  closed or withdrawn when the row clears. What identifies "the same incident", and
  what happens when a row flaps, is open.
- **Rate limiting.** How many deliveries a channel may make in a window, and what
  happens past the limit.
- **Config shape.** Where the keys live (host config, the app's own settings, or
  both) and their names.
- **Where incidents are derived.** Whether the daemon takes over deriving rows from
  the app, or exposes a separate incident list both read (§ Context).

## Consequences

- **A bad row can reach someone who is not looking,** on a host nobody watches.
- **The issues channel widens faber's GitHub grant** by Issues: write while on.
- **Notifications depend on the app running** in a logged-in session. A host with no
  one logged in delivers only through GitHub issues.
- **Every new attention row is a potential delivery,** so its wording has to stand
  on its own outside the popover.

## Alternatives considered

**Mirror-specific alerting.** Rejected: every bad row has the same visibility gap,
and one mechanism serves them all.

**The daemon posts macOS notifications itself.** Not possible after the cut: the
daemon runs as `_faber`, which has no user session to post into (ADR-0025).

**Posting to the affected GitHub runs** (a check run or a comment). Rejected: it
would mix faber's faults into CI's verdict, which ADR-0044 keeps exactly as GitHub
reported it, and `jobs-fetch-through-a-mirror-only-faber-fills` § 9 posts nothing to
runs.
