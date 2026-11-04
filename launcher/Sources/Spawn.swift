// Starting a per-VM launcher child, and telling a live child from a reused pid.

import Darwin
import Foundation

/// The environment every child starts with: fixed, so nothing of the supervisor's own
/// environment (and nothing a caller set) reaches a VM's launcher.
let childEnvironment = ["PATH=/usr/bin:/bin:/usr/sbin:/sbin", "LANG=C"]

/// The descriptor a child finds its slot lock on.
let childSlotLockFD: Int32 = 3

/// Spawn `executable` with `arguments` (argv[0] included) as the leader of a process group
/// of its own, so the supervisor's process group (which launchd abandons, ADR-0026) is not
/// its, and no signal meant for the supervisor reaches it. Every descriptor is closed but
/// stdin (/dev/null), stdout and stderr, and `lockFD`, which the child gets as descriptor 3.
/// Every signal starts at its default disposition, unblocked. The environment is
/// childEnvironment, never the supervisor's.
func spawnChild(executable: String, arguments: [String], lockFD: Int32?) throws -> pid_t {
    var attr: posix_spawnattr_t?
    guard posix_spawnattr_init(&attr) == 0 else { throw Failure("posix_spawnattr_init failed") }
    defer { posix_spawnattr_destroy(&attr) }
    var actions: posix_spawn_file_actions_t?
    guard posix_spawn_file_actions_init(&actions) == 0 else { throw Failure("posix_spawn_file_actions_init failed") }
    defer { posix_spawn_file_actions_destroy(&actions) }

    let flags = POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK
    var defaults: sigset_t = 0
    for sig in 1..<NSIG where sig != SIGKILL && sig != SIGSTOP { defaults |= sigset_t(1) << sigset_t(sig - 1) }
    var none: sigset_t = 0
    var steps: [Int32] = [
        posix_spawnattr_setflags(&attr, Int16(flags)),
        posix_spawnattr_setpgroup(&attr, 0),
        posix_spawnattr_setsigdefault(&attr, &defaults),
        posix_spawnattr_setsigmask(&attr, &none),
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0),
        posix_spawn_file_actions_addinherit_np(&actions, 1),
        posix_spawn_file_actions_addinherit_np(&actions, 2),
    ]
    if let lockFD {
        steps.append(lockFD == childSlotLockFD
            ? posix_spawn_file_actions_addinherit_np(&actions, lockFD)
            : posix_spawn_file_actions_adddup2(&actions, lockFD, childSlotLockFD))
    }
    if let bad = steps.first(where: { $0 != 0 }) { throw Failure("posix_spawn setup: \(errnoText(bad))") }

    let argv = arguments.map { strdup($0) } + [nil]
    let envp = childEnvironment.map { strdup($0) } + [nil]
    defer {
        argv.forEach { free($0) }
        envp.forEach { free($0) }
    }
    var pid: pid_t = 0
    let rc = posix_spawn(&pid, executable, &actions, &attr, argv, envp)
    guard rc == 0 else { throw Failure("posix_spawn \(executable): \(errnoText(rc))") }
    return pid
}

/// A live process as the kernel reports it: its start time, in microseconds since the epoch
/// (kinfo_proc's p_starttime), and its effective uid. nil when no such process exists.
func processInfo(_ pid: pid_t) -> (startedUs: UInt64, uid: uid_t)? {
    guard pid > 0 else { return nil }
    var info = kinfo_proc()
    var size = MemoryLayout<kinfo_proc>.stride
    var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid]
    guard sysctl(&mib, 4, &info, &size, nil, 0) == 0, size > 0 else { return nil }
    let tv = info.kp_proc.p_un.__p_starttime
    guard tv.tv_sec >= 0, tv.tv_usec >= 0 else { return nil }
    return (UInt64(tv.tv_sec) * 1_000_000 + UInt64(tv.tv_usec), info.kp_eproc.e_ucred.cr_uid)
}

/// Whether `pid` is still the process a marker recorded: alive, this account's, and started
/// at the recorded microsecond. A reused pid fails the start time, so it is never adopted.
func isSameProcess(pid: pid_t, startedUs: UInt64) -> Bool {
    guard let info = processInfo(pid) else { return false }
    return info.uid == geteuid() && info.startedUs == startedUs
}

/// The path of the binary this process runs, resolved once at start: the supervisor starts
/// its children from the path launchd started it from, never from a PATH lookup or an
/// argument. A release's atomic swap puts a new file at that path; faber then asks the
/// supervisor to exit (ADR-0026's update sequence). A child spawned in between runs the new
/// file under a marker with this build's child_protocol; if the two differ, the next
/// supervisor's handshake fails and the child finishes as `unmanaged`, never stopped.
func ownExecutablePath() throws -> String {
    var size: UInt32 = 0
    _ = _NSGetExecutablePath(nil, &size)
    var buffer = [CChar](repeating: 0, count: Int(size) + 1)
    guard _NSGetExecutablePath(&buffer, &size) == 0 else { throw Failure("cannot find this binary's path") }
    guard let real = realpath(buffer, nil) else { throw Failure("cannot resolve this binary's path: \(errnoText(errno))") }
    defer { free(real) }
    return String(cString: real)
}
