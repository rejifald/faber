// The guest agent protocol, docs/vm-agent-protocol.md: the frames between a launcher child
// (`faber-vm run --serve-socket`) and the guest agent inside a macOS clone, over vsock. Both
// sides compile this file, so both hold the other to the same closed vocabulary.
//
// The launcher treats everything the agent sends as hostile: a reply this file does not decode
// is a violation, and the launcher logs only a fixed code for it, never the bytes.

import Darwin
import Foundation

/// The one guest agent protocol version. There is no negotiation. Version 3 added `job` to
/// `exited`.
let agentProtocol = 3
/// The vsock port the agent listens on in the guest.
let agentPort: UInt32 = 6100
/// The largest agent-to-launcher frame, its LF included. No reply defined here is near it.
let maxAgentReply = 1024
/// The largest launcher-to-agent frame, its LF included: `begin` carries the JIT config.
let maxAgentRequest = maxFrame

private let agentIDRange: ClosedRange<UInt64> = 1...UInt64(UInt32.max)

/// A dotted-quad IPv4 address: four decimal octets 0...255, no leading zeros, nothing else.
func isDottedQuad(_ s: String) -> Bool {
    let parts = s.split(separator: ".", omittingEmptySubsequences: false)
    guard parts.count == 4 else { return false }
    return parts.allSatisfy { p in
        let b = Array(p.utf8)
        guard (1...3).contains(b.count), b.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }) else { return false }
        if b.count > 1, b[0] == 0x30 { return false }
        return Int(p).map { $0 <= 255 } ?? false
    }
}

enum AgentPhase: String, CaseIterable, Sendable {
    case waitingConsole = "waiting_console"
    case ready
    case starting
    case running
    case exited
    case failed
    /// `verify` was accepted: the agent runs the image's checks (ADR-0049 §7).
    case verifying
    /// The checks are done; `failed` names the ones that failed.
    case verified
}

/// One check of ADR-0049 §7, by its fixed code. The closed set `failed` carries, in this order:
/// the same words as src/vm/proto.rs's `Check` and docs/vm-supervisor-protocol.md.
enum CheckCode: String, CaseIterable, Sendable, Comparable {
    case consoleUser = "console_user"
    case runnerNotAdmin = "runner_not_admin"
    case adminsLocked = "admins_locked"
    case remoteLogin = "remote_login"
    case filevault
    case appleID = "apple_id"
    case softwareUpdate = "software_update"
    case xcode
    case runtimes
    case brewOwner = "brew_owner"

    private var rank: Int { CheckCode.allCases.firstIndex(of: self)! }
    static func < (a: CheckCode, b: CheckCode) -> Bool { a.rank < b.rank }
}

/// Each check once, in CheckCode's order: a strictly increasing list. Pure.
func isCheckList(_ list: [CheckCode]) -> Bool {
    zip(list, list.dropFirst()).allSatisfy { $0 < $1 }
}

/// Whether the JIT runner was ever handed a job before it exited: the runner's own fact, a
/// `Worker_*.log` in its `_diag`, which Runner.Worker writes only once GitHub has assigned the
/// runner a job (docs/vm-agent-protocol.md, *The runner's exit*). A closed word, never the
/// runner's text.
enum RunnerJob: String, CaseIterable, Sendable {
    /// A worker log is there: a job was assigned and its worker started.
    case started
    /// The runner's `_diag` holds no worker log: it exited without ever taking a job.
    case notStarted = "none"
}

/// What `status` reports. `exitCode`, `signal` and `job` only with `exited`, and at most one of
/// the first two; `failed` exactly with `verified`.
struct AgentStatus: Equatable, Sendable {
    var phase: AgentPhase
    var exitCode: Int?
    var signal: Int?
    var job: RunnerJob?
    var failed: [CheckCode]?

    init(_ phase: AgentPhase, exitCode: Int? = nil, signal: Int? = nil, job: RunnerJob? = nil,
         failed: [CheckCode]? = nil) {
        self.phase = phase
        self.exitCode = exitCode
        self.signal = signal
        self.job = job
        self.failed = failed
    }

    var isValid: Bool {
        if exitCode != nil, signal != nil { return false }
        if phase != .exited, exitCode != nil || signal != nil || job != nil { return false }
        if let exitCode, !(0...255).contains(exitCode) { return false }
        if let signal, !(1...31).contains(signal) { return false }
        if (phase == .verified) != (failed != nil) { return false }
        if let failed, !isCheckList(failed) { return false }
        return true
    }
}

// MARK: - What a verification expects

/// Xcode's version line after `Xcode `: first a digit, then `[A-Za-z0-9. ]`, at most 32 bytes,
/// no double or trailing space (`27.0`, `27.0 Beta 2`).
func isXcodeVersion(_ s: String) -> Bool {
    let digit = { (c: UInt8) in c >= 0x30 && c <= 0x39 }
    return matches(s, max: 32, first: digit) { isAlnum($0) || $0 == 0x2E || $0 == 0x20 }
        && !s.contains("  ") && !s.hasSuffix(" ")
}

/// A build token, Xcode's or a runtime's: 3 to 16 ASCII letters and digits.
func isBuildToken(_ s: String) -> Bool { s.utf8.count >= 3 && matches(s, max: 16, first: isAlnum, rest: isAlnum) }

/// A runtime platform, `iOS`, `watchOS`, …: 1 to 16 ASCII letters.
func isRuntimePlatform(_ s: String) -> Bool {
    let alpha = { (c: UInt8) in (c >= 0x41 && c <= 0x5A) || (c >= 0x61 && c <= 0x7A) }
    return matches(s, max: 16, first: alpha, rest: alpha)
}

/// A runtime version: first a digit, then digits and dots, at most 16 bytes.
func isRuntimeVersion(_ s: String) -> Bool {
    let digit = { (c: UInt8) in c >= 0x30 && c <= 0x39 }
    return matches(s, max: 16, first: digit) { digit($0) || $0 == 0x2E }
}

/// The most runtimes a verification expects.
let maxExpectedRuntimes = 64

struct XcodeExpect: Equatable, Sendable {
    var version: String
    var build: String
}

struct RuntimeExpect: Equatable, Sendable {
    var platform: String
    var version: String
    var build: String
}

/// What the guest's checks are held to, from the image's manifest: faber sends expectations in,
/// and gets fixed codes out. No manifest is no Xcode and no runtimes.
struct VerifyExpect: Equatable, Sendable {
    var xcode: XcodeExpect?
    var runtimes: [RuntimeExpect]

    var isValid: Bool {
        if let x = xcode, !(isXcodeVersion(x.version) && isBuildToken(x.build)) { return false }
        return runtimes.count <= maxExpectedRuntimes
            && runtimes.allSatisfy { isRuntimePlatform($0.platform) && isRuntimeVersion($0.version) && isBuildToken($0.build) }
    }

    /// `{"xcode":{"version":V,"build":B},"runtimes":[{"platform":P,"version":V,"build":B}, …]}`.
    func write(_ w: inout JSONWriter) throws {
        guard isValid else { throw ProtoError.invalid("expect") }
        w.raw("{")
        if let x = xcode {
            w.key("xcode")
            w.raw("{")
            w.field("version", x.version, first: true)
            w.field("build", x.build)
            w.raw("},")
        }
        w.key("runtimes")
        w.raw("[")
        for (i, r) in runtimes.enumerated() {
            if i > 0 { w.raw(",") }
            w.raw("{")
            w.field("platform", r.platform, first: true)
            w.field("version", r.version)
            w.field("build", r.build)
            w.raw("}")
        }
        w.raw("]}")
    }

    static func decode(_ value: JSON) throws -> VerifyExpect {
        let f = try Fields(value, "expect", required: ["runtimes"], optional: ["xcode"])
        var xcode: XcodeExpect?
        if let x = f.value("xcode") {
            let xf = try Fields(x, "expect.xcode", required: ["version", "build"])
            xcode = XcodeExpect(version: try xf.string("version", isXcodeVersion), build: try xf.string("build", isBuildToken))
        }
        let items = try f.array("runtimes")
        guard items.count <= maxExpectedRuntimes else { throw ProtoError.invalid("expect.runtimes count") }
        var runtimes: [RuntimeExpect] = []
        for item in items {
            let rf = try Fields(item, "expect.runtime", required: ["platform", "version", "build"])
            runtimes.append(RuntimeExpect(platform: try rf.string("platform", isRuntimePlatform),
                                          version: try rf.string("version", isRuntimeVersion),
                                          build: try rf.string("build", isBuildToken)))
        }
        return VerifyExpect(xcode: xcode, runtimes: runtimes)
    }
}

/// Write `failed` as a JSON array of codes.
func writeCheckList(_ list: [CheckCode], _ w: inout JSONWriter) {
    w.raw("[")
    for (i, c) in list.enumerated() {
        if i > 0 { w.raw(",") }
        w.string(c.rawValue)
    }
    w.raw("]")
}

/// Read `failed`: an array of codes from the closed set, each once, in order. Anything else,
/// an unknown code, free text, an object, a repeat, is refused.
func decodeCheckList(_ items: [JSON], _ what: String) throws -> [CheckCode] {
    guard items.count <= CheckCode.allCases.count else { throw ProtoError.invalid("\(what) count") }
    var out: [CheckCode] = []
    for item in items {
        guard case .string(let s) = item, let c = CheckCode(rawValue: s) else { throw ProtoError.invalid(what) }
        out.append(c)
    }
    guard isCheckList(out) else { throw ProtoError.invalid("\(what) order") }
    return out
}

enum AgentRequest: Sendable {
    case hello(protocol: Int)
    case status
    /// `addr` is a dotted quad; `port` 1...65535; `count` 1...16.
    case probe(addr: String, port: Int, count: Int)
    case begin(jitConfig: JitConfig)
    /// Run the image's checks instead of a job (ADR-0049 §7). Valid once, in `ready`, after
    /// `probe`, in place of `begin`.
    case verify(VerifyExpect)
}

enum AgentErrorCode: String, CaseIterable, Sendable {
    case versionMismatch = "version_mismatch"
    case handshakeRequired = "handshake_required"
    case badRequest = "bad_request"
    case wrongPhase = "wrong_phase"
}

enum AgentReply: Equatable, Sendable {
    case hello(protocol: Int)
    case status(AgentStatus)
    case probed(sent: Int)
    case accepted
    case error(AgentErrorCode)
}

struct AgentRequestFrame: Sendable {
    var id: UInt32
    var request: AgentRequest
}

struct AgentReplyFrame: Equatable, Sendable {
    var id: UInt32
    var reply: AgentReply
}

let probeCountRange: ClosedRange<UInt64> = 1...16
let probePortRange: ClosedRange<UInt64> = 1...65535

// MARK: - Requests

func encodeAgentRequest(_ frame: AgentRequestFrame) throws -> [UInt8] {
    guard frame.id >= 1 else { throw ProtoError.invalid("frame id") }
    var w = JSONWriter()
    w.raw("{")
    w.field("id", UInt64(frame.id), first: true)
    w.raw(",")
    w.key("request")
    w.raw("{")
    switch frame.request {
    case .hello(let p):
        w.key("hello")
        w.raw("{")
        w.field("protocol", UInt64(p), first: true)
        w.raw("}")
    case .status:
        w.key("status")
        w.raw("{}")
    case .probe(let addr, let port, let count):
        guard isDottedQuad(addr), probePortRange.contains(UInt64(max(0, port))),
              probeCountRange.contains(UInt64(max(0, count))) else { throw ProtoError.invalid("probe") }
        w.key("probe")
        w.raw("{")
        w.field("addr", addr, first: true)
        w.field("port", UInt64(port))
        w.field("count", UInt64(count))
        w.raw("}")
    case .begin(let jit):
        w.key("begin")
        w.raw("{")
        w.field("jit_config", jit.expose(), first: true)
        w.raw("}")
    case .verify(let expect):
        w.key("verify")
        try expect.write(&w)
    }
    w.raw("}}")
    return try w.terminated(cap: maxAgentRequest)
}

func decodeAgentRequest(_ line: [UInt8]) throws -> AgentRequestFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line, cap: maxAgentRequest)), "frame", required: ["id", "request"])
    let id = UInt32(try top.int("id", agentIDRange))
    let (op, body) = try singleMember(top.value("request")!, "request")
    let request: AgentRequest
    switch op {
    case "hello":
        request = .hello(protocol: Int(try Fields(body, "hello", required: ["protocol"]).int("protocol", 1...65535)))
    case "status":
        try requireEmpty(body, "status")
        request = .status
    case "probe":
        let f = try Fields(body, "probe", required: ["addr", "port", "count"])
        request = .probe(addr: try f.string("addr", isDottedQuad), port: Int(try f.int("port", probePortRange)),
                         count: Int(try f.int("count", probeCountRange)))
    case "begin":
        let f = try Fields(body, "begin", required: ["jit_config"])
        guard case .string(let s)? = f.value("jit_config"), let jit = JitConfig(s) else {
            throw ProtoError.invalid("begin.jit_config")
        }
        request = .begin(jitConfig: jit)
    case "verify":
        request = .verify(try VerifyExpect.decode(body))
    default:
        throw ProtoError.invalid("request: unknown operation")
    }
    return AgentRequestFrame(id: id, request: request)
}

/// The `id` of a request the agent refused, when it is readable enough to have one: the agent
/// answers `bad_request` under it, and otherwise closes without a reply.
func readableAgentFrameID(_ line: [UInt8]) -> UInt32? {
    guard let body = try? frameBody(line, cap: maxAgentRequest), case .object(let o)? = try? StrictJSONReader.parse(body),
          case .integer(let n)? = o.values["id"], agentIDRange.contains(n) else { return nil }
    return UInt32(n)
}

// MARK: - Replies

func encodeAgentReply(_ frame: AgentReplyFrame) throws -> [UInt8] {
    guard frame.id >= 1 else { throw ProtoError.invalid("frame id") }
    var w = JSONWriter()
    w.raw("{")
    w.field("id", UInt64(frame.id), first: true)
    w.raw(",")
    w.key("reply")
    w.raw("{")
    switch frame.reply {
    case .hello(let p):
        w.key("hello")
        w.raw("{")
        w.field("protocol", UInt64(p), first: true)
        w.raw("}")
    case .status(let s):
        guard s.isValid else { throw ProtoError.invalid("status") }
        w.key("status")
        w.raw("{")
        w.field("phase", s.phase.rawValue, first: true)
        if let c = s.exitCode { w.field("exit_code", UInt64(c)) }
        if let n = s.signal { w.field("signal", UInt64(n)) }
        if let job = s.job { w.field("job", job.rawValue) }
        if let failed = s.failed {
            w.raw(",")
            w.key("failed")
            writeCheckList(failed, &w)
        }
        w.raw("}")
    case .probed(let sent):
        guard (0...16).contains(sent) else { throw ProtoError.invalid("probed") }
        w.key("probed")
        w.raw("{")
        w.field("sent", UInt64(sent), first: true)
        w.raw("}")
    case .accepted:
        w.key("accepted")
        w.raw("{}")
    case .error(let code):
        w.key("error")
        w.raw("{")
        w.field("code", code.rawValue, first: true)
        w.raw("}")
    }
    w.raw("}}")
    return try w.terminated(cap: maxAgentReply)
}

/// The launcher's decoder for the agent's replies: at most maxAgentReply bytes, closed objects,
/// every value in range. What it refuses is a violation; the error names no guest byte.
func decodeAgentReply(_ line: [UInt8]) throws -> AgentReplyFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line, cap: maxAgentReply)), "frame", required: ["id", "reply"])
    let id = UInt32(try top.int("id", agentIDRange))
    let (op, body) = try singleMember(top.value("reply")!, "reply")
    let reply: AgentReply
    switch op {
    case "hello":
        reply = .hello(protocol: Int(try Fields(body, "hello", required: ["protocol"]).int("protocol", 1...65535)))
    case "status":
        let f = try Fields(body, "status", required: ["phase"], optional: ["exit_code", "signal", "job", "failed"])
        let s = AgentStatus(try f.enumValue("phase", AgentPhase.self),
                            exitCode: f.has("exit_code") ? Int(try f.int("exit_code", 0...255)) : nil,
                            signal: f.has("signal") ? Int(try f.int("signal", 1...31)) : nil,
                            job: f.has("job") ? try f.enumValue("job", RunnerJob.self) : nil,
                            failed: f.has("failed") ? try decodeCheckList(f.array("failed"), "status.failed") : nil)
        guard s.isValid else { throw ProtoError.invalid("status") }
        reply = .status(s)
    case "probed":
        reply = .probed(sent: Int(try Fields(body, "probed", required: ["sent"]).int("sent", 0...16)))
    case "accepted":
        try requireEmpty(body, "accepted")
        reply = .accepted
    case "error":
        reply = .error(try Fields(body, "error", required: ["code"]).enumValue("code", AgentErrorCode.self))
    default:
        throw ProtoError.invalid("reply: unknown operation")
    }
    return AgentReplyFrame(id: id, reply: reply)
}

// MARK: - The hand-off (the agent to `runner-exec`, inside the guest)

/// The largest hand-off frame from the agent to `runner-exec`, its LF included.
let maxHandoffToRunner = maxFrame
/// The largest hand-off frame from `runner-exec` to the agent, its LF included.
let maxHandoffToAgent = 256

/// One of `runner-exec`'s steps before run.sh starts, as it reports it to root. A closed
/// vocabulary: root formats the console line itself, from these words and integers, and never
/// writes a byte `runner-exec` chose (docs/vm-agent-protocol.md, *The hand-off's console lines*).
enum HandoffStep: String, CaseIterable, Sendable {
    /// The JIT config was read from root.
    case configRead = "config_read"
    /// The tools disk's manifest.json was read.
    case manifest
    /// `~/faber-runner` was created.
    case mkdir
    /// The runner tarball was unpacked into it.
    case extract
    /// run.sh was found and spawned.
    case spawn
}

/// How a step ended, and the one integer that says why: a process's exit status or signal, or
/// an errno. At most one of the three.
struct HandoffStepResult: Equatable, Sendable {
    let step: HandoffStep
    let ok: Bool
    var exitCode: Int? = nil
    var signal: Int? = nil
    var errno: Int? = nil

    var isValid: Bool {
        [exitCode, signal, errno].compactMap { $0 }.count <= 1
            && exitCode.map { (0...255).contains($0) } ?? true
            && signal.map { (1...31).contains($0) } ?? true
            && errno.map { (1...255).contains($0) } ?? true
    }
}

/// What `runner-exec` reports on the hand-off connection.
enum HandoffReport: Equatable, Sendable {
    /// One step before run.sh, for the console (never the wire to the host).
    case step(HandoffStepResult)
    /// run.sh was spawned.
    case started
    /// run.sh exited: at most one of the two, as AgentStatus holds them, and whether the runner
    /// was ever handed a job (nil when runner-exec could not tell).
    case exited(exitCode: Int?, signal: Int?, job: RunnerJob? = nil)
    /// The runner could not be started.
    case failed
}

func encodeHandoffConfig(_ jit: JitConfig) throws -> [UInt8] {
    var w = JSONWriter()
    w.raw("{")
    w.field("jit_config", jit.expose(), first: true)
    w.raw("}")
    return try w.terminated(cap: maxHandoffToRunner)
}

func decodeHandoffConfig(_ line: [UInt8]) throws -> JitConfig {
    let f = try Fields(StrictJSONReader.parse(frameBody(line, cap: maxHandoffToRunner)), "handoff", required: ["jit_config"])
    guard case .string(let s)? = f.value("jit_config"), let jit = JitConfig(s) else { throw ProtoError.invalid("handoff.jit_config") }
    return jit
}

func encodeHandoffReport(_ report: HandoffReport) throws -> [UInt8] {
    var w = JSONWriter()
    w.raw("{")
    switch report {
    case .step(let r):
        guard r.isValid else { throw ProtoError.invalid("step") }
        w.key("step")
        w.raw("{")
        w.field("name", r.step.rawValue, first: true)
        w.field("result", r.ok ? "ok" : "failed")
        if let n = r.exitCode { w.field("exit_code", UInt64(n)) }
        if let n = r.signal { w.field("signal", UInt64(n)) }
        if let n = r.errno { w.field("errno", UInt64(n)) }
        w.raw("}")
    case .started:
        w.key("started")
        w.raw("{}")
    case .exited(let code, let signal, let job):
        guard AgentStatus(.exited, exitCode: code, signal: signal, job: job).isValid else { throw ProtoError.invalid("exited") }
        w.key("exited")
        w.raw("{")
        var first = true
        if let code {
            w.field("exit_code", UInt64(code), first: first)
            first = false
        }
        if let signal {
            w.field("signal", UInt64(signal), first: first)
            first = false
        }
        if let job { w.field("job", job.rawValue, first: first) }
        w.raw("}")
    case .failed:
        w.key("failed")
        w.raw("{}")
    }
    w.raw("}")
    return try w.terminated(cap: maxHandoffToAgent)
}

func decodeHandoffReport(_ line: [UInt8]) throws -> HandoffReport {
    let (op, body) = try singleMember(StrictJSONReader.parse(frameBody(line, cap: maxHandoffToAgent)), "report")
    switch op {
    case "step":
        let f = try Fields(body, "step", required: ["name", "result"], optional: ["exit_code", "signal", "errno"])
        let result = try f.string("result") { $0 == "ok" || $0 == "failed" }
        let r = HandoffStepResult(step: try f.enumValue("name", HandoffStep.self), ok: result == "ok",
                                  exitCode: f.has("exit_code") ? Int(try f.int("exit_code", 0...255)) : nil,
                                  signal: f.has("signal") ? Int(try f.int("signal", 1...31)) : nil,
                                  errno: f.has("errno") ? Int(try f.int("errno", 1...255)) : nil)
        guard r.isValid else { throw ProtoError.invalid("step") }
        return .step(r)
    case "started":
        try requireEmpty(body, "started")
        return .started
    case "exited":
        let f = try Fields(body, "exited", required: [], optional: ["exit_code", "signal", "job"])
        let code = f.has("exit_code") ? Int(try f.int("exit_code", 0...255)) : nil
        let signal = f.has("signal") ? Int(try f.int("signal", 1...31)) : nil
        let job = f.has("job") ? try f.enumValue("job", RunnerJob.self) : nil
        guard code == nil || signal == nil else { throw ProtoError.invalid("exited") }
        return .exited(exitCode: code, signal: signal, job: job)
    case "failed":
        try requireEmpty(body, "failed")
        return .failed
    default:
        throw ProtoError.invalid("report: unknown operation")
    }
}

// MARK: - Deadline I/O

/// Milliseconds left until `deadline`, at least 0.
func millisecondsLeft(_ deadline: Date) -> Int32 {
    Int32(max(0, min(Double(Int32.max), deadline.timeIntervalSinceNow * 1000)))
}

/// Why a deadline read or write stopped.
enum DeadlineIOError: Error, Equatable {
    case timeout
    case closed
    case tooLarge
    case notOneLine
    case io
}

/// Wait for `events` on `fd` until `deadline`; true when ready, false on timeout.
func waitFor(_ fd: Int32, _ events: Int16, until deadline: Date) throws -> Bool {
    while true {
        var p = pollfd(fd: fd, events: events, revents: 0)
        let n = poll(&p, 1, millisecondsLeft(deadline))
        if n < 0 {
            if errno == EINTR { continue }
            throw DeadlineIOError.io
        }
        if n == 0 { return false }
        if p.revents & Int16(POLLNVAL) != 0 { throw DeadlineIOError.io }
        return true
    }
}

/// Write every byte before `deadline` to a non-blocking descriptor.
func writeAll(_ fd: Int32, _ bytes: [UInt8], until deadline: Date) throws {
    var done = 0
    while done < bytes.count {
        guard try waitFor(fd, Int16(POLLOUT), until: deadline) else { throw DeadlineIOError.timeout }
        let n = bytes[done...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        if n < 0 {
            if errno == EINTR || errno == EAGAIN { continue }
            if errno == EPIPE || errno == ECONNRESET { throw DeadlineIOError.closed }
            throw DeadlineIOError.io
        }
        done += n
    }
}

/// Read one LF-terminated frame of at most `cap` bytes before `deadline` from a non-blocking
/// descriptor. It never asks for more than the cap leaves room for, and bytes after the LF are
/// a second frame sent unasked: lockstep is broken.
func readFrame(_ fd: Int32, cap: Int, until deadline: Date) throws -> [UInt8] {
    var buffer: [UInt8] = []
    var chunk = [UInt8](repeating: 0, count: cap)
    while true {
        let room = cap - buffer.count
        guard room > 0 else { throw DeadlineIOError.tooLarge }
        guard try waitFor(fd, Int16(POLLIN), until: deadline) else { throw DeadlineIOError.timeout }
        let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, room) }
        if n < 0 {
            if errno == EINTR || errno == EAGAIN { continue }
            if errno == ECONNRESET { throw DeadlineIOError.closed }
            throw DeadlineIOError.io
        }
        if n == 0 { throw DeadlineIOError.closed }
        buffer.append(contentsOf: chunk[0..<n])
        if let lf = buffer.firstIndex(of: 0x0A) {
            guard lf == buffer.count - 1 else { throw DeadlineIOError.notOneLine }
            return buffer
        }
    }
}

/// Reads LF-terminated frames from a stream the peer writes without waiting to be asked, such as
/// `runner-exec`'s hand-off reports: several frames can arrive in one read, so what follows the
/// first LF is kept for the next call, not refused as `readFrame` does for a lockstep peer.
struct StreamFrameReader {
    private var buffer: [UInt8] = []

    /// The next frame, LF included, of at most `cap` bytes, before `deadline`.
    mutating func next(_ fd: Int32, cap: Int, until deadline: Date) throws -> [UInt8] {
        var chunk = [UInt8](repeating: 0, count: cap)
        while true {
            if let lf = buffer.firstIndex(of: 0x0A) {
                guard lf < cap else { throw DeadlineIOError.tooLarge }
                let frame = Array(buffer[0...lf])
                buffer.removeSubrange(0...lf)
                return frame
            }
            let room = cap - buffer.count
            guard room > 0 else { throw DeadlineIOError.tooLarge }
            guard try waitFor(fd, Int16(POLLIN), until: deadline) else { throw DeadlineIOError.timeout }
            let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, room) }
            if n < 0 {
                if errno == EINTR || errno == EAGAIN { continue }
                if errno == ECONNRESET { throw DeadlineIOError.closed }
                throw DeadlineIOError.io
            }
            if n == 0 { throw DeadlineIOError.closed }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }
}

/// Whether the peer sent anything (or closed) while nothing was asked: nil when quiet,
/// otherwise `.closed` for an EOF or `.notOneLine` for unasked bytes.
func unaskedInput(_ fd: Int32) -> DeadlineIOError? {
    var p = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
    guard poll(&p, 1, 0) > 0 else { return nil }
    var byte: UInt8 = 0
    let n = recv(fd, &byte, 1, MSG_PEEK | MSG_DONTWAIT)
    if n > 0 { return .notOneLine }
    if n == 0 { return .closed }
    switch errno {
    case EAGAIN, EINTR: return nil
    case ENOTSOCK: return p.revents & Int16(POLLHUP) != 0 ? .closed : .notOneLine
    default: return .closed
    }
}

func setNonBlocking(_ fd: Int32) {
    let flags = fcntl(fd, F_GETFL)
    if flags >= 0 { _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK) }
}
