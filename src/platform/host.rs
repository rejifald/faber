//! Controlling the Docker host itself: reviving a wedged daemon, and finding
//! the filesystem whose free space actually matters (ADR-0007).
//!
//! Both are genuinely platform-divergent, and the disk one is a correctness
//! trap rather than a cosmetic difference.
//!
//! # The disk faber must measure
//!
//! The disk-floor gate pauses admission and reclaims CI images when free space
//! falls below `disk_floor_gib`. It only works if it measures the volume Docker
//! actually writes to.
//!
//! On **macOS**, Docker's data lives inside a VM disk image, and the thing the
//! host's `df` can see is the file backing it. Since ADR-0026 those are faber's
//! own VM disks, files under the VM home, so macOS measures the volume holding
//! that directory (the root volume where there is no VM home). Asking Docker
//! where its root directory is would answer with a path *inside* the VM, which
//! the host cannot stat. Docker Desktop's container directory is not faber's
//! and is no longer measured.
//!
//! That volume is only half of it under SYSTEM (#410). The Linux VM's disk is a
//! fixed-size image, and Docker's data root is inside it: it fills on its own
//! while the Mac's volume reads healthy. So [`has_linux_vm`] says a second
//! reading is needed, taken inside the VM over its Docker socket
//! ([`crate::host::read_linux_vm_disk`]), and the Docker-side gates act on the
//! tighter of the two ([`crate::host::tighter`]).
//!
//! On **Linux** the daemon writes to its `data-root`, which defaults to
//! `/var/lib/docker` but is operator-configurable in `daemon.json` — and which
//! is very often a different filesystem from `$HOME` (a dedicated partition, an
//! LVM volume, a separate disk). Falling back to `$HOME`, as the macOS path
//! does, silently measures the wrong volume: the floor then either never fires
//! while Docker's disk fills, or fires constantly and prunes for nothing. The
//! correct source is `docker info --format '{{.DockerRootDir}}'`, which on Linux
//! is a real host path.
//!
//! # Reviving a wedged daemon
//!
//! On macOS, faber's Docker is its own Linux VM once `sudo faber setup` has run
//! (ADR-0026), and reviving it is asking the VM supervisor to stop that VM; the
//! engine driver ([`crate::vm::engine`]) then starts it again. Docker Desktop is
//! the operator's app and is never quit, killed or relaunched: on a macOS host
//! that has not been set up there is no revival at all.
//!
//! On Linux, dockerd is a **system** service. A user-level faber cannot restart
//! it without a privilege grant, so revival is best-effort by design: faber
//! attempts it, and a refusal is recorded as a fault while admission stays
//! closed (Gate 0 already fails closed on an unreachable daemon, so the
//! degradation path exists and is tested). An operator who wants ADR-0007's full
//! promise can grant exactly one command via polkit or sudoers; nothing about
//! installing faber should *require* that.
//!
//! Under the SYSTEM layout (ADR-0025) a Linux host has no revival: faber runs as
//! `_faber` there, and the engine it cannot reach is the machine's, not faber's
//! to restart from a role account. [`crate::host::revival`] decides who restarts
//! what ([`crate::host::Custodian`]), and each `revive_docker` below refuses on
//! its own as well, so no future caller can reach a restart that is not faber's.

use std::time::Duration;

/// How long `docker info` gets to answer before faber gives up on reading
/// `DockerRootDir`. The same bound [`crate::host::disk_free_mib`] gives `df`:
/// this is a local daemon query, not a pull or a build, so a hang past this
/// means the daemon isn't answering, not that it's merely slow.
#[cfg(target_os = "linux")]
const DOCKER_INFO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `systemctl restart docker` gets before faber gives up on it.
///
/// Generous: this one command *is* the whole restart (telling a service manager
/// to do its job), and the upstream `docker.service` unit is `Type=notify`, so `systemctl` itself
/// blocks until dockerd reports ready — which can take real time while it
/// restores networks and reconnects running containers. The common failure, a
/// missing privilege grant, returns in well under a second, so the generous
/// cap costs nothing on that path.
#[cfg(target_os = "linux")]
const SYSTEMCTL_RESTART_TIMEOUT: Duration = Duration::from_secs(60);

/// The path whose free space the disk-floor gate (ADR-0007) must measure — see
/// the module doc for why this is genuinely platform-specific rather than one
/// shared implementation.
///
/// `None` means "could not be determined": [`crate::host::docker_data_path`]
/// propagates that straight through to [`crate::host::below_disk_floor`],
/// which already treats an unknown free-space reading as "not breached" rather
/// than a guess. That is the whole point of returning `None` here instead of a
/// fallback path — see the module doc for why guessing `$HOME` on Linux is the
/// bug this function exists to avoid.
#[cfg(target_os = "macos")]
pub async fn docker_disk_path() -> Option<String> {
    Some(macos_disk_path(
        crate::platform::layout(),
        &crate::platform::vm_home(),
    ))
}

/// The volume the macOS floor measures (ADR-0026 § Disk): every VM disk is a
/// file under the VM home, so that volume is the one jobs fill. Under LEGACY
/// there is no VM home, and the root volume is the machine's one data volume.
/// Docker Desktop's container directory is never named: it is not faber's.
#[cfg(any(target_os = "macos", test))]
fn macos_disk_path(layout: crate::platform::Layout, vm_home: &std::path::Path) -> String {
    match layout {
        crate::platform::Layout::System => vm_home.display().to_string(),
        crate::platform::Layout::Legacy => "/".to_string(),
    }
}

/// Which volume [`docker_disk_path`] names (#410): the Mac's volume holding the
/// VM disks under SYSTEM, the Docker volume everywhere else.
pub fn docker_disk_volume() -> crate::host::DiskVolume {
    if has_linux_vm() {
        crate::host::DiskVolume::MacVms
    } else {
        crate::host::DiskVolume::Docker
    }
}

/// Whether Docker's data root is inside a Linux VM of faber's, on a disk of its
/// own that [`docker_disk_path`] does not measure (#410): a macOS SYSTEM host
/// (ADR-0026). The VM's disk is then read over its Docker socket
/// ([`crate::host::read_linux_vm_disk`]).
pub fn has_linux_vm() -> bool {
    linux_vm_in(crate::platform::layout())
}

/// [`has_linux_vm`] for a given layout: only a Mac's SYSTEM layout runs Docker
/// in a VM of faber's. A Linux host's Docker is the machine's own, whatever the
/// layout.
fn linux_vm_in(layout: crate::platform::Layout) -> bool {
    cfg!(target_os = "macos") && layout == crate::platform::Layout::System
}

/// `docker info --format '{{.DockerRootDir}}'` — the real host path Docker
/// writes to on Linux.
///
/// `None` on any failure to ask (unreachable daemon, timeout, an unparsable or
/// empty answer) rather than a fallback guess — see the module doc for why,
/// and [`crate::host::docker_data_path`] for how every caller already treats
/// that `None` the same as an unreadable `df`: not acted on blind.
#[cfg(target_os = "linux")]
pub async fn docker_disk_path() -> Option<String> {
    // Routed through the same allowlisted constructor as every other `docker`
    // spawn (AGENTS.md/ADR-0025 §8) rather than `run_bounded_output`'s plain
    // `Command::new("docker")`, which would inherit this process's whole
    // environment.
    let mut cmd = crate::docker::docker_command();
    cmd.args(["info", "--format", "{{.DockerRootDir}}"]);
    let out = crate::host::run_bounded_output_cmd(cmd, DOCKER_INFO_TIMEOUT, "docker info").await?;
    parse_docker_root_dir(&out)
}

/// Trim `docker info`'s answer down to a usable path, or `None` for an empty
/// one — a daemon that answers but reports nothing is as unusable a reading as
/// a failed call, and must not hand the caller an empty string to `df`.
///
/// `None` as well for anything that is not an absolute path (#105). The answer is
/// the daemon's, and it is handed to `df` as an argument: one starting with `-`
/// would be read as an option, and a relative one would measure wherever faber
/// happens to run.
///
/// Split out from the `docker info` call itself, and gated on `test` as well as
/// Linux, purely so this parsing has a seam this crate's other platforms can
/// exercise too: the process I/O around it needs a real Linux host to run, but
/// the string handling doesn't, and shouldn't go unverified just because this
/// build happens to be on macOS.
#[cfg(any(target_os = "linux", test))]
fn parse_docker_root_dir(raw: &str) -> Option<String> {
    let path = raw.trim();
    crate::untrusted::host_path(path).then(|| path.to_string())
}

/// `None` — no Docker-writable volume is modeled on this platform. Exists only
/// so the crate keeps compiling here; see [`crate::platform`]'s module doc.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub async fn docker_disk_path() -> Option<String> {
    None
}

/// Refuse a Docker restart that is not faber's to make. The actuator's own copy
/// of [`crate::host::revival`]'s rule ([`crate::host::Custodian`]); see the module
/// doc.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refuse_unless(expected: crate::host::Custodian) -> anyhow::Result<()> {
    match crate::host::Custodian::for_this_host() {
        crate::host::Custodian::NotOurs(why) => anyhow::bail!("{why}"),
        found if found != expected => anyhow::bail!("{found:?} is not {expected:?}"),
        _ => Ok(()),
    }
}

/// How long `sysctl` gets to answer. A local kernel query: a hang past this is
/// not slowness.
#[cfg(target_os = "macos")]
const SYSCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// The machine itself, as distinct from the Docker daemon's view of it — ADR-0021
/// §1: its memory and cores, its memory pressure, and its swap.
///
/// On macOS, one `sysctl` call for the five unprivileged names the ADR lists, and
/// a second for the performance/efficiency split, which exists only on Apple
/// silicon and whose absence is not a failed reading. The binary is named by its
/// path: a launchd job's `PATH` is not the operator's.
///
/// `None` on any failure to read the memory or the core count. The limiter then
/// keeps the last good reading until it is too old to be evidence, and the
/// pressure gate opens (see [`crate::machine::MachineState::hold`]).
#[cfg(target_os = "macos")]
pub async fn read_machine() -> Option<crate::machine::MachineReading> {
    let out = crate::host::run_bounded_output(
        "/usr/sbin/sysctl",
        &[
            "hw.memsize",
            "hw.ncpu",
            "kern.memorystatus_vm_pressure_level",
            "vm.swapusage",
            "vm.compressor_bytes_used",
        ],
        SYSCTL_TIMEOUT,
    )
    .await?;
    let mut reading = parse_sysctl(&out)?;
    if let Some(split) = crate::host::run_bounded_output(
        "/usr/sbin/sysctl",
        &["hw.perflevel0.logicalcpu", "hw.perflevel1.logicalcpu"],
        SYSCTL_TIMEOUT,
    )
    .await
    {
        let split = sysctl_values(&split);
        reading.perf_cpus = split("hw.perflevel0.logicalcpu").and_then(crate::untrusted::number);
        reading.efficiency_cpus =
            split("hw.perflevel1.logicalcpu").and_then(crate::untrusted::number);
    }
    Some(reading)
}

/// The machine on Linux: `/proc/meminfo` for memory and swap, the process's CPU
/// affinity for its cores (what it may actually run on, not what the board has),
/// and `/proc/pressure/memory`, which is shown and not gated on — ADR-0021 §2c
/// keeps that gate inert until thresholds are chosen from a Linux host's
/// measurements.
#[cfg(target_os = "linux")]
pub async fn read_machine() -> Option<crate::machine::MachineReading> {
    let meminfo = tokio::fs::read_to_string("/proc/meminfo").await.ok()?;
    let psi = tokio::fs::read_to_string("/proc/pressure/memory")
        .await
        .ok();
    let cpus = std::thread::available_parallelism().ok()?.get() as u64;
    parse_linux_machine(&meminfo, psi.as_deref(), cpus)
}

/// `None` — the machine is not modeled on this platform. Exists only so the
/// crate keeps compiling here; see [`crate::platform`]'s module doc.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub async fn read_machine() -> Option<crate::machine::MachineReading> {
    None
}

/// `sysctl`'s `name: value` lines, as a lookup by name.
#[cfg(any(target_os = "macos", test))]
fn sysctl_values<'a>(text: &'a str) -> impl Fn(&str) -> Option<&'a str> + 'a {
    move |name| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim())
        })
    }
}

/// A reading of the machine from `sysctl`'s output. `None` without a plausible
/// memory size and core count: everything the share is planned on comes from
/// those two. The rest is optional, each on its own — a kernel that renames one
/// of them costs that field, not the reading.
#[cfg(any(target_os = "macos", test))]
fn parse_sysctl(text: &str) -> Option<crate::machine::MachineReading> {
    use crate::machine::{MachineReading, PressureLevel};
    use crate::untrusted::{number, MAX_CPUS, MAX_MEM_BYTES};
    let value = sysctl_values(text);
    let mem_bytes = value("hw.memsize").and_then(number)?;
    let cpus = value("hw.ncpu").and_then(number)?;
    if !(1..=MAX_MEM_BYTES).contains(&mem_bytes) || !(1..=MAX_CPUS).contains(&cpus) {
        return None;
    }
    let swap = value("vm.swapusage");
    Some(MachineReading {
        mem_mib: mem_bytes >> 20,
        cpus,
        pressure: value("kern.memorystatus_vm_pressure_level")
            .and_then(number)
            .and_then(PressureLevel::from_sysctl),
        swap_used_mib: swap.and_then(|s| swapusage_mib(s, "used")),
        swap_total_mib: swap.and_then(|s| swapusage_mib(s, "total")),
        compressor_mib: value("vm.compressor_bytes_used")
            .and_then(number)
            .map(|b| b >> 20),
        ..MachineReading::default()
    })
}

/// One figure out of `vm.swapusage` (`total = 8192.00M  used = 6144.25M  free =
/// 2047.75M  (encrypted)`), in MiB.
#[cfg(any(target_os = "macos", test))]
fn swapusage_mib(text: &str, field: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let raw = words
        .windows(3)
        .find(|w| w[0] == field && w[1] == "=")
        .map(|w| w[2])?;
    let (digits, scale) = match raw.char_indices().last()? {
        (i, 'K') => (&raw[..i], 1.0 / 1024.0),
        (i, 'M') => (&raw[..i], 1.0),
        (i, 'G') => (&raw[..i], 1024.0),
        _ => return None,
    };
    let mib = digits.parse::<f64>().ok()? * scale;
    (mib.is_finite() && mib >= 0.0).then(|| mib.round() as u64)
}

/// A reading of the machine from `/proc/meminfo`, `/proc/pressure/memory` (when
/// the kernel has PSI) and the affinity core count.
#[cfg(any(target_os = "linux", test))]
fn parse_linux_machine(
    meminfo: &str,
    psi: Option<&str>,
    cpus: u64,
) -> Option<crate::machine::MachineReading> {
    let kib = |name: &str| {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            crate::untrusted::number(rest.trim().trim_end_matches("kB").trim())
        })
    };
    let mem_mib = kib("MemTotal")? >> 10;
    if mem_mib == 0 || !(1..=crate::untrusted::MAX_CPUS).contains(&cpus) {
        return None;
    }
    let swap_total = kib("SwapTotal");
    let swap_free = kib("SwapFree");
    Some(crate::machine::MachineReading {
        mem_mib,
        cpus,
        psi_some_avg60: psi.and_then(crate::cpu::parse_psi_some_avg60),
        swap_used_mib: swap_total
            .zip(swap_free)
            .map(|(t, f)| t.saturating_sub(f) >> 10),
        swap_total_mib: swap_total.map(|t| t >> 10),
        ..crate::machine::MachineReading::default()
    })
}

/// Restart faber's Linux VM on macOS (ADR-0026, "Size and recovery"): ask the VM
/// supervisor to stop it and wait until it reports `stopped`; the engine driver
/// sees it leave service and starts it again. Refused on a host that has not been
/// set up, where the Docker faber reaches is the operator's Docker Desktop, which
/// faber never quits, kills or relaunches.
///
/// [`crate::host::revive_docker`] owns confirming Docker actually answers
/// afterwards; this function's job ends at the VM being stopped.
#[cfg(target_os = "macos")]
pub async fn revive_docker() -> anyhow::Result<()> {
    refuse_unless(crate::host::Custodian::Vm)?;
    crate::vm::engine::restart_this_host().await
}

/// Restart the system `docker` service on Linux.
///
/// dockerd is a **system** service; a user-session faber typically has no
/// standing privilege to restart it, so `systemctl restart docker` failing
/// with an access-denied (or, under polkit, an interactive-auth-required)
/// error is the expected outcome on most installs, not a bug — revival here is
/// best-effort by design; see the module doc. What matters is that the failure
/// comes back as an ordinary `Err`, never a panic and never reported as
/// success, so the caller records it as a fault and Gate 0's existing
/// fail-closed path keeps admission shut regardless of whether this function
/// ever gets to run at all. An operator who wants faber able to do this grants
/// it exactly one command via polkit or sudoers; that grant is not something
/// installing faber requires. Refused under SYSTEM (see the module doc).
#[cfg(target_os = "linux")]
pub async fn revive_docker() -> anyhow::Result<()> {
    refuse_unless(crate::host::Custodian::Service)?;
    crate::host::run_bounded(
        "systemctl",
        &["restart", "docker"],
        SYSTEMCTL_RESTART_TIMEOUT,
    )
    .await
    .map_err(|e| anyhow::anyhow!("systemctl restart docker: {e}"))
}

/// Not implemented on this platform — there is no daemon-revival mechanism to
/// call. Exists only so the crate keeps compiling here; see
/// [`crate::platform`]'s module doc.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub async fn revive_docker() -> anyhow::Result<()> {
    anyhow::bail!("reviving the Docker daemon is not implemented on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_measures_the_vm_home_volume_or_root_never_docker_desktop() {
        use crate::platform::Layout;
        let home = std::path::Path::new("/var/db/faber-vm");
        assert_eq!(macos_disk_path(Layout::System, home), "/var/db/faber-vm");
        assert_eq!(macos_disk_path(Layout::Legacy, home), "/");
        for l in [Layout::System, Layout::Legacy] {
            assert!(!macos_disk_path(l, home).contains("com.docker"));
        }
    }

    #[test]
    fn only_a_macs_system_layout_has_docker_in_a_vm_of_faber_s() {
        // #410: there, the host volume is not Docker's disk, and the VM's own disk
        // is read as well.
        use crate::platform::Layout;
        assert!(!linux_vm_in(Layout::Legacy));
        assert_eq!(linux_vm_in(Layout::System), cfg!(target_os = "macos"));
    }

    /// `sysctl` as an example 32 GiB, 10-core Mac under memory pressure answers it.
    const SYSCTL: &str = "hw.memsize: 34359738368\nhw.ncpu: 10\n\
        kern.memorystatus_vm_pressure_level: 2\n\
        vm.swapusage: total = 4096.00M  used = 3072.25M  free = 1023.75M  (encrypted)\n\
        vm.compressor_bytes_used: 4294967296\n";

    #[test]
    fn sysctl_reads_the_machine_its_pressure_and_its_swap() {
        use crate::machine::PressureLevel;
        let r = parse_sysctl(SYSCTL).unwrap();
        assert_eq!((r.mem_mib, r.cpus), (32 * 1024, 10));
        assert_eq!(r.pressure, Some(PressureLevel::Warn));
        assert_eq!(r.swap_used_mib, Some(3_072));
        assert_eq!(r.swap_total_mib, Some(4_096));
        assert_eq!(r.compressor_mib, Some(4_096));
        assert_eq!(
            (r.perf_cpus, r.efficiency_cpus, r.psi_some_avg60),
            (None, None, None)
        );
        let split = sysctl_values("hw.perflevel0.logicalcpu: 4\nhw.perflevel1.logicalcpu: 6\n");
        assert_eq!(split("hw.perflevel1.logicalcpu"), Some("6"));
        assert_eq!(split("hw.perflevel2.logicalcpu"), None);
    }

    #[test]
    fn a_sysctl_reading_without_memory_or_cores_is_no_reading() {
        assert_eq!(parse_sysctl("hw.ncpu: 10\n"), None);
        assert_eq!(parse_sysctl("hw.memsize: 34359738368\n"), None);
        assert_eq!(parse_sysctl("hw.memsize: 34359738368\nhw.ncpu: 0\n"), None);
        assert_eq!(parse_sysctl("hw.memsize: -1\nhw.ncpu: 10\n"), None);
        // An unknown level or an unreadable swap line costs that field only.
        let r = parse_sysctl(
            "hw.memsize: 34359738368\nhw.ncpu: 10\nkern.memorystatus_vm_pressure_level: 3\n\
             vm.swapusage: garbled\n",
        )
        .unwrap();
        assert_eq!((r.pressure, r.swap_used_mib), (None, None));
    }

    #[test]
    fn swapusage_reads_each_unit() {
        let line = "total = 2.00G  used = 512.50K  free = 0.00M  (encrypted)";
        assert_eq!(swapusage_mib(line, "total"), Some(2048));
        assert_eq!(swapusage_mib(line, "used"), Some(1));
        assert_eq!(swapusage_mib(line, "free"), Some(0));
        assert_eq!(swapusage_mib(line, "spare"), None);
        assert_eq!(swapusage_mib("used = 12T", "used"), None);
        assert_eq!(swapusage_mib("used = NaNM", "used"), None);
    }

    #[test]
    fn linux_reads_meminfo_swap_and_memory_psi() {
        let meminfo = "MemTotal:       33554432 kB\nMemFree:  1000 kB\n\
                       SwapTotal:       4194304 kB\nSwapFree:        2097152 kB\n";
        let psi = "some avg10=1.50 avg60=0.75 avg300=0.10 total=12345\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let r = parse_linux_machine(meminfo, Some(psi), 10).unwrap();
        assert_eq!((r.mem_mib, r.cpus), (32 * 1024, 10));
        assert_eq!(
            (r.swap_used_mib, r.swap_total_mib),
            (Some(2048), Some(4096))
        );
        assert_eq!(r.psi_some_avg60, Some(0.75));
        // No level on Linux: the pressure gate is inert there (ADR-0021 §2c).
        assert_eq!(r.pressure, None);
        // No PSI, no swap: still a reading of the machine's size.
        let bare = parse_linux_machine("MemTotal: 2048 kB\n", None, 2).unwrap();
        assert_eq!(
            (bare.mem_mib, bare.swap_used_mib, bare.psi_some_avg60),
            (2, None, None)
        );
        assert_eq!(parse_linux_machine("MemFree: 1 kB\n", None, 2), None);
        assert_eq!(parse_linux_machine(meminfo, None, 0), None);
    }

    #[test]
    fn a_real_path_survives_trimming() {
        assert_eq!(
            parse_docker_root_dir("/var/lib/docker\n"),
            Some("/var/lib/docker".to_string())
        );
        // An operator-configured data-root on its own volume — exactly the
        // case a `$HOME` fallback would get wrong (see the module doc).
        assert_eq!(
            parse_docker_root_dir("/mnt/docker-data\n"),
            Some("/mnt/docker-data".to_string())
        );
    }

    #[test]
    fn an_empty_answer_is_unknown_not_root() {
        // A daemon that answers with nothing is not evidence the root is `/`
        // — treat it exactly like a call that failed outright.
        assert_eq!(parse_docker_root_dir(""), None);
        assert_eq!(parse_docker_root_dir("\n"), None);
        assert_eq!(parse_docker_root_dir("   \n"), None);
    }

    #[test]
    fn an_answer_that_is_not_an_absolute_path_is_no_path() {
        // #105: the answer goes to `df` as an argument, so an option or a
        // relative path is not one faber measures.
        assert_eq!(parse_docker_root_dir("--help\n"), None);
        assert_eq!(parse_docker_root_dir("var/lib/docker\n"), None);
        assert_eq!(parse_docker_root_dir("/var/lib\u{1b}[2J/docker\n"), None);
    }
}
