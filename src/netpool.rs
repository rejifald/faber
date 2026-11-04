//! faber's own slice of the host's IPv4 address space.
//!
//! Every per-job sandbox gets a private bridge network ([`crate::docker`]), and
//! by default Docker picks its subnet from the daemon's *global* default address
//! pools. That default is a shared resource with a small, fixed capacity — a
//! stock daemon offers 31 slots (fifteen `/16`s carved out of `172.16.0.0/12`,
//! plus sixteen `/20`s out of `192.168.0.0/16`) — and **every** tool on the
//! machine draws from it. Compose stacks, other agents' sandboxes and faber's
//! runners all compete for the same 31 slots.
//!
//! That competition is not hypothetical, and faber loses it. On one host an
//! unrelated tool had accumulated container-less compose networks until they
//! held the pool, and faber's provisions failed for weeks on end with
//!
//! ```text
//! creating network faber-net-…: all predefined address pools have been fully subnetted
//! ```
//!
//! The important part of that incident is what it was *not*. faber's own
//! teardown was correct — every network it had made was live and attached — so
//! there was nothing to reclaim and no leak to fix. faber was simply one
//! claimant among several on a pool none of them owned.
//!
//! **So the fix is to stop competing, not to fight harder.** Given a CIDR block
//! the operator has set aside, faber allocates each job's subnet out of *that*
//! and passes it to `docker network create --subnet`, which makes its capacity a
//! property of the block the operator chose rather than of whatever else the
//! machine happened to do that week. It also means faber never has to form an
//! opinion about a resource it did not create: the alternative fix — deleting
//! the squatting networks — would require faber to decide that somebody else's
//! network is disposable, which is exactly the inference
//! [ADR-0016](../docs/adrs/0016-reclamation-is-delegated-not-inferred.md)
//! forbids. Reserving costs nothing and touches nothing.
//!
//! See
//! [ADR-0020](../docs/adrs/0020-faber-reserves-its-own-address-pool.md) for the
//! incident, and for why widening the daemon's own pools is not the same fix.
//!
//! Absent a declared block this module is inert and Docker allocates as it
//! always has — the same "absent key means the behaviour that shipped before it
//! existed" contract as the ADR-0016 grants, which matters because this reaches
//! unattended follower hosts through ADR-0006.

use std::fmt;

/// The widest prefix a pool may declare. Narrower than the `/0` the notation
/// permits, because every shift below works on `32 - prefix` and a `/0` would
/// make that a 32-bit shift — undefined for `u32` in Rust. `/8` is also the
/// widest block any of the private ranges actually offers.
const MIN_PREFIX: u8 = 8;

/// The narrowest block that can still host a bridge network: a `/30` leaves two
/// usable addresses, which is a gateway and one container.
const MAX_PREFIX: u8 = 30;

/// Most slots a pool may be asked to carve. A `10.0.0.0/8` split into `/30`s is
/// four million subnets; allocation scans linearly, so an unbounded declaration
/// turns a typo into a hang. No real host needs more than this, and a declaration
/// that wants more is a mistake worth reporting at startup rather than at 3am.
const MAX_SLOTS: u32 = 65_536;

/// An IPv4 CIDR block, normalised to its network address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    base: u32,
    prefix: u8,
}

impl Cidr {
    /// Parse dotted-quad `a.b.c.d/len`.
    ///
    /// The address is masked down to its network address rather than rejected
    /// when it carries host bits — `10.63.1.0/16` means `10.63.0.0/16`, which is
    /// what Docker itself does with the same string and what an operator writing
    /// it means. Accepting only the canonical spelling would fail a config over
    /// a difference with no consequence.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, len) = s
            .split_once('/')
            .ok_or_else(|| format!("'{s}' is not a CIDR block (expected e.g. 10.63.0.0/16)"))?;
        let prefix: u8 = len
            .trim()
            .parse()
            .map_err(|_| format!("'{s}' has a prefix length that is not a number"))?;
        if !(MIN_PREFIX..=MAX_PREFIX).contains(&prefix) {
            return Err(format!(
                "'{s}' has prefix /{prefix}; faber accepts /{MIN_PREFIX} to /{MAX_PREFIX}"
            ));
        }
        let octets: Vec<&str> = addr.trim().split('.').collect();
        if octets.len() != 4 {
            return Err(format!("'{s}' is not a dotted-quad IPv4 address"));
        }
        let mut base: u32 = 0;
        for o in &octets {
            let v: u8 = o
                .parse()
                .map_err(|_| format!("'{s}' has an octet that is not 0-255"))?;
            base = (base << 8) | u32::from(v);
        }
        Ok(Self {
            base: base & Self::mask(prefix),
            prefix,
        })
    }

    /// The netmask for `prefix`. Safe for the whole accepted range because
    /// [`MIN_PREFIX`] keeps the shift under 32.
    fn mask(prefix: u8) -> u32 {
        u32::MAX << (32 - prefix)
    }

    /// Whether two blocks share any address. True exactly when the wider block
    /// contains the narrower one's network address, so it is one comparison
    /// rather than a range intersection.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        let shared = self.prefix.min(other.prefix);
        let m = Self::mask(shared);
        (self.base & m) == (other.base & m)
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.base.to_be_bytes();
        write!(f, "{}.{}.{}.{}/{}", b[0], b[1], b[2], b[3], self.prefix)
    }
}

/// A declared block plus the subnet size faber carves out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPool {
    pool: Cidr,
    size: u8,
}

impl NetPool {
    /// Validate a declaration. Rejects a size that is wider than the pool that
    /// must contain it, and one that would carve more than [`MAX_SLOTS`].
    pub fn new(pool: Cidr, size: u8) -> Result<Self, String> {
        if !(MIN_PREFIX..=MAX_PREFIX).contains(&size) {
            return Err(format!(
                "per-job subnet size /{size} is outside /{MIN_PREFIX}-/{MAX_PREFIX}"
            ));
        }
        if size < pool.prefix {
            return Err(format!(
                "per-job subnet size /{size} is wider than the pool {pool} that must contain it"
            ));
        }
        let slots = 1u64 << (size - pool.prefix);
        if slots > u64::from(MAX_SLOTS) {
            return Err(format!(
                "{pool} split into /{size} subnets is {slots} slots; \
                 faber accepts at most {MAX_SLOTS} — widen the subnet size or narrow the pool"
            ));
        }
        Ok(Self { pool, size })
    }

    /// Parse and validate the two config keys together.
    pub fn declare(pool: &str, size: u8) -> Result<Self, String> {
        Self::new(Cidr::parse(pool)?, size)
    }

    /// How many concurrent job networks this pool can address. This is a hard
    /// ceiling on concurrency that the operator has chosen, so it is worth
    /// reporting next to the runner cap at startup.
    pub fn slots(&self) -> u32 {
        1u32 << (self.size - self.pool.prefix)
    }

    /// The `i`th subnet, wrapping. Wrapping rather than returning `Option` is
    /// what lets [`NetPool::allocate`] start its scan anywhere and still be a
    /// simple loop.
    pub fn nth(&self, i: u32) -> Cidr {
        let i = i % self.slots();
        Cidr {
            base: self.pool.base + (i << (32 - self.size)),
            prefix: self.size,
        }
    }

    /// The first subnet in the pool that overlaps nothing in `taken`, scanning
    /// from `start` and wrapping once.
    ///
    /// `start` is not cosmetic. Two provisions that begin at the same moment both
    /// read the same `taken` set — the daemon is the only arbiter of what is
    /// really free, and reading it is not atomic with creating — so a scan that
    /// always began at zero would have them pick the *same* subnet every time
    /// and collide on every concurrent ramp. Starting each one at an offset
    /// derived from its own network name spreads them across the pool, which
    /// turns the common case into no collision at all and leaves the retry in
    /// [`crate::docker`] for the genuinely unlucky case and for foreign tools
    /// allocating at the same instant.
    pub fn allocate(&self, taken: &[Cidr], start: u32) -> Option<Cidr> {
        let slots = self.slots();
        (0..slots)
            .map(|k| self.nth(start.wrapping_add(k)))
            .find(|c| !taken.iter().any(|t| t.overlaps(c)))
    }
}

impl fmt::Display for NetPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in /{} subnets", self.pool, self.size)
    }
}

/// A stable scan offset for a network name — see [`NetPool::allocate`].
///
/// FNV-1a rather than `DefaultHasher`: the value has to be the same for the same
/// name across processes and releases, because a restarted faber adopting
/// in-flight work (ADR-0011) must not suddenly prefer different slots.
pub fn offset_for(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Pull every subnet out of the daemon's `network inspect` output.
///
/// Lines that are not CIDRs are skipped rather than failing the parse: the
/// listing is a *safety* input — the more of the host's address space it
/// accounts for, the fewer collisions — and one unparseable IPv6 block must not
/// cost faber the whole allocation.
pub fn parse_subnets<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Cidr> {
    lines
        .into_iter()
        .flat_map(|l| l.split_whitespace())
        .filter_map(|t| Cidr::parse(t).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Cidr {
        Cidr::parse(s).expect("test CIDR parses")
    }

    #[test]
    fn parses_and_renders_a_block() {
        assert_eq!(c("10.63.0.0/16").to_string(), "10.63.0.0/16");
        assert_eq!(c("192.168.5.0/24").to_string(), "192.168.5.0/24");
    }

    #[test]
    fn host_bits_are_masked_to_the_network_address() {
        // What an operator writes when they mean the block, and what Docker
        // itself accepts for the same string.
        assert_eq!(c("10.63.1.2/16"), c("10.63.0.0/16"));
    }

    #[test]
    fn rejects_malformed_blocks() {
        for bad in [
            "10.63.0.0",      // no prefix
            "10.63.0.0/x",    // prefix not a number
            "10.63.0.0/4",    // wider than MIN_PREFIX
            "10.63.0.0/31",   // narrower than MAX_PREFIX
            "10.63.0/16",     // not a dotted quad
            "10.63.0.999/16", // octet out of range
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn overlap_is_symmetric_and_catches_containment() {
        assert!(c("10.63.0.0/16").overlaps(&c("10.63.3.0/24")));
        assert!(c("10.63.3.0/24").overlaps(&c("10.63.0.0/16")));
        assert!(!c("10.63.0.0/16").overlaps(&c("10.64.0.0/16")));
        assert!(!c("10.63.3.0/24").overlaps(&c("10.63.4.0/24")));
    }

    #[test]
    fn a_pool_reports_its_slot_count() {
        let p = NetPool::declare("10.63.0.0/16", 24).expect("valid");
        assert_eq!(p.slots(), 256);
        assert_eq!(p.nth(0).to_string(), "10.63.0.0/24");
        assert_eq!(p.nth(5).to_string(), "10.63.5.0/24");
        // Wrapping is what lets `allocate` start its scan anywhere.
        assert_eq!(p.nth(256).to_string(), "10.63.0.0/24");
    }

    #[test]
    fn rejects_a_subnet_wider_than_its_pool() {
        let e = NetPool::declare("10.63.0.0/24", 16).expect_err("should refuse");
        assert!(e.contains("wider than the pool"), "{e}");
    }

    #[test]
    fn rejects_a_declaration_that_would_carve_too_many_slots() {
        let e = NetPool::declare("10.0.0.0/8", 30).expect_err("should refuse");
        assert!(e.contains("at most"), "{e}");
    }

    #[test]
    fn allocation_skips_everything_already_taken() {
        let p = NetPool::declare("10.63.0.0/16", 24).expect("valid");
        let taken = vec![c("10.63.0.0/24"), c("10.63.1.0/24")];
        assert_eq!(p.allocate(&taken, 0), Some(c("10.63.2.0/24")));
    }

    #[test]
    fn allocation_skips_a_wider_foreign_block_covering_part_of_the_pool() {
        // The case that makes overlap (not equality) the test: something else on
        // the host holds a /18 that swallows the pool's first 64 subnets.
        let p = NetPool::declare("10.63.0.0/16", 24).expect("valid");
        let taken = vec![c("10.63.0.0/18")];
        assert_eq!(p.allocate(&taken, 0), Some(c("10.63.64.0/24")));
    }

    #[test]
    fn allocation_wraps_and_finds_a_hole_behind_the_start() {
        let p = NetPool::declare("10.63.0.0/22", 24).expect("valid"); // 4 slots
        let taken = vec![c("10.63.1.0/24"), c("10.63.2.0/24"), c("10.63.3.0/24")];
        // Starts at slot 2, wraps past the end, finds slot 0.
        assert_eq!(p.allocate(&taken, 2), Some(c("10.63.0.0/24")));
    }

    #[test]
    fn a_full_pool_allocates_nothing() {
        let p = NetPool::declare("10.63.0.0/23", 24).expect("valid"); // 2 slots
        let taken = vec![c("10.63.0.0/24"), c("10.63.1.0/24")];
        assert_eq!(p.allocate(&taken, 0), None);
    }

    #[test]
    fn different_names_start_at_different_slots() {
        // The property that keeps two simultaneous ramps off the same subnet.
        let p = NetPool::declare("10.63.0.0/16", 24).expect("valid");
        let a = offset_for("faber-net-1-1700") % p.slots();
        let b = offset_for("faber-net-2-1700") % p.slots();
        assert_ne!(a, b);
    }

    #[test]
    fn the_same_name_always_starts_at_the_same_slot() {
        assert_eq!(
            offset_for("faber-net-42-1700"),
            offset_for("faber-net-42-1700")
        );
    }

    #[test]
    fn subnet_parsing_ignores_what_it_cannot_read() {
        let got = parse_subnets(["172.17.0.0/16 fd00::/64", "", "192.168.0.0/20"]);
        assert_eq!(got, vec![c("172.17.0.0/16"), c("192.168.0.0/20")]);
    }
}
