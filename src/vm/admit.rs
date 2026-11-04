//! Admission for VMs (ADR-0026 § "Size and recovery", § "Disk", § "Admission").
//!
//! Every decision here is a pure function of numbers the limiter hands it, so
//! each can be tested without a host. The limiter owns the state and calls
//! these under its lock.
//!
//! What gates a macOS VM job, in the order the first refusal is reported:
//!
//! 1. **The wall.** No job starts while the pf wall's verdict is red, stale,
//!    unread or out of date. Jobs already running finish.
//! 2. **The disk floor** (ADR-0024, ADR-0032): VM disks are files on the host
//!    volume, so the floor gates macOS VM jobs as it gates Docker ones.
//! 3. **The VM session** (ADR-0051): where the setup record names the session
//!    account, its supervisor must answer, or no macOS VM can start
//!    ([`VmSession`]). A record without the account has no such gate.
//! 4. **The golden image** (ADR-0049 §7): a job's image must have passed
//!    verification on this host ([`crate::vm::verify`]). A verification run
//!    itself is not gated on it, and a limiter told nothing of images gates none.
//! 5. **The macOS class**: at most [`MACOS_VM_CAP`] per host, Apple's limit.
//! 6. **The machine share** (ADR-0021): each VM takes [`MACOS_VM_MIB`] from
//!    what the declared share leaves, after the Linux VM's reservation.
//!
//! Fail closed: a wall nobody has read, and a share nobody has declared, both
//! refuse. A VM is new in this release, so there is no earlier behaviour for an
//! absent key to keep. The disk floor is the exception, and keeps ADR-0024's
//! shape: an off floor or a stale reading opens, so a stopped `df` cannot hold
//! admission shut forever.

use crate::host::{fmt_gib, ShortOfRoom};
use crate::vm::verify::Standing;
use crate::wall::report::Judgement;

/// Apple's licence allows two additional macOS copies in VMs per Mac, and the
/// framework refuses a third with `VZErrorDomain 6`.
pub const MACOS_VM_CAP: usize = 2;

/// vCPUs one macOS VM takes (ADR-0026 § "Resources").
pub const MACOS_VM_VCPUS: u64 = 4;

/// Memory one macOS VM takes from the share, MiB: the top of the 6–8 GiB range,
/// so the ledger is an upper bound.
pub const MACOS_VM_MIB: u64 = 8 * 1024;

/// What a macOS VM job is charged against free disk, MiB. The golden image is
/// about 40 GB, but a clone grows only by what its job writes, so this is the
/// ramp charge of ADR-0032 §1 sized for a macOS build: that of a dind job.
pub const MACOS_VM_DISK_CHARGE_MIB: u64 = 10 * 1024;

/// How long an `Open` stays good without a new judgement, ms. The checker writes
/// every few seconds and the reader already refuses a stale file; this is the
/// limiter's own bound, so a poller that dies cannot leave the last green in
/// force (ADR-0026: a dead checker is red).
pub const WALL_MAX_AGE_MS: u64 = 60_000;

/// What the limiter knows of the wall's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WallGate {
    /// No verdict has been read yet. Closed: an unread wall proves nothing.
    #[default]
    Unread,
    /// Green, current rules.
    Open,
    /// Red, stale, unreadable or untrusted; the reader's sentence says which.
    Red(String),
    /// Green, but the stored rules are older than this release wants. Closed:
    /// the old rules keep applying to what runs, but no new job starts on a
    /// wall that is not the one this release expects.
    OutOfDate { installed: u32, wanted: u32 },
}

impl WallGate {
    /// The reader's judgement ([`crate::wall::report::judge`]) as a gate.
    pub fn from_judgement(j: Judgement) -> Self {
        match j {
            Judgement::Green => WallGate::Open,
            Judgement::Red(why) => WallGate::Red(why),
            Judgement::OutOfDate { installed, wanted } => WallGate::OutOfDate { installed, wanted },
        }
    }

    /// The gate as it stands at `now_ms`, given when it was last judged: an
    /// `Open` older than [`WALL_MAX_AGE_MS`] (or dated in the future) is red.
    pub fn at(&self, observed_ms: u64, now_ms: u64) -> WallGate {
        match self {
            WallGate::Open if now_ms.abs_diff(observed_ms) > WALL_MAX_AGE_MS => {
                WallGate::Red(format!(
                    "no wall verdict for {}s: the poller has stopped",
                    now_ms.saturating_sub(observed_ms) / 1000
                ))
            }
            other => other.clone(),
        }
    }

    /// Whether a job may start.
    pub fn admits(&self) -> bool {
        matches!(self, WallGate::Open)
    }

    /// `"open"`, `"red"`, `"out_of_date"` or `"unread"`: the key `/api/state`
    /// shows, distinct from every other hold.
    pub fn as_str(&self) -> &'static str {
        match self {
            WallGate::Open => "open",
            WallGate::Red(_) => "red",
            WallGate::OutOfDate { .. } => "out_of_date",
            WallGate::Unread => "unread",
        }
    }

    /// A sentence saying why the wall is closed, `None` when it is open.
    pub fn detail(&self) -> Option<String> {
        match self {
            WallGate::Open => None,
            WallGate::Unread => Some("the wall's verdict has not been read yet".to_string()),
            WallGate::Red(why) => Some(why.clone()),
            WallGate::OutOfDate { installed, wanted } => Some(format!(
                "the wall's rules are version {installed} and this faber wants {wanted}: \
                 run `sudo faber setup` to install the new ones"
            )),
        }
    }
}

/// The reboot mode setup gave the session account (ADR-0051), as the record
/// says it: what the operator does to bring the session back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    Unlock,
    AutoLogin,
    Manual,
    /// Not in the record, or a mode this release does not know.
    #[default]
    Unknown,
}

impl SessionMode {
    /// The record's `reboot_mode` text (`setup::session::RebootMode::name`).
    pub fn from_record(mode: Option<&str>) -> Self {
        match mode {
            Some("unlock") => SessionMode::Unlock,
            Some("auto-login") => SessionMode::AutoLogin,
            Some("manual") => SessionMode::Manual,
            _ => SessionMode::Unknown,
        }
    }

    /// What brings the session back, in this mode.
    pub fn action(self) -> String {
        let account = crate::platform::layout::FABER_MAC_ACCOUNT;
        match self {
            SessionMode::Unlock => {
                format!("unlock the disk as {account} after a reboot, at the FileVault screen")
            }
            SessionMode::AutoLogin => format!(
                "the account is not logged in; check auto-login (macOS logs {account} in at \
                 boot, and ignores auto-login while FileVault is on)"
            ),
            SessionMode::Manual => format!("log {account} in"),
            SessionMode::Unknown => format!("log {account} in, or re-run `sudo faber setup`"),
        }
    }
}

/// Whether the macOS VM session's supervisor answers (ADR-0051), as the
/// session probe last found it ([`crate::vm::session`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VmSession {
    /// No session account recorded, or no macOS VMs here: no gate, as before.
    #[default]
    NotRequired,
    /// Recorded and not probed yet. Closed, as an unread wall is.
    Unprobed(SessionMode),
    /// Its supervisor answered the last handshake.
    Up,
    /// It did not; `why` is the client's error.
    Down { mode: SessionMode, why: String },
    /// The recorded account does not resolve to `_faber_mac` (never made, or
    /// removed, or another uid): faber does not connect, and setup is the fix.
    NoAccount(String),
}

impl VmSession {
    pub fn admits(&self) -> bool {
        matches!(self, VmSession::NotRequired | VmSession::Up)
    }

    /// `"not_required"`, `"unprobed"`, `"up"` or `"down"`, for `/api/state`.
    pub fn as_str(&self) -> &'static str {
        match self {
            VmSession::NotRequired => "not_required",
            VmSession::Unprobed(_) => "unprobed",
            VmSession::Up => "up",
            VmSession::Down { .. } => "down",
            VmSession::NoAccount(_) => "no_account",
        }
    }
}

/// Why a VM job did not start. The first refusal in the order above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmRefusal {
    Wall(WallGate),
    Disk(ShortOfRoom),
    /// The setup record names a session account this host does not have.
    NoSessionAccount(String),
    /// The session account's supervisor does not answer: no session runs it.
    NoVmSession {
        mode: SessionMode,
        why: Option<String>,
    },
    /// The job's golden image has not passed verification on this host
    /// (ADR-0049 §7), or one is running.
    ImageUnverified {
        image: String,
        standing: Standing,
    },
    /// Two macOS VMs are already running.
    MacosCap {
        running: usize,
        max: usize,
    },
    /// `[host] keep_memory_gib` is unstated or leaves nothing, or the machine
    /// has not been measured: there is no share to plan the VM within.
    ShareUndeclared,
    /// The share has no room for another VM after the Linux VM's reservation
    /// and the macOS VMs already running.
    Memory {
        need_mib: u64,
        held_mib: u64,
        linux_mib: u64,
        left_mib: u64,
    },
}

impl VmRefusal {
    /// The gate's key and a line saying why, in the shape of
    /// [`crate::limiter::WaitView`].
    pub fn describe(&self) -> (&'static str, String) {
        match self {
            VmRefusal::Wall(w) => (
                "wall",
                format!(
                    "VM admission stopped: {}",
                    w.detail()
                        .unwrap_or_else(|| "the wall is closed".to_string())
                ),
            ),
            VmRefusal::Disk(s) => (
                "disk",
                format!(
                    "{} GiB free on the VM disks' volume, less {} GiB held for jobs starting \
                     and this one, is under the {} GiB admission floor",
                    fmt_gib(s.free_mib),
                    fmt_gib(s.held_mib),
                    s.floor_gib
                ),
            ),
            VmRefusal::NoSessionAccount(why) => {
                ("vm_session", format!("no macOS VM session: {why}"))
            }
            VmRefusal::NoVmSession { mode, why } => (
                "vm_session",
                format!(
                    "no macOS VM session: {}{}",
                    mode.action(),
                    why.as_deref()
                        .map(|w| format!(" (the session's supervisor: {w})"))
                        .unwrap_or_default()
                ),
            ),
            VmRefusal::ImageUnverified { image, standing } => (
                "golden_image",
                standing
                    .detail(image)
                    .unwrap_or_else(|| format!("the golden image {image} is verified")),
            ),
            VmRefusal::MacosCap { running, max } => (
                "macos_cap",
                format!("{running} of {max} macOS VMs are running, Apple's limit per Mac"),
            ),
            VmRefusal::ShareUndeclared => (
                "share",
                "no machine share to plan a VM within: state host.keep_memory_gib (ADR-0021)"
                    .to_string(),
            ),
            VmRefusal::Memory {
                need_mib,
                held_mib,
                linux_mib,
                left_mib,
            } => (
                "vm_memory",
                format!(
                    "a macOS VM needs {} GiB, and the share leaves {} GiB, of which the Linux \
                     VM holds {} GiB and running macOS VMs {} GiB",
                    fmt_gib(*need_mib),
                    fmt_gib(*left_mib),
                    fmt_gib(*linux_mib),
                    fmt_gib(*held_mib)
                ),
            ),
        }
    }
}

/// Everything [`decide_macos`] needs.
#[derive(Debug, Clone)]
pub struct MacosInput<'a> {
    pub wall: &'a WallGate,
    /// The disk gate's refusal for this job's charge, from
    /// [`crate::host::short_of_room`].
    pub disk: Option<ShortOfRoom>,
    /// The macOS VM session (ADR-0051).
    pub session: VmSession,
    /// The job's golden image and how it stands (ADR-0049 §7), `None` when no
    /// image is gated: a verification run, or a limiter told nothing of images.
    pub image: Option<(&'a str, &'a Standing)>,
    pub macos_running: usize,
    /// macOS VMs faber holds without running a job in them: `stopping`,
    /// `unmanaged`, or from before instance ids (adoption), or one it could not
    /// confirm stopped. Apple's licence limit counts running VMs whoever owns
    /// them, so each takes one of `macos_cap` too. Their memory is not charged:
    /// an `unmanaged` VM's size is unknown to faber.
    pub macos_held: usize,
    pub macos_cap: usize,
    /// What the share leaves CI of the machine's memory
    /// ([`crate::machine::MachineShare::memory_left_mib`]); `None` when no
    /// share is declared or the machine is unmeasured.
    pub share_left_mib: Option<u64>,
    /// The long-lived Linux VM's memory, reserved inside the share. Zero while
    /// Docker Desktop's VM serves Docker: it is outside the share.
    pub linux_vm_mib: u64,
    pub vm_mib: u64,
}

/// May one more macOS VM job start?
pub fn decide_macos(i: &MacosInput) -> Result<(), VmRefusal> {
    if !i.wall.admits() {
        return Err(VmRefusal::Wall(i.wall.clone()));
    }
    if let Some(short) = i.disk {
        return Err(VmRefusal::Disk(short));
    }
    match &i.session {
        VmSession::NotRequired | VmSession::Up => {}
        VmSession::Unprobed(mode) => {
            return Err(VmRefusal::NoVmSession {
                mode: *mode,
                why: None,
            })
        }
        VmSession::Down { mode, why } => {
            return Err(VmRefusal::NoVmSession {
                mode: *mode,
                why: Some(why.clone()),
            })
        }
        VmSession::NoAccount(why) => return Err(VmRefusal::NoSessionAccount(why.clone())),
    }
    if let Some((image, standing)) = i.image {
        if !standing.admits() {
            return Err(VmRefusal::ImageUnverified {
                image: image.to_string(),
                standing: standing.clone(),
            });
        }
    }
    let counted = i.macos_running.saturating_add(i.macos_held);
    if counted >= i.macos_cap {
        return Err(VmRefusal::MacosCap {
            running: counted,
            max: i.macos_cap,
        });
    }
    let Some(left_mib) = i.share_left_mib else {
        return Err(VmRefusal::ShareUndeclared);
    };
    let held_mib = (i.macos_running as u64).saturating_mul(i.vm_mib);
    let used = held_mib.saturating_add(i.linux_vm_mib);
    if used.saturating_add(i.vm_mib) > left_mib {
        return Err(VmRefusal::Memory {
            need_mib: i.vm_mib,
            held_mib,
            linux_mib: i.linux_vm_mib,
            left_mib,
        });
    }
    Ok(())
}

/// May a Docker job start, as far as the wall goes? `required` is set once
/// Docker runs in the Linux VM faber owns: Docker Desktop's jobs are not behind
/// the wall, and a host that never installed it must not lose Docker admission
/// to a release that added it (AGENTS.md: narrowing a default is a behaviour
/// change). Once required, it fails closed like the macOS class.
pub fn docker_wall_refusal(required: bool, wall: &WallGate) -> Option<VmRefusal> {
    (required && !wall.admits()).then(|| VmRefusal::Wall(wall.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024;

    fn input(wall: &WallGate) -> MacosInput<'_> {
        MacosInput {
            wall,
            disk: None,
            session: VmSession::NotRequired,
            image: None,
            macos_running: 0,
            macos_held: 0,
            macos_cap: MACOS_VM_CAP,
            share_left_mib: Some(32 * GIB),
            linux_vm_mib: 16 * GIB,
            vm_mib: MACOS_VM_MIB,
        }
    }

    /// ADR-0051's "no VM session": behind the wall and the disk, ahead of the
    /// cap, worded by the reboot mode.
    #[test]
    fn no_vm_session_refuses_ahead_of_the_cap_in_the_modes_words() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.macos_running = MACOS_VM_CAP;
        assert!(matches!(decide_macos(&i), Err(VmRefusal::MacosCap { .. })));
        i.session = VmSession::Up;
        assert!(matches!(decide_macos(&i), Err(VmRefusal::MacosCap { .. })));
        i.session = VmSession::Down {
            mode: SessionMode::Manual,
            why: "not reachable".into(),
        };
        let refusal = decide_macos(&i).unwrap_err();
        assert_eq!(
            refusal.describe(),
            (
                "vm_session",
                "no macOS VM session: log _faber_mac in (the session's supervisor: not \
                 reachable)"
                    .to_string()
            )
        );
        for (mode, words) in [
            (
                SessionMode::Unlock,
                "unlock the disk as _faber_mac after a reboot",
            ),
            (
                SessionMode::AutoLogin,
                "the account is not logged in; check auto-login",
            ),
            (SessionMode::Manual, "log _faber_mac in"),
            (SessionMode::Unknown, "re-run `sudo faber setup`"),
        ] {
            i.session = VmSession::Unprobed(mode);
            let (gate, detail) = decide_macos(&i).unwrap_err().describe();
            assert_eq!(gate, "vm_session");
            assert!(detail.contains(words), "{mode:?}: {detail}");
        }
        // The wall comes first.
        let red = WallGate::Red("pf is off".into());
        let mut i = input(&red);
        i.session = VmSession::Unprobed(SessionMode::Manual);
        assert!(matches!(decide_macos(&i), Err(VmRefusal::Wall(_))));
    }

    #[test]
    fn a_held_vm_takes_one_of_the_two_macos_slots() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.macos_held = 1;
        assert_eq!(decide_macos(&i), Ok(()), "one held, none running: one fits");
        i.macos_running = 1;
        assert_eq!(
            decide_macos(&i),
            Err(VmRefusal::MacosCap { running: 2, max: 2 }),
            "one running and one held is the licence limit"
        );
        i.macos_running = 0;
        i.macos_held = 2;
        assert!(matches!(decide_macos(&i), Err(VmRefusal::MacosCap { .. })));
    }

    #[test]
    fn a_held_vm_is_not_charged_memory() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.macos_held = 1;
        // For example, a 32 GiB share and a Linux VM at 16: room for the held
        // one's 8 and one more 8.
        i.share_left_mib = Some(24 * GIB);
        assert_eq!(decide_macos(&i), Ok(()));
    }

    #[test]
    fn an_open_wall_with_room_admits() {
        assert_eq!(decide_macos(&input(&WallGate::Open)), Ok(()));
    }

    #[test]
    fn every_wall_state_but_open_refuses() {
        for w in [
            WallGate::Unread,
            WallGate::Red("pf is off".into()),
            WallGate::OutOfDate {
                installed: 1,
                wanted: 2,
            },
        ] {
            assert_eq!(
                decide_macos(&input(&w)),
                Err(VmRefusal::Wall(w.clone())),
                "{w:?}"
            );
        }
    }

    #[test]
    fn the_wall_is_reported_before_every_other_refusal() {
        let w = WallGate::Red("x".into());
        let mut i = input(&w);
        i.macos_running = 2;
        i.share_left_mib = None;
        i.disk = Some(ShortOfRoom {
            free_mib: 0,
            volume: crate::host::DiskVolume::Docker,
            held_mib: 0,
            floor_gib: 10,
        });
        assert!(matches!(decide_macos(&i), Err(VmRefusal::Wall(_))));
    }

    #[test]
    fn an_open_wall_goes_stale_into_red() {
        let t = 1_000_000;
        assert_eq!(WallGate::Open.at(t, t + WALL_MAX_AGE_MS), WallGate::Open);
        assert!(matches!(
            WallGate::Open.at(t, t + WALL_MAX_AGE_MS + 1),
            WallGate::Red(_)
        ));
        // Dated in the future is no evidence either.
        assert!(matches!(
            WallGate::Open.at(t + WALL_MAX_AGE_MS + 1, t),
            WallGate::Red(_)
        ));
        // Closed states stay as they are.
        assert_eq!(WallGate::Unread.at(0, u64::MAX), WallGate::Unread);
    }

    #[test]
    fn the_judgement_maps_to_the_gate() {
        assert_eq!(WallGate::from_judgement(Judgement::Green), WallGate::Open);
        assert_eq!(
            WallGate::from_judgement(Judgement::Red("r".into())),
            WallGate::Red("r".into())
        );
        assert_eq!(
            WallGate::from_judgement(Judgement::OutOfDate {
                installed: 1,
                wanted: 2
            }),
            WallGate::OutOfDate {
                installed: 1,
                wanted: 2
            }
        );
        assert_eq!(WallGate::default(), WallGate::Unread);
    }

    #[test]
    fn a_third_macos_vm_is_refused_however_much_memory_is_left() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.macos_running = 2;
        i.share_left_mib = Some(512 * GIB);
        assert_eq!(
            decide_macos(&i),
            Err(VmRefusal::MacosCap { running: 2, max: 2 })
        );
        i.macos_running = 1;
        assert_eq!(decide_macos(&i), Ok(()));
    }

    #[test]
    fn the_disk_floor_gates_a_macos_job() {
        let w = WallGate::Open;
        let short = ShortOfRoom {
            free_mib: 12 * GIB,
            volume: crate::host::DiskVolume::Docker,
            held_mib: 10 * GIB,
            floor_gib: 10,
        };
        let mut i = input(&w);
        i.disk = Some(short);
        assert_eq!(decide_macos(&i), Err(VmRefusal::Disk(short)));
    }

    #[test]
    fn no_declared_share_refuses() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.share_left_mib = None;
        assert_eq!(decide_macos(&i), Err(VmRefusal::ShareUndeclared));
    }

    #[test]
    fn the_linux_vm_is_reserved_inside_the_share() {
        // For example, a share of 32 GiB. The Linux VM holds 16, so two 8 GiB
        // macOS VMs fit exactly and a third byte would not.
        let w = WallGate::Open;
        let mut i = input(&w);
        i.macos_running = 1;
        assert_eq!(decide_macos(&i), Ok(()));
        i.share_left_mib = Some(32 * GIB - 1);
        assert!(matches!(
            decide_macos(&i),
            Err(VmRefusal::Memory {
                held_mib, linux_mib, ..
            }) if held_mib == 8 * GIB && linux_mib == 16 * GIB
        ));
    }

    #[test]
    fn a_linux_vm_larger_than_the_share_refuses_without_underflow() {
        let w = WallGate::Open;
        let mut i = input(&w);
        i.linux_vm_mib = u64::MAX;
        assert!(matches!(decide_macos(&i), Err(VmRefusal::Memory { .. })));
    }

    #[test]
    fn docker_is_untouched_until_the_wall_is_required() {
        for w in [WallGate::Unread, WallGate::Red("x".into())] {
            assert_eq!(docker_wall_refusal(false, &w), None);
            assert_eq!(
                docker_wall_refusal(true, &w),
                Some(VmRefusal::Wall(w.clone()))
            );
        }
        assert_eq!(docker_wall_refusal(true, &WallGate::Open), None);
    }

    #[test]
    fn every_refusal_names_its_gate() {
        let w = WallGate::Red("pf is off".into());
        let (gate, line) = VmRefusal::Wall(w).describe();
        assert_eq!(gate, "wall");
        assert!(line.contains("pf is off"), "{line}");
        assert_eq!(VmRefusal::ShareUndeclared.describe().0, "share");
        assert_eq!(
            VmRefusal::MacosCap { running: 2, max: 2 }.describe().0,
            "macos_cap"
        );
    }
}
