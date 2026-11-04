// Starting the JIT runner in runner's GUI session (docs/vm-agent-protocol.md, Inside the guest).
//
// Root cannot start a process in another user's Aqua session directly, so it writes a
// LaunchAgent that runs this binary as `runner-exec` there, and bootstraps it into
// gui/<uid>. The JIT config never goes into that plist, onto an argv or onto disk: root holds
// it in memory and hands it over a one-shot Unix socket that only runner's uid may use.
//
// Each step writes one line to the guest's console (HandoffDiagnostics.swift), root's own
// steps directly and runner-exec's through root, so a job VM stopped with `start_failed` says
// where the hand-off broke.

import Darwin
import Foundation

/// How long each step of the hand-off may take. Each socket has its deadline.
enum HandoffDeadline {
    /// Each launchctl the agent runs: `print gui/<uid>`, `bootstrap`, and, when runner-exec
    /// never connects, `print gui/<uid>/<label>`. Plus Checks.readerGrace for its output.
    static let launchctl: TimeInterval = 10
    /// From launchctl bootstrap to runner-exec connecting.
    static let connect: TimeInterval = 60
    /// Writing the config, and reading it.
    static let config: TimeInterval = 10
    /// From the config to `started`: unpacking the runner tarball.
    static let started: TimeInterval = 110
    /// From `started` to `exited`. GitHub's own limit on a self-hosted job is 5 days.
    static let job: TimeInterval = 6 * 24 * 3600
    /// The longest the hand-off can take from its start to `started`, all three launchctl runs
    /// counted although the last runs only once the connect deadline has passed. The
    /// launcher's own deadline (launcher/Sources/Agent.swift, `started`) must exceed this plus
    /// the Software Update writes before it.
    static var untilStarted: TimeInterval {
        3 * (launchctl + Checks.readerGrace) + connect + config + started
    }
}

/// Whether a hand-off peer may receive the config: only `runner`'s uid. Pure.
func admitHandoffPeer(_ peer: uid_t?, runnerUID: uid_t) -> Bool {
    runnerUID != 0 && admitPeer(peer, expected: runnerUID)
}

// MARK: - Root's side

/// On `begin`, on a thread of its own: write the plist, open the hand-off socket, bootstrap
/// the LaunchAgent, hand runner-exec the config, then follow its reports into `core`. Each
/// step writes one line to `console`.
func startRunnerInSession(_ jit: JitConfig, _ core: AgentCore,
                          console: @escaping @Sendable (String) -> Void = { agentLog($0) }) {
    Thread.detachNewThread {
        do {
            try runHandoff(jit, core, DiagnosticSink(write: console),
                           run: { runCheckCommand($0, timeout: $1, withStderr: true) })
        } catch {
            agentLog("start runner: \(error)")
            core.report(.failed)
        }
    }
}

/// The hand-off, root's side. `run` runs launchctl with a deadline (runCheckCommand in the
/// guest). Every way out before `started` writes its step's line first. Every way out after it
/// writes the runner's tail (RunnerTail.swift), read by `readTail` from runner's home.
func runHandoff(_ jit: JitConfig, _ core: AgentCore, _ sink: DiagnosticSink, run: GuestCommand,
                readTail: (String, uid_t) -> RunnerTailFacts = { readRunnerTails(home: $0, uid: $1) }) throws {
    func say(_ step: String, _ ok: Bool, _ why: String) { sink.emit(handoffLine(step, ok: ok, why)) }
    func stop(_ step: String, _ why: String) -> Failure {
        say(step, false, why)
        return Failure("\(step): \(why)")
    }

    // session: the console user is runner, and runner's GUI domain is there to bootstrap into.
    guard let pw = getpwnam(Guest.runnerUser) else { throw stop("session", "no \(Guest.runnerUser) account") }
    let uid = pw.pointee.pw_uid
    let gid = pw.pointee.pw_gid
    let home = String(cString: pw.pointee.pw_dir)
    guard uid != 0 else { throw stop("session", "\(Guest.runnerUser) is uid 0") }
    let console = consoleUser()
    guard let console, console.name == Guest.runnerUser, console.uid == uid else {
        let who = console.map { "\($0.name) uid \($0.uid)" } ?? "nobody"
        throw stop("session", "the console user is \(who), not \(Guest.runnerUser) uid \(uid)")
    }
    let gui = run([Guest.launchctl, "print", "gui/\(uid)"], HandoffDeadline.launchctl)
    say("session", gui?.0 == 0, "console user \(Guest.runnerUser) uid \(uid); launchctl print gui/\(uid): \(commandEnd(gui))")

    // tools_access: what launchd needs to exec runner-exec, and what runner-exec reads. Only
    // observed: a mode-bit model cannot see an ACL, so the hand-off goes on either way.
    let manifest = try? readManifest()
    let tarball = manifest.map { Guest.toolsMount + "/" + $0.tarball }
    if let denied = toolsDiskDenied(uid: uid, groups: accountGroups(Guest.runnerUser, gid: gid), tarball: tarball) {
        say("tools_access", false, denied)
    } else if let manifest {
        say("tools_access", true, "\(Guest.runnerUser) can run faber-agent and read manifest.json and \(manifest.tarball)")
    } else {
        say("tools_access", false, "\(Guest.manifestPath) is not faber's manifest, read as root")
    }

    do {
        try requireRootDirectory(Guest.stateRoot)
        try requireRootDirectory(Guest.handoffDir)
        try writeRootFile(Guest.plistPath, try launchAgentPlist(), mode: 0o644)
    } catch {
        throw stop("plist", "\(error)")
    }
    say("plist", true, Guest.plistPath)

    let listener: Int32
    do {
        listener = try listenHandoff(uid: uid, gid: gid)
    } catch {
        throw stop("socket", "\(error)")
    }
    defer {
        close(listener)
        unlinkSocket(Guest.handoffSocket)
    }

    let boot = run(launchctlBootstrapArgv(uid: uid), HandoffDeadline.launchctl)
    guard let boot, boot.0 == 0 else {
        let said = boot.map { firstLine(String(decoding: $0.1, as: UTF8.self), max: 100) } ?? ""
        throw stop("bootstrap", "launchctl bootstrap gui/\(uid): \(commandEnd(boot)) '\(said)'")
    }
    say("bootstrap", true, "launchctl bootstrap gui/\(uid): exit 0")

    let conn: Int32
    do {
        conn = try acceptHandoff(listener, runnerUID: uid, until: Date().addingTimeInterval(HandoffDeadline.connect))
    } catch {
        // launchd's own words for the service: whether it ran, and how it ended.
        let svc = run([Guest.launchctl, "print", "gui/\(uid)/\(Guest.runnerLabel)"], HandoffDeadline.launchctl)
        let facts = svc.map { $0.0 == 0 ? launchdServiceFacts(String(decoding: $0.1, as: UTF8.self)) : "print exit \($0.0)" }
            ?? "print did not run or timed out"
        throw stop("connect", "\(error) within \(Int(HandoffDeadline.connect))s; launchd: \(facts)")
    }
    defer { close(conn) }
    // One-shot: the listener goes as soon as runner-exec is in.
    close(listener)
    unlinkSocket(Guest.handoffSocket)
    say("connect", true, "runner-exec connected as uid \(uid)")

    do {
        try writeAll(conn, encodeHandoffConfig(jit), until: Date().addingTimeInterval(HandoffDeadline.config))
    } catch {
        throw stop("config", "handing runner-exec the config: \(error)")
    }
    // The agent's copy goes out of scope here; nothing else holds it.
    say("config", true, "handed to runner-exec")

    let dir = home + "/" + Guest.runnerDirName
    var deadline = Date().addingTimeInterval(HandoffDeadline.started)
    var startedAt: Date?
    // runner-exec streams its reports without waiting, so two can arrive in one read.
    var frames = StreamFrameReader()
    while true {
        let report: HandoffReport
        do {
            report = try decodeHandoffReport(frames.next(conn, cap: maxHandoffToAgent, until: deadline))
        } catch {
            // Lost before the runner started: it never did. After: it ran, and its end is unknown.
            if let startedAt {
                let tail = readTail(home, uid)
                say("run", false, "the hand-off ended \(Int(Date().timeIntervalSince(startedAt)))s after run.sh started, "
                    + "with no exit status; job=\(jobWord(tail.job))")
                emitTail(tail, sink)
                core.report(.exited(exitCode: nil, signal: nil, job: tail.job))
            } else {
                say("run", false, "no report from runner-exec: \(error)")
                core.report(.failed)
            }
            agentLog("hand-off ended without a report")
            return
        }
        switch report {
        case .step(let r):
            // runner-exec's steps come before run.sh; job code runs as runner after it.
            guard startedAt == nil else { continue }
            say(r.step.rawValue, r.ok, runnerStepWhy(r, tarball: tarball ?? "the runner tarball", dir: dir))
        case .started:
            guard startedAt == nil else { continue }
            startedAt = Date()
            say("run", true, "run.sh started in \(dir)")
            core.report(.running)
            deadline = Date().addingTimeInterval(HandoffDeadline.job)
        case .exited(let code, let signal, let reported):
            let how = code.map { "exit \($0)" } ?? signal.map { "signal \($0)" } ?? "an unknown status"
            if let startedAt {
                // runner-exec's word first; root's own reading of the same directory when it
                // has none.
                let tail = readTail(home, uid)
                let job = reported ?? tail.job
                say("run", code == 0 && job != .notStarted,
                    "run.sh ended with \(how) after \(Int(Date().timeIntervalSince(startedAt)))s; job=\(jobWord(job))")
                emitTail(tail, sink)
                core.report(.exited(exitCode: code, signal: signal, job: job))
            } else {
                say("run", false, "run.sh ended with \(how) before runner-exec reported it started")
                core.report(.failed)
            }
            return
        case .failed:
            say("run", false, "runner-exec could not start run.sh")
            core.report(.failed)
            return
        }
    }
}

/// The job word on a console line: `started`, `none`, or `unknown`. Pure.
func jobWord(_ job: RunnerJob?) -> String {
    job?.rawValue ?? "unknown"
}

/// The runner's tail to the console, with a budget of its own (RunnerTail.maxLines).
private func emitTail(_ tail: RunnerTailFacts, _ sink: DiagnosticSink) {
    let lines = runnerTailBlock(tail)
    let tailSink = sink.sibling(limit: RunnerTail.maxLines)
    for line in lines { tailSink.emit(line) }
}

/// Write `data` to `path` as root's, `mode`, replacing what is there only if it is a regular
/// file: never through a symlink.
private func writeRootFile(_ path: String, _ data: Data, mode: mode_t) throws {
    var st = stat()
    if lstat(path, &st) == 0 {
        guard st.st_mode & S_IFMT == S_IFREG else { throw Failure("\(path): exists and is not a regular file") }
        guard unlink(path) == 0 else { throw Failure("\(path): unlink: \(errnoText(errno))") }
    }
    let fd = open(path, O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW | O_CLOEXEC, mode)
    guard fd >= 0 else { throw Failure("\(path): \(errnoText(errno))") }
    defer { close(fd) }
    guard fchmod(fd, mode) == 0 else { throw Failure("\(path): fchmod: \(errnoText(errno))") }
    try writeAll(fd, Array(data))
}

private func unlinkSocket(_ path: String) {
    var st = stat()
    if lstat(path, &st) == 0, st.st_mode & S_IFMT == S_IFSOCK { unlink(path) }
}

/// The hand-off socket: in root's 0755 directory, owned by runner, mode 0600, non-blocking.
/// getpeereid gates every connection as well.
private func listenHandoff(uid: uid_t, gid: gid_t) throws -> Int32 {
    var st = stat()
    if lstat(Guest.handoffSocket, &st) == 0 {
        guard st.st_mode & S_IFMT == S_IFSOCK, unlink(Guest.handoffSocket) == 0 else {
            throw Failure("\(Guest.handoffSocket): exists and cannot be replaced")
        }
    }
    var addr = try unixSocketAddress(Guest.handoffSocket)
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw Failure("socket: \(errnoText(errno))") }
    _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
    let old = umask(0o177)
    let bound = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    let code = errno
    umask(old)
    guard bound == 0 else {
        close(fd)
        throw Failure("\(Guest.handoffSocket): bind: \(errnoText(code))")
    }
    guard chown(Guest.handoffSocket, uid, gid) == 0, chmod(Guest.handoffSocket, 0o600) == 0, listen(fd, 4) == 0 else {
        let e = errno
        close(fd)
        unlink(Guest.handoffSocket)
        throw Failure("\(Guest.handoffSocket): \(errnoText(e))")
    }
    setNonBlocking(fd)
    return fd
}

/// Accept the one connection from runner's uid before `deadline`. Any other peer is closed
/// unread, and the wait goes on.
func acceptHandoff(_ listener: Int32, runnerUID: uid_t, until deadline: Date) throws -> Int32 {
    while true {
        guard try waitFor(listener, Int16(POLLIN), until: deadline) else { throw Failure("runner-exec never connected") }
        let conn = accept(listener, nil, nil)
        if conn < 0 {
            if errno == EINTR || errno == EAGAIN || errno == ECONNABORTED { continue }
            throw Failure("hand-off accept: \(errnoText(errno))")
        }
        _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
        guard admitHandoffPeer(peerUID(conn), runnerUID: runnerUID) else {
            close(conn)
            continue
        }
        setNonBlocking(conn)
        var on: Int32 = 1
        _ = setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        return conn
    }
}

func unixSocketAddress(_ path: String) throws -> sockaddr_un {
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8)
    let capacity = MemoryLayout.size(ofValue: addr.sun_path)
    guard !bytes.isEmpty, bytes.count < capacity else { throw Failure("\(path): too long for a socket path") }
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
        raw.copyBytes(from: bytes)
        raw[bytes.count] = 0
    }
    addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
    return addr
}

// MARK: - runner-exec, as runner in its GUI session

/// `faber-agent runner-exec`: read the config from root, unpack the runner into a fresh
/// directory in this account's home, run it with the config in its environment, and report.
/// Each step before run.sh is reported to root as well, as a HandoffStep and one integer: this
/// account cannot write the guest's console, so root writes the line.
func runnerExec() -> Int32 {
    let conn: Int32
    do {
        conn = try connectHandoff()
    } catch {
        agentLog("runner-exec: \(error)")
        return 1
    }
    defer { close(conn) }
    func report(_ r: HandoffReport) {
        if let bytes = try? encodeHandoffReport(r) { try? writeAll(conn, bytes, until: Date().addingTimeInterval(5)) }
    }
    let jit: JitConfig
    do {
        jit = try decodeHandoffConfig(readFrame(conn, cap: maxHandoffToRunner,
                                                until: Date().addingTimeInterval(HandoffDeadline.config)))
    } catch {
        agentLog("runner-exec: reading the config: \(error)")
        report(.step(HandoffStepResult(step: .configRead, ok: false)))
        report(.failed)
        return 1
    }
    report(.step(HandoffStepResult(step: .configRead, ok: true)))
    do {
        let (pid, dir) = try startRunner(jit, step: { report(.step($0)) })
        report(.started)
        let end = guestWait(pid)
        // Whether the runner ever took a job: its own `_diag`, read once it is gone.
        let job = readRunnerJob(dir: dir)
        switch end {
        case .code(let c): report(.exited(exitCode: c, signal: nil, job: job))
        case .signal(let s): report(.exited(exitCode: nil, signal: s, job: job))
        case .unknown: report(.exited(exitCode: nil, signal: nil, job: job))
        }
        return 0
    } catch {
        agentLog("runner-exec: \(error)")
        report(.failed)
        return 1
    }
}

/// A step that failed, with the one integer that says why: `errno`, or how a process ended.
/// A value outside the hand-off's ranges is left out, never clamped into another meaning. Pure.
func failedStep(_ step: HandoffStep, errno e: Int32? = nil, exit: GuestExit? = nil) -> HandoffStepResult {
    var r = HandoffStepResult(step: step, ok: false)
    if let e, (1...255).contains(e) { r.errno = Int(e) }
    switch exit {
    case .code(let c)?: r.exitCode = (0...255).contains(c) ? c : nil
    case .signal(let s)?: r.signal = (1...31).contains(s) ? s : nil
    default: break
    }
    return r
}

/// Connect to root's hand-off socket, and check the peer is root.
private func connectHandoff() throws -> Int32 {
    guard getuid() != 0 else { throw Failure("runner-exec runs as \(Guest.runnerUser), never as root") }
    var addr = try unixSocketAddress(Guest.handoffSocket)
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw Failure("socket: \(errnoText(errno))") }
    _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
    setTimeouts(fd, seconds: Int(HandoffDeadline.config))
    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    guard ok == 0 else {
        let e = errno
        close(fd)
        throw Failure("\(Guest.handoffSocket): connect: \(errnoText(e))")
    }
    guard admitPeer(peerUID(fd), expected: 0) else {
        close(fd)
        throw Failure("the hand-off socket is not root's")
    }
    setNonBlocking(fd)
    var on: Int32 = 1
    _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
    return fd
}

/// Read the manifest at most Guest.maxManifest bytes, through no symlink.
func readManifest(_ path: String = Guest.manifestPath) throws -> GuestManifest {
    let fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { throw Failure("\(path): \(errnoText(errno))") }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, st.st_mode & S_IFMT == S_IFREG else { throw Failure("\(path): not a regular file") }
    var buffer = [UInt8](repeating: 0, count: Guest.maxManifest + 1)
    var got = 0
    while got < buffer.count {
        let n = buffer[got...].withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
        if n < 0, errno == EINTR { continue }
        if n <= 0 { break }
        got += n
    }
    do {
        return try parseManifest(Array(buffer[0..<got]))
    } catch {
        throw Failure("\(path): not faber's manifest (\(error))")
    }
}

/// Unpack and spawn run.sh; the caller waits for it. `step` hears how each step ended. Returns
/// run.sh's pid and the runner's directory.
private func startRunner(_ jit: JitConfig, step: (HandoffStepResult) -> Void) throws -> (pid_t, String) {
    func failed(_ r: HandoffStepResult, _ why: String) -> Failure {
        step(r)
        return Failure(why)
    }
    guard access(Guest.manifestPath, R_OK) == 0 else {
        let e = errno
        throw failed(failedStep(.manifest, errno: e), "\(Guest.manifestPath): \(errnoText(e))")
    }
    let manifest: GuestManifest
    do {
        manifest = try readManifest()
    } catch {
        throw failed(failedStep(.manifest), "\(error)")
    }
    step(HandoffStepResult(step: .manifest, ok: true))

    guard let pw = getpwuid(getuid()) else { throw failed(failedStep(.mkdir), "no passwd entry for this uid") }
    let home = String(cString: pw.pointee.pw_dir)
    let user = String(cString: pw.pointee.pw_name)
    guard home.hasPrefix("/"), home != "/" else { throw failed(failedStep(.mkdir), "this account's home is not usable") }
    let dir = home + "/" + Guest.runnerDirName
    guard mkdir(dir, 0o700) == 0 else {
        let e = errno
        throw failed(failedStep(.mkdir, errno: e),
                     "\(dir): \(e == EEXIST ? "already exists; a clone runs one job" : errnoText(e))")
    }
    step(HandoffStepResult(step: .mkdir, ok: true))

    let tarball = Guest.toolsMount + "/" + manifest.tarball
    guard access(tarball, R_OK) == 0 else {
        let e = errno
        throw failed(failedStep(.extract, errno: e), "\(tarball): \(errnoText(e))")
    }
    let untar = guestWait(try guestSpawn(tarArgv(tarball: tarball, into: dir), env: fixedEnvironment))
    guard untar == .code(0) else { throw failed(failedStep(.extract, exit: untar), "tar \(manifest.tarball): \(untar)") }
    step(HandoffStepResult(step: .extract, ok: true, exitCode: 0))

    let run = runnerArgv(dir: dir)
    var st = stat()
    guard lstat(run[0], &st) == 0, st.st_mode & S_IFMT == S_IFREG else {
        throw failed(failedStep(.spawn, errno: ENOENT), "run.sh: not in the runner tarball")
    }
    guard access(run[0], X_OK) == 0 else {
        let e = errno
        throw failed(failedStep(.spawn, errno: e), "run.sh: \(errnoText(e))")
    }
    let tmp = ProcessInfo.processInfo.environment["TMPDIR"]
    // run.sh's stdout and stderr, for root to copy the last lines of to the console once it
    // exits (RunnerTail.swift). Without the file the runner still runs, its output unread.
    let output = open(dir + "/" + RunnerTail.outputName, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_APPEND | O_CLOEXEC, 0o600)
    defer { if output >= 0 { close(output) } }
    do {
        let pid = try guestSpawn(run, env: runnerEnvironment(jit: jit, home: home, user: user, tmpdir: tmp), cwd: dir,
                                 output: output >= 0 ? output : nil)
        return (pid, dir)
    } catch {
        throw failed(failedStep(.spawn), "\(error)")
    }
}
