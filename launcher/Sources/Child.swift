// The child protocol (README.md, "The child protocol"): what a per-VM launcher, `run
// --serve-socket`, serves to the supervisor that started or adopted it, and the supervisor's
// client for it. The child keeps running the binary it started from for its whole life, so
// the version it speaks is fixed at spawn and recorded in its claim marker.

import Darwin
import Foundation

// MARK: - The guest agent

/// The guest agent, over vsock: it reports ready, sends the wall probe and starts the JIT
/// runner. A macOS child speaks to it through AgentChannel (Agent.swift); the Linux VM has
/// none (its Docker relay is another slice), so it is absent there and the VM stays `booting`.
protocol GuestAgent: Sendable {
    var isPresent: Bool { get }
    func sendProbe() throws
    /// Hands the JIT configuration to the guest. It is held in memory only, never logged.
    func begin(jitConfig: JitConfig?) throws
    /// Has the guest run the golden image's checks instead of a job (ADR-0049 §7).
    func verify(expect: VerifyExpect) throws
}

extension GuestAgent {
    /// Only a macOS guest's agent verifies; any other refuses.
    func verify(expect: VerifyExpect) throws { throw Failure("this guest does not verify") }
}

struct AbsentGuestAgent: GuestAgent {
    var isPresent: Bool { false }
    func sendProbe() throws { throw Failure("no guest agent") }
    func begin(jitConfig: JitConfig?) throws { throw Failure("no guest agent") }
}

// MARK: - The child's state

/// The VM's state as the child reports it, shared between the VM's queue (Runner) and the
/// thread that serves the socket.
final class ChildStatus: @unchecked Sendable {
    private let lock = NSLock()
    private var current: VmState = .starting
    private var failedChecks: [CheckCode]?

    var state: VmState {
        lock.lock()
        defer { lock.unlock() }
        return current
    }

    /// The state, and the failed checks when it is `verified`.
    var snapshot: (VmState, [CheckCode]?) {
        lock.lock()
        defer { lock.unlock() }
        return (current, current == .verified ? failedChecks : nil)
    }

    /// The checks' verdict: `verifying` → `verified` with `failed`; true when it moved.
    func verdict(_ failed: [CheckCode]) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard current == .verifying else { return false }
        failedChecks = failed
        current = .verified
        return true
    }

    /// Move to `next` unless the VM is already stopping: nothing leaves `stopping`.
    func set(_ next: VmState) {
        lock.lock()
        defer { lock.unlock() }
        if current != .stopping { current = next }
    }

    /// The transition `next` only if the state is still `from`; true when it moved.
    func move(from: VmState, to next: VmState) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard current == from else { return false }
        current = next
        return true
    }
}

// MARK: - The child's server

/// Serves the child protocol on `<state-dir>/slot-N.sock`, one connection at a time, to its
/// own account only. The supervisor opens a connection per exchange, so a hung or dead peer
/// costs at most one read timeout.
final class ChildServer: @unchecked Sendable {
    let slot: Int
    let claim: String
    let status: ChildStatus
    let agent: GuestAgent
    let path: URL
    private let fd: Int32
    private let socketID: FileID
    private let onStop: @Sendable () -> Void
    static let timeoutSeconds = 5

    init(path: URL, slot: Int, claim: String, status: ChildStatus, agent: GuestAgent,
         onStop: @escaping @Sendable () -> Void) throws {
        (fd, socketID) = try listenUnix(path, mode: 0o600)
        self.path = path
        self.slot = slot
        self.claim = claim
        self.status = status
        self.agent = agent
        self.onStop = onStop
    }

    func start() {
        Thread.detachNewThread { [self] in serve() }
    }

    /// Stop listening and remove the socket (tests; a real child just exits).
    func close() {
        Darwin.shutdown(fd, SHUT_RDWR)
        Darwin.close(fd)
        removeSocket(path, id: socketID)
    }

    private func serve() {
        while true {
            let conn = accept(fd, nil, nil)
            if conn < 0 {
                if errno == EINTR || errno == ECONNABORTED { continue }
                return  // the listening socket was closed
            }
            _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
            var on: Int32 = 1
            _ = setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            if admitPeer(peerUID(conn), expected: geteuid()) {
                setTimeouts(conn, seconds: Self.timeoutSeconds)
                handle(conn)
            }
            Darwin.close(conn)
        }
    }

    private func handle(_ conn: Int32) {
        var reader = FrameReader(fd: conn)
        var greeted = false
        while true {
            guard let line = try? reader.next() else { return }
            let frame: ChildRequestFrame
            do {
                frame = try decodeChildRequest(line)
            } catch {
                if let id = readableFrameID(line) { send(conn, id, .error(.badRequest)) }
                return
            }
            let (reply, keepOpen) = answer(frame.request, greeted: &greeted)
            guard send(conn, frame.id, reply), keepOpen else { return }
        }
    }

    /// The reply to one request, and whether the connection stays open after it.
    func answer(_ request: ChildRequest, greeted: inout Bool) -> (ChildReply, Bool) {
        if !greeted {
            guard case .hello(let version) = request else { return (.error(.handshakeRequired), false) }
            guard version == childProtocol else { return (.error(.versionMismatch, childProtocol: childProtocol), false) }
            greeted = true
            return (.hello(childProtocol: childProtocol, slot: slot, claim: claim), true)
        }
        switch request {
        case .hello:
            return (.error(.badRequest), false)
        case .status:
            let (state, failed) = status.snapshot
            return (.status(state: state, stopReason: nil, failed: failed), true)
        case .probe:
            guard agent.isPresent, status.state == .ready, (try? agent.sendProbe()) != nil,
                  status.move(from: .ready, to: .probeSent) else { return (.error(.wrongState), true) }
            return (.status(state: .probeSent, stopReason: nil), true)
        case .begin(let jit):
            guard agent.isPresent, status.state == .probeSent, (try? agent.begin(jitConfig: jit)) != nil,
                  status.move(from: .probeSent, to: .inService) else { return (.error(.wrongState), true) }
            return (.status(state: .inService, stopReason: nil), true)
        case .verify(let expect):
            guard agent.isPresent, status.state == .probeSent, (try? agent.verify(expect: expect)) != nil,
                  status.move(from: .probeSent, to: .verifying) else { return (.error(.wrongState), true) }
            return (.status(state: .verifying, stopReason: nil), true)
        case .stop:
            status.set(.stopping)
            onStop()
            return (.done, true)
        }
    }

    @discardableResult
    private func send(_ conn: Int32, _ id: UInt32, _ reply: ChildReply) -> Bool {
        guard let bytes = try? encodeChildResponse(ChildResponseFrame(id: id, reply: reply)) else { return false }
        return (try? writeAll(conn, bytes)) != nil
    }
}

// MARK: - The supervisor's client

/// Why a call to a child failed. The supervisor turns any of these into `unmanaged` at
/// adoption, and never into a stop.
enum ChildCallError: Error, CustomStringConvertible {
    case unreachable(String)
    case refused(String)

    var description: String {
        switch self {
        case .unreachable(let why): "child unreachable: \(why)"
        case .refused(let why): "child refused: \(why)"
        }
    }
}

/// One exchange with a child: connect, check the peer is this account, hello at `version`,
/// confirm the child serves `slot` and `claim`, then send `request` and return its reply.
func callChild(socket: URL, version: Int, slot: Int, claim: String, request: ChildRequest?,
               timeoutMilliseconds: Int = 2000) throws -> ChildReply? {
    let fd: Int32
    do {
        fd = try connectUnix(socket, timeoutMilliseconds: timeoutMilliseconds)
    } catch {
        throw ChildCallError.unreachable("\(error)")
    }
    defer { close(fd) }
    guard admitPeer(peerUID(fd), expected: geteuid()) else { throw ChildCallError.refused("peer is another account") }
    var reader = FrameReader(fd: fd)
    func exchange(_ id: UInt32, _ r: ChildRequest) throws -> ChildReply {
        do {
            try writeAll(fd, encodeChildRequest(ChildRequestFrame(id: id, request: r)))
            guard let line = try reader.next() else { throw ChildCallError.unreachable("closed without a reply") }
            let frame = try decodeChildResponse(line)
            guard frame.id == id else { throw ChildCallError.refused("reply id does not match") }
            return frame.reply
        } catch let error as ChildCallError {
            throw error
        } catch {
            throw ChildCallError.unreachable("\(error)")
        }
    }
    guard case .hello(let v, let s, let c) = try exchange(1, .hello(childProtocol: version)) else {
        throw ChildCallError.refused("handshake failed")
    }
    guard v == version, s == slot, c == claim else { throw ChildCallError.refused("child names another version, slot or claim") }
    guard let request else { return nil }
    return try exchange(2, request)
}
