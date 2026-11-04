// The supervisor's side of docs/vm-supervisor-protocol.md: it decodes faber's requests and
// encodes its replies. The same framing and strictness carry the supervisor-to-child protocol
// (README.md, "The child protocol"), the claim markers and the network plan.
//
// The strict reader, the writer, the frame check and JitConfig are in Shared/Codec.swift,
// which the guest agent shares.

import Darwin
import Foundation

/// The one faber-supervisor protocol version this build speaks. There is no negotiation.
/// Version 3 added the stop reason `runner_exited_without_job`; version 4 added `start.disk_bytes`
/// and a Linux record's `disk_bytes` / `disk_allocated_bytes` (ADR-0061 §4).
let supervisorProtocol = 4
/// The most claims or records any list carries.
let maxList = 16
/// The latest timestamp a record carries, in seconds: the year 2100.
let maxUnix: UInt64 = 4_102_444_800
/// The largest size `start.disk_bytes` may grow a Linux disk to: 1 TiB, this supervisor's own
/// bound whatever faber asks (ADR-0061 §4).
let maxDiskBytes: UInt64 = 1 << 40
/// `start.disk_bytes` is a whole number of MiB.
let diskBytesAlign: UInt64 = 1 << 20
/// The largest disk size a record reports: twice maxDiskBytes, room for a filesystem's rounding.
let maxDiskReport: UInt64 = 2 << 40

/// faber's claim id: first `[A-Za-z0-9]`, then `[A-Za-z0-9._-]`, 1 to 64 bytes.
func isClaim(_ s: String) -> Bool {
    matches(s, max: 64, first: isAlnum) { isAlnum($0) || $0 == 0x2E || $0 == 0x5F || $0 == 0x2D }
}

/// A name in the image store or the tools-disk cache, never a path: first `[a-z0-9]`, then
/// `[a-z0-9._-]`, 1 to 64 bytes. No `/` and no leading `.`, so it cannot leave its directory.
func isStoreName(_ s: String) -> Bool {
    let first = { (c: UInt8) in (c >= 0x30 && c <= 0x39) || (c >= 0x61 && c <= 0x7A) }
    return matches(s, max: 64, first: first) { first($0) || $0 == 0x2E || $0 == 0x5F || $0 == 0x2D }
}

// MARK: - The closed sets

enum VmKind: String, Sendable {
    case macos
    case linux
}

enum VmState: String, CaseIterable, Sendable {
    case starting
    case booting
    case ready
    case probeSent = "probe_sent"
    case inService = "in_service"
    case jobFinished = "job_finished"
    /// `verify` was sent: the guest agent runs the image's checks (ADR-0049 §7).
    case verifying
    /// The checks are done; the record carries `failed`.
    case verified
    case stopping
    case stopped
    case unmanaged
}

enum StopReason: String, CaseIterable, Sendable {
    case requested
    case guestShutdown = "guest_shutdown"
    case startFailed = "start_failed"
    case wallViolation = "wall_violation"
    case agentError = "agent_error"
    case launcherExited = "launcher_exited"
    /// The JIT runner exited without ever being handed a job (the child's exit 6).
    case runnerExitedWithoutJob = "runner_exited_without_job"
}

enum ErrorCode: String, CaseIterable, Sendable {
    case versionMismatch = "version_mismatch"
    case handshakeRequired = "handshake_required"
    case badRequest = "bad_request"
    case slotBusy = "slot_busy"
    case claimExists = "claim_exists"
    case unknownClaim = "unknown_claim"
    case wrongState = "wrong_state"
    case unmanaged
    case imageNotFound = "image_not_found"
    case toolsNotFound = "tools_not_found"
    case capacity
    case startFailed = "start_failed"
    case `internal`
}

// MARK: - Requests (faber to the supervisor)

struct StartRequest: Equatable, Sendable {
    var slot: Int
    var kind: VmKind
    var claim: String
    var image: String
    /// Required for macOS, forbidden for Linux.
    var tools: String?
    var cpus: Int
    var memoryMiB: Int
    /// Linux only, optional (v4): grow the stopped VM's disk file to this size before it boots.
    var diskBytes: UInt64? = nil
}

enum Request: Sendable {
    case hello(protocol: Int)
    case start(StartRequest)
    case probe(claim: String)
    /// `jitConfig` is required for macOS and forbidden for Linux, which the supervisor checks
    /// against the VM, since the request does not say which kind the claim is.
    case begin(claim: String, jitConfig: JitConfig?)
    /// Run the golden image's checks in a proven macOS VM instead of a job (ADR-0049 §7).
    case verify(claim: String, expect: VerifyExpect)
    case stop(claim: String)
    case list
    case adopt(claims: [String])
    case exit
}

struct RequestFrame: Sendable {
    var id: UInt32
    var request: Request
}

private let frameIDRange: ClosedRange<UInt64> = 1...UInt64(UInt32.max)

func decodeRequest(_ line: [UInt8]) throws -> RequestFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line)), "frame", required: ["id", "request"])
    let id = UInt32(try top.int("id", frameIDRange))
    let (op, body) = try singleMember(top.value("request")!, "request")
    let request: Request
    switch op {
    case "hello":
        let f = try Fields(body, "hello", required: ["protocol"])
        request = .hello(protocol: Int(try f.int("protocol", 1...65535)))
    case "start":
        let f = try Fields(body, "start", required: ["slot", "kind", "claim", "image", "cpus", "memory_mib"],
                           optional: ["tools", "disk_bytes"])
        let start = StartRequest(
            slot: Int(try f.int("slot", 0...15)),
            kind: try f.enumValue("kind", VmKind.self),
            claim: try f.string("claim", isClaim),
            image: try f.string("image", isStoreName),
            tools: try f.optionalString("tools", isStoreName),
            cpus: Int(try f.int("cpus", 1...64)),
            memoryMiB: Int(try f.int("memory_mib", 1024...262_144)),
            diskBytes: f.has("disk_bytes") ? try f.int("disk_bytes", diskBytesAlign...maxDiskBytes) : nil)
        guard (start.kind == .macos) == (start.tools != nil) else { throw ProtoError.invalid("start: tools") }
        if let d = start.diskBytes {
            guard start.kind == .linux else { throw ProtoError.invalid("start: disk_bytes on macos") }
            guard d % diskBytesAlign == 0 else { throw ProtoError.invalid("start: disk_bytes not whole MiB") }
        }
        request = .start(start)
    case "probe":
        request = .probe(claim: try Fields(body, "probe", required: ["claim"]).string("claim", isClaim))
    case "begin":
        let f = try Fields(body, "begin", required: ["claim"], optional: ["jit_config"])
        var jit: JitConfig?
        if f.has("jit_config") {
            guard case .string(let s)? = f.value("jit_config"), let j = JitConfig(s) else {
                throw ProtoError.invalid("begin.jit_config")
            }
            jit = j
        }
        request = .begin(claim: try f.string("claim", isClaim), jitConfig: jit)
    case "verify":
        let f = try Fields(body, "verify", required: ["claim", "expect"])
        request = .verify(claim: try f.string("claim", isClaim), expect: try VerifyExpect.decode(f.value("expect")!))
    case "stop":
        request = .stop(claim: try Fields(body, "stop", required: ["claim"]).string("claim", isClaim))
    case "list":
        try requireEmpty(body, "list")
        request = .list
    case "adopt":
        let items = try Fields(body, "adopt", required: ["claims"]).array("claims")
        guard (1...maxList).contains(items.count) else { throw ProtoError.invalid("adopt: claims count") }
        var claims: [String] = []
        for item in items {
            guard case .string(let s) = item, isClaim(s) else { throw ProtoError.invalid("adopt.claims") }
            guard !claims.contains(s) else { throw ProtoError.invalid("adopt: duplicate claim") }
            claims.append(s)
        }
        request = .adopt(claims: claims)
    case "exit":
        try requireEmpty(body, "exit")
        request = .exit
    default:
        throw ProtoError.invalid("request: unknown operation")
    }
    return RequestFrame(id: id, request: request)
}

/// The `id` of a frame the decoder refused, when the frame is still readable enough to have
/// one: the supervisor then answers `bad_request` under it, and otherwise closes without a
/// reply (docs/vm-supervisor-protocol.md, Transport).
func readableFrameID(_ line: [UInt8]) -> UInt32? {
    guard let body = try? frameBody(line), case .object(let o)? = try? StrictJSONReader.parse(body),
          case .integer(let n)? = o.values["id"], frameIDRange.contains(n) else { return nil }
    return UInt32(n)
}

// MARK: - Replies (the supervisor to faber)

struct VmRecord: Equatable, Sendable {
    var slot: Int
    var kind: VmKind
    var claim: String
    var state: VmState
    /// Present exactly when `state` is `stopped`.
    var stopReason: StopReason?
    /// Present exactly when `state` is `verified`, and only for macOS: the failed checks' codes.
    var failed: [CheckCode]? = nil
    var startedUnix: UInt64
    /// A Linux VM's disk file (v4): its length and the bytes it holds. Both or neither, never on
    /// macOS; nil when the supervisor cannot tell which file the VM boots from.
    var diskBytes: UInt64? = nil
    var diskAllocatedBytes: UInt64? = nil

    init(slot: Int, kind: VmKind, claim: String, state: VmState, stopReason: StopReason?,
         failed: [CheckCode]? = nil, startedUnix: UInt64, disk: (bytes: UInt64, allocated: UInt64)? = nil) {
        self.slot = slot
        self.kind = kind
        self.claim = claim
        self.state = state
        self.stopReason = stopReason
        self.failed = failed
        self.startedUnix = startedUnix
        diskBytes = disk?.bytes
        diskAllocatedBytes = disk?.allocated
    }
}

enum Reply: Sendable {
    case hello(protocol: Int, supervisorVersion: String)
    case vm(VmRecord)
    case vms([VmRecord])
    case adopted(found: [VmRecord], unknown: [String])
    case done
    /// `protocol` is present for `version_mismatch` and for no other code.
    case error(ErrorCode, protocol: Int? = nil)
}

struct ResponseFrame: Sendable {
    var id: UInt32
    var reply: Reply
}

extension VmRecord {
    func validate() throws {
        guard (0...15).contains(slot), isClaim(claim), startedUnix <= maxUnix else { throw ProtoError.invalid("vm") }
        guard (state == .stopped) == (stopReason != nil) else { throw ProtoError.invalid("vm: stop_reason") }
        guard (state == .verified) == (failed != nil) else { throw ProtoError.invalid("vm: failed") }
        if let failed { guard kind == .macos, isCheckList(failed) else { throw ProtoError.invalid("vm: failed") } }
        guard (diskBytes == nil) == (diskAllocatedBytes == nil) else { throw ProtoError.invalid("vm: disk fields") }
        if let diskBytes, let diskAllocatedBytes {
            guard kind == .linux, diskBytes <= maxDiskReport, diskAllocatedBytes <= maxDiskReport else {
                throw ProtoError.invalid("vm: disk")
            }
        }
    }

    func write(_ w: inout JSONWriter) {
        w.raw("{")
        w.field("slot", UInt64(slot), first: true)
        w.field("kind", kind.rawValue)
        w.field("claim", claim)
        w.field("state", state.rawValue)
        if let stopReason { w.field("stop_reason", stopReason.rawValue) }
        if let failed {
            w.raw(",")
            w.key("failed")
            writeCheckList(failed, &w)
        }
        w.field("started_unix", startedUnix)
        if let diskBytes, let diskAllocatedBytes {
            w.field("disk_bytes", diskBytes)
            w.field("disk_allocated_bytes", diskAllocatedBytes)
        }
        w.raw("}")
    }
}

extension Reply {
    func validate() throws {
        switch self {
        case .hello(let p, let v):
            guard (1...65535).contains(p), isSupervisorVersion(v) else { throw ProtoError.invalid("hello") }
        case .vm(let r):
            try r.validate()
        case .vms(let list):
            guard list.count <= maxList else { throw ProtoError.invalid("vms: count") }
            try list.forEach { try $0.validate() }
            guard zip(list, list.dropFirst()).allSatisfy({ $0.slot < $1.slot }) else {
                throw ProtoError.invalid("vms: slot order")
            }
            guard Set(list.map(\.claim)).count == list.count else { throw ProtoError.invalid("vms: duplicate claim") }
        case .adopted(let found, let unknown):
            guard found.count <= maxList, unknown.count <= maxList else { throw ProtoError.invalid("adopted: count") }
            try found.forEach { try $0.validate() }
            guard unknown.allSatisfy(isClaim) else { throw ProtoError.invalid("adopted.unknown") }
            guard Set(found.map(\.slot)).count == found.count else { throw ProtoError.invalid("adopted: duplicate slot") }
            let claims = found.map(\.claim) + unknown
            guard Set(claims).count == claims.count else { throw ProtoError.invalid("adopted: duplicate claim") }
        case .done:
            break
        case .error(let code, let p):
            guard (code == .versionMismatch) == (p != nil), p.map({ (1...65535).contains($0) }) ?? true else {
                throw ProtoError.invalid("error: protocol")
            }
        }
    }
}

func encodeResponse(_ frame: ResponseFrame) throws -> [UInt8] {
    guard frame.id >= 1 else { throw ProtoError.invalid("frame id") }
    try frame.reply.validate()
    var w = JSONWriter()
    w.raw("{")
    w.field("id", UInt64(frame.id), first: true)
    w.raw(",")
    w.key("reply")
    w.raw("{")
    switch frame.reply {
    case .hello(let p, let v):
        w.key("hello")
        w.raw("{")
        w.field("protocol", UInt64(p), first: true)
        w.field("supervisor_version", v)
        w.raw("}")
    case .vm(let r):
        w.key("vm")
        r.write(&w)
    case .vms(let list):
        w.key("vms")
        w.raw("{")
        w.key("vms")
        w.raw("[")
        for (i, r) in list.enumerated() {
            if i > 0 { w.raw(",") }
            r.write(&w)
        }
        w.raw("]}")
    case .adopted(let found, let unknown):
        w.key("adopted")
        w.raw("{")
        w.key("found")
        w.raw("[")
        for (i, r) in found.enumerated() {
            if i > 0 { w.raw(",") }
            r.write(&w)
        }
        w.raw("],")
        w.key("unknown")
        w.raw("[")
        for (i, c) in unknown.enumerated() {
            if i > 0 { w.raw(",") }
            w.string(c)
        }
        w.raw("]}")
    case .done:
        w.key("done")
        w.raw("{}")
    case .error(let code, let p):
        w.key("error")
        w.raw("{")
        w.field("code", code.rawValue, first: true)
        if let p { w.field("protocol", UInt64(p)) }
        w.raw("}")
    }
    w.raw("}}")
    return try w.terminated()
}

// MARK: - The child protocol (the supervisor to one launcher child)

/// The supervisor-to-child protocol this build's `run --serve-socket` speaks, and the one
/// version the supervisor records in a marker it writes. A supervisor must keep speaking
/// every version a supported older child could have been started with (the versioning rule).
let childProtocol = launcherProtocol
/// Every child protocol version this supervisor can adopt a child at.
///
/// Version 2 adds `verify`, `verifying`, `verified` and `failed` (ADR-0049 §7) and nothing
/// else, so a version-1 child speaks a subset: the supervisor never sends it `verify`.
let supportedChildProtocols: Set<Int> = [1, 2]

enum ChildRequest: Sendable {
    case hello(childProtocol: Int)
    case status
    case probe
    case begin(jitConfig: JitConfig?)
    /// Child protocol 2 and later.
    case verify(VerifyExpect)
    case stop
}

enum ChildErrorCode: String, CaseIterable, Sendable {
    case versionMismatch = "version_mismatch"
    case handshakeRequired = "handshake_required"
    case badRequest = "bad_request"
    case wrongState = "wrong_state"
}

enum ChildReply: Equatable, Sendable {
    case hello(childProtocol: Int, slot: Int, claim: String)
    /// `failed` exactly with `verified`.
    case status(state: VmState, stopReason: StopReason?, failed: [CheckCode]? = nil)
    case done
    case error(ChildErrorCode, childProtocol: Int? = nil)
}

struct ChildRequestFrame: Sendable {
    var id: UInt32
    var request: ChildRequest
}

struct ChildResponseFrame: Equatable, Sendable {
    var id: UInt32
    var reply: ChildReply
}

/// A child never reports `unmanaged`: that is the supervisor's word for a child it cannot
/// speak to.
private func isChildState(_ s: VmState) -> Bool { s != .unmanaged }

func encodeChildRequest(_ frame: ChildRequestFrame) throws -> [UInt8] {
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
        w.field("child_protocol", UInt64(p), first: true)
        w.raw("}")
    case .status:
        w.key("status")
        w.raw("{}")
    case .probe:
        w.key("probe")
        w.raw("{}")
    case .begin(let jit):
        w.key("begin")
        w.raw("{")
        if let jit { w.field("jit_config", jit.expose(), first: true) }
        w.raw("}")
    case .verify(let expect):
        w.key("verify")
        try expect.write(&w)
    case .stop:
        w.key("stop")
        w.raw("{}")
    }
    w.raw("}}")
    return try w.terminated()
}

func decodeChildRequest(_ line: [UInt8]) throws -> ChildRequestFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line)), "frame", required: ["id", "request"])
    let id = UInt32(try top.int("id", frameIDRange))
    let (op, body) = try singleMember(top.value("request")!, "request")
    let request: ChildRequest
    switch op {
    case "hello":
        request = .hello(childProtocol: Int(try Fields(body, "hello", required: ["child_protocol"]).int("child_protocol", 1...65535)))
    case "status":
        try requireEmpty(body, "status")
        request = .status
    case "probe":
        try requireEmpty(body, "probe")
        request = .probe
    case "begin":
        let f = try Fields(body, "begin", required: [], optional: ["jit_config"])
        var jit: JitConfig?
        if f.has("jit_config") {
            guard case .string(let s)? = f.value("jit_config"), let j = JitConfig(s) else {
                throw ProtoError.invalid("begin.jit_config")
            }
            jit = j
        }
        request = .begin(jitConfig: jit)
    case "verify":
        request = .verify(try VerifyExpect.decode(body))
    case "stop":
        try requireEmpty(body, "stop")
        request = .stop
    default:
        throw ProtoError.invalid("request: unknown operation")
    }
    return ChildRequestFrame(id: id, request: request)
}

func encodeChildResponse(_ frame: ChildResponseFrame) throws -> [UInt8] {
    var w = JSONWriter()
    w.raw("{")
    w.field("id", UInt64(frame.id), first: true)
    w.raw(",")
    w.key("reply")
    w.raw("{")
    switch frame.reply {
    case .hello(let p, let slot, let claim):
        guard (0...15).contains(slot), isClaim(claim) else { throw ProtoError.invalid("hello") }
        w.key("hello")
        w.raw("{")
        w.field("child_protocol", UInt64(p), first: true)
        w.field("slot", UInt64(slot))
        w.field("claim", claim)
        w.raw("}")
    case .status(let state, let reason, let failed):
        guard isChildState(state), (state == .stopped) == (reason != nil), (state == .verified) == (failed != nil),
              failed.map(isCheckList) ?? true else { throw ProtoError.invalid("status") }
        w.key("status")
        w.raw("{")
        w.field("state", state.rawValue, first: true)
        if let reason { w.field("stop_reason", reason.rawValue) }
        if let failed {
            w.raw(",")
            w.key("failed")
            writeCheckList(failed, &w)
        }
        w.raw("}")
    case .done:
        w.key("done")
        w.raw("{}")
    case .error(let code, let p):
        guard (code == .versionMismatch) == (p != nil) else { throw ProtoError.invalid("error") }
        w.key("error")
        w.raw("{")
        w.field("code", code.rawValue, first: true)
        if let p { w.field("child_protocol", UInt64(p)) }
        w.raw("}")
    }
    w.raw("}}")
    return try w.terminated()
}

func decodeChildResponse(_ line: [UInt8]) throws -> ChildResponseFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line)), "frame", required: ["id", "reply"])
    let id = UInt32(try top.int("id", frameIDRange))
    let (op, body) = try singleMember(top.value("reply")!, "reply")
    let reply: ChildReply
    switch op {
    case "hello":
        let f = try Fields(body, "hello", required: ["child_protocol", "slot", "claim"])
        reply = .hello(childProtocol: Int(try f.int("child_protocol", 1...65535)),
                       slot: Int(try f.int("slot", 0...15)), claim: try f.string("claim", isClaim))
    case "status":
        let f = try Fields(body, "status", required: ["state"], optional: ["stop_reason", "failed"])
        let state = try f.enumValue("state", VmState.self)
        let reason = f.has("stop_reason") ? try f.enumValue("stop_reason", StopReason.self) : nil
        let failed = f.has("failed") ? try decodeCheckList(f.array("failed"), "status.failed") : nil
        guard isChildState(state), (state == .stopped) == (reason != nil), (state == .verified) == (failed != nil) else {
            throw ProtoError.invalid("status")
        }
        reply = .status(state: state, stopReason: reason, failed: failed)
    case "done":
        try requireEmpty(body, "done")
        reply = .done
    case "error":
        let f = try Fields(body, "error", required: ["code"], optional: ["child_protocol"])
        let code = try f.enumValue("code", ChildErrorCode.self)
        let p = f.has("child_protocol") ? Int(try f.int("child_protocol", 1...65535)) : nil
        guard (code == .versionMismatch) == (p != nil) else { throw ProtoError.invalid("error") }
        reply = .error(code, childProtocol: p)
    default:
        throw ProtoError.invalid("reply: unknown operation")
    }
    return ChildResponseFrame(id: id, reply: reply)
}

// MARK: - Claim markers

/// The supervisor's record of which claim runs in which launcher child: `slot-<N>.claim` in
/// the state directory (docs/vm-supervisor-protocol.md, Claim markers). Only the supervisor
/// reads one.
struct ClaimMarker: Equatable, Sendable {
    static let literal = "org.faber.kind=vm-claim"
    /// The bytes a file must contain to be a marker at all. A file without them is left alone.
    static let signature = "\"marker\":\"\(literal)\""

    var childProtocol: Int
    var slot: Int
    var kind: VmKind
    var claim: String
    /// 0 until the child is spawned.
    var pid: Int32
    var pidStartedUs: UInt64
    var startedUnix: UInt64
    /// The clone's directory name inside the clone directory: macOS only.
    var bundle: String?

    func encode() throws -> [UInt8] {
        guard (0...15).contains(slot), isClaim(claim), (kind == .macos) == (bundle != nil),
              bundle.map(isStoreName) ?? true else { throw ProtoError.invalid("marker") }
        var w = JSONWriter()
        w.raw("{")
        w.field("marker", Self.literal, first: true)
        w.field("child_protocol", UInt64(childProtocol))
        w.field("slot", UInt64(slot))
        w.field("kind", kind.rawValue)
        w.field("claim", claim)
        w.field("pid", UInt64(pid))
        w.field("pid_started_us", pidStartedUs)
        w.field("started_unix", startedUnix)
        if let bundle { w.field("bundle", bundle) }
        w.raw("}")
        return try w.terminated()
    }

    static func decode(_ bytes: [UInt8]) throws -> ClaimMarker {
        let f = try Fields(StrictJSONReader.parse(frameBody(bytes)), "marker",
                           required: ["marker", "child_protocol", "slot", "kind", "claim", "pid",
                                      "pid_started_us", "started_unix"],
                           optional: ["bundle"])
        _ = try f.string("marker") { $0 == literal }
        let m = ClaimMarker(
            childProtocol: Int(try f.int("child_protocol", 1...65535)),
            slot: Int(try f.int("slot", 0...15)),
            kind: try f.enumValue("kind", VmKind.self),
            claim: try f.string("claim", isClaim),
            pid: Int32(try f.int("pid", 0...UInt64(Int32.max))),
            pidStartedUs: try f.int("pid_started_us", 0...maxUnix * 1_000_000),
            startedUnix: try f.int("started_unix", 0...maxUnix),
            bundle: try f.optionalString("bundle", isStoreName))
        guard (m.kind == .macos) == (m.bundle != nil) else { throw ProtoError.invalid("marker.bundle") }
        return m
    }
}

// MARK: - The network plan

/// The guest addresses setup planned: `/etc/faber/wall/plan.json`, src/wall/model.rs's
/// `NetworkPlan`. Read strictly, so a field this release does not know is refused.
struct NetworkPlan: Equatable, Sendable {
    struct SlotAddress: Equatable, Sendable {
        let id: Int
        let addr: IPv4
    }

    struct Network: Equatable, Sendable {
        let id: Int
        let gateway: IPv4
        let subnet: IPv4Subnet
        let slots: [SlotAddress]
    }

    let networks: [Network]

    static let maxBytes = 1 << 20

    static func parse(_ bytes: [UInt8]) throws -> NetworkPlan {
        guard bytes.count <= maxBytes else { throw Failure("network plan: larger than \(maxBytes) bytes") }
        do {
            let top = try Fields(StrictJSONReader.parse(bytes), "plan", required: ["networks"])
            var networks: [Network] = []
            for n in try top.array("networks") {
                let f = try Fields(n, "network", required: ["id", "gateway", "subnet", "slots"])
                guard let gateway = IPv4.parse(try f.string("gateway") { !$0.isEmpty }) else {
                    throw ProtoError.invalid("network.gateway")
                }
                guard let subnet = try? IPv4Subnet.parse(try f.string("subnet") { !$0.isEmpty }) else {
                    throw ProtoError.invalid("network.subnet")
                }
                var slots: [SlotAddress] = []
                for s in try f.array("slots") {
                    let sf = try Fields(s, "slot", required: ["id", "addr"], optional: ["profiles"])
                    guard let addr = IPv4.parse(try sf.string("addr") { !$0.isEmpty }) else {
                        throw ProtoError.invalid("slot.addr")
                    }
                    if sf.has("profiles") {
                        for p in try sf.array("profiles") {
                            guard case .string = p else { throw ProtoError.invalid("slot.profiles") }
                        }
                    }
                    slots.append(SlotAddress(id: Int(try sf.int("id", 0...255)), addr: addr))
                }
                networks.append(Network(id: Int(try f.int("id", 0...255)), gateway: gateway, subnet: subnet, slots: slots))
            }
            return NetworkPlan(networks: networks)
        } catch let error as ProtoError {
            throw Failure("network plan: \(error)")
        }
    }

    /// Protocol slot N's subnet and reserved address: the plan's one VmSlot whose id is N. The
    /// launcher's vmnet gateway is always the subnet's second address, so a plan that says
    /// otherwise cannot be honoured.
    func address(slot: Int) throws -> VmnetPlan {
        let found = networks.flatMap { n in n.slots.filter { $0.id == slot }.map { (n, $0) } }
        guard found.count == 1, let (network, entry) = found.first else {
            throw Failure("network plan: slot \(slot) is \(found.isEmpty ? "not in the plan" : "in the plan more than once")")
        }
        guard network.gateway == network.subnet.gateway else {
            throw Failure("network plan: network \(network.id)'s gateway \(network.gateway) is not \(network.subnet)'s second address, which vmnet uses")
        }
        return try VmnetPlan(subnet: network.subnet, reservation: entry.addr)
    }
}
