// The protocol codec against the golden fixtures shared with the Rust side
// (tests/fixtures/vm-proto/), plus the edges src/vm/proto.rs's tests pin.

import Darwin
import Foundation

/// A response decoder for the tests only: the supervisor never decodes a response. It is as
/// strict as decodeRequest, so the fixtures prove the encoder's output is the same JSON value.
func decodeResponseForTest(_ line: [UInt8]) throws -> ResponseFrame {
    let top = try Fields(StrictJSONReader.parse(frameBody(line)), "frame", required: ["id", "reply"])
    let id = UInt32(try top.int("id", 1...UInt64(UInt32.max)))
    let (op, body) = try singleMember(top.value("reply")!, "reply")
    func record(_ v: JSON) throws -> VmRecord {
        let f = try Fields(v, "vm", required: ["slot", "kind", "claim", "state", "started_unix"], optional: ["stop_reason", "failed", "disk_bytes", "disk_allocated_bytes"])
        return VmRecord(slot: Int(try f.int("slot", 0...15)), kind: try f.enumValue("kind", VmKind.self),
                        claim: try f.string("claim", isClaim), state: try f.enumValue("state", VmState.self),
                        stopReason: f.has("stop_reason") ? try f.enumValue("stop_reason", StopReason.self) : nil,
                        failed: f.has("failed") ? try decodeCheckList(f.array("failed"), "vm.failed") : nil,
                        startedUnix: try f.int("started_unix", 0...maxUnix),
                        disk: f.has("disk_bytes") || f.has("disk_allocated_bytes")
                            ? (try f.int("disk_bytes", 0...UInt64.max), try f.int("disk_allocated_bytes", 0...UInt64.max))
                            : nil)
    }
    let reply: Reply
    switch op {
    case "hello":
        let f = try Fields(body, "hello", required: ["protocol", "supervisor_version"])
        reply = .hello(protocol: Int(try f.int("protocol", 1...65535)),
                       supervisorVersion: try f.string("supervisor_version", isSupervisorVersion))
    case "vm":
        reply = .vm(try record(body))
    case "vms":
        reply = .vms(try Fields(body, "vms", required: ["vms"]).array("vms").map(record))
    case "adopted":
        let f = try Fields(body, "adopted", required: ["found", "unknown"])
        let unknown = try f.array("unknown").map { v -> String in
            guard case .string(let s) = v, isClaim(s) else { throw ProtoError.invalid("unknown") }
            return s
        }
        reply = .adopted(found: try f.array("found").map(record), unknown: unknown)
    case "done":
        try requireEmpty(body, "done")
        reply = .done
    case "error":
        let f = try Fields(body, "error", required: ["code"], optional: ["protocol"])
        reply = .error(try f.enumValue("code", ErrorCode.self),
                       protocol: f.has("protocol") ? Int(try f.int("protocol", 1...65535)) : nil)
    default:
        throw ProtoError.invalid("reply")
    }
    try reply.validate()
    return ResponseFrame(id: id, reply: reply)
}

/// The handshake verdict faber applies: a hello at exactly our version.
private func passesHello(_ reply: Reply) -> Bool {
    if case .hello(let p, _) = reply { return p == supervisorProtocol }
    return false
}

private func bytes(_ s: String) -> [UInt8] { Array(s.utf8) }

@MainActor
func testProto(_ t: Harness, fixtures: String?) {
    // The fixtures must be there: a missing directory fails, never skips.
    guard let fixtures, isDirectory(URL(fileURLWithPath: fixtures)) else {
        t.check("fixture directory given and present (test.sh passes tests/fixtures/vm-proto)", false, fixtures ?? "none")
        return
    }
    func files(_ sub: String) -> [(String, [UInt8])] {
        let dir = URL(fileURLWithPath: fixtures).appendingPathComponent(sub)
        let names = ((try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? [])
            .filter { $0.hasSuffix(".json") }.sorted()
        t.check("fixtures in \(sub)", !names.isEmpty, dir.path)
        return names.compactMap { name in
            (try? Data(contentsOf: dir.appendingPathComponent(name))).map { (name, Array($0)) }
        }
    }

    for (name, line) in files("requests/valid") {
        t.accepts("valid request \(name) decodes") { _ = try decodeRequest(line) }
    }
    for (name, line) in files("requests/invalid") {
        t.check("invalid request \(name) is refused", (try? decodeRequest(line)) == nil)
    }
    for (name, line) in files("responses/valid") {
        do {
            let frame = try decodeResponseForTest(line)
            let encoded = try encodeResponse(frame)
            t.check("valid response \(name) re-encodes byte for byte", encoded == line,
                    String(decoding: encoded, as: UTF8.self))
        } catch {
            t.check("valid response \(name) decodes and encodes", false, "\(error)")
        }
    }
    for (name, line) in files("responses/invalid") {
        t.check("invalid response \(name) is refused", (try? decodeResponseForTest(line)) == nil)
    }
    for (name, line) in files("responses/refused-hello") {
        let frame = try? decodeResponseForTest(line)
        t.check("refused-hello \(name) decodes but fails the handshake", frame.map { !passesHello($0.reply) } ?? false)
    }

    // Decoded values, not only acceptance.
    let dir = URL(fileURLWithPath: fixtures)
    if let data = try? Data(contentsOf: dir.appendingPathComponent("requests/valid/start-macos.json")),
       let frame = try? decodeRequest(Array(data)), case .start(let s) = frame.request {
        t.check("start-macos decodes its fields", frame.id == 2 && s == StartRequest(
            slot: 0, kind: .macos, claim: "123456789-1760700000", image: "macos-27",
            tools: "runner-2.330.0-agent-0.4.0", cpus: 4, memoryMiB: 8192))
    } else {
        t.check("start-macos decodes to a start", false)
    }
    if let data = try? Data(contentsOf: dir.appendingPathComponent("requests/valid/begin-macos.json")),
       let frame = try? decodeRequest(Array(data)), case .begin(let claim, let jit?) = frame.request {
        t.check("begin-macos carries its jit_config", claim == "123456789-1760700000" && jit.expose() == "ZmFrZS1qaXQtY29uZmln")
        t.check("a jit_config never prints", !"\(jit) \(String(reflecting: jit))".contains("ZmFrZS"))
        var dumped = ""
        dump(frame.request, to: &dumped)
        t.check("a jit_config never dumps", !dumped.contains("ZmFrZS"), dumped)
    } else {
        t.check("begin-macos decodes to a begin", false)
    }

    testStrictness(t)
    testEncoderAndChildProtocol(t)
    testMarkersAndPlan(t)
}

@MainActor
private func testStrictness(_ t: Harness) {
    let ok = { (s: String) in (try? decodeRequest(bytes(s + "\n"))) != nil }
    t.check("list accepted", ok(#"{"id":1,"request":{"list":{}}}"#))
    t.check("fields in any order", ok(#"{"request":{"list":{}},"id":1}"#))
    t.check("whitespace between tokens is JSON (serde_json accepts it too)", ok(#"{"id":1, "request":{"list":{}}}"#))
    t.check("a body with a field is refused", !ok(#"{"id":1,"request":{"list":{"x":1}}}"#))
    t.check("a repeated nested key is refused", !ok(#"{"id":1,"request":{"stop":{"claim":"a","claim":"b"}}}"#))
    for id in ["1.0", "1e0", "1E0", "-1", "-0", "0", "01", "4294967296", "\"1\"", "true", "null", "18446744073709551616"] {
        t.check("id \(id) refused", !ok(#"{"id":\#(id),"request":{"list":{}}}"#))
    }
    t.check("id 4294967295 accepted", ok(#"{"id":4294967295,"request":{"list":{}}}"#))
    for p in ["0", "65536", "-1", "1.0"] {
        t.check("protocol \(p) refused", !ok(#"{"id":1,"request":{"hello":{"protocol":\#(p)}}}"#))
    }
    t.check("protocol 65535 accepted", ok(#"{"id":1,"request":{"hello":{"protocol":65535}}}"#))
    t.check("a null optional is refused", !ok(#"{"id":1,"request":{"start":{"slot":0,"kind":"linux","claim":"a","image":"b","tools":null,"cpus":1,"memory_mib":1024}}}"#))
    t.check("begin without jit_config decodes (the supervisor checks it against the VM)", ok(#"{"id":1,"request":{"begin":{"claim":"a"}}}"#))
    t.check("an escaped claim that decodes into the grammar is accepted", ok(#"{"id":1,"request":{"stop":{"claim":"ab"}}}"#))
    t.check("a lone surrogate is refused", !ok(#"{"id":1,"request":{"stop":{"claim":"\ud800"}}}"#))
    t.check("a control character in a string is refused", !ok("{\"id\":1,\"request\":{\"stop\":{\"claim\":\"a\u{1}\"}}}"))
    t.check("an array where an object belongs is refused", !ok(#"{"id":1,"request":{"stop":["a"]}}"#))
    t.check("deep nesting is refused, not recursed into", !ok(String(repeating: "[", count: 5000)))
    t.check("an empty frame is refused", (try? decodeRequest([0x0A])) == nil)
    t.check("a frame with no LF is refused", (try? decodeRequest(bytes(#"{"id":1,"request":{"list":{}}}"#))) == nil)
    t.check("a non-UTF-8 frame is refused", (try? decodeRequest([0xFF, 0x0A])) == nil)
    let big = [UInt8](repeating: 0x61, count: maxFrame) + [0x0A]
    t.check("a frame past MAX_FRAME is refused as too large", { do { _ = try decodeRequest(big); return false } catch { return error as? ProtoError == .frameTooLarge } }())

    for s in ["a", String(repeating: "a", count: 64), "A1._-"] { t.check("claim \(s.prefix(8)) ok", isClaim(s)) }
    for s in ["", String(repeating: "a", count: 65), "-a", ".a", "a/b", "a b"] { t.check("claim '\(s.prefix(8))' refused", !isClaim(s)) }
    t.check("image name grammar", isStoreName("a.b_c-d") && !isStoreName("Mac") && !isStoreName("../x") && !isStoreName(".x"))
    t.check("supervisor version grammar", isSupervisorVersion("0.4.0+g1-x") && !isSupervisorVersion("0.4_0"))
    t.check("jit_config bounds", JitConfig(String(repeating: "a", count: 8192)) != nil
        && JitConfig(String(repeating: "a", count: 8193)) == nil && JitConfig("ab-c") == nil && JitConfig("") == nil)

    // A refusal names a category, never the peer's bytes.
    for f in [#"{"id":1,"request":{"list":{}},"zzsecretzz":1}"#, #"{"id":1,"request":{"zzsecretzz":{}}}"#,
              #"{"id":1,"request":{"stop":{"claim":"zzsecretzz!"}}}"#, #"{"id":"zzsecretzz","request":{"list":{}}}"#,
              #"{"id":1,"request":{"start":{"slot":0,"kind":"zzsecretzz","claim":"a","image":"b","cpus":1,"memory_mib":1024}}}"#] {
        do {
            _ = try decodeRequest(bytes(f + "\n"))
            t.check("hostile frame refused", false, f)
        } catch {
            t.check("a refusal does not echo the peer", !"\(error) \(String(reflecting: error))".contains("zzsecretzz"), "\(error)")
        }
    }
    t.check("a readable refused frame still yields its id", readableFrameID(bytes(#"{"id":7,"request":{"bogus":{}}}"# + "\n")) == 7)
    t.check("an unreadable frame yields no id", readableFrameID(bytes("{\"id\":7,\n")) == nil)
}

@MainActor
private func testEncoderAndChildProtocol(_ t: Harness) {
    let rec = { (slot: Int, state: VmState, reason: StopReason?) in
        VmRecord(slot: slot, kind: .macos, claim: "c\(slot)", state: state, stopReason: reason, startedUnix: 1_760_700_000)
    }
    t.check("a stopped record without a reason is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .vm(rec(0, .stopped, nil))))) == nil)
    t.check("a reason on a running record is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .vm(rec(0, .ready, .requested))))) == nil)
    t.check("a version_mismatch without protocol is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .error(.versionMismatch)))) == nil)
    t.check("protocol on another error is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .error(.internal, protocol: 1)))) == nil)
    t.check("a list out of slot order is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .vms([rec(1, .ready, nil), rec(0, .ready, nil)])))) == nil)
    let sixteen = (0..<16).map { i in
        VmRecord(slot: i, kind: .linux, claim: String(i, radix: 16) + String(repeating: "a", count: 63),
                 state: .stopped, stopReason: .launcherExited, startedUnix: maxUnix)
    }
    t.check("the largest list fits in a frame", ((try? encodeResponse(ResponseFrame(id: UInt32.max, reply: .vms(sixteen))))?.count ?? .max) < maxFrame)
    for state in VmState.allCases {
        var r = rec(0, state, state == .stopped ? .agentError : nil)
        if state == .verified { r.failed = [.filevault, .brewOwner] }
        let line = try? encodeResponse(ResponseFrame(id: 3, reply: .vm(r)))
        t.check("state \(state.rawValue) round-trips", line.flatMap { try? decodeResponseForTest($0) }.map {
            if case .vm(let back) = $0.reply { return back == r }
            return false
        } ?? false)
    }

    // The child protocol: every request and reply survives a round trip.
    let requests: [ChildRequest] = [.hello(childProtocol: 1), .status, .probe, .begin(jitConfig: JitConfig("ZmFrZQ==")),
                                    .begin(jitConfig: nil), .stop]
    for r in requests {
        let back = (try? encodeChildRequest(ChildRequestFrame(id: 9, request: r))).flatMap { try? decodeChildRequest($0) }
        let same: Bool
        switch (r, back?.request) {
        case (.hello(let a), .hello(let b)?): same = a == b
        case (.status, .status?), (.probe, .probe?), (.stop, .stop?): same = true
        case (.begin(let a), .begin(let b)?): same = a?.expose() == b?.expose()
        default: same = false
        }
        t.check("child request \(r) round-trips", same && back?.id == 9)
    }
    let replies: [ChildReply] = [.hello(childProtocol: 1, slot: 3, claim: "abc"), .status(state: .booting, stopReason: nil),
                                 .status(state: .stopped, stopReason: .guestShutdown), .done,
                                 .error(.wrongState), .error(.versionMismatch, childProtocol: 1)]
    for r in replies {
        let back = (try? encodeChildResponse(ChildResponseFrame(id: 4, reply: r))).flatMap { try? decodeChildResponse($0) }
        t.check("child reply \(r) round-trips", back == ChildResponseFrame(id: 4, reply: r))
    }
    t.check("a child never reports unmanaged", (try? encodeChildResponse(ChildResponseFrame(id: 1, reply: .status(state: .unmanaged, stopReason: nil)))) == nil)
    t.check("a child reply with an unknown field is refused",
            (try? decodeChildResponse(bytes(#"{"id":1,"reply":{"status":{"state":"booting","x":1}}}"# + "\n"))) == nil)
    t.check("the child begin never logs its jit_config",
            !"\(ChildRequest.begin(jitConfig: JitConfig("c2VjcmV0")))".contains("c2VjcmV0"))
}

@MainActor
private func testMarkersAndPlan(_ t: Harness) {
    let m = ClaimMarker(childProtocol: 1, slot: 0, kind: .macos, claim: "123456789-1760700000", pid: 4242,
                        pidStartedUs: 1_760_700_000_123_456, startedUnix: 1_760_700_000, bundle: "clone-0")
    let doc = #"{"marker":"org.faber.kind=vm-claim","child_protocol":1,"slot":0,"kind":"macos","claim":"123456789-1760700000","pid":4242,"pid_started_us":1760700000123456,"started_unix":1760700000,"bundle":"clone-0"}"# + "\n"
    t.check("a marker encodes as the document shows", (try? m.encode()) == bytes(doc))
    t.check("a marker decodes", (try? ClaimMarker.decode(bytes(doc))) == m)
    t.check("a marker carries the signature", hasMarkerSignature(bytes(doc)))
    t.check("a Linux marker with a bundle is refused",
            (try? ClaimMarker.decode(bytes(doc.replacingOccurrences(of: "\"macos\"", with: "\"linux\""))) ) == nil)
    t.check("a macOS marker without a bundle is refused",
            (try? ClaimMarker.decode(bytes(doc.replacingOccurrences(of: #","bundle":"clone-0""#, with: "")))) == nil)
    t.check("another marker literal is refused",
            (try? ClaimMarker.decode(bytes(doc.replacingOccurrences(of: "vm-claim", with: "vm-clone")))) == nil)
    t.check("a bundle that is a path is refused",
            (try? ClaimMarker.decode(bytes(doc.replacingOccurrences(of: "clone-0", with: "../x")))) == nil)

    let plan = #"""
    {"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24",
      "slots":[{"id":0,"addr":"192.168.100.2","profiles":["default"]},{"id":3,"addr":"192.168.100.9"}]},
     {"id":2,"gateway":"10.9.0.5","subnet":"10.9.0.0/24","slots":[{"id":4,"addr":"10.9.0.6"}]}]}
    """#
    do {
        let p = try NetworkPlan.parse(bytes(plan))
        let a = try p.address(slot: 3)
        t.check("plan slot 3 is its subnet and address", a.subnet.description == "192.168.100.0/24" && a.guest.description == "192.168.100.9")
        t.refuses("a slot absent from the plan", containing: "not in the plan") { _ = try p.address(slot: 7) }
        t.refuses("a gateway vmnet cannot honour", containing: "second address") { _ = try p.address(slot: 4) }
    } catch {
        t.check("a valid plan parses", false, "\(error)")
    }
    t.refuses("a plan with an unknown field", containing: "unknown field") {
        _ = try NetworkPlan.parse(bytes(#"{"networks":[],"extra":1}"#))
    }
    t.refuses("a plan slot with an unknown field", containing: "unknown field") {
        _ = try NetworkPlan.parse(bytes(#"{"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":0,"addr":"192.168.100.2","x":1}]}]}"#))
    }
    t.refuses("a plan with a bad address") {
        _ = try NetworkPlan.parse(bytes(#"{"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":0,"addr":"nope"}]}]}"#))
    }
    t.refuses("a plan slot repeated across networks", containing: "more than once") {
        let p = try NetworkPlan.parse(bytes(#"{"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":0,"addr":"192.168.100.2"}]},{"id":2,"gateway":"192.168.101.1","subnet":"192.168.101.0/24","slots":[{"id":0,"addr":"192.168.101.2"}]}]}"#))
        _ = try p.address(slot: 0)
    }
}
