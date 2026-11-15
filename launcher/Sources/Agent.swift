// The launcher's side of the guest agent protocol (docs/vm-agent-protocol.md): the phase
// machine the agent is held to, and the channel that speaks to it over one connection.
//
// Everything the agent sends is hostile input. A reply this side does not expect, in shape,
// order or time, is a violation: the child logs `event=agent-error what=<code>` with one of the
// fixed codes below, never a guest byte, and force-stops the VM with exit code 5, which the
// supervisor reports as `agent_error`. Nothing the agent sends is forwarded; the supervisor
// only ever sees the child's own states.

import Darwin
import Foundation

/// A fixed code for each way the agent can break the protocol. These are the only words of
/// the agent's that ever reach a log, and they are the launcher's, not the guest's.
enum AgentViolation: String, Error, CaseIterable, Sendable {
    /// The connection closed or failed after the handshake.
    case connectionLost = "connection_lost"
    /// The agent speaks another protocol version.
    case versionMismatch = "version_mismatch"
    /// A reply this protocol does not decode, or one over the 1 024-byte cap.
    case badReply = "bad_reply"
    /// A well-formed reply that does not answer the request asked: another id or operation.
    case wrongReply = "wrong_reply"
    /// An error reply: the launcher only ever sends what is valid.
    case errorReply = "error_reply"
    /// Bytes with no request outstanding.
    case unasked
    /// A phase change the protocol does not allow.
    case badPhase = "bad_phase"
    /// The agent reported `failed`: the runner could not be started.
    case startFailed = "start_failed"
    /// The probe was not sent in full.
    case probeFailed = "probe_failed"
    /// A deadline passed: ready, a reply, or the runner starting.
    case timeout
}

/// The deadlines the launcher holds the agent to.
struct AgentDeadlines: Sendable {
    /// From the VM running to the first `ready`.
    var ready: TimeInterval = 600
    /// From a request to its reply. probe and begin are answered inside the supervisor's 2 s
    /// call to the child, so this must stay under it, with room to wait for the lock.
    var reply: TimeInterval = 1.5
    /// From `begin` to `running` (or `exited`). It must exceed the sum of the guest's own
    /// hand-off deadlines (launcher/agent/Sources/Handoff.swift, `untilStarted`: three
    /// launchctl runs of 10 + 5 reader grace, connect 60, config 10, started 110 = 225 s) and
    /// the Software Update writes that precede the hand-off (launcher/agent/Sources/Updates.swift:
    /// 5 x (5 + 5 reader grace) = 50 s), 275 s in all, or a start the guest still counts as on
    /// time is stopped here as agent_error.
    var started: TimeInterval = 300
    /// From `verify` to `verified` (ADR-0049 §7): the checks run `xcodebuild` and `simctl`, and
    /// link a trivial program with `cc` and `swiftc` as runner, which take seconds to a minute
    /// on a fresh clone. faber's own bound is longer
    /// (src/vm/verify.rs, CHECK_LIMIT), so this one decides.
    var verified: TimeInterval = 360
    /// How long a `status` reply may take once the agent is in service: after `begin` (a job)
    /// or `verify` (the image checks). A guest saturating its vCPUs, which is exactly a long
    /// compile, can answer later than `reply`, and one late answer among the thousands a long
    /// job makes must not stop the VM (issue #83). The agent is still hostile input, so this is
    /// a bound, never none: an agent silent this long is `timeout`. Nothing else waits on the
    /// exchange lock in service, since probe, begin and verify are all behind it. Before that,
    /// `status` keeps `reply`: probe and begin wait on the same lock inside the supervisor's
    /// 2 s call.
    var statusSilence: TimeInterval = 30
    /// The status poll's period.
    var poll: TimeInterval = 1
    /// How long probe and begin wait for a status exchange in flight before giving up
    /// without a violation. With `reply` it must fit the supervisor's 2 s child call.
    var lockWait: TimeInterval = 0.3
}

/// The phases the agent may report, in order, and what each means to the child. Pure, so a
/// test can walk every transition.
///
/// Accepted: `waiting_console` → `ready` → (`probe`) → (`begin`) → `starting` → `running` →
/// `exited`. A phase may repeat. After `begin` any forward move is accepted, since a 1 s poll
/// can miss a short phase (`starting` straight to `exited`, or `running` first seen). Anything
/// else is `bad_phase`, and `failed` is `start_failed`.
///
/// A verification (ADR-0049 §7) takes `verify` in `begin`'s place: `ready` → (`probe`) →
/// (`verify`) → `verifying` → `verified`, which repeats with the same `failed` codes. A phase of
/// the job's, or a verdict that changes, is `bad_phase`.
struct AgentPhaseMachine: Sendable {
    enum Event: Equatable, Sendable {
        case none
        /// The first `ready`: the child moves `booting` → `ready`.
        case becameReady
        /// The first `running`.
        case started
        /// `exited`, reported again on every poll: the child moves `in_service` → `job_finished`,
        /// unless the runner never took a job (`job` is `none`): then the child stops the VM
        /// itself, exit 6, which the supervisor reports as `runner_exited_without_job`.
        case runnerExited(exitCode: Int?, signal: Int?, job: RunnerJob?)
        /// The checks' verdict, reported again on every poll: the child moves `verifying` →
        /// `verified` with these codes.
        case verified([CheckCode])
    }

    let deadlines: AgentDeadlines
    /// When the VM was seen running: the ready deadline counts from here.
    var since: Date
    private(set) var last: AgentPhase?
    private(set) var everReady = false
    private(set) var probed = false
    private(set) var begunAt: Date?
    private(set) var verifyAt: Date?
    /// Whether `begin` or `verify` was accepted: the agent is running a job or the image's
    /// checks, and a `status` reply may take up to `statusSilence`.
    var inService: Bool { begunAt != nil || verifyAt != nil }
    private(set) var verdict: [CheckCode]?

    init(deadlines: AgentDeadlines, since: Date) {
        self.deadlines = deadlines
        self.since = since
    }

    private static func afterBeginRank(_ p: AgentPhase?) -> Int {
        switch p {
        case .starting: 1
        case .running: 2
        case .exited: 3
        default: 0  // ready, the phase begin was sent in
        }
    }

    mutating func observe(_ s: AgentStatus) throws -> Event {
        guard s.isValid else { throw AgentViolation.badReply }
        if s.phase == .failed { throw AgentViolation.startFailed }
        defer { last = s.phase }
        if verifyAt != nil {
            switch s.phase {
            case .ready:
                // The phase verify was sent in, seen once more before the agent moved on.
                guard last == .ready else { throw AgentViolation.badPhase }
                return .none
            case .verifying:
                guard last == .ready || last == .verifying else { throw AgentViolation.badPhase }
                return .none
            case .verified:
                guard let failed = s.failed, verdict == nil || verdict == failed else { throw AgentViolation.badPhase }
                verdict = failed
                return .verified(failed)
            default:
                throw AgentViolation.badPhase  // a job's phase in a verification
            }
        }
        if s.phase == .verifying || s.phase == .verified { throw AgentViolation.badPhase }
        if begunAt == nil {
            switch s.phase {
            case .waitingConsole:
                guard last == nil || last == .waitingConsole else { throw AgentViolation.badPhase }
                return .none
            case .ready:
                guard last == nil || last == .waitingConsole || last == .ready else { throw AgentViolation.badPhase }
                if everReady { return .none }
                everReady = true
                return .becameReady
            default:
                throw AgentViolation.badPhase  // starting, running or exited before begin
            }
        }
        let rank = Self.afterBeginRank(s.phase)
        guard rank > 0, rank >= Self.afterBeginRank(last) else { throw AgentViolation.badPhase }
        switch s.phase {
        case .running: return last == .running ? .none : .started
        case .exited: return .runnerExited(exitCode: s.exitCode, signal: s.signal, job: s.job)
        default: return .none
        }
    }

    /// A missed deadline at `now`: ready not reached in time, or the runner not started.
    func checkDeadlines(now: Date) throws {
        if !everReady, now.timeIntervalSince(since) > deadlines.ready { throw AgentViolation.timeout }
        if let begunAt, last != .running, last != .exited, now.timeIntervalSince(begunAt) > deadlines.started {
            throw AgentViolation.timeout
        }
        if let verifyAt, last != .verified, now.timeIntervalSince(verifyAt) > deadlines.verified {
            throw AgentViolation.timeout
        }
    }

    /// probe is valid once, in `ready`, before begin. A refusal here is the host's own
    /// ordering, not the guest's, so it is no violation.
    var mayProbe: Bool { last == .ready && !probed && begunAt == nil && verifyAt == nil }

    mutating func probeReplied(sent: Int, count: Int) throws {
        guard sent == count else { throw AgentViolation.probeFailed }
        probed = true
    }

    /// begin is valid once, in `ready`, after probe.
    var mayBegin: Bool { last == .ready && probed && begunAt == nil && verifyAt == nil }

    mutating func begun(at date: Date) { begunAt = date }

    /// verify is valid once, in `ready`, after probe, in place of begin.
    var mayVerify: Bool { mayBegin }

    mutating func verifying(at date: Date) { verifyAt = date }
}

/// What an exchange's reply must be: the operation that answers the request, and nothing
/// else. Pure.
func checkAgentReply(_ reply: AgentReply, to request: AgentRequest) throws -> AgentReply {
    switch (request, reply) {
    case (.hello, .error(.versionMismatch)):
        throw AgentViolation.versionMismatch
    case (_, .error):
        throw AgentViolation.errorReply
    case (.hello, .hello(let p)):
        guard p == agentProtocol else { throw AgentViolation.versionMismatch }
    case (.status, .status), (.probe, .probed), (.begin, .accepted), (.verify, .accepted):
        break
    default:
        throw AgentViolation.wrongReply
    }
    return reply
}

/// The child's log line for the runner's exit: integers and a closed word this side parsed and
/// formats itself, nothing of the guest's copied. Pure.
func runnerExitedEvent(exitCode: Int?, signal: Int?, job: RunnerJob?) -> String {
    let how = exitCode.map { "exit_code=\($0)" } ?? signal.map { "signal=\($0)" } ?? "status=unknown"
    return "event=runner-exited \(how) job=\(job?.rawValue ?? "unknown")"
}

/// The violation a failed read or write is.
func agentViolation(_ error: Error) -> AgentViolation {
    if let v = error as? AgentViolation { return v }
    switch error as? DeadlineIOError {
    case .timeout?: return .timeout
    case .tooLarge?: return .badReply
    case .notOneLine?: return .unasked
    case .closed?, .io?, nil: return .connectionLost
    }
}

/// Raised to the child server for a probe or begin the agent cannot take now. It names the
/// cause only for the launcher's own log.
struct AgentRefusal: Error, CustomStringConvertible {
    let description: String
}

/// The launcher's guest agent over one stream connection: the vsock connection for a real VM
/// (AgentVsock.swift), or a socketpair in the tests. It polls `status` on a thread of its own
/// and sends `probe` and `begin` for the child server, one exchange at a time.
final class AgentChannel: GuestAgent, @unchecked Sendable {
    let status: ChildStatus
    let deadlines: AgentDeadlines
    let probeTarget: String
    private let onViolation: @Sendable (AgentViolation) -> Void
    /// The runner exited without ever taking a job: the VM is stopped, exit 6.
    private let onNoJob: @Sendable () -> Void

    /// Held for a whole exchange and for the machine: lockstep, and one writer of the machine.
    private let exchangeLock = NSLock()
    private var machine: AgentPhaseMachine
    private var fd: Int32 = -1
    private var nextID: UInt32 = 1
    /// Whatever owns the descriptor (the VZ connection): kept alive for the VM's life.
    private var keepAlive: AnyObject?
    private let connected = DispatchSemaphore(value: 0)
    private let stateLock = NSLock()
    private var failed = false
    private var loggedExit = false

    /// `probeTarget` is the dotted-quad address the probe goes to: the slot's gateway.
    init(status: ChildStatus, probeTarget: String, deadlines: AgentDeadlines = AgentDeadlines(), since: Date = Date(),
         onNoJob: @escaping @Sendable () -> Void = {},
         onViolation: @escaping @Sendable (AgentViolation) -> Void) {
        self.status = status
        self.probeTarget = probeTarget
        self.deadlines = deadlines
        self.onNoJob = onNoJob
        self.onViolation = onViolation
        machine = AgentPhaseMachine(deadlines: deadlines, since: since)
    }

    var isPresent: Bool { true }

    /// The VM is running: start the ready clock and the poll thread, which waits for
    /// `connect(fd:owner:)` until the ready deadline.
    func start(now: Date = Date()) {
        exchangeLock.lock()
        machine.since = now
        exchangeLock.unlock()
        Thread.detachNewThread { [self] in pollLoop() }
    }

    /// The connection is up: `fd` is a stream to the agent, owned by `owner`.
    func connect(fd: Int32, owner: AnyObject?) {
        exchangeLock.lock()
        setNonBlocking(fd)
        var on: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        self.fd = fd
        keepAlive = owner
        exchangeLock.unlock()
        connected.signal()
    }

    var readyDeadline: Date {
        exchangeLock.lock()
        defer { exchangeLock.unlock() }
        return machine.since.addingTimeInterval(deadlines.ready)
    }

    var hasFailed: Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        return failed
    }

    /// Report a violation once, and stop polling. The VM stopping already (a requested stop,
    /// or the guest shutting down) closes the connection too, which is no violation.
    func fail(_ v: AgentViolation) {
        stateLock.lock()
        let first = !failed
        failed = true
        stateLock.unlock()
        guard first, status.state != .stopping else { return }
        say("event=agent-error what=\(v.rawValue)")
        onViolation(v)
    }

    /// One request and its one reply, within `within` (default `reply`). The caller holds
    /// exchangeLock.
    private func exchange(_ request: AgentRequest, within: TimeInterval? = nil) throws -> AgentReply {
        guard fd >= 0 else { throw AgentViolation.connectionLost }
        if let unasked = unaskedInput(fd) { throw unasked == .closed ? AgentViolation.connectionLost : AgentViolation.unasked }
        let id = nextID
        nextID = nextID == UInt32.max ? 1 : nextID + 1
        let deadline = Date().addingTimeInterval(within ?? deadlines.reply)
        let line: [UInt8]
        do {
            try writeAll(fd, try encodeAgentRequest(AgentRequestFrame(id: id, request: request)), until: deadline)
            line = try readFrame(fd, cap: maxAgentReply, until: deadline)
        } catch {
            throw agentViolation(error)
        }
        let frame: AgentReplyFrame
        do {
            frame = try decodeAgentReply(line)
        } catch {
            throw AgentViolation.badReply
        }
        guard frame.id == id else { throw AgentViolation.wrongReply }
        return try checkAgentReply(frame.reply, to: request)
    }

    /// One status exchange, the machine's verdict, and the child's state moved to match.
    /// Returns true once the job is over and polling may stop.
    private func pollOnce() throws -> Bool {
        exchangeLock.lock()
        defer { exchangeLock.unlock() }
        try machine.checkDeadlines(now: Date())
        // In service, a slow answer is waited for up to `statusSilence` (issue #83); before it,
        // `reply` as for every other request.
        let within = machine.inService ? deadlines.statusSilence : deadlines.reply
        guard case .status(let s) = try exchange(.status, within: within) else { throw AgentViolation.wrongReply }
        switch try machine.observe(s) {
        case .none, .started:
            return false
        case .becameReady:
            // From booting; from starting too, should the framework's running state race the poll.
            if !status.move(from: .booting, to: .ready) { _ = status.move(from: .starting, to: .ready) }
            return false
        case .runnerExited(let code, let signal, let job):
            if !loggedExit {
                loggedExit = true
                say(runnerExitedEvent(exitCode: code, signal: signal, job: job))
            }
            if job == .notStarted {
                // Not a job that finished: the runner left without ever being handed one. The VM
                // stops with its own reason, and the supervisor keeps its console.
                onNoJob()
                return true
            }
            // begin's reply moves the child to in_service; until it has, keep polling.
            return status.move(from: .inService, to: .jobFinished) || status.state == .jobFinished
        case .verified(let failed):
            // Codes this side decoded from the closed set, never the guest's words.
            if status.state == .verifying { say("event=verified failed=\(failed.count)") }
            // verify's reply moves the child to verifying; until it has, keep polling.
            return status.verdict(failed) || status.state == .verified
        }
    }

    private func pollLoop() {
        guard connected.wait(timeout: .now() + max(0, readyDeadline.timeIntervalSinceNow)) == .success else {
            fail(.timeout)
            return
        }
        do {
            exchangeLock.lock()
            defer { exchangeLock.unlock() }
            _ = try exchange(.hello(protocol: agentProtocol))
        } catch {
            fail(agentViolation(error))
            return
        }
        while !hasFailed, status.state != .stopping {
            do {
                if try pollOnce() { return }
            } catch {
                fail(agentViolation(error))
                return
            }
            Thread.sleep(forTimeInterval: deadlines.poll)
        }
    }

    /// For probe and begin, called by the child server inside the supervisor's 2 s call: wait
    /// briefly for a status exchange in flight, then run `body` holding the lock.
    private func withExchange<T>(_ body: () throws -> T) throws -> T {
        guard !hasFailed else { throw AgentRefusal(description: "the guest agent has failed") }
        guard exchangeLock.lock(before: Date().addingTimeInterval(deadlines.lockWait)) else {
            throw AgentRefusal(description: "the guest agent is busy")
        }
        defer { exchangeLock.unlock() }
        do {
            return try body()
        } catch let refusal as AgentRefusal {
            throw refusal
        } catch {
            let v = agentViolation(error)
            fail(v)
            throw v
        }
    }

    /// `probe`: count 3 datagrams to port 9 (discard) on the slot's gateway, the host's own
    /// address on the VM's network, which the anchor's `host` rule for the slot counts.
    func sendProbe() throws {
        try withExchange {
            guard machine.mayProbe else { throw AgentRefusal(description: "probe: the agent is not ready") }
            let count = 3
            guard case .probed(let sent) = try exchange(.probe(addr: probeTarget, port: 9, count: count)) else {
                throw AgentViolation.wrongReply
            }
            try machine.probeReplied(sent: sent, count: count)
        }
    }

    /// `begin`: hand the JIT config to the agent. It is never logged; JitConfig redacts itself.
    func begin(jitConfig: JitConfig?) throws {
        guard let jitConfig else { throw AgentRefusal(description: "begin: a macOS VM needs a JIT config") }
        try withExchange {
            guard machine.mayBegin else { throw AgentRefusal(description: "begin: not after a probe in ready") }
            guard case .accepted = try exchange(.begin(jitConfig: jitConfig)) else { throw AgentViolation.wrongReply }
            machine.begun(at: Date())
        }
    }

    /// `verify`: have the agent run the image's checks against `expect` (ADR-0049 §7). No JIT
    /// config exists for such a VM.
    func verify(expect: VerifyExpect) throws {
        try withExchange {
            guard machine.mayVerify else { throw AgentRefusal(description: "verify: not after a probe in ready") }
            guard case .accepted = try exchange(.verify(expect)) else { throw AgentViolation.wrongReply }
            machine.verifying(at: Date())
        }
    }
}
