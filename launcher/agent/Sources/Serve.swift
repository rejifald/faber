// `faber-agent serve`: the agent's side of docs/vm-agent-protocol.md. Root starts it from the
// tools disk (launcher/golden/bootstrap.sh). It listens on vsock port 6100, serves one
// connection at a time, from the host only, and answers the launcher's requests.

import Darwin
import Foundation
import SystemConfiguration

/// The runner's progress after `begin`, as the hand-off reports it.
enum RunnerProgress: Equatable, Sendable {
    case starting
    case running
    case exited(exitCode: Int?, signal: Int?, job: RunnerJob? = nil)
    case failed
}

/// The agent's state, across connections, and its answer to each request. The guest's facts
/// (the console user, sending the probe, starting the runner) come in as closures, so a test
/// can drive every phase without a guest.
final class AgentCore: @unchecked Sendable {
    private let lock = NSLock()
    private var probed = false
    private var begun = false
    private var progress = RunnerProgress.starting
    /// A verification (ADR-0049 §7) was accepted; its verdict once the checks are done.
    private var verifying = false
    private var verdict: [CheckCode]?

    private let consoleIsRunner: @Sendable () -> Bool
    private let probe: @Sendable (String, Int, Int) -> Int
    private let startRunner: @Sendable (JitConfig, AgentCore) -> Void
    private let runChecks: @Sendable (VerifyExpect) -> [CheckCode]
    /// Writes the build's Software Update keys again (Updates.swift). It never fails the agent.
    private let reapplyUpdates: @Sendable (ReapplyMoment) -> Void

    init(consoleIsRunner: @escaping @Sendable () -> Bool,
         probe: @escaping @Sendable (String, Int, Int) -> Int,
         startRunner: @escaping @Sendable (JitConfig, AgentCore) -> Void,
         runChecks: @escaping @Sendable (VerifyExpect) -> [CheckCode] = { _ in CheckCode.allCases },
         reapplyUpdates: @escaping @Sendable (ReapplyMoment) -> Void = { _ in }) {
        self.consoleIsRunner = consoleIsRunner
        self.probe = probe
        self.startRunner = startRunner
        self.runChecks = runChecks
        self.reapplyUpdates = reapplyUpdates
    }

    /// `serve` is starting: the Software Update keys are written again before the agent
    /// listens, so no clone reports ready with them as its boot left them.
    func serveStarting() {
        reapplyUpdates(.serve)
    }

    /// The checks are done: the verdict, reported on every status from now on.
    func checked(_ failed: [CheckCode]) {
        lock.lock()
        defer { lock.unlock() }
        if verdict == nil { verdict = failed.sorted() }
    }

    /// The hand-off's news. Nothing moves back: an exit or a failure is final.
    func report(_ next: RunnerProgress) {
        lock.lock()
        defer { lock.unlock() }
        switch progress {
        case .exited, .failed: return
        case .running where next == .starting: return
        default: progress = next
        }
    }

    private func statusLocked() -> AgentStatus {
        if verifying {
            return verdict.map { AgentStatus(.verified, failed: $0) } ?? AgentStatus(.verifying)
        }
        guard begun else { return AgentStatus(consoleIsRunner() ? .ready : .waitingConsole) }
        switch progress {
        case .starting: return AgentStatus(.starting)
        case .running: return AgentStatus(.running)
        case .exited(let code, let signal, let job): return AgentStatus(.exited, exitCode: code, signal: signal, job: job)
        case .failed: return AgentStatus(.failed)
        }
    }

    var status: AgentStatus {
        lock.lock()
        defer { lock.unlock() }
        return statusLocked()
    }

    /// The reply to one request on a connection, and whether the connection stays open.
    func answer(_ request: AgentRequest, greeted: inout Bool) -> (AgentReply, Bool) {
        if !greeted {
            guard case .hello(let version) = request else { return (.error(.handshakeRequired), false) }
            guard version == agentProtocol else { return (.error(.versionMismatch), false) }
            greeted = true
            return (.hello(protocol: agentProtocol), true)
        }
        switch request {
        case .hello:
            return (.error(.badRequest), false)
        case .status:
            return (.status(status), true)
        case .probe(let addr, let port, let count):
            lock.lock()
            guard !begun, !verifying, !probed, statusLocked().phase == .ready else {
                lock.unlock()
                return (.error(.wrongPhase), true)
            }
            probed = true
            lock.unlock()
            return (.probed(sent: probe(addr, port, count)), true)
        case .begin(let jit):
            lock.lock()
            guard !begun, !verifying, probed, statusLocked().phase == .ready else {
                lock.unlock()
                return (.error(.wrongPhase), true)
            }
            begun = true
            progress = .starting
            lock.unlock()
            // Reply first; the slow work runs after, on a thread of its own: the Software Update
            // keys written again, so a job starts with them off whatever rewrote them since
            // `serve` started, then the hand-off (the plist, launchctl, the runner).
            let reapply = reapplyUpdates
            let start = startRunner
            Thread.detachNewThread { [self] in
                reapply(.begin)
                start(jit, self)
            }
            return (.accepted, true)
        case .verify(let expect):
            lock.lock()
            guard !begun, !verifying, probed, statusLocked().phase == .ready else {
                lock.unlock()
                return (.error(.wrongPhase), true)
            }
            verifying = true
            lock.unlock()
            // Reply first; the checks (xcodebuild, simctl) run on a thread of their own.
            let run = runChecks
            Thread.detachNewThread { [self] in checked(run(expect)) }
            return (.accepted, true)
        }
    }
}

// MARK: - The guest's facts

/// Whether the console user is `runner`, with the uid `runner` has in the directory.
func consoleUserIsRunner() -> Bool {
    var uid: uid_t = 0
    var gid: gid_t = 0
    guard let name = SCDynamicStoreCopyConsoleUser(nil, &uid, &gid) as String?, name == Guest.runnerUser,
          let pw = getpwnam(Guest.runnerUser) else { return false }
    return pw.pointee.pw_uid == uid && uid != 0
}

/// Send `count` UDP datagrams of 8 zero bytes to `addr`:`port` from the guest's NAT address,
/// and return how many sendto(2) accepted.
func sendProbeDatagrams(addr: String, port: Int, count: Int) -> Int {
    var sin = sockaddr_in()
    sin.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    sin.sin_family = sa_family_t(AF_INET)
    sin.sin_port = UInt16(port).bigEndian
    guard inet_pton(AF_INET, addr, &sin.sin_addr) == 1 else { return 0 }
    let fd = socket(AF_INET, SOCK_DGRAM, 0)
    guard fd >= 0 else { return 0 }
    defer { close(fd) }
    let payload = [UInt8](repeating: 0, count: 8)
    var sent = 0
    for _ in 0..<count {
        let n = withUnsafePointer(to: &sin) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                payload.withUnsafeBytes { sendto(fd, $0.baseAddress, $0.count, 0, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) }
            }
        }
        if n == payload.count { sent += 1 }
    }
    return sent
}

// MARK: - The vsock server

/// A connection with nothing for this long is closed. The launcher polls every second.
let agentIdleSeconds = 60

/// Serve the launcher on vsock port 6100, one connection at a time, from the host only.
func serveAgent(_ core: AgentCore) throws -> Never {
    let fd = socket(AF_VSOCK, SOCK_STREAM, 0)
    guard fd >= 0 else { throw Failure("vsock socket: \(errnoText(errno))") }
    _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
    var addr = sockaddr_vm()
    addr.svm_len = UInt8(MemoryLayout<sockaddr_vm>.size)
    addr.svm_family = sa_family_t(AF_VSOCK)
    addr.svm_port = agentPort
    addr.svm_cid = VMADDR_CID_ANY
    let bound = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_vm>.size)) }
    }
    guard bound == 0 else { throw Failure("vsock bind port \(agentPort): \(errnoText(errno))") }
    guard listen(fd, 4) == 0 else { throw Failure("vsock listen: \(errnoText(errno))") }
    agentLog("serving protocol=\(agentProtocol) port=\(agentPort)")

    let busy = Busy()
    while true {
        var peer = sockaddr_vm()
        var len = socklen_t(MemoryLayout<sockaddr_vm>.size)
        let conn = withUnsafeMutablePointer(to: &peer) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { accept(fd, $0, &len) }
        }
        if conn < 0 {
            if errno == EINTR || errno == ECONNABORTED { continue }
            throw Failure("vsock accept: \(errnoText(errno))")
        }
        _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
        // Only the host can reach this port; check anyway. One connection at a time.
        guard admitVsockPeer(cid: peer.svm_cid, family: peer.svm_family), busy.take() else {
            close(conn)
            continue
        }
        Thread.detachNewThread {
            serveConnection(conn, core)
            close(conn)
            busy.release()
        }
    }
}

/// A vsock peer may talk only if it is the host. Pure.
func admitVsockPeer(cid: UInt32, family: sa_family_t) -> Bool {
    family == sa_family_t(AF_VSOCK) && cid == VMADDR_CID_HOST
}

final class Busy: @unchecked Sendable {
    private let lock = NSLock()
    private var held = false
    func take() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if held { return false }
        held = true
        return true
    }
    func release() {
        lock.lock()
        held = false
        lock.unlock()
    }
}

/// One connection: lockstep requests, each frame at most 16 384 bytes.
func serveConnection(_ conn: Int32, _ core: AgentCore) {
    var on: Int32 = 1
    _ = setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
    var rcv = timeval(tv_sec: agentIdleSeconds, tv_usec: 0)
    var snd = timeval(tv_sec: 5, tv_usec: 0)
    _ = setsockopt(conn, SOL_SOCKET, SO_RCVTIMEO, &rcv, socklen_t(MemoryLayout<timeval>.size))
    _ = setsockopt(conn, SOL_SOCKET, SO_SNDTIMEO, &snd, socklen_t(MemoryLayout<timeval>.size))
    var reader = FrameReader(fd: conn, cap: maxAgentRequest)
    var greeted = false
    while true {
        guard let line = try? reader.next() else { return }
        let frame: AgentRequestFrame
        do {
            frame = try decodeAgentRequest(line)
        } catch {
            if let id = readableAgentFrameID(line) { _ = sendAgentReply(conn, id, .error(.badRequest)) }
            return
        }
        let (reply, keepOpen) = core.answer(frame.request, greeted: &greeted)
        guard sendAgentReply(conn, frame.id, reply), keepOpen else { return }
    }
}

private func sendAgentReply(_ conn: Int32, _ id: UInt32, _ reply: AgentReply) -> Bool {
    guard let bytes = try? encodeAgentReply(AgentReplyFrame(id: id, reply: reply)) else { return false }
    return (try? writeAll(conn, bytes)) != nil
}
