// The guest agent protocol (docs/vm-agent-protocol.md): its codec, the launcher's phase
// machine and channel, and the agent's own pure pieces. No VM and no guest: the channel is
// driven over a socketpair, by the agent's real request handler or by a scripted fake.

import Darwin
import Foundation

private let secret = "c2VjcmV0LWppdC1jb25maWctZm9yLXRlc3Rz"

@MainActor
func testAgent(_ t: Harness, _ scratch: Scratch) {
    testAgentCodec(t)
    testHandoffCodec(t)
    testStreamFrameReader(t)
    testPhaseMachine(t)
    testAgentCore(t)
    testAgentChannel(t)
    testAgentChannelNoJob(t)
    testAgentViolations(t)
    testGuestPieces(t)
    testHandoffGate(t, scratch)
}

private func line(_ s: String) -> [UInt8] { Array((s + "\n").utf8) }

private func replyOK(_ s: String) -> Bool { (try? decodeAgentReply(line(s))) != nil }
private func requestOK(_ s: String) -> Bool { (try? decodeAgentRequest(line(s))) != nil }

@MainActor
private func testAgentCodec(_ t: Harness) {
    let jit = JitConfig(secret)!
    for (name, request, want) in [
        ("hello", AgentRequest.hello(protocol: 3), #"{"id":1,"request":{"hello":{"protocol":3}}}"#),
        ("status", .status, #"{"id":1,"request":{"status":{}}}"#),
        ("probe", .probe(addr: "192.168.100.1", port: 9, count: 3), #"{"id":1,"request":{"probe":{"addr":"192.168.100.1","port":9,"count":3}}}"#),
        ("begin", .begin(jitConfig: jit), #"{"id":1,"request":{"begin":{"jit_config":"\#(secret)"}}}"#),
    ] {
        let bytes = try? encodeAgentRequest(AgentRequestFrame(id: 1, request: request))
        t.check("agent request \(name) encodes as the spec shows", bytes == line(want), bytes.map { String(decoding: $0, as: UTF8.self) } ?? "nil")
        t.check("agent request \(name) decodes", requestOK(want))
    }
    t.check("a begin request prints redacted", !"\(AgentRequest.begin(jitConfig: jit))".contains(secret))

    let replies: [(AgentReply, String)] = [
        (.hello(protocol: 3), #"{"id":7,"reply":{"hello":{"protocol":3}}}"#),
        (.status(AgentStatus(.waitingConsole)), #"{"id":7,"reply":{"status":{"phase":"waiting_console"}}}"#),
        (.status(AgentStatus(.exited, exitCode: 0)), #"{"id":7,"reply":{"status":{"phase":"exited","exit_code":0}}}"#),
        (.status(AgentStatus(.exited, signal: 9)), #"{"id":7,"reply":{"status":{"phase":"exited","signal":9}}}"#),
        (.status(AgentStatus(.exited)), #"{"id":7,"reply":{"status":{"phase":"exited"}}}"#),
        (.status(AgentStatus(.exited, exitCode: 0, job: .notStarted)), #"{"id":7,"reply":{"status":{"phase":"exited","exit_code":0,"job":"none"}}}"#),
        (.status(AgentStatus(.exited, signal: 15, job: .started)), #"{"id":7,"reply":{"status":{"phase":"exited","signal":15,"job":"started"}}}"#),
        (.status(AgentStatus(.exited, job: .notStarted)), #"{"id":7,"reply":{"status":{"phase":"exited","job":"none"}}}"#),
        (.probed(sent: 3), #"{"id":7,"reply":{"probed":{"sent":3}}}"#),
        (.accepted, #"{"id":7,"reply":{"accepted":{}}}"#),
        (.error(.wrongPhase), #"{"id":7,"reply":{"error":{"code":"wrong_phase"}}}"#),
    ]
    for (reply, want) in replies {
        let bytes = try? encodeAgentReply(AgentReplyFrame(id: 7, reply: reply))
        t.check("agent reply encodes: \(want)", bytes == line(want))
        t.check("agent reply decodes back: \(want)", (try? decodeAgentReply(line(want)))?.reply == reply)
    }

    // The launcher's decoder: everything outside the closed vocabulary is refused.
    for (name, frame) in [
        ("an unknown field", #"{"id":2,"reply":{"status":{"phase":"ready","extra":1}}}"#),
        ("a missing field", #"{"id":2,"reply":{"status":{}}}"#),
        ("a duplicate key", #"{"id":2,"reply":{"status":{"phase":"ready","phase":"ready"}}}"#),
        ("a duplicate top-level key", #"{"id":2,"id":2,"reply":{"accepted":{}}}"#),
        ("an unknown phase", #"{"id":2,"reply":{"status":{"phase":"done"}}}"#),
        ("an exit code outside exited", #"{"id":2,"reply":{"status":{"phase":"running","exit_code":0}}}"#),
        ("both exit code and signal", #"{"id":2,"reply":{"status":{"phase":"exited","exit_code":0,"signal":9}}}"#),
        ("an exit code over 255", #"{"id":2,"reply":{"status":{"phase":"exited","exit_code":256}}}"#),
        ("signal 0", #"{"id":2,"reply":{"status":{"phase":"exited","signal":0}}}"#),
        ("signal 32", #"{"id":2,"reply":{"status":{"phase":"exited","signal":32}}}"#),
        ("a negative exit code", #"{"id":2,"reply":{"status":{"phase":"exited","exit_code":-1}}}"#),
        ("a job word outside exited", #"{"id":2,"reply":{"status":{"phase":"running","job":"none"}}}"#),
        ("an unknown job word", #"{"id":2,"reply":{"status":{"phase":"exited","exit_code":0,"job":"maybe"}}}"#),
        ("a job word that is a number", #"{"id":2,"reply":{"status":{"phase":"exited","job":0}}}"#),
        ("a float", #"{"id":2,"reply":{"probed":{"sent":3.0}}}"#),
        ("sent over 16", #"{"id":2,"reply":{"probed":{"sent":17}}}"#),
        ("id 0", #"{"id":0,"reply":{"accepted":{}}}"#),
        ("an id past u32", #"{"id":4294967296,"reply":{"accepted":{}}}"#),
        ("a field in accepted", #"{"id":2,"reply":{"accepted":{"x":1}}}"#),
        ("an unknown error code", #"{"id":2,"reply":{"error":{"code":"slot_busy"}}}"#),
        ("an unknown operation", #"{"id":2,"reply":{"vm":{}}}"#),
        ("two operations", #"{"id":2,"reply":{"accepted":{},"probed":{"sent":1}}}"#),
        ("a protocol in an error", #"{"id":2,"reply":{"error":{"code":"version_mismatch","protocol":1}}}"#),
        ("a supervisor-style hello", #"{"id":2,"reply":{"hello":{"protocol":1,"supervisor_version":"x"}}}"#),
        ("a null", #"{"id":2,"reply":{"status":{"phase":null}}}"#),
        ("whitespace at the edge", #" {"id":2,"reply":{"accepted":{}}}"#),
    ] {
        t.check("an agent reply with \(name) is refused", !replyOK(frame))
    }
    let pad = String(repeating: "a", count: maxAgentReply)
    t.check("an agent reply over 1 024 bytes is refused",
            (try? decodeAgentReply(line(#"{"id":2,"reply":{"status":{"phase":"ready"}},"x":""#) + Array(pad.utf8) + Array("\"}\n".utf8))) == nil)
    let fits = #"{"id":2,"reply":{"accepted":{}}}"#
    t.check("a short reply fits the cap", replyOK(fits))
    do {
        _ = try decodeAgentReply(Array(repeating: 0x20, count: maxAgentReply) + [0x0A])
        t.check("a 1 025-byte reply is refused as too large", false)
    } catch {
        t.check("a 1 025-byte reply is refused as too large", error as? ProtoError == .frameTooLarge, "\(error)")
    }

    // The agent's decoder holds the launcher's frames to the same strictness.
    for (name, frame) in [
        ("probe count 0", #"{"id":3,"request":{"probe":{"addr":"192.168.100.1","port":9,"count":0}}}"#),
        ("probe count 17", #"{"id":3,"request":{"probe":{"addr":"192.168.100.1","port":9,"count":17}}}"#),
        ("probe port 0", #"{"id":3,"request":{"probe":{"addr":"192.168.100.1","port":0,"count":3}}}"#),
        ("probe port 65536", #"{"id":3,"request":{"probe":{"addr":"192.168.100.1","port":65536,"count":3}}}"#),
        ("a three-part address", #"{"id":3,"request":{"probe":{"addr":"192.168.100","port":9,"count":3}}}"#),
        ("a leading zero", #"{"id":3,"request":{"probe":{"addr":"192.168.100.01","port":9,"count":3}}}"#),
        ("an octet over 255", #"{"id":3,"request":{"probe":{"addr":"256.168.100.1","port":9,"count":3}}}"#),
        ("a host name", #"{"id":3,"request":{"probe":{"addr":"example.com","port":9,"count":3}}}"#),
        ("a probe without count", #"{"id":3,"request":{"probe":{"addr":"192.168.100.1","port":9}}}"#),
        ("a jit_config outside base64", #"{"id":4,"request":{"begin":{"jit_config":"not base64!"}}}"#),
        ("an empty jit_config", #"{"id":4,"request":{"begin":{"jit_config":""}}}"#),
        ("a begin without jit_config", #"{"id":4,"request":{"begin":{}}}"#),
        ("a field in status", #"{"id":2,"request":{"status":{"x":1}}}"#),
        ("an unknown request", #"{"id":2,"request":{"stop":{}}}"#),
        ("hello without protocol", #"{"id":1,"request":{"hello":{}}}"#),
    ] {
        t.check("an agent request with \(name) is refused", !requestOK(frame))
    }
    let tooLong = #"{"id":4,"request":{"begin":{"jit_config":""# + String(repeating: "A", count: 8193) + #""}}}"#
    t.check("a jit_config over 8 192 bytes is refused", !requestOK(tooLong))
    let longest = #"{"id":4,"request":{"begin":{"jit_config":""# + String(repeating: "A", count: 8192) + #""}}}"#
    t.check("a jit_config of 8 192 bytes fits a frame", requestOK(longest))
    t.check("a refused request's id is still read", readableAgentFrameID(line(#"{"id":9,"request":{"nope":{}}}"#)) == 9)
    t.check("encoding refuses an out-of-range probe", (try? encodeAgentRequest(AgentRequestFrame(id: 1, request: .probe(addr: "1.2.3.4", port: 9, count: 0)))) == nil)
    t.check("encoding refuses a status with a stray exit code", (try? encodeAgentReply(AgentReplyFrame(id: 1, reply: .status(AgentStatus(.running, exitCode: 1))))) == nil)
    t.check("encoding refuses a status with a stray job word", (try? encodeAgentReply(AgentReplyFrame(id: 1, reply: .status(AgentStatus(.running, job: .started))))) == nil)
    t.check("dotted quads", isDottedQuad("0.0.0.0") && isDottedQuad("255.255.255.255") && !isDottedQuad("1.2.3.4.5") && !isDottedQuad("1..2.3") && !isDottedQuad("+1.2.3.4"))
}

@MainActor
private func testHandoffCodec(_ t: Harness) {
    let jit = JitConfig(secret)!
    let config = try? encodeHandoffConfig(jit)
    t.check("the hand-off config round-trips", config.flatMap { try? decodeHandoffConfig($0) }?.expose() == secret)
    t.check("a hand-off config with another field is refused", (try? decodeHandoffConfig(line(#"{"jit_config":"QQ==","x":1}"#))) == nil)
    for report in [HandoffReport.started, .exited(exitCode: 3, signal: nil), .exited(exitCode: nil, signal: 15),
                   .exited(exitCode: nil, signal: nil), .failed,
                   .exited(exitCode: 0, signal: nil, job: .notStarted), .exited(exitCode: nil, signal: 15, job: .started),
                   .exited(exitCode: nil, signal: nil, job: .notStarted),
                   .step(HandoffStepResult(step: .configRead, ok: true)),
                   .step(HandoffStepResult(step: .extract, ok: false, exitCode: 2)),
                   .step(HandoffStepResult(step: .spawn, ok: false, signal: 9)),
                   .step(HandoffStepResult(step: .mkdir, ok: false, errno: 17))] {
        let bytes = try? encodeHandoffReport(report)
        t.check("hand-off report \(report) round-trips", bytes.flatMap { try? decodeHandoffReport($0) } == report)
    }
    t.check("a report with both exit code and signal is refused", (try? decodeHandoffReport(line(#"{"exited":{"exit_code":1,"signal":2}}"#))) == nil)
    t.check("an exit is written with its job word last",
            (try? encodeHandoffReport(.exited(exitCode: nil, signal: 15, job: .notStarted)))
                == line(#"{"exited":{"signal":15,"job":"none"}}"#))
    t.check("an exit with an unknown job word is refused",
            (try? decodeHandoffReport(line(#"{"exited":{"exit_code":0,"job":"maybe"}}"#))) == nil)
    t.check("an unknown report is refused", (try? decodeHandoffReport(line(#"{"done":{}}"#))) == nil)
    t.check("a step is written in its fixed shape",
            (try? encodeHandoffReport(.step(HandoffStepResult(step: .extract, ok: false, exitCode: 2))))
                == line(#"{"step":{"name":"extract","result":"failed","exit_code":2}}"#))
    for (why, frame) in [
        ("an unknown step", #"{"step":{"name":"rm","result":"ok"}}"#),
        ("free text in place of a result", #"{"step":{"name":"extract","result":"tar said no"}}"#),
        ("a free-text field", #"{"step":{"name":"extract","result":"failed","why":"x"}}"#),
        ("two integers", #"{"step":{"name":"extract","result":"failed","exit_code":1,"errno":2}}"#),
        ("an errno out of range", #"{"step":{"name":"mkdir","result":"failed","errno":0}}"#),
        ("an exit code out of range", #"{"step":{"name":"extract","result":"failed","exit_code":256}}"#),
        ("a missing result", #"{"step":{"name":"extract"}}"#),
    ] {
        t.check("a step with \(why) is refused", (try? decodeHandoffReport(line(frame))) == nil)
    }
    t.check("a step with two integers is never encoded",
            (try? encodeHandoffReport(.step(HandoffStepResult(step: .extract, ok: false, exitCode: 1, signal: 2)))) == nil)
    t.check("a report over 256 bytes is refused",
            (try? decodeHandoffReport(line(#"{"failed":{}}"# + String(repeating: " ", count: 300)))) == nil)
}

@MainActor
private func testPhaseMachine(_ t: Harness) {
    let d = AgentDeadlines(ready: 600, reply: 1.5, started: 120)
    let t0 = Date(timeIntervalSince1970: 1_760_700_000)
    func machine(_ phases: [AgentPhase], probe: Bool = false, begin: Bool = false) throws -> AgentPhaseMachine {
        var m = AgentPhaseMachine(deadlines: d, since: t0)
        for p in phases { _ = try m.observe(AgentStatus(p)) }
        if probe { try m.probeReplied(sent: 3, count: 3) }
        if begin { m.begun(at: t0) }
        return m
    }
    func verdict(_ m: AgentPhaseMachine, _ s: AgentStatus) -> AgentViolation? {
        var m = m
        do {
            _ = try m.observe(s)
            return nil
        } catch {
            return error as? AgentViolation ?? .badReply
        }
    }

    // The whole accepted walk, with the events the child acts on.
    do {
        var m = AgentPhaseMachine(deadlines: d, since: t0)
        var events: [AgentPhaseMachine.Event] = []
        for p in [AgentPhase.waitingConsole, .waitingConsole, .ready] { events.append(try m.observe(AgentStatus(p))) }
        t.check("probe is allowed in ready", m.mayProbe)
        t.check("begin is not allowed before probe", !m.mayBegin)
        events.append(try m.observe(AgentStatus(.ready)))
        try m.probeReplied(sent: 3, count: 3)
        t.check("probe is allowed once", !m.mayProbe)
        t.check("begin is allowed after probe", m.mayBegin)
        m.begun(at: t0)
        t.check("begin is allowed once", !m.mayBegin)
        for p in [AgentPhase.starting, .starting, .running, .running] { events.append(try m.observe(AgentStatus(p))) }
        events.append(try m.observe(AgentStatus(.exited, exitCode: 0)))
        events.append(try m.observe(AgentStatus(.exited, exitCode: 0)))
        t.check("the walk's events", events == [.none, .none, .becameReady, .none, .none, .none, .started, .none,
                                               .runnerExited(exitCode: 0, signal: nil, job: nil),
                                               .runnerExited(exitCode: 0, signal: nil, job: nil)], "\(events)")
    } catch {
        t.check("the accepted walk", false, "\(error)")
    }

    do {
        // The agent's word on whether the runner took a job travels with the exit.
        var m = try machine([.ready], probe: true, begin: true)
        let none = try m.observe(AgentStatus(.exited, exitCode: 0, job: .notStarted))
        t.check("an exit with no job carries the agent's word", none == .runnerExited(exitCode: 0, signal: nil, job: .notStarted), "\(none)")
        var n = try machine([.ready], probe: true, begin: true)
        let started = try n.observe(AgentStatus(.exited, signal: 15, job: .started))
        t.check("an exit after a job carries it too", started == .runnerExited(exitCode: nil, signal: 15, job: .started), "\(started)")
    } catch {
        t.check("an exit's job word", false, "\(error)")
    }
    t.check("the runner's exit line: its code and the job word",
            runnerExitedEvent(exitCode: 0, signal: nil, job: .notStarted) == "event=runner-exited exit_code=0 job=none")
    t.check("the runner's exit line: a signal",
            runnerExitedEvent(exitCode: nil, signal: 9, job: .started) == "event=runner-exited signal=9 job=started")
    t.check("the runner's exit line: nothing known",
            runnerExitedEvent(exitCode: nil, signal: nil, job: nil) == "event=runner-exited status=unknown job=unknown")

    do {
        // Before begin: only waiting_console then ready.
        let fresh = try machine([])
        let waiting = try machine([.waitingConsole])
        let ready = try machine([.waitingConsole, .ready])
        let probed = try machine([.ready], probe: true)
        for (name, m) in [("first status", fresh), ("waiting_console", waiting), ("ready", ready), ("probed", probed)] {
            for p in [AgentPhase.starting, .running, .exited] {
                t.check("\(p.rawValue) before begin (after \(name)) is bad_phase", verdict(m, AgentStatus(p)) == .badPhase)
            }
            t.check("failed (after \(name)) is start_failed", verdict(m, AgentStatus(.failed)) == .startFailed)
        }
        t.check("waiting_console after ready is bad_phase", verdict(ready, AgentStatus(.waitingConsole)) == .badPhase)
        t.check("ready again is fine", verdict(ready, AgentStatus(.ready)) == nil)

        // After begin: forward moves only.
        let begun = try machine([.ready], probe: true, begin: true)
        var s2 = try machine([.ready], probe: true, begin: true)
        _ = try s2.observe(AgentStatus(.starting))
        var running = s2
        _ = try running.observe(AgentStatus(.running))
        var exited = running
        _ = try exited.observe(AgentStatus(.exited, exitCode: 1))
        t.check("ready after begin is bad_phase", verdict(begun, AgentStatus(.ready)) == .badPhase)
        t.check("waiting_console after begin is bad_phase", verdict(begun, AgentStatus(.waitingConsole)) == .badPhase)
        t.check("running first seen after begin is fine (a missed starting)", verdict(begun, AgentStatus(.running)) == nil)
        t.check("starting straight to exited is fine", verdict(s2, AgentStatus(.exited, signal: 9)) == nil)
        t.check("starting after running is bad_phase", verdict(running, AgentStatus(.starting)) == .badPhase)
        t.check("ready after running is bad_phase", verdict(running, AgentStatus(.ready)) == .badPhase)
        t.check("running after exited is bad_phase", verdict(exited, AgentStatus(.running)) == .badPhase)
        t.check("starting after exited is bad_phase", verdict(exited, AgentStatus(.starting)) == .badPhase)
        t.check("failed after begin is start_failed", verdict(s2, AgentStatus(.failed)) == .startFailed)
        t.check("failed after running is start_failed", verdict(running, AgentStatus(.failed)) == .startFailed)
        t.check("an invalid status is bad_reply", verdict(running, AgentStatus(.running, exitCode: 1)) == .badReply)

        // A short probe is a violation; probe and begin out of order are the host's refusal.
        var p = try machine([.ready])
        do {
            try p.probeReplied(sent: 2, count: 3)
            t.check("a probe not sent in full is probe_failed", false)
        } catch {
            t.check("a probe not sent in full is probe_failed", error as? AgentViolation == .probeFailed)
        }
        t.check("probe is not allowed while waiting", !waiting.mayProbe)
        t.check("probe is not allowed after begin", !begun.mayProbe)

        // Deadlines.
        t.accepts("ready inside its deadline") { try fresh.checkDeadlines(now: t0.addingTimeInterval(599)) }
        t.check("ready missed is timeout", (try? fresh.checkDeadlines(now: t0.addingTimeInterval(601))) == nil)
        t.accepts("once ready, the ready deadline is spent") { try ready.checkDeadlines(now: t0.addingTimeInterval(10_000)) }
        t.check("begin without running in 120 s is timeout", (try? s2.checkDeadlines(now: t0.addingTimeInterval(121))) == nil)
        let guestBegin = HandoffDeadline.untilStarted
            + Double(updatesOffWrites().count) * (UpdatePolicy.writeTimeout + Checks.readerGrace)
        t.check("the guest's hand-off counts its three launchctl runs",
                HandoffDeadline.untilStarted == 3 * (HandoffDeadline.launchctl + Checks.readerGrace)
                    + HandoffDeadline.connect + HandoffDeadline.config + HandoffDeadline.started)
        t.check("the default started deadline outlasts the guest's Software Update writes and hand-off",
                AgentDeadlines().started > guestBegin, "\(guestBegin)")
        t.accepts("begin then running in time") { try running.checkDeadlines(now: t0.addingTimeInterval(10_000)) }
    } catch {
        t.check("phase machine fixtures", false, "\(error)")
    }

    // A reply must answer the request asked.
    func replyVerdict(_ r: AgentReply, _ q: AgentRequest) -> AgentViolation? {
        do {
            _ = try checkAgentReply(r, to: q)
            return nil
        } catch {
            return error as? AgentViolation
        }
    }
    t.check("hello answers hello", replyVerdict(.hello(protocol: agentProtocol), .hello(protocol: agentProtocol)) == nil)
    t.check("hello at another version is version_mismatch", replyVerdict(.hello(protocol: agentProtocol - 1), .hello(protocol: agentProtocol)) == .versionMismatch)
    t.check("a version_mismatch error is version_mismatch", replyVerdict(.error(.versionMismatch), .hello(protocol: agentProtocol)) == .versionMismatch)
    t.check("an error reply is a violation", replyVerdict(.error(.wrongPhase), .status) == .errorReply)
    t.check("status answered by probed is wrong_reply", replyVerdict(.probed(sent: 3), .status) == .wrongReply)
    t.check("probe answered by accepted is wrong_reply", replyVerdict(.accepted, .probe(addr: "1.2.3.4", port: 9, count: 3)) == .wrongReply)
    t.check("begin answered by status is wrong_reply", replyVerdict(.status(AgentStatus(.starting)), .begin(jitConfig: JitConfig(secret)!)) == .wrongReply)
    t.check("an I/O timeout is timeout", agentViolation(DeadlineIOError.timeout) == .timeout)
    t.check("an EOF is connection_lost", agentViolation(DeadlineIOError.closed) == .connectionLost)
    t.check("an oversized reply is bad_reply", agentViolation(DeadlineIOError.tooLarge) == .badReply)
    t.check("bytes after a reply are unasked", agentViolation(DeadlineIOError.notOneLine) == .unasked)
    t.check("every violation code is the spec's grammar", AgentViolation.allCases.allSatisfy { $0.rawValue.utf8.allSatisfy { ($0 >= 0x61 && $0 <= 0x7A) || $0 == 0x5F } })
}

/// A value the agent's closures set from other threads.
private final class Box<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    var get: T { lock.lock(); defer { lock.unlock() }; return value }
    func set(_ v: T) { lock.lock(); value = v; lock.unlock() }
}

@MainActor
private func testAgentCore(_ t: Harness) {
    let console = Box(false)
    let started = Box<String?>(nil)
    let core = AgentCore(consoleIsRunner: { console.get }, probe: { _, _, count in count },
                         startRunner: { jit, _ in started.set(jit.expose()) })
    var greeted = false
    t.check("a request before hello is handshake_required and closes", core.answer(.status, greeted: &greeted) == (.error(.handshakeRequired), false))
    t.check("hello at another version is version_mismatch and closes", core.answer(.hello(protocol: agentProtocol - 1), greeted: &greeted) == (.error(.versionMismatch), false))
    t.check("hello at the agent's version is answered", core.answer(.hello(protocol: agentProtocol), greeted: &greeted) == (.hello(protocol: agentProtocol), true))
    t.check("a second hello is bad_request and closes", core.answer(.hello(protocol: agentProtocol), greeted: &greeted) == (.error(.badRequest), false))
    let jit = JitConfig(secret)!
    let probe = AgentRequest.probe(addr: "192.168.100.1", port: 9, count: 3)
    t.check("waiting for the console", core.answer(.status, greeted: &greeted) == (.status(AgentStatus(.waitingConsole)), true))
    t.check("probe while waiting is wrong_phase", core.answer(probe, greeted: &greeted) == (.error(.wrongPhase), true))
    console.set(true)
    t.check("ready once runner is the console user", core.answer(.status, greeted: &greeted) == (.status(AgentStatus(.ready)), true))
    t.check("begin before probe is wrong_phase", core.answer(.begin(jitConfig: jit), greeted: &greeted) == (.error(.wrongPhase), true))
    t.check("probe in ready sends", core.answer(probe, greeted: &greeted) == (.probed(sent: 3), true))
    t.check("a second probe is wrong_phase", core.answer(probe, greeted: &greeted) == (.error(.wrongPhase), true))
    t.check("begin after probe is accepted", core.answer(.begin(jitConfig: jit), greeted: &greeted) == (.accepted, true))
    t.check("begin hands the config on, unchanged", eventually { started.get == secret })
    t.check("a second begin is wrong_phase", core.answer(.begin(jitConfig: jit), greeted: &greeted) == (.error(.wrongPhase), true))
    t.check("after begin: starting", core.status == AgentStatus(.starting))
    core.report(.running)
    t.check("then running", core.status == AgentStatus(.running))
    core.report(.starting)
    t.check("nothing moves back to starting", core.status == AgentStatus(.running))
    core.report(.exited(exitCode: nil, signal: 15))
    core.report(.running)
    t.check("an exit is final", core.status == AgentStatus(.exited, signal: 15))
    t.check("the host's vsock CID is admitted", admitVsockPeer(cid: UInt32(VMADDR_CID_HOST), family: sa_family_t(AF_VSOCK)))
    t.check("another CID is refused", !admitVsockPeer(cid: 3, family: sa_family_t(AF_VSOCK)) && !admitVsockPeer(cid: VMADDR_CID_ANY, family: sa_family_t(AF_VSOCK)))
    t.check("another family is refused", !admitVsockPeer(cid: UInt32(VMADDR_CID_HOST), family: sa_family_t(AF_UNIX)))
}

/// runner-exec streams its hand-off reports without waiting, so several can arrive in one read:
/// the reader hands them out one at a time, where `readFrame` (lockstep) refuses the second.
@MainActor
private func testStreamFrameReader(_ t: Harness) {
    guard let (a, b) = socketPair() else {
        t.check("socketpair", false, errnoText(errno))
        return
    }
    defer { close(a); close(b) }
    setNonBlocking(a)
    let deadline = { Date().addingTimeInterval(2) }
    let one = line(#"{"a":1}"#), two = line(#"{"b":2}"#), three = line(#"{"c":3}"#)
    let all = one + two + three
    _ = all.withUnsafeBytes { write(b, $0.baseAddress, $0.count) }
    var reader = StreamFrameReader()
    t.check("three frames in one read come out in order",
            (try? reader.next(a, cap: 256, until: deadline())) == one
                && (try? reader.next(a, cap: 256, until: deadline())) == two
                && (try? reader.next(a, cap: 256, until: deadline())) == three)
    // A frame split across writes is completed from the next read.
    let half = Array(two[0..<3])
    _ = half.withUnsafeBytes { write(b, $0.baseAddress, $0.count) }
    let rest = Array(two[3...]) + one
    _ = rest.withUnsafeBytes { write(b, $0.baseAddress, $0.count) }
    t.check("a split frame is joined, and the next one kept",
            (try? reader.next(a, cap: 256, until: deadline())) == two
                && (try? reader.next(a, cap: 256, until: deadline())) == one)
    // The cap counts one frame, not the buffer: an LF-less run over it is too large.
    let long = [UInt8](repeating: 0x61, count: 40)
    _ = long.withUnsafeBytes { write(b, $0.baseAddress, $0.count) }
    var small = StreamFrameReader()
    t.check("a frame over the cap is too large",
            (try? small.next(a, cap: 16, until: deadline())) == nil)
    close(b)
    var closed = StreamFrameReader()
    t.check("a closed peer ends the stream", (try? closed.next(a, cap: 256, until: deadline())) == nil)
}

private let fastDeadlines = AgentDeadlines(ready: 5, reply: 0.5, started: 2, poll: 0.05, lockWait: 0.4)

private func socketPair() -> (Int32, Int32)? {
    var pair: [Int32] = [-1, -1]
    guard socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0 else { return nil }
    return (pair[0], pair[1])
}

/// Poll until `ok` holds, for up to `seconds`.
private func eventually(_ seconds: Double = 3, _ ok: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while Date() < deadline {
        if ok() { return true }
        usleep(20_000)
    }
    return ok()
}

/// The channel against the agent's real request handler, over a socketpair: the whole walk.
@MainActor
private func testAgentChannel(_ t: Harness) {
    guard let (host, guest) = socketPair() else {
        t.check("socketpair", false, errnoText(errno))
        return
    }
    let console = Box(false)
    let violation = Box<AgentViolation?>(nil)
    let core = AgentCore(consoleIsRunner: { console.get }, probe: { _, _, count in count }, startRunner: { jit, core in
        guard jit.expose() == secret else { return core.report(.failed) }
        Thread.detachNewThread {
            core.report(.running)
            usleep(150_000)
            core.report(.exited(exitCode: 7, signal: nil))
        }
    })
    Thread.detachNewThread { serveConnection(guest, core) }
    let status = ChildStatus()
    _ = status.move(from: .starting, to: .booting)
    let channel = AgentChannel(status: status, probeTarget: "192.168.100.1", deadlines: fastDeadlines) { violation.set($0) }
    channel.start()
    channel.connect(fd: host, owner: nil)

    usleep(200_000)
    t.check("waiting for the console keeps the child booting", status.state == .booting)
    do {
        try channel.sendProbe()
        t.check("probe before ready is refused", false)
    } catch {
        t.check("probe before ready is the host's refusal, not a violation", error is AgentRefusal && violation.get == nil, "\(error)")
    }
    console.set(true)
    t.check("ready moves the child booting -> ready", eventually { status.state == .ready })
    do {
        try channel.begin(jitConfig: JitConfig(secret))
        t.check("begin before probe is refused", false)
    } catch {
        t.check("begin before probe is the host's refusal", error is AgentRefusal && violation.get == nil, "\(error)")
    }
    t.accepts("probe in ready") { try channel.sendProbe() }
    _ = status.move(from: .ready, to: .probeSent)  // what the child server does with the reply
    t.accepts("begin after probe") { try channel.begin(jitConfig: JitConfig(secret)) }
    _ = status.move(from: .probeSent, to: .inService)
    t.check("the runner's exit moves the child in_service -> job_finished", eventually { status.state == .jobFinished })
    t.check("no violation in the whole walk", violation.get == nil, "\(String(describing: violation.get))")
    status.set(.stopping)
    usleep(150_000)
    close(host)
    close(guest)
}

/// A runner that exits without ever taking a job is not a job that finished: the channel asks
/// for the no-job stop, once, and the child never says `job_finished`.
@MainActor
private func testAgentChannelNoJob(_ t: Harness) {
    guard let (host, guest) = socketPair() else {
        t.check("socketpair", false, errnoText(errno))
        return
    }
    let violation = Box<AgentViolation?>(nil)
    let noJob = Box(0)
    let core = AgentCore(consoleIsRunner: { true }, probe: { _, _, count in count }, startRunner: { _, core in
        Thread.detachNewThread {
            core.report(.running)
            usleep(150_000)
            core.report(.exited(exitCode: 0, signal: nil, job: .notStarted))
        }
    })
    Thread.detachNewThread { serveConnection(guest, core) }
    let status = ChildStatus()
    _ = status.move(from: .starting, to: .booting)
    let channel = AgentChannel(status: status, probeTarget: "192.168.100.1", deadlines: fastDeadlines,
                               onNoJob: { noJob.set(noJob.get + 1) }) { violation.set($0) }
    channel.start()
    channel.connect(fd: host, owner: nil)
    _ = eventually { status.state == .ready }
    t.accepts("probe, for the no-job walk") { try channel.sendProbe() }
    _ = status.move(from: .ready, to: .probeSent)
    t.accepts("begin, for the no-job walk") { try channel.begin(jitConfig: JitConfig(secret)) }
    _ = status.move(from: .probeSent, to: .inService)
    t.check("a runner that took no job asks for the no-job stop", eventually { noJob.get > 0 })
    usleep(300_000)
    t.check("the no-job stop is asked for once, and polling stops", noJob.get == 1, "\(noJob.get)")
    t.check("the child never reports job_finished for it", status.state == .inService, "\(status.state)")
    t.check("no violation in the no-job walk", violation.get == nil, "\(String(describing: violation.get))")
    status.set(.stopping)
    usleep(150_000)
    close(host)
    close(guest)
}

/// A fake agent that answers each request with what `script` returns for it (raw bytes, or
/// nil to stay silent), after a valid hello unless `hello` says otherwise.
private func scripted(_ fd: Int32, hello: String? = nil, _ script: @escaping @Sendable (Int, AgentRequestFrame) -> [UInt8]?) {
    Thread.detachNewThread {
        var reader = FrameReader(fd: fd, cap: maxAgentRequest)
        var n = 0
        while let raw = try? reader.next(), let frame = try? decodeAgentRequest(raw) {
            let out: [UInt8]?
            if n == 0 {
                out = Array(((hello ?? #"{"id":\#(frame.id),"reply":{"hello":{"protocol":\#(agentProtocol)}}}"#) + "\n").utf8)
            } else {
                out = script(n, frame)
            }
            n += 1
            if let out { _ = try? writeAll(fd, out) }
        }
    }
}

private func ready(_ id: UInt32) -> [UInt8] { line(#"{"id":\#(id),"reply":{"status":{"phase":"ready"}}}"#) }

@MainActor
private func testAgentViolations(_ t: Harness) {
    func run(_ name: String, want: AgentViolation, deadlines: AgentDeadlines = fastDeadlines, hello: String? = nil,
             drive: ((AgentChannel) -> Void)? = nil, closeGuest: Bool = false,
             _ script: @escaping @Sendable (Int, AgentRequestFrame) -> [UInt8]?) {
        guard let (host, guest) = socketPair() else {
            t.check("socketpair", false)
            return
        }
        let violation = Box<AgentViolation?>(nil)
        let calls = Box(0)
        let status = ChildStatus()
        _ = status.move(from: .starting, to: .booting)
        scripted(guest, hello: hello, script)
        let channel = AgentChannel(status: status, probeTarget: "192.168.64.1", deadlines: deadlines) {
            violation.set($0)
            calls.set(calls.get + 1)
        }
        channel.start()
        channel.connect(fd: host, owner: nil)
        if closeGuest {
            usleep(100_000)
            shutdown(guest, SHUT_RDWR)
        }
        drive?(channel)
        let seen = eventually { violation.get != nil }
        t.check("\(name): \(want.rawValue)", seen && violation.get == want, "\(String(describing: violation.get))")
        usleep(100_000)
        t.check("\(name): reported once, and polling stopped", calls.get == 1 && channel.hasFailed)
        close(host)
        close(guest)
    }
    run("an oversized reply", want: .badReply) { _, _ in Array(repeating: 0x61, count: maxAgentReply + 10) + [0x0A] }
    run("a malformed reply", want: .badReply) { _, f in line(#"{"id":\#(f.id),"reply":{"status":{"phase":"ready","x":1}}}"#) }
    run("a reply under another id", want: .wrongReply) { _, f in ready(f.id + 1) }
    run("a reply to another request", want: .wrongReply) { _, f in line(#"{"id":\#(f.id),"reply":{"accepted":{}}}"#) }
    run("an error reply", want: .errorReply) { _, f in line(#"{"id":\#(f.id),"reply":{"error":{"code":"wrong_phase"}}}"#) }
    run("a hello at another version", want: .versionMismatch, hello: #"{"id":1,"reply":{"hello":{"protocol":1}}}"#) { _, f in ready(f.id) }
    run("running before begin", want: .badPhase) { _, f in line(#"{"id":\#(f.id),"reply":{"status":{"phase":"running"}}}"#) }
    run("ready, then waiting again", want: .badPhase) { n, f in
        n == 1 ? ready(f.id) : line(#"{"id":\#(f.id),"reply":{"status":{"phase":"waiting_console"}}}"#)
    }
    run("failed", want: .startFailed) { _, f in line(#"{"id":\#(f.id),"reply":{"status":{"phase":"failed"}}}"#) }
    run("a silent agent", want: .timeout) { _, _ in nil }
    run("two replies to one request", want: .unasked) { _, f in ready(f.id) + ready(f.id) }
    run("a reply with a trailing byte", want: .unasked) { _, f in ready(f.id) + [0x7B] }
    run("a closed connection", want: .connectionLost, closeGuest: true) { _, f in ready(f.id) }
    run("never ready", want: .timeout, deadlines: AgentDeadlines(ready: 0.3, reply: 0.5, started: 2, poll: 0.05, lockWait: 0.4)) { _, f in
        line(#"{"id":\#(f.id),"reply":{"status":{"phase":"waiting_console"}}}"#)
    }
    // A probe not sent in full: the poll answers ready, the probe answers sent 2 of 3.
    run("a short probe", want: .probeFailed, drive: { channel in
        _ = eventually { channel.status.state == .ready }
        _ = try? channel.sendProbe()
    }) { _, f in
        // The probe is answered short; every status, ready.
        if case .probe = f.request { return line(#"{"id":\#(f.id),"reply":{"probed":{"sent":2}}}"#) }
        return ready(f.id)
    }
}

@MainActor
private func testGuestPieces(_ t: Harness) {
    let jit = JitConfig(secret)!
    // The JIT config is on no argv the agent builds, and in no file.
    let argvs = [launchctlBootstrapArgv(uid: 501), tarArgv(tarball: Guest.toolsMount + "/actions-runner-osx-arm64-2.330.0.tar.gz", into: "/Users/runner/faber-runner"),
                 runnerArgv(dir: "/Users/runner/faber-runner")]
    t.check("no argv carries the JIT config", argvs.allSatisfy { !$0.joined(separator: " ").contains(secret) })
    t.check("launchctl bootstraps into gui/<uid> with a fixed argv", argvs[0] == ["/bin/launchctl", "bootstrap", "gui/501", Guest.plistPath])
    t.check("tar unpacks into the fresh directory", argvs[1] == ["/usr/bin/tar", "-xzf", Guest.toolsMount + "/actions-runner-osx-arm64-2.330.0.tar.gz", "-C", "/Users/runner/faber-runner"])
    t.check("run.sh takes no arguments", argvs[2] == ["/Users/runner/faber-runner/run.sh"])
    let env = runnerEnvironment(jit: jit, home: "/Users/runner", user: "runner", tmpdir: "/var/folders/x/T/")
    t.check("run.sh's environment carries the config once, under the runner's own variable",
            env.filter { $0.contains(secret) } == ["ACTIONS_RUNNER_INPUT_JITCONFIG=" + secret])
    t.check("run.sh's environment is UTF-8", env.contains("LANG=en_US.UTF-8") && env.contains("LC_ALL=en_US.UTF-8"))
    t.check("a TMPDIR with a control byte is dropped",
            !runnerEnvironment(jit: jit, home: "/h", user: "runner", tmpdir: "/x\n").contains { $0.hasPrefix("TMPDIR=") })
    t.check("the other processes' environment carries nothing of the job", !fixedEnvironment.joined().contains(secret))
    do {
        let data = try launchAgentPlist()
        t.check("the LaunchAgent plist holds no secret", !String(decoding: data, as: UTF8.self).contains(secret))
        let p = plist(data)
        t.check("the LaunchAgent runs this agent's runner-exec", (p?["ProgramArguments"] as? [String]) == [Guest.agentBinary, "runner-exec"])
        t.check("the LaunchAgent is Aqua-only", (p?["LimitLoadToSessionType"] as? String) == "Aqua")
        let e = p?["EnvironmentVariables"] as? [String: String]
        t.check("the LaunchAgent's locale is UTF-8", e?["LANG"] == "en_US.UTF-8" && e?["LC_ALL"] == "en_US.UTF-8")
        t.check("the LaunchAgent runs once", (p?["KeepAlive"] as? Bool) == false && (p?["RunAtLoad"] as? Bool) == true)
    } catch {
        t.check("the LaunchAgent plist", false, "\(error)")
    }

    // The manifest, as faber writes it (serde_json::to_vec_pretty).
    let sha = String(repeating: "ab", count: 32)
    let pretty = "{\n  \"runner_version\": \"2.330.0\",\n  \"runner_sha256\": \"\(sha)\",\n  \"agent_version\": \"0.4.0+gabc1234\",\n  \"agent_sha256\": \"\(sha)\"\n}"
    let m = try? parseManifest(Array(pretty.utf8))
    t.check("faber's pretty-printed manifest is read", m?.runnerVersion == "2.330.0" && m?.agentVersion == "0.4.0+gabc1234")
    t.check("the tarball name follows the runner version", m?.tarball == "actions-runner-osx-arm64-2.330.0.tar.gz")
    func manifest(_ edit: (String) -> String) -> Bool { (try? parseManifest(Array(edit(pretty).utf8))) != nil }
    t.check("a manifest with another field is refused", !manifest { $0.replacingOccurrences(of: "\n}", with: ",\n  \"x\": \"y\"\n}") })
    t.check("a manifest missing a field is refused", !manifest { $0.replacingOccurrences(of: "  \"agent_version\": \"0.4.0+gabc1234\",\n", with: "") })
    t.check("a two-part runner version is refused", !manifest { $0.replacingOccurrences(of: "2.330.0", with: "2.330") })
    t.check("a runner version with a path is refused", !manifest { $0.replacingOccurrences(of: "2.330.0", with: "2.330.0/../x") })
    t.check("a leading zero is refused", !manifest { $0.replacingOccurrences(of: "2.330.0", with: "2.0330.0") })
    t.check("an uppercase digest is refused", !manifest { $0.replacingOccurrences(of: "\"runner_sha256\": \"\(sha)\"", with: "\"runner_sha256\": \"\(sha.uppercased())\"") })
    t.check("a duplicate key is refused", !manifest { $0.replacingOccurrences(of: "\n}", with: ",\n  \"runner_version\": \"2.330.0\"\n}") })
    t.check("a manifest over 4 KiB is refused", (try? parseManifest(Array((pretty + String(repeating: " ", count: 4096)).utf8))) == nil)

    t.check("exit status 0", guestExit(status: 0) == .code(0))
    t.check("exit status 3", guestExit(status: 3 << 8) == .code(3))
    t.check("killed by SIGKILL", guestExit(status: 9) == .signal(9))
    t.check("a stopped status is unknown", guestExit(status: 0x7f) == .unknown)
    var st = stat()
    st.st_mode = S_IFDIR | 0o755
    st.st_uid = 0
    t.accepts("a root-owned 0755 directory") { try checkRootDirectory(st, path: "/d") }
    st.st_mode = S_IFDIR | 0o775
    t.refuses("a group-writable directory", containing: "writable") { try checkRootDirectory(st, path: "/d") }
    st.st_mode = S_IFLNK | 0o755
    t.refuses("a symlink", containing: "symlink") { try checkRootDirectory(st, path: "/d") }
    st.st_mode = S_IFDIR | 0o755
    st.st_uid = 501
    t.refuses("another owner", containing: "root") { try checkRootDirectory(st, path: "/d") }

    // The launcher's probe target and its option.
    do {
        let shared = try RunOptions(parse(["--slot", "1", "--network", "shared-nat"], valued: RunOptions.valued), forRun: false)
        t.check("a shared-NAT VM probes Apple's default gateway", probeTarget(shared) == "192.168.64.1")
        let vmnet = try RunOptions(parse(["--slot", "1", "--subnet", "192.168.100.0/24"], valued: RunOptions.valued), forRun: false)
        t.check("a vmnet VM probes its subnet's gateway", probeTarget(vmnet) == "192.168.100.1")
        t.check("the agent's ready deadline defaults to 600 s", vmnet.agentReadyTimeout == 600)
        let custom = try RunOptions(parse(["--slot", "1", "--network", "shared-nat", "--agent-ready-timeout", "90"],
                                          valued: RunOptions.valued), forRun: false)
        t.check("--agent-ready-timeout sets it", custom.agentReadyTimeout == 90)
    } catch {
        t.check("probe target options", false, "\(error)")
    }
    for bad in ["0", "-1", "1.5", "86401", "x"] {
        t.refuses("--agent-ready-timeout \(bad) is refused", usage: true) {
            _ = try RunOptions(parse(["--slot", "1", "--network", "shared-nat", "--agent-ready-timeout", bad], valued: RunOptions.valued), forRun: false)
        }
    }
}

/// The hand-off's peer gate: only runner's uid gets the config.
@MainActor
private func testHandoffGate(_ t: Harness, _ scratch: Scratch) {
    let me = geteuid()
    t.check("runner's uid is admitted", admitHandoffPeer(501, runnerUID: 501))
    t.check("another uid is refused", !admitHandoffPeer(502, runnerUID: 501))
    t.check("root is refused", !admitHandoffPeer(0, runnerUID: 501))
    t.check("an unknown peer is refused", !admitHandoffPeer(nil, runnerUID: 501))
    t.check("a runner uid of 0 admits nobody", !admitHandoffPeer(0, runnerUID: 0))
    guard me != 0 else {
        t.skip("hand-off accept", "the tests run as root, whom the gate refuses by design")
        return
    }
    // A real socket: this process is the only peer it can offer, so the admitted case runs with
    // runnerUID = this uid, and the refused case with any other.
    let dir = URL(fileURLWithPath: "/tmp").appendingPathComponent("fh.\(getpid())")
    defer { try? FileManager.default.removeItem(at: dir) }
    do {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
        for (name, runnerUID, admitted) in [("this uid is admitted", me, true), ("another uid is closed unread", me &+ 1, false)] {
            let path = dir.appendingPathComponent("s-\(admitted)")
            let (listener, _) = try listenUnix(path, mode: 0o600)
            setNonBlocking(listener)
            let client = try connectUnix(path, timeoutSeconds: 2)
            do {
                let conn = try acceptHandoff(listener, runnerUID: runnerUID, until: Date().addingTimeInterval(0.5))
                t.check(name, admitted)
                close(conn)
            } catch {
                var byte: UInt8 = 0
                let n = read(client, &byte, 1)
                t.check(name, !admitted && n == 0, "\(error) read=\(n)")
            }
            close(client)
            close(listener)
        }
    } catch {
        t.check("hand-off accept fixtures", false, "\(error)")
    }
}
