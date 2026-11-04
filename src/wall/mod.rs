//! The VM wall (ADR-0026): a pf anchor that keeps job code inside its VMs off
//! the host, the LAN and the tailnet.
//!
//! A guest on Apple's NAT has the host's network position: it reaches services
//! the host listens on, the LAN router and the tailnet's resolver. So a root
//! daemon keeps an anchor loaded in the `com.apple/*` slot that `/etc/pf.conf`
//! already evaluates. Its rules apply to the VMs' interfaces only and deny by
//! default: DHCP, DNS to the listed resolvers, the operator's grants, and
//! public unicast IPv4 from a reserved guest address pass; everything else,
//! IPv6 included, is dropped.
//!
//! # Stored rules, live facts
//!
//! ADR-0026's 2026-10-17 amendment: the daemon enforces a rule set that setup
//! or `sudo faber grant` rendered and stored root-owned, never what its own
//! code would render now. So new code alone never changes what a guest can
//! reach: a release that renders differently waits for an operator's sudo act.
//! "Render" ([`render::render`], plan + grants → [`stored::StoredWall`]) and
//! "enforce the stored set" ([`sys::tick`]) are separate functions, and the
//! checker never calls the first.
//!
//! Only two live facts are bound at enforce time ([`stored::instantiate`]):
//! which bridge holds each network's gateway (substituted for the `{if}`
//! token), and the host's own IPv4 addresses (the anchor's `<faber_host>`
//! table). A changed host address is a table replace, not a rule change.
//!
//! # DNS and grants
//!
//! Guests resolve only through the listed resolvers, Cloudflare's `1.1.1.1`
//! and `1.0.0.1` unless `grants.toml`'s `[dns] resolvers` replaces them:
//! the gateway's DNS proxy answers with the host's view of names, tailnet
//! ones included. DNS to the gateway and everywhere else is dropped.
//!
//! LAN or tailnet access is a grant ([`grants`]): a root-owned
//! `grants.toml`, per profile, each destination an IPv4 network, a port and
//! tcp or udp, never a hostname. A grant applies to a VM slot when the slot
//! lists its profile; absent, there is no access. A grant opens only what the
//! wall's own invariants leave: never the host, DNS, another VM, a broadcast
//! or multicast, however wide its network (`192.168.0.0/16` and `0.0.0.0/0`
//! included).
//!
//! # Rule order
//!
//! Per slot, first match wins: host, DNS to resolvers, DNS elsewhere, the
//! fence, grants, non-global ranges, public ([`render`] has the detail). The
//! host block comes first on purpose: neither a resolver nor a grant can ever
//! open the host itself, not even a grant whose network contains the host's
//! LAN address. The fence comes before every grant for the same reason: it
//! drops every VM subnet in the plan (the slot's own, and so its directed
//! broadcast, included), `0.0.0.0/8`, `127.0.0.0/8`, multicast (mDNS among
//! it) and `240.0.0.0/4`, which holds the limited broadcast.
//!
//! # The image build VM
//!
//! The throwaway VM `sudo faber setup` boots to build the Linux image runs
//! under this anchor too (#236), as a job VM with no grants: its network,
//! [`crate::setup::linux_image::build_network`], is one more network in the
//! plan, and its one slot lists no profile, so no grant can apply to it.
//! Setup boots it only when the stored set holds that network in exactly that
//! shape and the verdict is green for that set
//! ([`crate::setup::linux_image::wall_verdict`]). The guest then fetches
//! nothing until DNS to its gateway goes unanswered: the per-slot host block
//! comes first and the anchor loads as a unit, so that silence shows the whole
//! set is bound to the bridge.
//!
//! # Known limits
//!
//! - **States float.** macOS pf's default `state-policy` is `floating`: a
//!   state matches on any interface. A state the host itself opened could
//!   therefore match a guest packet spoofed to fit it and pass before any
//!   rule here. That matters only if the operator's own rules create such
//!   states toward addresses a guest could spoof; faber sets no state policy,
//!   since the main ruleset is the operator's.
//! - **A new host address is open for up to one tick.** An address the host
//!   gains is blocked once the next tick replaces `<faber_host>`; until then
//!   a guest reaches it as any other address in its range: a public one, or
//!   one inside a grant, passes.
//! - **Hairpin NAT is public traffic.** A guest that connects to the host's
//!   public WAN address leaves as public unicast; if the router reflects it
//!   back to a port forwarded to the host, it arrives as traffic from the
//!   internet would. Only the router can tell, and the wall does not try.
//!
//! # Pure core, one seam
//!
//! Everything that decides is a pure function of text and typed values, so it
//! compiles and is tested on every OS:
//!
//! - [`model`]: the network plan setup writes, and its binding to the
//!   interfaces the host has right now.
//! - [`grants`]: the grants file, parsed, validated and written back.
//! - [`grant_edit`]: what `sudo faber grant` decides before it writes: an
//!   operator's request checked, and `config.toml`'s narrowing applied.
//! - [`render`]: plan and grants to rules, one per line, in the exact form
//!   `pfctl` prints them back, so "the anchor holds exactly the rules
//!   installed" is a line-by-line comparison.
//! - [`stored`]: the stored set, its digest, and its binding to the host.
//! - [`pfctl`]: parsers for what `pfctl` prints.
//! - [`verdict`]: green or red, every reason, and the remedies root may apply.
//! - [`report`]: the verdict file root writes and `_faber` reads, with the
//!   anchor's counters on a green tick.
//! - [`counters`]: per-VM labels, and whether a probe from inside a guest was
//!   stopped by the anchor itself.
//! - [`trust`]: ADR-0025's ancestor rule for the stored set and the verdict
//!   file.
//!
//! [`sys`] is the seam: the [`sys::WallHost`] trait, the tick that observes,
//! repairs and observes again, and the macOS implementation that runs
//! `/sbin/pfctl` and `/sbin/ifconfig`. [`grant_cmd`] is `sudo faber grant`,
//! the root write path, behind a seam of its own; [`paths`] says where the
//! root-owned files live.
//!
//! # Fail closed
//!
//! The wall is checked, not trusted. Anything the checker cannot establish —
//! a command that fails, output it does not recognise (`ifconfig -a`
//! included), a stored set it cannot read or bind, a host table that differs,
//! a bridge pf skips or does not list, a translation that skips the filter —
//! is red, never a guess. A dead checker is red too: the verdict file carries
//! its time, and a reader treats a stale one as red ([`report::judge`]).
//!
//! # Installed by setup
//!
//! `sudo faber setup` writes the plan ([`paths::PLAN`]) and the stored set
//! ([`paths::STORED`]), and installs the checker as a root LaunchDaemon
//! running the root-owned CLI copy, `/usr/local/bin/faber wall-daemon`
//! ([`cli::run_daemon`], [`crate::setup::vm_runtime`]).
//!
//! # Read by `_faber`
//!
//! [`reader`] judges the verdict file for the daemon: [`reader::watch`] feeds
//! the limiter's gate ([`crate::limiter::Limiter::observe_wall`]), which
//! [`crate::vm::admit`] stops VM admission on, and [`crate::vm::wall_proof`]
//! proves each VM's probe from the counters the checker publishes in it.

pub mod cli;
pub mod counters;
pub mod grant_cmd;
pub mod grant_edit;
pub mod grants;
pub mod model;
pub mod paths;
pub mod pfctl;
pub mod reader;
pub mod render;
pub mod report;
pub mod stored;
pub mod sys;
pub mod trust;
pub mod verdict;

/// The anchor faber loads. `com.apple/*` evaluates its children in name
/// order, so the `000.` prefix puts it ahead of Apple's own
/// (`com.apple/250.ApplicationFirewall`); [`verdict::decide`] checks that it is
/// in fact first.
pub const ANCHOR: &str = "com.apple/000.faber-vm-wall";

/// The anchor point in the main ruleset that evaluates [`ANCHOR`]. The main
/// ruleset is the operator's, and faber never rewrites it.
pub const ANCHOR_POINT: &str = "com.apple/*";

/// The anchor's parent, whose children `pfctl -a com.apple -sA` lists.
pub const ANCHOR_PARENT: &str = "com.apple";

/// The version of the rules this binary renders, stamped into every stored
/// set it renders. A release that needs new rules bumps it; the reader shows
/// the wall as out of date until setup re-runs and stores new rules
/// (ADR-0026), and the stored rules keep applying.
pub const RULES_VERSION: u32 = 1;

/// Every label faber puts on a rule starts with this.
pub const LABEL_PREFIX: &str = "faber-wall";
