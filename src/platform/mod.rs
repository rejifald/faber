//! Every filesystem path faber uses, in one place.
//!
//! Before this module existed, each of ~25 call sites across 8 files built its
//! own path inline from `std::env::var_os("HOME")` — one per concept (config,
//! overrides, logs, cache, the host lock, ...), each hand-rolled at the point
//! of use. That made porting faber to Linux a search-and-replace exercise with
//! no single place to get the new rules right. This module is that place.
//!
//! The public API below is deliberately plain functions, not a trait: faber
//! only ever runs under one OS per process, so there is nothing to
//! dynamically dispatch over, and a trait would only add a vtable and an impl
//! block for no behavioral gain.
//!
//! `Option<PathBuf>` is preserved everywhere the pre-existing code already
//! treated a missing `$HOME` as a real, handled case — this module invents no
//! new panics.
//!
//! # Two layouts
//!
//! A host runs faber in one of two layouts, and [`layout()`] says which:
//!
//! - **LEGACY.** faber runs as whoever started it, and every path hangs off
//!   that account's environment. This is the layout every host had before
//!   ADR-0025, byte for byte; [`legacy`] computes it from an [`Env`].
//! - **SYSTEM.** faber runs as its own role account, `_faber` (ADR-0025), and
//!   every path is a fixed location from [`system`]'s table: state under
//!   `/Library/Application Support/faber` on macOS and `/var/lib/faber` on
//!   Linux. A host is SYSTEM exactly when `sudo faber setup` has written
//!   `/etc/faber/setup.json` and the file passes [`record`]'s trust rule.
//!   Without a record the host is LEGACY. With a record that fails the rule it
//!   is LEGACY as well, and [`Resolution::warning`] says why.
//!
//! Every accessor below keeps its signature and answers for the layout this
//! process runs in. Code that needs a particular layout, such as setup reading
//! the operator's LEGACY files and writing their SYSTEM copies, uses the
//! values [`legacy`] and [`system`] directly.
//!
//! ## Why the SYSTEM tree is root-anchored
//!
//! ADR-0025's root invariant: root never executes or trusts anything `_faber`
//! or `_faber_vm` can write. `sudo faber setup` and `sudo faber uninstall` run
//! as root and act on these paths, so two things follow:
//!
//! - **The decision rests on a file only root can write.** The setup record
//!   lives in the root-owned `/etc/faber` and must be root-owned itself. A
//!   record the daemon's account could edit would let that account choose what
//!   root removes on uninstall.
//! - **The paths are literals, not derivations.** A SYSTEM path that followed
//!   `$HOME` or an `XDG_*` variable would let whoever sets the variable point
//!   root at a tree of their choosing. So [`SystemLayout`] reads nothing from
//!   the environment, and under SYSTEM even [`home`] is the table's value.
//!   The service definitions set the daemon's `HOME` to the same directory, so
//!   the two agree.
//!
//! The daemon's state lives in `_faber`-owned directories at these fixed
//! locations. Root creates and repairs only their top level, and never
//! changes ownership or modes recursively inside them.
//!
//! ## The layout is decided once per process
//!
//! [`resolution`] reads the setup record on the first call to anything in this
//! module that needs the layout, and caches the answer for the life of the
//! process. The daemon's boot makes that call before it resolves any path. A
//! record that setup writes or repairs later is seen by the next process only.
//! That is deliberate: a daemon whose config, lock and state paths could switch
//! trees mid-run would split its state across two layouts.
//!
//! The environment is still read fresh on every LEGACY call, exactly as before
//! (see `env`).
//!
//! This crate's own unit tests never read the host's `/etc/faber`: under
//! `cfg(test)` the layout is always LEGACY. SYSTEM behaviour is tested through
//! the pure values instead: [`layout_from`], [`legacy`] and
//! [`SystemLayout::for_os`].
//!
//! # macOS is load-bearing for LEGACY
//!
//! Every macOS LEGACY path reproduces, byte for byte, what the inline
//! `std::env::var_os("HOME")` call sites already produced — existing installs
//! depend on these exact strings. `macos::tests::macos_paths_match_todays_literals`
//! is the regression guard; do not change [`macos`]'s output without a
//! migration plan for hosts already running on the old layout.
//!
//! Linux LEGACY follows the XDG Base Directory spec instead, since there was no
//! installed base to stay compatible with. Windows exists to keep the crate
//! compiling there; it has no SYSTEM layout, and nothing exercises it.

use std::path::PathBuf;
use std::sync::OnceLock;

// Three further platform seams, each owned by its own file so that the paths
// above stay a pure, testable unit. All are declared unconditionally: the
// cfg-selection lives inside them, next to the behaviour it selects.
pub mod host;
pub mod power;
pub mod service;

// The two layouts as pure values, and the setup record that chooses between
// them. Compiled on every target, so both OSes' SYSTEM tables are tested on
// both CI legs.
pub mod layout;
pub mod record;

// Who the system lists in a group, for the daemon's check that nobody but
// `_faber` can read its token.
#[cfg(unix)]
pub mod group_db;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("faber::platform has no implementation for this target_os");

/// The environment the LEGACY layout is derived from, for this OS.
pub use imp::Env;
pub use layout::{layout_from, legacy, Layout, Os, Paths, Resolution, SystemLayout};
pub use record::{SetupRecord, TrustError};

/// This platform's environment, read fresh on every call.
///
/// Reading it per call rather than caching it keeps the pre-existing behaviour
/// exactly: each accessor used to consult `std::env::var_os` itself, so a
/// process whose `$HOME` changed mid-run saw the change. Nothing relies on
/// that, but a refactor that quietly froze it would be a behaviour change
/// hiding inside a test-hygiene fix.
///
/// The point of the indirection is that this is the *only* place the process is
/// read. Every rule applied to those values lives on `imp::Env` as a pure
/// method, which is what lets the layout be tested by constructing an `Env`
/// instead of calling `std::env::set_var` from a test thread — unsound in a
/// multi-threaded program, and the cause of a ~8% flake in unrelated tests
/// (issue #47).
fn env() -> Env {
    Env::from_process()
}

// --- which layout -----------------------------------------------------------

/// This process's layout decision, and what it rests on: the trusted setup
/// record under SYSTEM, or the reason an existing record was refused.
///
/// Taken on the first call and cached for the life of the process; see the
/// module doc for why.
pub fn resolution() -> &'static Resolution {
    static RESOLVED: OnceLock<Resolution> = OnceLock::new();
    RESOLVED.get_or_init(read_resolution)
}

/// SYSTEM or LEGACY, for this process.
pub fn layout() -> Layout {
    resolution().layout()
}

/// The trusted setup record, when this process runs SYSTEM.
pub fn setup_record() -> Option<&'static SetupRecord> {
    resolution().record()
}

/// Read and judge the setup record.
#[cfg(all(any(target_os = "macos", target_os = "linux"), not(test)))]
fn read_resolution() -> Resolution {
    layout_from(record::load_trusted(&setup_record_path()))
}

/// LEGACY, without reading anything: on Windows there is no SYSTEM layout, and
/// unit tests must not depend on whether the machine running them has been
/// set up.
#[cfg(any(test, not(any(target_os = "macos", target_os = "linux"))))]
fn read_resolution() -> Resolution {
    Resolution::Legacy
}

// --- the SYSTEM layout ------------------------------------------------------

/// This OS's SYSTEM layout, whichever layout this process runs in. Setup reads
/// its destinations from here before the setup record exists.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn system() -> SystemLayout {
    SystemLayout::for_os(Os::HOST)
}

/// `/usr/local/bin/faber`: the root-owned CLI copy that `sudo faber` runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn cli_bin_path() -> PathBuf {
    system().cli_bin
}

/// `/etc/faber/setup.json`: the record whose trusted presence selects SYSTEM.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn setup_record_path() -> PathBuf {
    system().setup_record
}

/// `/etc/faber`: the root-owned directory holding the setup record.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn faber_etc_dir() -> PathBuf {
    system().etc_dir
}

/// `_faber_vm`'s HOME, reserved for ADR-0026's VMs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn vm_home() -> PathBuf {
    system().vm_home
}

/// The VM supervisor's socket: `vm_home()/run/supervisor.sock` (ADR-0026).
/// Setup makes both `vm_home()` and its `run/` owned by `_faber_vm`, group
/// `_faber`, mode 0710 ([`crate::setup::vm_runtime`]), so `_faber` can reach
/// the socket and list nothing; the client still checks the peer's uid on
/// every connection.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn vm_supervisor_socket() -> PathBuf {
    vm_home().join("run").join("supervisor.sock")
}

/// The macOS VM supervisor's socket in ADR-0051's session account:
/// `/var/db/faber-mac/run/supervisor.sock`. Setup makes the account's home and
/// its `run/` owned by `_faber_mac`, group `_faber`, mode 0710
/// ([`crate::setup::session`]), the same shape as [`vm_supervisor_socket`]'s,
/// and the client checks the peer is `_faber_mac` on every connection.
pub fn session_supervisor_socket() -> PathBuf {
    std::path::Path::new(layout::FABER_MAC_HOME)
        .join("run")
        .join("supervisor.sock")
}

/// The Docker socket of faber's Linux VM (ADR-0026): `docker.sock` beside the
/// supervisor's socket. The supervisor's launcher bridges the guest's Docker to
/// it once the VM is in service and removes it when the VM stops; it is created
/// 0660 with the directory's group, so `_faber` is the one account that may use
/// it. faber's `docker` calls reach it through `DOCKER_HOST`
/// ([`crate::docker`]'s `docker_command`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn vm_docker_socket() -> PathBuf {
    vm_supervisor_socket().with_file_name("docker.sock")
}

/// The `docker` program every Docker call of this process runs
/// ([`crate::docker`]'s `docker_command`): the pinned CLI setup installs,
/// [`layout::DOCKER_CLI`], by absolute path on a SYSTEM macOS host, and
/// `docker` from `PATH` everywhere else, as before (ADR-0026, amended
/// 2026-10-21).
pub fn docker_cli() -> &'static str {
    docker_cli_for(layout(), cfg!(target_os = "macos"))
}

/// [`docker_cli`] for a given layout, on macOS or not. Pure.
pub fn docker_cli_for(layout: Layout, macos: bool) -> &'static str {
    match (layout, macos) {
        (Layout::System, true) => layout::DOCKER_CLI,
        _ => "docker",
    }
}

// --- the accessors ----------------------------------------------------------

/// The paths a process running in `layout` resolves. Under SYSTEM, `env` is not
/// consulted at all.
fn paths_for(layout: Layout, env: &Env) -> Paths {
    match layout {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Layout::System => system().paths(),
        // LEGACY; on Windows, which has no SYSTEM table, the only arm.
        _ => legacy(env),
    }
}

/// This process's paths.
fn paths() -> Paths {
    paths_for(layout(), &env())
}

/// `$HOME` (or the closest platform analog) — `None` when it can't be found.
/// Under SYSTEM, `_faber`'s HOME: the state directory.
pub fn home() -> Option<PathBuf> {
    paths().home
}

/// The config file: routing profiles + daemon settings.
pub fn config_path() -> Option<PathBuf> {
    paths().config_path
}

/// The live-tunable autolimiter overrides file.
pub fn overrides_path() -> Option<PathBuf> {
    paths().overrides_path
}

/// Where `stderr.log` and `audit.jsonl` live.
pub fn log_dir() -> Option<PathBuf> {
    paths().log_dir
}

/// Where update staging, quarantine, and boot breadcrumbs live.
pub fn state_dir() -> Option<PathBuf> {
    paths().state_dir
}

/// Where downloaded `actions/runner` binaries are cached.
pub fn cache_dir() -> Option<PathBuf> {
    paths().cache_dir
}

/// The host-wide advisory lock file (ADR-0011 §4).
pub fn lock_path() -> Option<PathBuf> {
    paths().lock_path
}

/// The control API's Unix socket (ADR-0028): `control.sock` in the state
/// directory, under either layout.
pub fn control_socket() -> Option<PathBuf> {
    paths().control_socket
}

/// The uid of the process at the other end of a Unix-socket connection, as the
/// kernel reports it: `getpeereid` on macOS, `SO_PEERCRED` on Linux (tokio
/// picks the call). The control API decides who is calling from this and from
/// nothing the caller says (ADR-0028).
pub fn peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    stream.peer_cred().map(|cred| cred.uid())
}

/// Where per-job registry-credential directories are written.
pub fn registry_auth_dir() -> Option<PathBuf> {
    paths().registry_auth_dir
}

/// The binary the service runs and the self-updater replaces:
/// `state_dir()/bin/faber` under LEGACY, as the updater has always derived it,
/// and `D/bin/faber` under SYSTEM.
pub fn daemon_bin_path() -> Option<PathBuf> {
    paths().daemon_bin_path
}

/// Where `faber setup` installs the shared binary, by default.
pub fn default_bin_path() -> PathBuf {
    paths().default_bin_path
}

/// Every location the menu-bar app could be installed, in priority order.
/// Empty on platforms with no menu-bar app, and always empty under SYSTEM: the
/// app belongs to the operator's session, and the daemon never touches it.
pub fn installed_app_dirs() -> Vec<PathBuf> {
    paths().installed_app_dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit tests resolve LEGACY without looking at `/etc/faber`, so a
    /// developer machine that has run `sudo faber setup` tests the same as CI.
    #[test]
    fn unit_tests_never_read_the_hosts_setup_record() {
        assert_eq!(resolution(), &Resolution::Legacy);
        assert_eq!(layout(), Layout::Legacy);
        assert_eq!(setup_record(), None);
    }

    /// Under LEGACY every accessor is exactly today's derivation from the
    /// live environment. Reads `$HOME`; never writes it.
    #[test]
    fn legacy_accessors_are_todays_derivations() {
        let today = legacy(&Env::from_process());
        assert_eq!(home(), today.home);
        assert_eq!(config_path(), today.config_path);
        assert_eq!(overrides_path(), today.overrides_path);
        assert_eq!(log_dir(), today.log_dir);
        assert_eq!(state_dir(), today.state_dir);
        assert_eq!(cache_dir(), today.cache_dir);
        assert_eq!(lock_path(), today.lock_path);
        assert_eq!(control_socket(), today.control_socket);
        assert_eq!(registry_auth_dir(), today.registry_auth_dir);
        assert_eq!(default_bin_path(), today.default_bin_path);
        assert_eq!(installed_app_dirs(), today.installed_app_dirs);
        // The new accessor and the updater name the same binary.
        assert_eq!(
            daemon_bin_path(),
            crate::update::UpdatePaths::from_home().map(|u| u.bin())
        );
    }

    /// The SYSTEM macOS daemon runs the pinned CLI by absolute path; every
    /// other layout and OS keeps `docker` from `PATH`.
    #[test]
    fn the_docker_cli_is_the_pinned_one_only_on_a_system_mac() {
        assert_eq!(
            docker_cli_for(Layout::System, true),
            "/usr/local/libexec/faber/docker"
        );
        assert!(std::path::Path::new(docker_cli_for(Layout::System, true)).is_absolute());
        assert_eq!(docker_cli_for(Layout::Legacy, true), "docker");
        assert_eq!(docker_cli_for(Layout::System, false), "docker");
        assert_eq!(docker_cli_for(Layout::Legacy, false), "docker");
        // Unit tests run LEGACY.
        assert_eq!(docker_cli(), "docker");
    }

    #[test]
    fn legacy_dispatch_is_the_legacy_value() {
        let env = Env::with_home("/Users/tester");
        assert_eq!(paths_for(Layout::Legacy, &env), legacy(&env));
    }

    /// SYSTEM paths are the table's literals whatever the environment says:
    /// the property that keeps root's paths out of any account's hands.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn system_dispatch_never_consults_the_environment() {
        let table = system().paths();
        assert_eq!(
            paths_for(Layout::System, &Env::with_home("/Users/tester")),
            table
        );
        assert_eq!(paths_for(Layout::System, &Env::default()), table);
        assert_eq!(table.installed_app_dirs, Vec::<PathBuf>::new());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_system_accessors_are_this_hosts_table() {
        assert_eq!(system(), SystemLayout::for_os(Os::HOST));
        assert_eq!(cli_bin_path(), PathBuf::from("/usr/local/bin/faber"));
        assert_eq!(setup_record_path(), PathBuf::from("/etc/faber/setup.json"));
        assert_eq!(faber_etc_dir(), PathBuf::from("/etc/faber"));
        #[cfg(target_os = "macos")]
        assert_eq!(vm_home(), PathBuf::from("/var/db/faber-vm"));
        #[cfg(target_os = "linux")]
        assert_eq!(vm_home(), PathBuf::from("/var/lib/faber-vm"));
        assert_eq!(
            vm_supervisor_socket(),
            vm_home().join("run").join("supervisor.sock")
        );
        assert_eq!(
            vm_docker_socket(),
            vm_home().join("run").join("docker.sock")
        );
    }
}
