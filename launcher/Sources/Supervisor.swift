// `faber-vm supervise`: the VM supervisor of docs/vm-supervisor-protocol.md (ADR-0026, slice 3).
//
// It runs as `_faber_vm` under a LaunchDaemon (slice 6), listens on a socket only `_faber` may
// use, and starts, watches and stops VMs, one `faber-vm run` child per VM. It creates no VZ
// object itself: everything is plain POSIX on one kqueue, on one thread, so no two requests
// ever race over a slot.
//
// The children are not the supervisor's to keep alive or to kill on its way out. Each runs
// in a process group of its own, the LaunchDaemon sets AbandonProcessGroup, and `exit` (or
// SIGTERM, launchd's stop) leaves every one running. A supervisor that starts again finds
// them by their claim markers and adopts them through the child protocol; one it cannot
// speak to is `unmanaged`: it keeps its slot, it is never signalled, and when its process
// exits the supervisor tears down after it.

import Darwin
import Foundation

struct SupervisorConfig {
    var socket: URL
    var peerUID: uid_t
    var stateDir: URL
    var imageDir: URL
    var toolsDir: URL
    var cloneDir: URL
    /// nil: `--network shared-nat`, every VM on Apple's shared NAT.
    var plan: NetworkPlan?
    /// `--linux-dir`: where the long-lived Linux bundles live, each made once from its image.
    /// nil: a Linux image boots in place from --image-dir.
    var linuxDir: URL? = nil
    /// `--docker-socket`: the Linux child's Docker bridge. nil: docker.sock beside --socket.
    var dockerSocket: URL? = nil
    /// The binary each child runs: this one, resolved once (ownExecutablePath).
    var executable: String
    var version: String = faberVMVersion
    /// launchd's stop. Off in tests, which run the supervisor on a thread of their own.
    var handleSignals = true
    /// Connections served at once; one more is accepted and closed at once.
    var maxConnections = 4
    /// A connection that sends nothing for this long, or never finishes a frame, is closed.
    var idleSeconds: TimeInterval = 30

    /// The socket a Linux child's `begin` opens: in the supervisor socket's own directory,
    /// which setup makes `_faber_vm:_faber` so `_faber` alone can reach either.
    var dockerSocketPath: URL {
        dockerSocket ?? socket.deletingLastPathComponent().appendingPathComponent("docker.sock")
    }
}

/// Why a child's VM stopped, from what the supervisor saw. Pure.
///
/// `exitCode` is nil when it is unknowable: a child killed by a signal, or one this
/// supervisor adopted and so cannot wait for. run exits 0 only when the guest shut itself
/// down, which needs a booted VM, 4 when a file the wall checked changed under it, 5 when the
/// guest agent broke its protocol (docs/vm-agent-protocol.md), and 6 when the JIT runner exited
/// without ever taking a job.
func stopReasonForExit(requested: Bool, exitCode: Int32?, everBooted: Bool) -> StopReason {
    if requested { return .requested }
    guard let exitCode else { return .launcherExited }
    switch exitCode {
    case 4: return .wallViolation
    case 5: return .agentError
    case 6: return .runnerExitedWithoutJob
    case 0: return .guestShutdown
    default: return everBooted ? .launcherExited : .startFailed
    }
}

// MARK: - Claim marker files

func markerName(slot: Int) -> String { "slot-\(slot).claim" }

private func writeMarkerFile(dirFD: Int32, name: String, bytes: [UInt8]) throws {
    let fd = openat(dirFD, name, O_CREAT | O_EXCL | O_NOFOLLOW | O_WRONLY | O_CLOEXEC, 0o600)
    guard fd >= 0 else { throw Failure("\(name): \(errnoText(errno))") }
    defer { close(fd) }
    try writeAll(fd, bytes)
    guard fsync(fd) == 0 else { throw Failure("\(name): fsync: \(errnoText(errno))") }
}

/// Write a new marker before the child is spawned. O_EXCL: a marker already there is never
/// overwritten, so a slot is never claimed twice.
func writeClaimMarker(dirFD: Int32, _ marker: ClaimMarker) throws {
    try writeMarkerFile(dirFD: dirFD, name: markerName(slot: marker.slot), bytes: marker.encode())
}

/// Replace a marker in one step, once the child's pid is known: a temporary file in the same
/// directory, then rename(2), so a reader sees the old marker or the new one, never half.
func replaceClaimMarker(dirFD: Int32, _ marker: ClaimMarker) throws {
    let name = markerName(slot: marker.slot)
    let temp = name + ".new"
    if unlinkat(dirFD, temp, 0) != 0, errno != ENOENT { throw Failure("\(temp): \(errnoText(errno))") }
    try writeMarkerFile(dirFD: dirFD, name: temp, bytes: marker.encode())
    guard renameat(dirFD, temp, dirFD, name) == 0 else {
        let code = errno
        unlinkat(dirFD, temp, 0)
        throw Failure("\(name): rename: \(errnoText(code))")
    }
}

/// A marker file's bytes, if the file is one this account owns: a regular file with one
/// link, never a symlink. nil when absent or not such a file, which the supervisor leaves alone.
func readMarkerFile(dirFD: Int32, name: String) -> [UInt8]? {
    let fd = openat(dirFD, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    guard fd >= 0 else { return nil }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, (try? checkOwnedFile(st, euid: geteuid(), path: name)) != nil else { return nil }
    var out = [UInt8](repeating: 0, count: maxFrame + 1)
    var got = 0
    while got < out.count {
        let n = out[got...].withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
        if n <= 0 { break }
        got += n
    }
    return Array(out[0..<got])
}

func hasMarkerSignature(_ bytes: [UInt8]) -> Bool {
    String(decoding: bytes, as: UTF8.self).contains(ClaimMarker.signature)
}

/// Whether nobody holds the slot's lock: the child that held it has exited. Testing takes
/// the lock for a moment and gives it back. A lock file that cannot be opened or checked is
/// treated as held, so a doubt never tears a VM down.
func slotLockIsFree(dirFD: Int32, slot: Int) -> Bool {
    let fd = openat(dirFD, "slot-\(slot).lock", O_RDWR | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    if fd < 0 { return errno == ENOENT }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0, (try? checkOwnedFile(st, euid: geteuid(), path: "lock")) != nil else { return false }
    guard flock(fd, LOCK_EX | LOCK_NB) == 0 else { return false }
    flock(fd, LOCK_UN)
    return true
}

// MARK: - The supervisor

/// One slot's VM, from its claim marker.
final class SupervisedVM {
    var marker: ClaimMarker
    var state: VmState {
        didSet {
            if state == .verifying || state == .verified { verifyRan = true }
            if state == .jobFinished { jobFinished = true }
        }
    }
    /// The VM went through `verify` (ADR-0049 §7): its console log is kept at teardown.
    private(set) var verifyRan = false
    /// The job's runner exited and the child said `job_finished`: a clean end, whose console
    /// log is deleted at teardown.
    private(set) var jobFinished = false
    var stopReason: StopReason?
    /// The checks a verification found failed (ADR-0049 §7), taken when the child moved
    /// `verifying` → `verified` and kept from then on.
    var failed: [CheckCode]?
    /// Spawned by this supervisor, so waitpid reports how it exited; an adopted child's exit
    /// status is unknowable.
    let ownChild: Bool
    /// Never spoken to, never stopped; left only for `stopped`.
    let unmanaged: Bool
    var stopRequested = false
    var everBooted = false
    /// The process is gone; teardown waits for the slot lock to come free.
    var exited = false
    /// A Linux VM's disk file, whose length and held blocks its record reports (ADR-0061 §4).
    /// Known from `start`; for an adopted VM, from the Linux directory when it holds one bundle.
    var disk: URL?
    var exitCode: Int32?

    init(marker: ClaimMarker, state: VmState, ownChild: Bool, unmanaged: Bool = false) {
        self.marker = marker
        self.state = unmanaged ? .unmanaged : state
        verifyRan = state == .verifying || state == .verified
        jobFinished = state == .jobFinished
        self.ownChild = ownChild
        self.unmanaged = unmanaged
        everBooted = ![.starting, .unmanaged].contains(state)
    }

    var isLive: Bool { state != .stopped }

    var record: VmRecord {
        VmRecord(slot: marker.slot, kind: marker.kind, claim: marker.claim, state: state,
                 stopReason: state == .stopped ? stopReason ?? .launcherExited : nil,
                 failed: state == .verified ? failed ?? CheckCode.allCases : nil, startedUnix: marker.startedUnix,
                 disk: marker.kind == .linux ? disk.flatMap(diskSize) : nil)
    }
}

final class Supervisor: @unchecked Sendable {
    let config: SupervisorConfig
    let stateDir: URL
    let cloneDir: URL
    let linuxDir: URL?
    private let stateFD: Int32
    private let kq: Int32
    private var listenFD: Int32 = -1
    private var socketID: FileID?
    /// Every slot's VM, a stopped one kept until its slot is started again.
    private(set) var vms: [Int: SupervisedVM] = [:]
    /// Slots whose marker carries faber's signature but cannot be read: held, and the clone
    /// sweep waits, since such a marker may name any clone.
    private(set) var unreadable: Set<Int> = []
    private var connections: [Int32: Connection] = [:]
    private var exitRequested = false

    private final class Connection {
        enum Phase { case greeting, ready, mismatched }
        let fd: Int32
        var phase = Phase.greeting
        var buffer: [UInt8] = []
        var lastActivity = Date()
        init(fd: Int32) { self.fd = fd }
    }

    private static let tick: UInt = 1
    private static let wake: UInt = 2
    /// A child's answer to probe, begin and stop.
    private static let childTimeoutMs = 2000
    /// A child's answer to status. The loop is single-threaded, so a wedged child must not
    /// stall `list` for long: on a timeout the last known state is served.
    private static let statusTimeoutMs = 500

    /// Check the directories, adopt what the markers name, sweep stale clones, then listen.
    init(_ config: SupervisorConfig) throws {
        self.config = config
        stateFD = try openStateDirectory(config.stateDir)
        stateDir = try resolvedURL(config.stateDir)
        do {
            cloneDir = try resolvedURL(config.cloneDir)
            // Clones are run's bundles, so the directory holding them must be this account's alone.
            try requirePrivateDirectory(cloneDir)
            // The Linux bundles are run's too, and one of them holds the only runner image.
            linuxDir = try config.linuxDir.map { try resolvedURL($0) }
            if let linuxDir { try requirePrivateDirectory(linuxDir) }
            for (flag, dir) in [("--image-dir", config.imageDir), ("--tools-dir", config.toolsDir)] {
                guard isDirectory(dir) else { throw Failure("\(flag) \(dir.path): not a directory (a symlink is refused)") }
            }
            // The sweep deletes inside the clone directory, so it must share nothing with the
            // golden images, the tools disks or the claim markers: compared resolved, so no
            // symlink or `..` can make two of them one.
            try requireSeparateCloneDir(cloneDir, from: [
                ("--image-dir", try resolvedURL(config.imageDir)),
                ("--tools-dir", try resolvedURL(config.toolsDir)),
                ("--state-dir", stateDir),
            ])
            kq = kqueue()
            guard kq >= 0 else { throw Failure("kqueue: \(errnoText(errno))") }
        } catch {
            close(stateFD)
            throw error
        }
        signal(SIGPIPE, SIG_IGN)
        adoptAtStart()
        // A console log no live VM writes is a job's left by a supervisor that died mid-job.
        for slot in 0..<Slot.count where vms[slot]?.isLive != true && !unreadable.contains(slot) {
            removeConsoleLogs(dirFD: stateFD, dir: stateDir, slot: slot)
        }
        sweepClones()
        let (fd, id) = try listenUnix(config.socket, mode: 0o660)
        listenFD = fd
        socketID = id
        _ = fcntl(fd, F_SETFL, O_NONBLOCK)
        register(UInt(fd), Int16(EVFILT_READ))
        register(Self.tick, Int16(EVFILT_TIMER), data: 1000)
        register(Self.wake, Int16(EVFILT_USER), flags: EV_ADD | EV_CLEAR)
        if config.handleSignals {
            for sig in [SIGTERM, SIGINT] {
                signal(sig, SIG_IGN)
                register(UInt(sig), Int16(EVFILT_SIGNAL))
            }
        }
        warn("supervise: listening on \(config.socket.path) protocol=\(supervisorProtocol) child_protocol=\(childProtocol) vms=\(vms.count)")
    }

    // MARK: The loop

    /// Serve until `exit` or SIGTERM, then close and remove the socket. Stops no VM.
    func run() {
        // kevent64: the plain kevent's struct and function share a name Swift cannot tell apart.
        var events = [kevent64_s](repeating: kevent64_s(), count: 16)
        while !exitRequested {
            let n = kevent64(kq, nil, 0, &events, Int32(events.count), 0, nil)
            if n < 0 {
                if errno == EINTR { continue }
                warn("supervise: kevent: \(errnoText(errno))")
                break
            }
            for ev in events[0..<Int(n)] {
                handle(ev)
                if exitRequested { break }
            }
        }
        for c in connections.values { close(c.fd) }
        connections = [:]
        close(listenFD)
        if let socketID { removeSocket(config.socket, id: socketID) }
        close(kq)
        close(stateFD)
        warn("supervise: exiting; \(vms.values.filter(\.isLive).count) VM(s) left running")
    }

    /// Ask the loop, from any thread, to return as `exit` would: no VM is stopped.
    func requestExit() {
        var ev = kevent64_s(ident: UInt64(Self.wake), filter: Int16(EVFILT_USER), flags: 0, fflags: UInt32(NOTE_TRIGGER),
                            data: 0, udata: 0, ext: (0, 0))
        _ = kevent64(kq, &ev, 1, nil, 0, 0, nil)
    }

    @discardableResult
    private func register(_ ident: UInt, _ filter: Int16, flags: Int32 = EV_ADD, fflags: UInt32 = 0, data: Int = 0) -> Int32 {
        var ev = kevent64_s(ident: UInt64(ident), filter: filter, flags: UInt16(flags), fflags: fflags,
                            data: Int64(data), udata: 0, ext: (0, 0))
        return kevent64(kq, &ev, 1, nil, 0, 0, nil) == 0 ? 0 : errno
    }

    private func handle(_ ev: kevent64_s) {
        switch Int32(ev.filter) {
        case EVFILT_READ where Int32(ev.ident) == listenFD:
            acceptAll()
        case EVFILT_READ:
            if let c = connections[Int32(ev.ident)] { readable(c) }
        case EVFILT_PROC:
            childExited(pid: pid_t(ev.ident))
        case EVFILT_TIMER:
            tick()
        case EVFILT_USER:
            exitRequested = true
        case EVFILT_SIGNAL:
            warn("supervise: signal \(ev.ident); exiting without stopping any VM")
            exitRequested = true
        default:
            break
        }
    }

    private func acceptAll() {
        while true {
            let fd = accept(listenFD, nil, nil)
            if fd < 0 { return }  // EAGAIN: nothing more queued
            _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
            // The peer's uid before anything else: another account gets not one byte.
            guard admitPeer(peerUID(fd), expected: config.peerUID), connections.count < config.maxConnections else {
                close(fd)
                continue
            }
            _ = fcntl(fd, F_SETFL, 0)  // blocking: a read follows readiness, a write has its timeout
            var on: Int32 = 1
            _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            setTimeouts(fd, seconds: 2)
            connections[fd] = Connection(fd: fd)
            register(UInt(fd), Int16(EVFILT_READ))
        }
    }

    private func drop(_ c: Connection) {
        connections[c.fd] = nil
        close(c.fd)  // closing removes its kevent
    }

    private func send(_ c: Connection, _ id: UInt32, _ reply: Reply) -> Bool {
        guard let bytes = try? encodeResponse(ResponseFrame(id: id, reply: reply)),
              (try? writeAll(c.fd, bytes)) != nil else {
            drop(c)
            return false
        }
        return true
    }

    /// Read what is there, never past MAX_FRAME, and answer once a whole frame has come.
    private func readable(_ c: Connection) {
        let room = maxFrame - c.buffer.count
        var chunk = [UInt8](repeating: 0, count: max(room, 1))
        let n = chunk.withUnsafeMutableBytes { read(c.fd, $0.baseAddress, room) }
        guard n > 0 else {
            if n < 0, errno == EINTR || errno == EAGAIN { return }
            drop(c)
            return
        }
        c.buffer.append(contentsOf: chunk[0..<n])
        c.lastActivity = Date()
        guard let lf = c.buffer.firstIndex(of: 0x0A) else {
            if c.buffer.count >= maxFrame { drop(c) }  // oversized: closed without a reply
            return
        }
        // Lockstep: bytes after the LF are a second request sent before this one's reply.
        guard lf == c.buffer.count - 1 else {
            drop(c)
            return
        }
        let line = c.buffer
        c.buffer = []
        answer(c, line)
    }

    private func answer(_ c: Connection, _ line: [UInt8]) {
        let frame: RequestFrame
        do {
            frame = try decodeRequest(line)
        } catch {
            // Unreadable: close without a reply. Readable but wrong: bad_request, then close.
            if c.phase != .mismatched, let id = readableFrameID(line) { _ = send(c, id, .error(.badRequest)) }
            drop(c)
            return
        }
        switch c.phase {
        case .greeting:
            guard case .hello(let version) = frame.request else {
                if send(c, frame.id, .error(.handshakeRequired)) { drop(c) }
                return
            }
            if version == supervisorProtocol {
                c.phase = .ready
                _ = send(c, frame.id, .hello(protocol: supervisorProtocol, supervisorVersion: config.version))
            } else {
                // The frozen core: after this, exactly one more request, and only exit.
                c.phase = .mismatched
                _ = send(c, frame.id, .error(.versionMismatch, protocol: supervisorProtocol))
            }
        case .mismatched:
            if case .exit = frame.request { exitNow(c, frame.id) } else { drop(c) }
        case .ready:
            switch frame.request {
            case .hello:
                if send(c, frame.id, .error(.badRequest)) { drop(c) }
            case .exit:
                exitNow(c, frame.id)
            default:
                // Who asks for a stop, for the log: a stop is the one request that ends a VM.
                if case .stop(let claim) = frame.request {
                    let uid = peerUID(c.fd).map { String($0) } ?? "unknown"
                    let pid = peerPID(c.fd).map { String($0) } ?? "unknown"
                    warn("supervise: stop \(claim) requested by uid \(uid) pid \(pid)")
                }
                let (reply, keepOpen) = serve(frame.request)
                if send(c, frame.id, reply), !keepOpen { drop(c) }
            }
        }
    }

    private func exitNow(_ c: Connection, _ id: UInt32) {
        if send(c, id, .done) { drop(c) }
        exitRequested = true
    }

    /// The reply to one handshaken request, and whether the connection stays open.
    func serve(_ request: Request) -> (Reply, Bool) {
        switch request {
        case .start(let s): return (start(s), true)
        case .probe(let claim): return (forward(claim, need: .ready, .probe), true)
        case .begin(let claim, let jit):
            guard let vm = live(claim) else { return (.error(.unknownClaim), true) }
            // Required for macOS, forbidden for Linux: the kind is the VM's, not the request's.
            guard (vm.marker.kind == .macos) == (jit != nil) else { return (.error(.badRequest), false) }
            return (forward(claim, need: .probeSent, .begin(jitConfig: jit)), true)
        case .verify(let claim, let expect):
            guard let vm = live(claim) else { return (.error(.unknownClaim), true) }
            // A macOS VM only, and only a child that speaks verify (child protocol 2).
            guard vm.marker.kind == .macos else { return (.error(.badRequest), false) }
            guard vm.marker.childProtocol >= 2 else { return (.error(.wrongState), true) }
            return (forward(claim, need: .probeSent, .verify(expect)), true)
        case .stop(let claim): return (stop(claim), true)
        case .list:
            refreshAll()
            return (.vms(vms.keys.sorted().compactMap { vms[$0]?.record }), true)
        case .adopt(let claims):
            refreshAll()
            let found = vms.keys.sorted().compactMap { vms[$0] }.filter { $0.isLive && claims.contains($0.marker.claim) }
            let foundClaims = Set(found.map(\.marker.claim))
            return (.adopted(found: found.map(\.record), unknown: claims.filter { !foundClaims.contains($0) }), true)
        case .hello, .exit:
            return (.error(.internal), false)  // answered by the handshake logic
        }
    }

    // MARK: Requests

    private func live(_ claim: String) -> SupervisedVM? {
        vms.values.first { $0.isLive && $0.marker.claim == claim }
    }

    private func childSocket(_ slot: Int) -> URL { stateDir.appendingPathComponent("slot-\(slot).sock") }

    private func call(_ vm: SupervisedVM, _ request: ChildRequest) throws -> ChildReply? {
        let timeout: Int
        if case .status = request { timeout = Self.statusTimeoutMs } else { timeout = Self.childTimeoutMs }
        return try callChild(socket: childSocket(vm.marker.slot), version: vm.marker.childProtocol, slot: vm.marker.slot,
                             claim: vm.marker.claim, request: request, timeoutMilliseconds: timeout)
    }

    /// Ask a managed, running child for its state. The supervisor owns the state machine: a
    /// child runs as the account a VM escape lands in, so what it reports is taken only when
    /// it is a transition `status` may make (childStatusMoves). Anything else, like an
    /// unreachable child, keeps the last known state, and is logged.
    private func refresh(_ vm: SupervisedVM) {
        guard vm.isLive, !vm.unmanaged, !vm.exited,
              case .status(let state, _, let failed)? = try? call(vm, .status) else { return }
        guard childStatusMoves(from: vm.state, to: state) else {
            warn("slot \(vm.marker.slot): child reported \(state.rawValue) in \(vm.state.rawValue); kept")
            return
        }
        if state != .starting { vm.everBooted = true }
        // The verdict is taken once, with the move into verified, and never changed after.
        if state == .verified, vm.state == .verifying { vm.failed = failed ?? CheckCode.allCases }
        // Nothing leaves stopping but stopped, which only the supervisor decides.
        vm.state = vm.stopRequested ? .stopping : state
    }

    private func refreshAll() {
        for vm in vms.values { refresh(vm) }
    }

    /// probe and begin: valid in one state only, answered by the child.
    private func forward(_ claim: String, need: VmState, _ request: ChildRequest) -> Reply {
        guard let vm = live(claim) else { return .error(.unknownClaim) }
        guard !vm.unmanaged else { return .error(.unmanaged) }
        refresh(vm)
        guard vm.state == need else { return .error(.wrongState) }
        // The one state each may lead to: probe_sent after probe, in_service after begin.
        let next: VmState
        switch request {
        case .probe: next = .probeSent
        case .verify: next = .verifying
        default: next = .inService
        }
        switch try? call(vm, request) {
        case .status(let state, _, _)? where state == next:
            vm.everBooted = true
            vm.state = vm.stopRequested ? .stopping : state
            return .vm(vm.record)
        case .status(let state, _, _)?:
            warn("slot \(vm.marker.slot): child answered \(state.rawValue), not \(next.rawValue); kept \(vm.state.rawValue)")
            return .error(.internal)
        case .error(.wrongState, _)?:
            return .error(.wrongState)
        default:
            return .error(.internal)
        }
    }

    private func stop(_ claim: String) -> Reply {
        guard let vm = live(claim) ?? vms.values.first(where: { $0.marker.claim == claim }) else {
            return .error(.unknownClaim)
        }
        if vm.unmanaged, vm.isLive { return .error(.unmanaged) }  // never stopped (the versioning rule)
        // Not on `state == .stopping`: a child may report `stopping` by itself and never exit,
        // and faber's first stop must still reach it (and the SIGTERM fallback below).
        if vm.stopRequested || vm.state == .stopped { return .done }
        vm.stopRequested = true
        vm.state = .stopping
        if (try? call(vm, .stop)) == nil, !vm.exited, vm.marker.pid > 0 {
            // Not serving yet (still preparing) or not answering: SIGTERM does what `stop`
            // does. Only a process proven to be this VM's child is signalled: one this
            // supervisor spawned and has not reaped (its pid cannot be reused), or an adopted
            // one whose start time still matches.
            if vm.ownChild || isSameProcess(pid: vm.marker.pid, startedUs: vm.marker.pidStartedUs) {
                kill(vm.marker.pid, SIGTERM)
            }
        }
        return .done
    }

    private func start(_ s: StartRequest) -> Reply {
        if let vm = vms[s.slot], vm.isLive { return .error(.slotBusy) }
        if unreadable.contains(s.slot) { return .error(.slotBusy) }
        if live(s.claim) != nil { return .error(.claimExists) }
        let image = config.imageDir.appendingPathComponent(s.image)
        let linuxBundle = s.kind == .linux ? linuxDir?.appendingPathComponent(s.image) : nil
        // A Linux bundle that exists boots as it is, whatever the image store holds now.
        guard isDirectory(image) || linuxBundle.map(isDirectory) == true else { return .error(.imageNotFound) }
        // Without --linux-dir the disk is root's image at 0444: never grown in place (ADR-0061 §4).
        if s.diskBytes != nil, linuxBundle == nil { return .error(.badRequest) }
        var tools: URL?
        if let name = s.tools {
            let url = toolsDiskURL(toolsDir: config.toolsDir, name: name)
            var st = stat()
            guard lstat(url.path, &st) == 0, st.st_mode & S_IFMT == S_IFREG else { return .error(.toolsNotFound) }
            tools = url
        }
        var network: VmnetPlan?
        if let plan = config.plan {
            do {
                network = try plan.address(slot: s.slot)
            } catch {
                warn("start slot \(s.slot): \(error)")
                return .error(.startFailed)
            }
        }
        // The slot lock first, then the marker: from here until the child exits the lock is
        // held, by this process and then by the child it hands it to, so a crash in between
        // never makes a live VM look gone.
        let lock: HeldLock
        do {
            lock = try HeldLock.acquire(in: stateFD, name: "slot-\(s.slot).lock",
                                        displayPath: stateDir.appendingPathComponent("slot-\(s.slot).lock").path,
                                        busy: "slot \(s.slot) is locked by a faber-vm this supervisor does not know")
        } catch {
            warn("start slot \(s.slot): \(error)")
            return .error(.slotBusy)
        }
        defer { close(lock.fd) }  // the child holds its own copy from the spawn on

        let previous = vms[s.slot]
        var marker = ClaimMarker(
            childProtocol: childProtocol, slot: s.slot, kind: s.kind, claim: s.claim, pid: 0, pidStartedUs: 0,
            startedUnix: min(UInt64(max(0, Date().timeIntervalSince1970)), maxUnix),
            bundle: s.kind == .macos ? "vm-\(s.slot)-\(UUID().uuidString.lowercased().prefix(8))" : nil)
        do {
            try writeClaimMarker(dirFD: stateFD, marker)
        } catch {
            warn("start slot \(s.slot): claim marker: \(error)")
            return .error(.startFailed)
        }
        let vm = SupervisedVM(marker: marker, state: .starting, ownChild: true)
        vms[s.slot] = vm  // live from here, so no sweep takes its clone
        func fail(_ why: String) -> Reply {
            warn("start slot \(s.slot): \(why)")
            if let bundle = marker.bundle { deleteClone(bundle, except: vm) }
            unlinkat(stateFD, markerName(slot: s.slot), 0)
            vms[s.slot] = previous
            return .error(.startFailed)
        }

        let bundle: URL
        if let name = marker.bundle {
            bundle = cloneDir.appendingPathComponent(name)
            do {
                _ = try clone(VMBundle(url: image), VMBundle(url: bundle), keepIdentifier: false)
            } catch {
                return fail("clone: \(error)")
            }
        } else if let linuxBundle {
            do {
                if try prepareLinuxBundle(image: image, bundle: linuxBundle) { warn("start slot \(s.slot): made \(linuxBundle.path)") }
            } catch {
                return fail("\(error)")
            }
            bundle = linuxBundle
            vm.disk = linuxBundle.appendingPathComponent("disk.raw")
            if let to = s.diskBytes {
                switch growLinuxDisk(VMBundle(url: linuxBundle).rawDisk, to: to) {
                case .grown(let from):
                    warn("start slot \(s.slot): grew \(linuxBundle.path)/disk.raw from \(from) to \(to) bytes")
                case .unchanged:
                    break
                case .refused(let why):
                    _ = fail("disk_bytes \(to): \(why)")
                    return .error(.badRequest)
                case .failed(let why):
                    return fail("disk_bytes \(to): \(why)")
                }
            }
        } else {
            bundle = image  // without --linux-dir the Linux VM boots in place
            if s.kind == .linux { vm.disk = image.appendingPathComponent("disk.raw") }
        }
        // A macOS child appends its console to console-slot<N>-<image>.log: start it empty.
        if s.kind == .macos { removeConsoleLogs(dirFD: stateFD, dir: stateDir, slot: s.slot) }
        let docker = s.kind == .linux ? (socket: config.dockerSocketPath, peerUID: config.peerUID) : nil
        let args = childArguments(executable: config.executable, bundle: bundle, start: s, stateDir: stateDir,
                                  tools: tools, toolsOwner: config.peerUID, network: network, docker: docker)
        let pid: pid_t
        do {
            pid = try spawnChild(executable: config.executable, arguments: args, lockFD: lock.fd)
        } catch {
            return fail("\(error)")
        }
        marker.pid = pid
        marker.pidStartedUs = processInfo(pid)?.startedUs ?? 0
        vm.marker = marker
        do {
            try replaceClaimMarker(dirFD: stateFD, marker)
        } catch {
            // The pid-0 marker stays: a later supervisor treats the child as unmanaged.
            warn("start slot \(s.slot): updating the claim marker: \(error)")
        }
        register(UInt(pid), Int16(EVFILT_PROC), fflags: UInt32(NOTE_EXIT))
        warn("start slot \(s.slot): pid \(pid)")
        return .vm(vm.record)
    }

    // MARK: Exits and teardown

    private func childExited(pid: pid_t) {
        guard pid > 0, let vm = vms.values.first(where: { $0.marker.pid == pid && $0.isLive && !$0.exited }) else { return }
        if vm.ownChild {
            var status: Int32 = 0
            let r = waitpid(pid, &status, WNOHANG)
            if r == 0 { return }  // not yet: the tick asks again
            // WIFEXITED / WEXITSTATUS: Swift does not import the macros.
            if r == pid, status & 0x7f == 0 { vm.exitCode = (status >> 8) & 0xff }
        }
        vm.exited = true
        if !vm.unmanaged { vm.state = .stopping }
        finishTeardown(vm)
    }

    /// Once the slot lock is free: delete a macOS clone, remove the child's socket and the
    /// claim marker, and report `stopped`. A lock still held means something still runs in the
    /// slot, so nothing is touched and the tick tries again.
    private func finishTeardown(_ vm: SupervisedVM) {
        let slot = vm.marker.slot
        guard slotLockIsFree(dirFD: stateFD, slot: slot) else { return }
        if let bundle = vm.marker.bundle { deleteClone(bundle, except: vm) }
        let reason = vm.unmanaged
            ? StopReason.launcherExited
            : stopReasonForExit(requested: vm.stopRequested, exitCode: vm.exitCode, everBooted: vm.everBooted)
        if vm.marker.kind == .macos {
            switch consoleLogDisposition(verifyRan: vm.verifyRan, jobFinished: vm.jobFinished, reason: reason) {
            case .keepVerify: keepConsoleLog(.verify, dirFD: stateFD, dir: stateDir, slot: slot)
            case .keepFailedJob: keepConsoleLog(.failedJob, dirFD: stateFD, dir: stateDir, slot: slot)
            case .remove: removeConsoleLogs(dirFD: stateFD, dir: stateDir, slot: slot)
            }
        }
        var st = stat()
        let sock = "slot-\(slot).sock"
        if fstatat(stateFD, sock, &st, AT_SYMLINK_NOFOLLOW) == 0, st.st_mode & S_IFMT == S_IFSOCK, st.st_uid == geteuid() {
            unlinkat(stateFD, sock, 0)
        }
        unlinkat(stateFD, markerName(slot: slot), 0)
        vm.stopReason = reason
        vm.state = .stopped
        warn("slot \(slot): stopped reason=\(vm.stopReason?.rawValue ?? "")")
    }

    private func tick() {
        let now = Date()
        for c in Array(connections.values) where now.timeIntervalSince(c.lastActivity) > config.idleSeconds {
            drop(c)
        }
        for vm in Array(vms.values) where vm.isLive {
            if vm.exited {
                finishTeardown(vm)
            } else if vm.ownChild {
                childExited(pid: vm.marker.pid)  // a missed or early exit: waitpid decides
                if vm.state == .starting { refresh(vm) }  // so a VM that boots is seen booting
            } else if vm.unmanaged {
                if slotLockIsFree(dirFD: stateFD, slot: vm.marker.slot) {
                    vm.exited = true
                    finishTeardown(vm)
                }
            } else if processInfo(vm.marker.pid) == nil {
                childExited(pid: vm.marker.pid)
            } else if vm.state == .starting {
                refresh(vm)
            }
        }
        for slot in unreadable where slotLockIsFree(dirFD: stateFD, slot: slot) {
            unlinkat(stateFD, markerName(slot: slot), 0)
            unreadable.remove(slot)
            if unreadable.isEmpty { sweepClones() }
        }
    }

    // MARK: Adoption

    /// Every `slot-<N>.claim` in the state directory. A free slot lock means the VM is gone:
    /// finish its teardown and report it stopped. A held one is adopted only if the pid is still
    /// the process the marker recorded and the child answers the handshake at the marker's
    /// version with the marker's claim and slot. Anything less is `unmanaged`.
    ///
    /// Every marker is loaded before anything is torn down, so "named by no live marker" is
    /// judged against all of them, and clone deletion waits while any marker is unreadable.
    private func adoptAtStart() {
        var gone: [SupervisedVM] = []
        for slot in 0..<Slot.count {
            let name = markerName(slot: slot)
            guard let bytes = readMarkerFile(dirFD: stateFD, name: name), hasMarkerSignature(bytes) else { continue }
            let lockFree = slotLockIsFree(dirFD: stateFD, slot: slot)
            guard let marker = try? ClaimMarker.decode(bytes), marker.slot == slot else {
                if lockFree {
                    unlinkat(stateFD, name, 0)
                } else {
                    unreadable.insert(slot)
                    warn("adopt slot \(slot): unreadable claim marker; holding the slot")
                }
                continue
            }
            if lockFree {
                let vm = SupervisedVM(marker: marker, state: .stopping, ownChild: false)
                vm.exited = true
                vms[slot] = vm
                gone.append(vm)
                continue
            }
            let verified = marker.pid > 0 && isSameProcess(pid: marker.pid, startedUs: marker.pidStartedUs)
            var vm = SupervisedVM(marker: marker, state: .unmanaged, ownChild: false, unmanaged: true)
            if verified, supportedChildProtocols.contains(marker.childProtocol) {
                let probe = SupervisedVM(marker: marker, state: .starting, ownChild: false)
                // Its real state, whatever it is, except `stopped`: a v1 child never reports it
                // (it exits instead), and believing it would free the slot and the clone of a
                // VM that still runs. Such a child is broken, so unmanaged.
                if case .status(let state, _, _)? = try? call(probe, .status), state != .stopped, state != .unmanaged {
                    vm = SupervisedVM(marker: marker, state: state, ownChild: false)
                }
            }
            if marker.kind == .linux, let linuxDir { vm.disk = soleLinuxDisk(in: linuxDir) }
            vms[slot] = vm
            warn("adopt slot \(slot): \(vm.state.rawValue)\(verified ? "" : " (pid not verified)")")
            // Only a verified pid is watched: an unverified one may be anyone's. Either way the
            // tick watches the slot lock.
            if verified, register(UInt(marker.pid), Int16(EVFILT_PROC), fflags: UInt32(NOTE_EXIT)) != 0 {
                vm.exited = true
                gone.append(vm)
            }
        }
        for vm in gone { finishTeardown(vm) }
    }

    // MARK: Clones

    /// Delete a clone, only if it is a real directory inside the clone directory, this
    /// account's, carries the `faber-clone` marker, and no other live VM's marker names it.
    @discardableResult
    func deleteClone(_ name: String, except: SupervisedVM? = nil) -> Bool {
        // An unreadable marker may name any clone: none is deleted until it is gone. The tick
        // sweeps once it is, which takes the clones teardown left behind.
        guard unreadable.isEmpty, isStoreName(name),
              !vms.values.contains(where: { $0 !== except && $0.isLive && $0.marker.bundle == name }) else { return false }
        let url = cloneDir.appendingPathComponent(name)
        var st = stat()
        let bundle = VMBundle(url: url)
        // A Linux bundle is never a clone (`clone` refuses one), so a marked one is a marker
        // that lies: the Linux VM's disk holds the only copy of a local-only image.
        guard lstat(url.path, &st) == 0, st.st_mode & S_IFMT == S_IFDIR, st.st_uid == geteuid(),
              bundle.isMarkedClone, !pathExists(bundle.rawDisk) else { return false }
        do {
            try FileManager.default.removeItem(at: url)
            warn("deleted clone \(name)")
            return true
        } catch {
            warn("deleting clone \(name): \(describe(error))")
            return false
        }
    }

    /// Delete every marked clone no live marker names; never by name alone. Skipped while a
    /// marker cannot be read, since it might name any clone.
    func sweepClones() {
        guard unreadable.isEmpty else {
            warn("sweep: skipped while slot(s) \(unreadable.sorted()) hold an unreadable claim marker")
            return
        }
        for name in (try? FileManager.default.contentsOfDirectory(atPath: cloneDir.path)) ?? [] {
            deleteClone(name)
        }
    }
}

/// Whether a child's `status` may move a VM from `current` to `reported`. The supervisor
/// owns the state machine, and a child's word moves it only along the transitions the child
/// can see for itself: the framework reporting the VM running (starting to booting), the
/// guest agent reporting ready (booting to ready, or straight from starting if a poll missed
/// booting), the JIT runner exiting (in_service to job_finished), or the VM stopping from any
/// state. probe_sent and in_service are reached only by the replies to probe and begin, and
/// stopped only by the supervisor seeing the child exit. Reporting the current state again
/// is no move and is accepted. Pure.
func childStatusMoves(from current: VmState, to reported: VmState) -> Bool {
    switch (current, reported) {
    case (_, .stopped), (_, .unmanaged), (.stopped, _), (.unmanaged, _): return false
    case (.stopping, .stopping): return true
    case (.stopping, _): return false
    case (_, .stopping): return true
    case let (a, b) where a == b: return true
    case (.starting, .booting), (.starting, .ready), (.booting, .ready), (.inService, .jobFinished): return true
    case (.verifying, .verified): return true
    default: return false
    }
}

/// The child's command line: `run` on the bundle, serving the child protocol, holding the
/// slot lock it is handed as descriptor 3, on the plan's network for its slot. The JIT
/// configuration is never on it: it reaches the child over its socket, at `begin`. A Linux
/// child also gets the Docker socket its `begin` opens and the one uid it admits.
func childArguments(executable: String, bundle: URL, start s: StartRequest, stateDir: URL,
                    tools: URL?, toolsOwner: uid_t? = nil, network: VmnetPlan?,
                    docker: (socket: URL, peerUID: uid_t)? = nil) -> [String] {
    var args = [executable, "run", bundle.path, "--slot", String(s.slot), "--state-dir", stateDir.path,
                "--claim", s.claim, "--serve-socket", stateDir.appendingPathComponent("slot-\(s.slot).sock").path,
                "--slot-lock-fd", String(childSlotLockFD),
                "--cpus", String(s.cpus), "--memory-mib", String(s.memoryMiB)]
    if let tools {
        args += ["--tools-disk", tools.path]
        // faber builds the tools disks in a directory of its own (setup: _faber 755).
        if let toolsOwner { args += ["--tools-owner", String(toolsOwner)] }
    }
    if let network {
        args += ["--network", "vmnet", "--subnet", network.subnet.description, "--reserve-ip", network.guest.description]
    } else {
        args += ["--network", "shared-nat"]
    }
    if s.kind == .linux, let docker {
        args += ["--docker-socket", docker.socket.path, "--docker-peer-uid", String(docker.peerUID)]
    }
    if s.kind == .macos {
        // The guest's console, for a verification's diagnostic lines (ADR-0049 §7): in the
        // state directory, this account's alone, and never on the wire to faber.
        args += ["--console-log", stateDir.appendingPathComponent(consoleLogName(slot: s.slot, image: s.image)).path,
                 "--console-log-max-mib", String(consoleLogMaxMiB)]
    }
    return args
}

// MARK: - The kept console logs

/// A macOS child's console log while it runs. A job that finished cleanly has its deleted at
/// teardown; a verification's is kept as `verify-<image>-latest.log`, and a job's that stopped
/// any other way as `job-<image>-failed-latest.log` (keepConsoleLog).
func consoleLogName(slot: Int, image: String) -> String { "console-slot\(slot)-\(image).log" }

/// The most a macOS guest's console log holds. The agent's verification writes a few KiB.
let consoleLogMaxMiB = 1

/// How many of an image's verification logs are kept, the latest included; the same for its
/// failed jobs' logs.
let verifyLogsKept = 5

func verifyLogLatestName(image: String) -> String { KeptConsoleLog.verify.latest(image: image) }
func verifyLogArchivedName(image: String, stamp: Int) -> String { KeptConsoleLog.verify.archived(image: image, stamp: stamp) }

/// The two kinds of console log kept at teardown, each with its own names and rotation.
enum KeptConsoleLog: Equatable {
    /// A verification's (ADR-0049 §7): its checks' lines.
    case verify
    /// A job VM's that stopped without finishing: the hand-off's lines (docs/vm-agent-protocol.md).
    case failedJob

    /// The common start of its names; `latest.log` or an archive's `<stamp>.log` follows.
    func prefix(image: String) -> String {
        switch self {
        case .verify: "verify-\(image)-"
        case .failedJob: "job-\(image)-failed-"
        }
    }
    func latest(image: String) -> String { prefix(image: image) + "latest.log" }
    func archived(image: String, stamp: Int) -> String { prefix(image: image) + "\(stamp).log" }
}

/// What teardown does with a macOS VM's console log.
enum ConsoleLogDisposition: Equatable {
    case keepVerify
    case keepFailedJob
    case remove
}

/// A verification's log is always kept. A job's is kept when its VM stopped without the job
/// finishing and without faber asking (an agent error, a wall violation, the guest or the
/// launcher going away): the hand-off's lines say why. A job that finished, or a stop faber
/// requested (a reclaim, a timeout, any teardown), has its deleted. Pure.
func consoleLogDisposition(verifyRan: Bool, jobFinished: Bool, reason: StopReason) -> ConsoleLogDisposition {
    if verifyRan { return .keepVerify }
    if jobFinished || reason == .requested { return .remove }
    return .keepFailedJob
}

/// The image a slot's console log names, if `name` is one. Pure.
func consoleLogImage(_ name: String, slot: Int) -> String? {
    let prefix = "console-slot\(slot)-", suffix = ".log"
    guard name.hasPrefix(prefix), name.hasSuffix(suffix), name.count > prefix.count + suffix.count else { return nil }
    let image = String(name.dropFirst(prefix.count).dropLast(suffix.count))
    return isStoreName(image) ? image : nil
}

/// The archived logs of `image` to delete so that `keep` remain with the latest: the oldest
/// first, by their stamp. A name of another image, or not of the form, is never chosen. Pure.
func verifyLogsToPrune(_ names: [String], image: String, keep: Int) -> [String] {
    keptLogsToPrune(names, prefix: KeptConsoleLog.verify.prefix(image: image), keep: keep)
}

/// verifyLogsToPrune for either kind: the archives are `<prefix><stamp>.log`. Pure.
func keptLogsToPrune(_ names: [String], prefix: String, keep: Int) -> [String] {
    let suffix = ".log"
    let stamped: [(Int, String)] = names.compactMap { name in
        guard name.hasPrefix(prefix), name.hasSuffix(suffix) else { return nil }
        let middle = name.dropFirst(prefix.count).dropLast(suffix.count)
        guard !middle.isEmpty, middle.utf8.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }), let n = Int(middle) else { return nil }
        return (n, name)
    }
    let archived = max(0, keep - 1)
    return stamped.sorted { $0.0 > $1.0 }.dropFirst(archived).map(\.1)
}

/// A regular file in the directory, this account's, with one link. Never a symlink.
private func ownedLogFile(dirFD: Int32, _ name: String) -> stat? {
    var st = stat()
    guard fstatat(dirFD, name, &st, AT_SYMLINK_NOFOLLOW) == 0, st.st_mode & S_IFMT == S_IFREG,
          st.st_uid == geteuid(), st.st_nlink == 1 else { return nil }
    return st
}

private func stateNames(_ dir: URL) -> [String] {
    (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
}

/// Delete every console log of `slot`: before a child starts, and after a job's VM.
func removeConsoleLogs(dirFD: Int32, dir: URL, slot: Int) {
    for name in stateNames(dir) where consoleLogImage(name, slot: slot) != nil && ownedLogFile(dirFD: dirFD, name) != nil {
        unlinkat(dirFD, name, 0)
    }
}

/// After a verification's VM: its console log becomes `verify-<image>-latest.log`, the one
/// before is archived as `verify-<image>-<mtime>.log`, and only verifyLogsKept remain. Every
/// file stays 0600 and this account's, in its 0700 state directory: only root and this account
/// read it, and nothing of it goes on the wire (docs/vm-supervisor-protocol.md).
func keepVerifyLog(dirFD: Int32, dir: URL, slot: Int) {
    keepConsoleLog(.verify, dirFD: dirFD, dir: dir, slot: slot)
}

/// keepVerifyLog for either kind: a failed job's becomes `job-<image>-failed-latest.log`, the
/// one before `job-<image>-failed-<mtime>.log`, rotated apart from the verification's.
func keepConsoleLog(_ kind: KeptConsoleLog, dirFD: Int32, dir: URL, slot: Int) {
    for name in stateNames(dir) {
        guard let image = consoleLogImage(name, slot: slot), ownedLogFile(dirFD: dirFD, name) != nil else { continue }
        let latest = kind.latest(image: image)
        if let st = ownedLogFile(dirFD: dirFD, latest) {
            renameat(dirFD, latest, dirFD, kind.archived(image: image, stamp: st.st_mtimespec.tv_sec))
        }
        fchmodat(dirFD, name, 0o600, AT_SYMLINK_NOFOLLOW)
        guard renameat(dirFD, name, dirFD, latest) == 0 else {
            warn("slot \(slot): keeping \(name): \(errnoText(errno))")
            unlinkat(dirFD, name, 0)
            continue
        }
        warn("slot \(slot): \(kind == .verify ? "verification" : "failed job") console kept as \(dir.appendingPathComponent(latest).path)")
        for old in keptLogsToPrune(stateNames(dir), prefix: kind.prefix(image: image), keep: verifyLogsKept)
        where ownedLogFile(dirFD: dirFD, old) != nil {
            unlinkat(dirFD, old, 0)
        }
    }
}

/// The long-lived Linux bundle `bundle`, made from the image directory `image` if absent:
/// a fresh 0700 directory and `image`/disk.raw cloned into it by clonefile(2), never a full
/// copy, made this account's and writable by it alone (0600). One that exists is booted as it
/// is: never recreated, replaced or deleted, since it holds the only copy of the local-only
/// runner image (AGENTS.md). On failure only the directory made here is removed. True when it
/// was made now.
func prepareLinuxBundle(image: URL, bundle: URL) throws -> Bool {
    var st = stat()
    if lstat(bundle.path, &st) == 0 {
        // run holds it to the private-directory rule; here it only must not be a symlink or a file.
        guard st.st_mode & S_IFMT == S_IFDIR else { throw Failure("\(bundle.path) exists and is not a directory; leaving it") }
        return false
    }
    let source = image.appendingPathComponent("disk.raw")
    _ = try regularFile(source)
    guard mkdir(bundle.path, 0o700) == 0 else { throw Failure("\(bundle.path): mkdir: \(errnoText(errno))") }
    let disk = bundle.appendingPathComponent("disk.raw")
    do {
        guard chmod(bundle.path, 0o700) == 0 else { throw Failure("\(bundle.path): chmod: \(errnoText(errno))") }
        // The base is root's and 0444; the clone is this account's, and the VM writes it.
        try cloneFile(source, disk, flags: CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)
        guard chmod(disk.path, 0o600) == 0 else { throw Failure("\(disk.path): chmod: \(errnoText(errno))") }
    } catch {
        unlink(disk.path)
        rmdir(bundle.path)
        throw error
    }
    return true
}

/// What growing a Linux disk came to (ADR-0061 §4).
enum DiskGrowth: Equatable {
    /// Extended from this many bytes.
    case grown(from: UInt64)
    /// Already this size.
    case unchanged
    /// Refused: the request asks what the supervisor never does (`bad_request`).
    case refused(String)
    /// The file could not be read or extended (`start_failed`).
    case failed(String)
}

/// Whether a disk of `current` bytes may grow to `target`, with `free` bytes free on its volume:
/// nil when it may, else why not. Never shrinks, and never takes more than the volume has: the
/// file is sparse, but what it may grow into is promised to the guest. Pure.
func diskGrowthRefusal(current: UInt64, target: UInt64, free: UInt64) -> String? {
    if target < current { return "smaller than the file (\(current) bytes); a disk never shrinks" }
    if target % diskBytesAlign != 0 { return "not a whole number of MiB" }
    if target > maxDiskBytes { return "above this supervisor's 1 TiB bound" }
    if target - current > free { return "growth of \(target - current) bytes is more than the \(free) bytes free on its volume" }
    return nil
}

/// Extend the stopped Linux VM's disk file `disk` to `target` bytes with ftruncate(2), after
/// diskGrowthRefusal. Opened O_NOFOLLOW and held to a regular file this account owns; extending in
/// place keeps its device and inode, which the wall's Linux check reads (Wall.swift).
func growLinuxDisk(_ disk: URL, to target: UInt64) -> DiskGrowth {
    let fd = open(disk.path, O_RDWR | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return .failed("\(disk.path): open: \(errnoText(errno))") }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0 else { return .failed("\(disk.path): fstat: \(errnoText(errno))") }
    guard st.st_mode & S_IFMT == S_IFREG, st.st_uid == geteuid() else {
        return .refused("\(disk.path) is not a regular file this account owns")
    }
    var fs = statfs()
    guard fstatfs(fd, &fs) == 0 else { return .failed("\(disk.path): fstatfs: \(errnoText(errno))") }
    let current = UInt64(st.st_size)
    if let why = diskGrowthRefusal(current: current, target: target, free: UInt64(fs.f_bavail) * UInt64(fs.f_bsize)) {
        return .refused(why)
    }
    if target == current { return .unchanged }
    guard ftruncate(fd, off_t(target)) == 0 else { return .failed("\(disk.path): ftruncate: \(errnoText(errno))") }
    return .grown(from: current)
}

/// A disk file's length and the bytes it holds, or nil when it cannot be read.
func diskSize(_ disk: URL) -> (bytes: UInt64, allocated: UInt64)? {
    var st = stat()
    guard lstat(disk.path, &st) == 0, st.st_mode & S_IFMT == S_IFREG else { return nil }
    return (UInt64(st.st_size), UInt64(st.st_blocks) * 512)
}

/// The disk of the one bundle in the Linux directory, for a VM adopted after a restart, whose
/// marker does not name its image. nil when there is not exactly one: never a guess.
func soleLinuxDisk(in dir: URL) -> URL? {
    guard let names = try? FileManager.default.contentsOfDirectory(atPath: dir.path) else { return nil }
    let disks = names.map { dir.appendingPathComponent($0).appendingPathComponent("disk.raw") }
        .filter { diskSize($0) != nil }
    return disks.count == 1 ? disks[0] : nil
}

/// Refuse a clone directory that equals, contains or is inside any of `others`, all paths
/// already resolved. By path components, so `/a/clones` is not inside `/a/clone`. Pure.
func requireSeparateCloneDir(_ clones: URL, from others: [(flag: String, dir: URL)]) throws {
    let mine = clones.standardizedFileURL.pathComponents
    for (flag, dir) in others {
        let theirs = dir.standardizedFileURL.pathComponents
        let shared = zip(mine, theirs).prefix { $0 == $1 }.count
        if shared == min(mine.count, theirs.count) {
            throw Failure("--clone-dir \(clones.path) overlaps \(flag) \(dir.path): the clone sweep deletes inside it, so it must share nothing with it")
        }
    }
}

/// The file a `tools` name resolves to in faber's tools-disk cache: `<name>.img`, the image
/// src/vm/tools_disk.rs builds beside its marker. The name is a name, never a path
/// (isStoreName), so the file cannot leave the cache directory.
func toolsDiskURL(toolsDir: URL, name: String) -> URL {
    toolsDir.appendingPathComponent(name + ".img")
}

func isDirectory(_ url: URL) -> Bool {
    var st = stat()
    return lstat(url.path, &st) == 0 && st.st_mode & S_IFMT == S_IFDIR
}

// MARK: - The commands

let superviseValued: Set<String> = ["--socket", "--peer-uid", "--state-dir", "--image-dir", "--tools-dir",
                                    "--clone-dir", "--network-plan", "--network", "--linux-dir", "--docker-socket"]

func supervisorConfig(_ args: [String]) throws -> SupervisorConfig {
    let parsed = try parse(args, valued: superviseValued)
    guard parsed.positional.isEmpty else { throw UsageError("supervise takes no positional arguments") }
    func required(_ name: String) throws -> String {
        guard let v = parsed.value(name), !v.isEmpty else { throw UsageError("supervise: \(name) is required") }
        return v
    }
    let uidText = try required("--peer-uid")
    guard let uid = uid_t(uidText), String(uid) == uidText else { throw UsageError("--peer-uid: not a uid: \(uidText)") }
    var plan: NetworkPlan?
    switch (parsed.value("--network-plan"), parsed.value("--network")) {
    case (let path?, nil):
        let url = absoluteURL(path)
        let fd = open(url.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw Failure("--network-plan \(url.path): \(errnoText(errno))") }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        let data = try handle.read(upToCount: NetworkPlan.maxBytes + 1) ?? Data()
        plan = try NetworkPlan.parse(Array(data))
    case (nil, "shared-nat"?):
        plan = nil
    case (nil, let other?):
        throw UsageError("supervise: --network takes only shared-nat, got \(other)")
    case (nil, nil), (_?, _?):
        throw UsageError("supervise: exactly one of --network-plan PATH and --network shared-nat")
    }
    let socket = absoluteURL(try required("--socket"))
    let docker = parsed.value("--docker-socket").map(absoluteURL)
    if let docker, docker == socket { throw UsageError("supervise: --docker-socket must not be --socket") }
    return SupervisorConfig(
        socket: socket, peerUID: uid,
        stateDir: absoluteURL(try required("--state-dir")), imageDir: absoluteURL(try required("--image-dir")),
        toolsDir: absoluteURL(try required("--tools-dir")), cloneDir: absoluteURL(try required("--clone-dir")),
        plan: plan, linuxDir: parsed.value("--linux-dir").map(absoluteURL), dockerSocket: docker,
        executable: try ownExecutablePath())
}

func superviseCommand(_ args: [String]) throws -> Int32 {
    let supervisor = try Supervisor(supervisorConfig(args))
    supervisor.run()
    return 0
}

/// `supervisor-call`: a debugging client for the owner, so a frame can be sent without any new
/// tool. It reads the frame from stdin, never argv, so a jit_config in it never shows in ps;
/// handshakes (unless --no-hello), sends the frame and prints each reply line. It prints
/// nothing it was given.
func supervisorCallCommand(_ args: [String]) throws -> Int32 {
    let parsed = try parse(args, valued: ["--socket"], switches: ["--no-hello"])
    guard let socket = parsed.value("--socket"), parsed.positional.isEmpty else {
        throw UsageError("supervisor-call --socket PATH [--no-hello] < frame")
    }
    let input = try FileHandle.standardInput.read(upToCount: maxFrame + 1) ?? Data()
    var frame = Array(input)
    guard frame.count <= maxFrame else {
        throw UsageError("supervisor-call: the frame on stdin is longer than \(maxFrame) bytes")
    }
    if frame.last != 0x0A { frame.append(0x0A) }
    guard frame.count <= maxFrame, !frame.dropLast().contains(0x0A), !frame.contains(0x0D) else {
        throw UsageError("supervisor-call: the frame must be one line of at most \(maxFrame - 1) bytes")
    }
    let fd = try connectUnix(absoluteURL(socket), timeoutSeconds: 10)
    defer { close(fd) }
    say("peer_uid=\(peerUID(fd).map { String($0) } ?? "unknown")")
    var reader = FrameReader(fd: fd)
    func exchange(_ bytes: [UInt8]) throws -> Bool {
        try writeAll(fd, bytes)
        guard let line = try reader.next() else {
            say("reply=none closed=true")
            return false
        }
        let text = line.dropLast()
        say(text.allSatisfy { $0 >= 0x20 && $0 < 0x7F } ? "reply=\(String(decoding: text, as: UTF8.self))" : "reply=unprintable")
        return true
    }
    if !parsed.switches.contains("--no-hello") {
        guard try exchange(Array("{\"id\":1,\"request\":{\"hello\":{\"protocol\":\(supervisorProtocol)}}}\n".utf8)) else { return 1 }
    }
    return try exchange(frame) ? 0 : 1
}
