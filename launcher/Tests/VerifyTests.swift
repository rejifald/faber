// ADR-0049 §7, the golden image's verification, on every hop it crosses: the agent's codec and
// its closed check codes, the launcher's phase machine, the agent core, the child server and
// the supervisor's transitions, and the guest checks' pure judges. Shared fixtures for the
// supervisor wire are in tests/fixtures/vm-proto (vm-verifying, vm-verified-*, verify*), which
// testProto reads with the Rust side.

import Darwin
import Foundation

private func vline(_ s: String) -> [UInt8] { Array((s + "\n").utf8) }

private let expect = VerifyExpect(xcode: XcodeExpect(version: "27.0", build: "18A5123b"),
                                  runtimes: [RuntimeExpect(platform: "iOS", version: "27.0", build: "24A335")])

@MainActor
func testVerify(_ t: Harness) {
    testVerifyCodec(t)
    testVerifyPhases(t)
    testVerifyCore(t)
    testVerifyChild(t)
    testCheckJudges(t)
}

@MainActor
private func testVerifyCodec(_ t: Harness) {
    t.check("the check codes are the Rust side's, in its order",
            CheckCode.allCases.map(\.rawValue) == ["console_user", "runner_not_admin", "admins_locked", "remote_login",
                                                    "filevault", "apple_id", "software_update", "xcode", "runtimes", "brew_owner"])
    let req = #"{"id":4,"request":{"verify":{"xcode":{"version":"27.0","build":"18A5123b"},"runtimes":[{"platform":"iOS","version":"27.0","build":"24A335"}]}}}"#
    let bytes = try? encodeAgentRequest(AgentRequestFrame(id: 4, request: .verify(expect)))
    t.check("verify encodes as the document says", bytes.map { String(decoding: $0, as: UTF8.self) } == req + "\n")
    if case .verify(let back)? = (try? decodeAgentRequest(vline(req)))?.request {
        t.check("verify decodes", back == expect)
    } else {
        t.check("verify decodes", false)
    }
    let none = #"{"id":4,"request":{"verify":{"runtimes":[]}}}"#
    t.check("verify without a manifest decodes", (try? decodeAgentRequest(vline(none))) != nil)
    for (name, bad) in [
        ("a short build", #"{"id":4,"request":{"verify":{"xcode":{"version":"27.0","build":"18"},"runtimes":[]}}}"#),
        ("a version with a quote", #"{"id":4,"request":{"verify":{"xcode":{"version":"27\"","build":"18A5123b"},"runtimes":[]}}}"#),
        ("no runtimes", #"{"id":4,"request":{"verify":{}}}"#),
        ("a runtime with a path", #"{"id":4,"request":{"verify":{"runtimes":[{"platform":"iOS","version":"27.0","build":"24A335","path":"/x"}]}}}"#),
        ("a jit config", #"{"id":4,"request":{"verify":{"runtimes":[],"jit_config":"ZmFrZQ=="}}}"#),
    ] {
        t.check("verify with \(name) is refused", (try? decodeAgentRequest(vline(bad))) == nil)
    }
    let verified = AgentStatus(.verified, failed: [.filevault, .appleID])
    let enc = try? encodeAgentReply(AgentReplyFrame(id: 9, reply: .status(verified)))
    t.check("verified carries its codes",
            enc.map { String(decoding: $0, as: UTF8.self) } == #"{"id":9,"reply":{"status":{"phase":"verified","failed":["filevault","apple_id"]}}}"# + "\n")
    t.check("a pass is an empty list", (try? decodeAgentReply(vline(#"{"id":9,"reply":{"status":{"phase":"verified","failed":[]}}}"#))) != nil)
    // What the guest might say instead of a code: every one is a bad reply, never forwarded.
    for (name, bad) in [
        ("an unknown code", #"{"id":9,"reply":{"status":{"phase":"verified","failed":["fine"]}}}"#),
        ("free text", #"{"id":9,"reply":{"status":{"phase":"verified","failed":[{"check":"xcode","said":"Xcode 26"}]}}}"#),
        ("a repeat", #"{"id":9,"reply":{"status":{"phase":"verified","failed":["xcode","xcode"]}}}"#),
        ("the wrong order", #"{"id":9,"reply":{"status":{"phase":"verified","failed":["brew_owner","filevault"]}}}"#),
        ("verified without codes", #"{"id":9,"reply":{"status":{"phase":"verified"}}}"#),
        ("codes while verifying", #"{"id":9,"reply":{"status":{"phase":"verifying","failed":[]}}}"#),
        ("codes on a job's phase", #"{"id":9,"reply":{"status":{"phase":"running","failed":[]}}}"#),
        ("a code as a number", #"{"id":9,"reply":{"status":{"phase":"verified","failed":[4]}}}"#),
    ] {
        t.check("a status with \(name) is refused", (try? decodeAgentReply(vline(bad))) == nil)
    }
}

@MainActor
private func testVerifyPhases(_ t: Harness) {
    let d = AgentDeadlines(ready: 600, reply: 1.5, started: 120, verified: 300)
    let t0 = Date(timeIntervalSince1970: 1_760_700_000)
    func proven() throws -> AgentPhaseMachine {
        var m = AgentPhaseMachine(deadlines: d, since: t0)
        _ = try m.observe(AgentStatus(.waitingConsole))
        _ = try m.observe(AgentStatus(.ready))
        try m.probeReplied(sent: 3, count: 3)
        return m
    }
    do {
        var m = try proven()
        t.check("verify is allowed after the probe", m.mayVerify)
        m.verifying(at: t0)
        t.check("begin is not allowed after verify", !m.mayBegin)
        t.check("verify only once", !m.mayVerify)
        t.check("ready again is fine", try m.observe(AgentStatus(.ready)) == .none)
        t.check("verifying", try m.observe(AgentStatus(.verifying)) == .none)
        t.check("verified carries its codes", try m.observe(AgentStatus(.verified, failed: [.xcode])) == .verified([.xcode]))
        t.check("the same verdict again", try m.observe(AgentStatus(.verified, failed: [.xcode])) == .verified([.xcode]))
        var changed = m
        t.check("a verdict that changes is bad_phase", violation { _ = try changed.observe(AgentStatus(.verified, failed: [])) } == .badPhase)
        var back = m
        t.check("verifying after verified is bad_phase", violation { _ = try back.observe(AgentStatus(.verifying)) } == .badPhase)
        t.accepts("verified is in time whenever") { try m.checkDeadlines(now: t0.addingTimeInterval(10_000)) }

        var job = try proven()
        job.verifying(at: t0)
        t.check("a job's phase in a verification is bad_phase", violation { _ = try job.observe(AgentStatus(.running)) } == .badPhase)

        var unasked = try proven()
        t.check("verifying unasked is bad_phase", violation { _ = try unasked.observe(AgentStatus(.verifying)) } == .badPhase)
        var unasked2 = try proven()
        t.check("verified unasked is bad_phase", violation { _ = try unasked2.observe(AgentStatus(.verified, failed: [])) } == .badPhase)

        var slow = try proven()
        slow.verifying(at: t0)
        _ = try slow.observe(AgentStatus(.verifying))
        t.accepts("checks within the deadline") { try slow.checkDeadlines(now: t0.addingTimeInterval(299)) }
        t.check("checks past the deadline are timeout", violation { try slow.checkDeadlines(now: t0.addingTimeInterval(301)) } == .timeout)
    } catch {
        t.check("verify phase fixtures", false, "\(error)")
    }
    t.check("verify is answered by accepted", (try? checkAgentReply(.accepted, to: .verify(expect))) != nil)
    t.check("verify answered by probed is wrong_reply", (try? checkAgentReply(.probed(sent: 3), to: .verify(expect))) == nil)
}

/// The violation `body` throws, if it throws one.
private func violation(_ body: () throws -> Void) -> AgentViolation? {
    do {
        try body()
        return nil
    } catch {
        return error as? AgentViolation
    }
}

/// A value the agent's closures set from other threads.
private final class VBox<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    var get: T { lock.lock(); defer { lock.unlock() }; return value }
    func set(_ v: T) { lock.lock(); value = v; lock.unlock() }
}

@MainActor
private func testVerifyCore(_ t: Harness) {
    let seen = VBox<VerifyExpect?>(nil)
    let gate = DispatchSemaphore(value: 0)
    let core = AgentCore(consoleIsRunner: { true }, probe: { _, _, n in n }, startRunner: { _, _ in },
                         runChecks: { e in
                             seen.set(e)
                             gate.wait()
                             return [.brewOwner, .filevault]
                         })
    var g = false
    _ = core.answer(.hello(protocol: agentProtocol), greeted: &g)
    t.check("verify before the probe is wrong_phase", core.answer(.verify(expect), greeted: &g) == (.error(.wrongPhase), true))
    _ = core.answer(.probe(addr: "192.168.100.1", port: 9, count: 3), greeted: &g)
    t.check("verify after the probe is accepted at once", core.answer(.verify(expect), greeted: &g) == (.accepted, true))
    t.check("a second verify is wrong_phase", core.answer(.verify(expect), greeted: &g) == (.error(.wrongPhase), true))
    t.check("begin after verify is wrong_phase", core.answer(.begin(jitConfig: JitConfig("ZmFrZQ==")!), greeted: &g) == (.error(.wrongPhase), true))
    t.check("verifying while the checks run", core.status == AgentStatus(.verifying))
    gate.signal()
    var done = false
    for _ in 0..<200 where !done {
        done = core.status.phase == .verified
        if !done { Thread.sleep(forTimeInterval: 0.01) }
    }
    t.check("verified, the codes sorted into the protocol's order", core.status == AgentStatus(.verified, failed: [.filevault, .brewOwner]))
    t.check("the checks were held to what faber sent", seen.get == expect)
}

@MainActor
private func testVerifyChild(_ t: Harness) {
    final class FakeAgent: GuestAgent, @unchecked Sendable {
        var isPresent: Bool { true }
        let got = VBox<VerifyExpect?>(nil)
        func sendProbe() throws {}
        func begin(jitConfig: JitConfig?) throws {}
        func verify(expect: VerifyExpect) throws { got.set(expect) }
    }
    let status = ChildStatus()
    let agent = FakeAgent()
    var template = Array("/tmp/fvv.XXXXXX".utf8CString)
    guard let made = mkdtemp(&template),
          let dir = try? resolvedURL(URL(fileURLWithPath: String(cString: made))),
          let server = try? ChildServer(path: dir.appendingPathComponent("slot-3.sock"), slot: 3, claim: "0-1-a1b2c3d4",
                                        status: status, agent: agent, onStop: {}) else {
        t.check("a child server for verify", false)
        return
    }
    defer {
        server.close()
        try? FileManager.default.removeItem(at: dir)
    }
    var g = false
    _ = server.answer(.hello(childProtocol: childProtocol), greeted: &g)
    status.set(.ready)
    t.check("verify in ready is wrong_state", server.answer(.verify(expect), greeted: &g) == (.error(.wrongState), true))
    status.set(.probeSent)
    t.check("verify from probe_sent is verifying", server.answer(.verify(expect), greeted: &g) == (.status(state: .verifying, stopReason: nil), true))
    t.check("the expectations reach the agent unchanged", agent.got.get == expect)
    t.check("verifying reports no codes", server.answer(.status, greeted: &g) == (.status(state: .verifying, stopReason: nil, failed: nil), true))
    t.check("the verdict moves verifying to verified", status.verdict([.xcode]))
    t.check("a second verdict does not move it", !status.verdict([]))
    t.check("verified reports its codes", server.answer(.status, greeted: &g) == (.status(state: .verified, stopReason: nil, failed: [.xcode]), true))
    // The Linux VM's agent, and no agent, never verify.
    t.check("no guest agent does not verify", (try? AbsentGuestAgent().verify(expect: expect)) == nil)

    // The supervisor takes verifying -> verified, and nothing else new.
    t.check("verifying to verified is a move", childStatusMoves(from: .verifying, to: .verified))
    t.check("verified to stopping is a move", childStatusMoves(from: .verified, to: .stopping))
    t.check("in_service to verified is not", !childStatusMoves(from: .inService, to: .verified))
    t.check("ready to verifying is not (only verify's reply moves it)", !childStatusMoves(from: .ready, to: .verifying))
    t.check("verified to verifying is not", !childStatusMoves(from: .verified, to: .verifying))
    t.check("a version-1 child is still adopted", supportedChildProtocols.contains(1) && supportedChildProtocols.contains(childProtocol))
    // The child wire carries the codes, and refuses them where they do not belong.
    let r = ChildReply.status(state: .verified, stopReason: nil, failed: [.remoteLogin])
    let back = (try? encodeChildResponse(ChildResponseFrame(id: 2, reply: r))).flatMap { try? decodeChildResponse($0) }
    t.check("a child's verdict round-trips", back?.reply == r)
    t.check("a child's verified without codes is refused",
            (try? encodeChildResponse(ChildResponseFrame(id: 2, reply: .status(state: .verified, stopReason: nil)))) == nil)
    let req = try? encodeChildRequest(ChildRequestFrame(id: 2, request: .verify(expect)))
    if case .verify(let e)? = req.flatMap({ try? decodeChildRequest($0) })?.request {
        t.check("a child's verify round-trips", e == expect)
    } else {
        t.check("a child's verify round-trips", false)
    }
    let rec = VmRecord(slot: 0, kind: .linux, claim: "c0", state: .verified, stopReason: nil, failed: [], startedUnix: 1)
    t.check("a Linux record with codes is refused", (try? encodeResponse(ResponseFrame(id: 1, reply: .vm(rec)))) == nil)
}

@MainActor
private func testCheckJudges(_ t: Harness) {
    let xcode = "Xcode 27.0\nBuild version 18A5123b\n"
    t.check("xcodebuild -version matches", xcodeMatches(xcode, expect.xcode))
    t.check("another build does not", !xcodeMatches("Xcode 27.0\nBuild version 18A5124a\n", expect.xcode))
    t.check("another version does not", !xcodeMatches("Xcode 27.0.1\nBuild version 18A5123b\n", expect.xcode))
    t.check("a beta's version line matches exactly",
            xcodeMatches("Xcode 27.0 Beta 2\nBuild version 18A5123b\n", XcodeExpect(version: "27.0 Beta 2", build: "18A5123b")))
    t.check("no manifest expects no Xcode in particular", xcodeMatches("", nil))

    let runtimes = Data(#"{"runtimes":[{"platform":"iOS","version":"27.0","buildversion":"24A335","isAvailable":true},{"platform":"watchOS","version":"12.0","buildversion":"23R1","isAvailable":false}]}"#.utf8)
    t.check("a ready runtime is ready", runtimesReady(runtimes, expect.runtimes))
    t.check("an unavailable runtime is not",
            !runtimesReady(runtimes, [RuntimeExpect(platform: "watchOS", version: "12.0", build: "23R1")]))
    t.check("a missing runtime is not", !runtimesReady(runtimes, [RuntimeExpect(platform: "iOS", version: "26.4", build: "23E1")]))
    t.check("unreadable output is not", !runtimesReady(Data("x".utf8), expect.runtimes))
    t.check("no runtimes expected is ready", runtimesReady(Data(), []))

    let off: [String: Any] = ["AutomaticDownload": false, "CriticalUpdateInstall": false, "ConfigDataInstall": false,
                              "AutomaticCheckEnabled": false]
    t.check("every update setting off", softwareUpdateOff(off))
    var macos = off
    macos["AutomaticallyInstallMacOSUpdates"] = true
    t.check("automatic macOS installs on fails", !softwareUpdateOff(macos))
    for key in Checks.updateKeys {
        var on = off
        on[key] = true
        t.check("\(key) on fails", !softwareUpdateOff(on))
        var unset = off
        unset[key] = nil
        t.check("\(key) unset fails: it defaults to on", !softwareUpdateOff(unset))
    }
    t.check("no preferences fails", !softwareUpdateOff(nil))

    t.check("an Apple ID account is signed in", appleIDSignedIn(["Accounts": [["AccountID": "x"]]]))
    t.check("no accounts is signed out", !appleIDSignedIn(["Accounts": []]) && !appleIDSignedIn(nil))
    t.check("FileVault off", fileVaultOff("FileVault is Off.\n"))
    t.check("FileVault on", !fileVaultOff("FileVault is On.\n"))
    t.check("the build's admin shell is locked", adminLocked(shell: "/usr/bin/false"))
    t.check("a real shell is not", !adminLocked(shell: "/bin/zsh"))
    // ADR-0051's spikes: pam_opendirectory refuses only /usr/bin/false; nologin passes a GUI login.
    t.check("nologin is not locked", !adminLocked(shell: "/sbin/nologin"))
    t.check("root without an authentication authority is locked",
            rootLocked("RecordName: root System Administrator\nUniqueID: 0\nUserShell: /bin/sh\n"))
    t.check("root with one is not",
            !rootLocked("AuthenticationAuthority: ;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>\nRecordName: root\n"))
    t.check("root disabled the build's way is locked",
            rootLocked("AuthenticationAuthority: ;DisabledUser;\nRecordName: root\n"))
    t.check("root disabled with a hash still listed is locked",
            rootLocked("AuthenticationAuthority: ;DisabledUser; ;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>\nRecordName: root\n"))
    t.check("a disabled authority on a continuation line is read",
            rootLocked("AuthenticationAuthority:\n ;DisabledUser;\nRecordName: root\n"))
    t.check("a continuation line of another attribute is not",
            !rootLocked("AuthenticationAuthority:\n ;ShadowHash;\nComment:\n ;DisabledUser;\nRecordName: root\n"))
    t.check("an empty record is no answer", !rootLocked(""))
    t.check("a member, nested groups included", memberVerdict("user is a member of the group\n") == true)
    t.check("not a member", memberVerdict("user is not a member of the group\n") == false)
    t.check("anything else is no answer", memberVerdict("Invalid user name\n") == nil && memberVerdict("") == nil)
    t.check("no sudo rights", sudoDenied("User runner is not allowed to run sudo on vm.\n", user: "runner"))
    t.check("a sudoers grant is a sudo right", !sudoDenied(
        "Matching Defaults entries for runner on vm:\n    env_reset\n\nUser runner may run the following commands on vm:\n    (ALL) NOPASSWD: ALL\n",
        user: "runner"))
    t.check("another user's denial is not runner's", !sudoDenied("User admin is not allowed to run sudo on vm.\n", user: "runner"))
    t.check("no answer is not a denial", !sudoDenied("", user: "runner"))
    t.check("sshd listed disabled is off", sshdOff(disabledList: "disabled services = {\n\t\"com.openssh.sshd\" => disabled\n}\n", printStatus: 0))
    t.check("sshd listed enabled is on", !sshdOff(disabledList: "\t\"com.openssh.sshd\" => enabled\n", printStatus: 113))
    t.check("unlisted and not loaded is off", sshdOff(disabledList: "disabled services = {\n}\n", printStatus: 113))
    t.check("unlisted and loaded is on", !sshdOff(disabledList: "", printStatus: 0))
    t.check("unlisted and launchctl failing is on", !sshdOff(disabledList: "", printStatus: nil) && !sshdOff(disabledList: "", printStatus: 1))
    t.check("an unanswered check fails", failedChecks([.consoleUser: true]) == Array(CheckCode.allCases.dropFirst()))
    t.check("all passing is a pass", failedChecks(Dictionary(uniqueKeysWithValues: CheckCode.allCases.map { ($0, true) })).isEmpty)
}

// MARK: - The verification's diagnostics (the guest's console, kept by the supervisor)

@MainActor
func testVerifyDiagnostics(_ t: Harness, _ scratch: Scratch) {
    testDiagnosticLines(t)
    testVerdictWhys(t)
    testAdminsFromTheDirectory(t)
    testVerifyLogFiles(t, scratch)
    testToolchainScript(t, scratch)
}

private final class Lines: @unchecked Sendable { var all: [String] = [] }

@MainActor
private func testDiagnosticLines(_ t: Harness) {
    let line = checkLine(.adminsLocked, .fail("admin _mbsetupuser shell /bin/bash"))
    t.check("a check's line names the check, its result and the fact",
            line == "FABER-GOLDEN-VERIFY check=admins_locked ok=false why=admin _mbsetupuser shell /bin/bash", line)
    t.check("a pass reads ok=true", checkLine(.xcode, .pass("x")) == "FABER-GOLDEN-VERIFY check=xcode ok=true why=x")
    t.check("the last line lists the failed codes",
            doneLine([.adminsLocked, .softwareUpdate]) == "FABER-GOLDEN-VERIFY done failed=admins_locked,software_update")
    t.check("a pass ends with none", doneLine([]) == "FABER-GOLDEN-VERIFY done failed=none")

    let hostile = checkLine(.consoleUser, .fail("console user is ev\nFABER-GOLDEN-VERIFY done failed=none\u{1b}[2J\u{0}\u{e9}"))
    t.check("a newline, an escape, a NUL and a non-ASCII byte become ?",
            !hostile.contains("\n") && !hostile.contains("\u{1b}") && !hostile.contains("\u{0}")
                && hostile.utf8.allSatisfy { (0x20...0x7e).contains($0) } && hostile.contains("ev?FABER"), hostile)
    let long = checkLine(.softwareUpdate, .fail(String(repeating: "k", count: 1000)))
    t.check("a line is cut to its cap", long.utf8.count == Diagnostics.maxLine, "\(long.utf8.count)")
    t.check("a multi-byte character cut at the cap still yields ASCII",
            diagnosticLine(String(repeating: "\u{e9}", count: 200)).utf8.allSatisfy { (0x20...0x7e).contains($0) })

    let got = Lines()
    let sink = DiagnosticSink { got.all.append($0) }
    for i in 0..<(Diagnostics.maxLines + 10) { sink.emit(diagnosticLine("n=\(i)")) }
    t.check("a verification writes at most its cap of lines", got.all.count == Diagnostics.maxLines, "\(got.all.count)")
    t.check("the first lines are the ones kept", got.all.first == "FABER-GOLDEN-VERIFY n=0")
    // A start, a line per check, softwareupdate's evidence line, and the end.
    t.check("a whole run fits under the cap", CheckCode.allCases.count + 3 <= Diagnostics.maxLines)
}

@MainActor
private func testVerdictWhys(_ t: Harness) {
    let off: [String: Any] = ["AutomaticDownload": false, "CriticalUpdateInstall": false, "ConfigDataInstall": false,
                              "AutomaticCheckEnabled": false]
    let pass = softwareUpdateVerdict(off)
    t.check("software update passes and lists what it saw",
            pass.ok && pass.why.contains("AutomaticCheckEnabled=0") && pass.why.contains("AutomaticallyInstallMacOSUpdates absent"), pass.why)
    var macos = off
    macos["AutomaticallyInstallMacOSUpdates"] = true
    t.check("automatic macOS installs on names the key and value",
            softwareUpdateVerdict(macos) == .fail("AutomaticallyInstallMacOSUpdates=1"))
    var on = off
    on["CriticalUpdateInstall"] = 1
    t.check("a key on names it", softwareUpdateVerdict(on) == .fail("CriticalUpdateInstall=1"))
    var gone = off
    gone["ConfigDataInstall"] = nil
    t.check("a key absent says so", softwareUpdateVerdict(gone).why.hasPrefix("ConfigDataInstall absent"))
    var typed = off
    typed["AutomaticDownload"] = "NO"
    let wrong = softwareUpdateVerdict(typed)
    t.check("a key of another type fails by its type", !wrong.ok && wrong.why.hasPrefix("AutomaticDownload=a "), wrong.why)
    t.check("no plist fails", softwareUpdateVerdict(nil) == .fail("plist unreadable"))

    t.check("the console user's line names who it is",
            consoleUserVerdict(name: "operator", uid: 501, runnerUID: 502) == .fail("console user is operator"))
    t.check("no console user", consoleUserVerdict(name: nil, uid: 0, runnerUID: 502) == .fail("no console user"))
    t.check("runner with another uid fails", !consoleUserVerdict(name: "runner", uid: 503, runnerUID: 502).ok)
    t.check("runner as root fails", !consoleUserVerdict(name: "runner", uid: 0, runnerUID: 0).ok)
    t.check("runner passes", consoleUserVerdict(name: "runner", uid: 502, runnerUID: 502).ok)

    t.check("sshd's line names its listing",
            sshdVerdict(disabledList: "\t\"com.openssh.sshd\" => enabled\n", printStatus: 113) == .fail("sshd listed as enabled"))
    t.check("an unlisted, loaded sshd names launchctl's exit",
            sshdVerdict(disabledList: "", printStatus: 0) == .fail("sshd not listed; launchctl print exit 0"))

    let runtimes = Data(#"{"runtimes":[{"platform":"watchOS","version":"12.0","buildversion":"23R1","isAvailable":false}]}"#.utf8)
    t.check("a missing runtime is named", runtimesVerdict(runtimes, expect.runtimes) == .fail("runtime iOS 27.0 (24A335) missing"))
    t.check("an unavailable runtime is named",
            runtimesVerdict(runtimes, [RuntimeExpect(platform: "watchOS", version: "12.0", build: "23R1")])
                == .fail("runtime watchOS 12.0 (23R1) not available"))
    let xcode = xcodeVerdict("Xcode 27.1\nBuild version 18B1\n", expect.xcode)
    t.check("another Xcode names what it got and wanted",
            !xcode.ok && xcode.why == "got 'Xcode 27.1' 'Build version 18B1' want 'Xcode 27.0' 'Build version 18A5123b'", xcode.why)
    t.check("a short first line of a tool", firstLine("FileVault is Off.\nmore\n") == "FileVault is Off.")
}

@MainActor
private func testAdminsFromTheDirectory(_ t: Harness) {
    // dscl -plist . -readall /Groups RecordName GeneratedUID GroupMembership NestedGroups, trimmed.
    let plist = """
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><array>
    <dict>
      <key>dsAttrTypeStandard:GeneratedUID</key><array><string>ABCDEFAB-CDEF-ABCD-EFAB-CDEF00000050</string></array>
      <key>dsAttrTypeStandard:GroupMembership</key><array><string>root</string><string>operator</string><string>_mbsetupuser</string></array>
      <key>dsAttrTypeStandard:RecordName</key><array><string>admin</string><string>BUILTIN\\Administrators</string></array>
    </dict>
    <dict>
      <key>dsAttrTypeStandard:GeneratedUID</key><array><string>ABCDEFAB-CDEF-ABCD-EFAB-CDEF00000001</string></array>
      <key>dsAttrTypeStandard:RecordName</key><array><string>daemon</string></array>
    </dict>
    </array></plist>
    """
    let groups = parseGroupRecords(Data(plist.utf8))
    t.check("dscl's group listing parses", groups?.count == 2 && groups?.first?.members == ["root", "operator", "_mbsetupuser"]
        && groups?.last?.members == [], "\(String(describing: groups))")
    t.check("anything else is no listing", parseGroupRecords(Data("x".utf8)) == nil
        && parseGroupRecords(Data(#"<plist version="1.0"><dict/></plist>"#.utf8)) == nil)

    let root = "RecordName: root System Administrator\nUniqueID: 0\n"
    let locked = ["root": "/bin/sh", "operator": "/usr/bin/false", "_mbsetupuser": "/usr/bin/false", "daemon": "/usr/bin/false",
                  "nobody": "/bin/sh", "runner": "/bin/zsh"]
    let pass = adminsVerdict(groups: groups, primaryAdmins: [], shells: locked, rootRecord: root)
    t.check("the build's admins, locked, pass", pass.ok, pass.why)
    t.check("the pass names every admin it judged", pass.why.hasSuffix(": operator _mbsetupuser"), pass.why)
    // The false-failure class: an account outside the admin group is never asked about, so
    // none of the system accounts (nobody's /bin/sh here) can fail the check.
    t.check("an account outside the admin group is not judged", pass.ok && !pass.why.contains("nobody"))

    var open = locked
    open["_mbsetupuser"] = "/bin/bash"
    t.check("an admin with a shell names the account and the shell",
            adminsVerdict(groups: groups, primaryAdmins: [], shells: open, rootRecord: root)
                == .fail("admin _mbsetupuser shell /bin/bash"))
    open["_mbsetupuser"] = "/sbin/nologin"
    t.check("nologin is not locked", !adminsVerdict(groups: groups, primaryAdmins: [], shells: open, rootRecord: root).ok)
    t.check("an account whose primary group is admin's is an admin",
            adminsVerdict(groups: groups, primaryAdmins: ["runner"], shells: locked, rootRecord: root)
                == .fail("admin runner shell /bin/zsh"))
    var missing = locked
    missing["operator"] = nil
    t.check("a member with no account cannot be judged, and fails",
            adminsVerdict(groups: groups, primaryAdmins: [], shells: missing, rootRecord: root)
                == .fail("admin operator has no account record"))
    t.check("a group listing that could not be read fails, saying how",
            adminsVerdict(groups: nil, groupsFailure: "dscl readall /Groups exit 64", primaryAdmins: [], shells: locked,
                          rootRecord: root) == .fail("dscl readall /Groups exit 64"))
    t.check("no admin group fails",
            !adminsVerdict(groups: [GroupRecord(names: ["staff"], guid: nil, members: [], nested: [])], primaryAdmins: [],
                           shells: locked, rootRecord: root).ok)
    let rootOpen = adminsVerdict(groups: groups, primaryAdmins: [], shells: locked,
                                 rootRecord: "AuthenticationAuthority: ;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>\nRecordName: root\n")
    t.check("root with an authority fails without showing its record",
            rootOpen == .fail("root has an AuthenticationAuthority without ;DisabledUser;"), rootOpen.why)
    t.check("root's record unread fails, saying how",
            adminsVerdict(groups: groups, primaryAdmins: [], shells: locked, rootRecord: nil, rootFailure: "dscl read /Users/root exit 56")
                == .fail("dscl read /Users/root exit 56"))

    // Nested groups: their members are admins, transitively, and a loop ends.
    let nested = [
        GroupRecord(names: ["admin"], guid: "G-ADMIN", members: ["operator"], nested: ["G-OPS"]),
        GroupRecord(names: ["ops"], guid: "G-OPS", members: ["deploy"], nested: ["G-INNER", "G-ADMIN"]),
        GroupRecord(names: ["inner"], guid: "G-INNER", members: ["deep", "operator"], nested: ["G-OPS"]),
    ]
    let found = adminMembers(nested)
    t.check("nested members are admins, each once, and a loop ends",
            found?.members == ["operator", "deploy", "deep"] && found?.unresolved == [], "\(String(describing: found))")
    t.check("a nested member with a shell fails",
            adminsVerdict(groups: nested, primaryAdmins: [],
                          shells: ["operator": "/usr/bin/false", "deploy": "/usr/bin/false", "deep": "/bin/zsh"], rootRecord: root)
                == .fail("admin deep shell /bin/zsh"))
    let dangling = [GroupRecord(names: ["admin"], guid: "G-ADMIN", members: [], nested: ["G-GONE"])]
    t.check("a nested group not in the directory cannot be judged, and fails",
            adminsVerdict(groups: dangling, primaryAdmins: [], shells: [:], rootRecord: root)
                == .fail("admin nests group G-GONE, which is not in the directory"))

    // GroupMembers: a user listed only by GeneratedUID is an admin all the same.
    let users = parseUserGUIDs(Data("""
    <plist version="1.0"><array>
    <dict><key>dsAttrTypeStandard:GeneratedUID</key><array><string>ffffeeee-0000-0000-0000-000000000000</string></array>
      <key>dsAttrTypeStandard:RecordName</key><array><string>root</string></array></dict>
    <dict><key>dsAttrTypeStandard:GeneratedUID</key><array><string>AAAA0000-0000-0000-0000-000000000001</string></array>
      <key>dsAttrTypeStandard:RecordName</key><array><string>hidden</string></array></dict>
    <dict><key>dsAttrTypeStandard:GeneratedUID</key><array><string>AAAA0000-0000-0000-0000-000000000002</string></array>
      <key>dsAttrTypeStandard:RecordName</key><array><string>nestedonly</string></array></dict>
    </array></plist>
    """.utf8))
    t.check("dscl's user listing parses, its GUIDs uppercased", users?["FFFFEEEE-0000-0000-0000-000000000000"] == "root"
        && users?["AAAA0000-0000-0000-0000-000000000001"] == "hidden", "\(String(describing: users))")
    let byGUID = [GroupRecord(names: ["admin"], guid: "G-ADMIN", members: ["operator"], nested: ["G-OPS"],
                              memberGUIDs: ["FFFFEEEE-0000-0000-0000-000000000000", "AAAA0000-0000-0000-0000-000000000001"]),
                  GroupRecord(names: ["ops"], guid: "G-OPS", members: [], nested: [],
                              memberGUIDs: ["aaaa0000-0000-0000-0000-000000000002"])]
    let guidShells = ["operator": "/usr/bin/false", "hidden": "/usr/bin/false", "nestedonly": "/usr/bin/false"]
    let guidPass = adminsVerdict(groups: byGUID, users: users, primaryAdmins: [], shells: guidShells, rootRecord: root)
    t.check("a member only by GUID, locked, passes, and is judged",
            guidPass.ok && guidPass.why.hasSuffix(": operator hidden nestedonly"), guidPass.why)
    t.check("a member only by GUID, unlocked, fails",
            adminsVerdict(groups: byGUID, users: users, primaryAdmins: [],
                          shells: guidShells.merging(["hidden": "/bin/zsh"]) { $1 }, rootRecord: root)
                == .fail("admin hidden shell /bin/zsh"))
    t.check("a nested group's member only by GUID, unlocked, fails",
            adminsVerdict(groups: byGUID, users: users, primaryAdmins: [],
                          shells: guidShells.merging(["nestedonly": "/bin/sh"]) { $1 }, rootRecord: root)
                == .fail("admin nestedonly shell /bin/sh"))
    var stray = byGUID
    stray[1].memberGUIDs.append("BBBB0000-0000-0000-0000-00000000000F")
    t.check("a member GUID no user carries cannot be judged, and fails",
            adminsVerdict(groups: stray, users: users, primaryAdmins: [], shells: guidShells, rootRecord: root)
                == .fail("an admin group lists member BBBB0000-0000-0000-0000-00000000000F, which is no user in the directory"))
    t.check("a user listing that could not be read fails, saying how",
            adminsVerdict(groups: byGUID, users: nil, usersFailure: "dscl readall /Users exit 1", primaryAdmins: [],
                          shells: guidShells, rootRecord: root) == .fail("dscl readall /Users exit 1"))
}

@MainActor
private func testVerifyLogFiles(_ t: Harness, _ scratch: Scratch) {
    t.check("a slot's console log names its image", consoleLogImage("console-slot3-xcode-27.1.log", slot: 3) == "xcode-27.1")
    t.check("another slot's is not this slot's", consoleLogImage("console-slot13-xcode-27.1.log", slot: 1) == nil
        && consoleLogImage("console-slot3-xcode.log", slot: 1) == nil)
    t.check("a name outside the store grammar is not a console log",
            consoleLogImage("console-slot3-.log", slot: 3) == nil && consoleLogImage("console-slot3-../x.log", slot: 3) == nil
                && consoleLogImage("console-slot3-X.log", slot: 3) == nil)
    let names = ["verify-xcode-100.log", "verify-xcode-300.log", "verify-xcode-200.log", "verify-xcode-400.log",
                 "verify-xcode-500.log", "verify-xcode-latest.log", "verify-xcode-27-100.log", "verify-xcode-27-latest.log",
                 "slot-1.claim", "verify-xcode-.log", "verify-xcode-1x.log"]
    t.check("pruning keeps the newest and never another image's",
            verifyLogsToPrune(names, image: "xcode", keep: 5) == ["verify-xcode-100.log"],
            "\(verifyLogsToPrune(names, image: "xcode", keep: 5))")
    t.check("keep 1 keeps only the latest", Set(verifyLogsToPrune(names, image: "xcode", keep: 1)).count == 5)

    // A macOS child's command line carries the console log; run accepts it.
    let mac = StartRequest(slot: 2, kind: .macos, claim: "c-2", image: "xcode-27.1", tools: "t", cpus: 2, memoryMiB: 4096)
    let state = URL(fileURLWithPath: "/v/state")
    let args = childArguments(executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/c/vm-2"), start: mac,
                              stateDir: state, tools: URL(fileURLWithPath: "/t/t"), network: nil)
    do {
        let o = try RunOptions(parse(Array(args.dropFirst(3)), valued: RunOptions.valued), forRun: true)
        t.check("a macOS child logs its console in the state directory, capped",
                o.consoleLog?.path == "/v/state/console-slot2-xcode-27.1.log" && o.consoleLogMaxMiB == consoleLogMaxMiB, "\(args)")
    } catch {
        t.check("run accepts the macOS child's console log", false, "\(error)")
    }
    let linux = StartRequest(slot: 14, kind: .linux, claim: "linux-engine", image: "linux", tools: nil, cpus: 4, memoryMiB: 8192)
    t.check("the Linux child gets no console log", !childArguments(
        executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/v/linux"), start: linux, stateDir: state, tools: nil,
        network: nil).contains("--console-log"))

    // On disk: kept, rotated, pruned; a job's is removed; nothing but this account's files.
    do {
        let dir = try scratch.dir("verify-logs")
        let fd = open(dir.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        defer { close(fd) }
        func put(_ name: String, _ text: String) throws { try scratch.write(dir.appendingPathComponent(name), Data(text.utf8)) }
        func read(_ name: String) -> String? { try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8) }
        func exists(_ name: String) -> Bool { FileManager.default.fileExists(atPath: dir.appendingPathComponent(name).path) }

        try put("console-slot0-img.log", "first\n")
        keepVerifyLog(dirFD: fd, dir: dir, slot: 0)
        t.check("a verification's console becomes the latest", read("verify-img-latest.log") == "first\n"
            && !exists("console-slot0-img.log"))
        var st = stat()
        t.check("the kept log is 0600", stat(dir.appendingPathComponent("verify-img-latest.log").path, &st) == 0
            && st.st_mode & 0o777 == 0o600, String(st.st_mode & 0o777, radix: 8))
        for n in 1...7 {
            try put("console-slot1-img.log", "run \(n)\n")
            // The latest gets a distinct mtime, which names it when it is archived.
            var times = [timespec(tv_sec: 1_000_000 + n, tv_nsec: 0), timespec(tv_sec: 1_000_000 + n, tv_nsec: 0)]
            _ = utimensat(fd, "verify-img-latest.log", &times, AT_SYMLINK_NOFOLLOW)
            keepVerifyLog(dirFD: fd, dir: dir, slot: 1)
        }
        let kept = ((try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []).filter { $0.hasPrefix("verify-img-") }
        t.check("only the last \(verifyLogsKept) are kept, the latest the newest",
                kept.count == verifyLogsKept && read("verify-img-latest.log") == "run 7\n"
                    && read("verify-img-1000007.log") == "run 6\n" && !exists("verify-img-1000003.log"), "\(kept.sorted())")

        try put("console-slot2-img.log", "a job\n")
        try put("console-slot22-img.log", "another slot\n")
        removeConsoleLogs(dirFD: fd, dir: dir, slot: 2)
        t.check("a job's console is removed, and only its slot's", !exists("console-slot2-img.log") && exists("console-slot22-img.log"))

        let target = try scratch.write(scratch.url.appendingPathComponent("not-a-log"), Data("x".utf8))
        try FileManager.default.createSymbolicLink(at: dir.appendingPathComponent("console-slot4-img.log"), withDestinationURL: target)
        keepVerifyLog(dirFD: fd, dir: dir, slot: 4)
        removeConsoleLogs(dirFD: fd, dir: dir, slot: 4)
        t.check("a symlink is never kept or followed", (try? String(contentsOf: target, encoding: .utf8)) == "x"
            && read("verify-img-latest.log") == "run 7\n" && exists("console-slot4-img.log"))
    } catch {
        t.check("the verification log files", false, "\(error)")
    }
}

// MARK: - The toolchain as runner sees it (the xcode check)

/// The script the xcode check runs as runner, against fake tools in `scratch`: each way the
/// toolchain can be missing fails with its own status, and only a C and a Swift program that
/// link and run pass. The first case is a broken image's: every shim says "No developer tools
/// were found" to runner while root's `xcodebuild -version` passes.
@MainActor
private func testToolchainScript(_ t: Harness, _ scratch: Scratch) {
    guard let bin = try? scratch.dir("toolchain-bin"), let dev = try? scratch.dir("toolchain-dev") else {
        t.check("the toolchain fixtures", false, "no scratch directory")
        return
    }
    func tool(_ name: String, _ body: String) -> String {
        let path = bin.appendingPathComponent(name).path
        FileManager.default.createFile(atPath: path, contents: Data(("#!/bin/sh\n" + body + "\n").utf8))
        _ = chmod(path, 0o755)
        return path
    }
    let noTools = "echo 'xcode-select: note: No developer tools were found, requesting install.' >&2; exit 1"
    let select = tool("xcode-select", "echo \(dev.path)")
    let unresolved = tool("xcode-select-none", "echo 'xcode-select: error: unable to get active developer directory' >&2; exit 2")
    let shim = tool("shim", noTools)
    // A compiler that links: it writes the program its -o names.
    let cc = tool("cc", "printf '#!/bin/sh\\nexit 0\\n' > \"$2\"; chmod 755 \"$2\"")
    let ccBroken = tool("cc-runs-not", "printf '#!/bin/sh\\nexit 3\\n' > \"$2\"; chmod 755 \"$2\"")
    let swiftc = tool("swiftc", "printf '#!/bin/sh\\necho faber\\n' > \"$2\"; chmod 755 \"$2\"")
    let swiftcWrong = tool("swiftc-wrong", "printf '#!/bin/sh\\necho other\\n' > \"$2\"; chmod 755 \"$2\"")

    func run(_ select: String, _ cc: String, _ swiftc: String) -> CheckVerdict {
        toolchainVerdict(runCheckCommand(["/bin/sh", "-c", Toolchain.script, "sh", select, cc, swiftc], timeout: 30))
    }
    func fails(_ v: CheckVerdict, _ code: Int32) -> Bool {
        !v.ok && v.why.contains(Toolchain.failures[code]!)
    }

    let v = run(select, shim, shim)
    t.check("the broken image's shim fails the check", fails(v, 23), v.why)
    let none = run(unresolved, cc, swiftc)
    t.check("an xcode-select -p that does not resolve fails", fails(none, 20), none.why)
    let ok = run(select, cc, swiftc)
    t.check("a C and a Swift program that link and run pass", ok.ok && ok.why.hasPrefix("as runner: cc and swiftc link in /"), ok.why)
    let runsNot = run(select, ccBroken, swiftc)
    t.check("a C program that does not run fails", fails(runsNot, 24), runsNot.why)
    let noSwift = run(select, cc, shim)
    t.check("a swiftc that cannot link fails", fails(noSwift, 25), noSwift.why)
    let wrong = run(select, cc, swiftcWrong)
    t.check("a Swift program that prints the wrong thing fails", fails(wrong, 26), wrong.why)
    let gone = run(tool("xcode-select-gone", "echo /nonexistent/Xcode.app/Contents/Developer"), cc, swiftc)
    t.check("a developer directory that is not there fails", fails(gone, 21), gone.why)
    if geteuid() == 0 {
        t.skip("an unreadable developer directory fails", "root reads any directory")
    } else {
        _ = chmod(dev.path, 0o000)
        let shut = run(select, cc, swiftc)
        _ = chmod(dev.path, 0o755)
        t.check("an unreadable developer directory fails", fails(shut, 21), shut.why)
    }

    // runCheckCommand's timeout signals the command's process group, which only exists if
    // Process makes the child its leader; Apple does not document that, so it is checked here.
    let leader = Process()
    leader.executableURL = URL(fileURLWithPath: "/bin/sleep")
    leader.arguments = ["30"]
    leader.environment = ["PATH": Guest.systemPath, "LANG": Guest.locale, "LC_ALL": Guest.locale]
    leader.standardInput = FileHandle.nullDevice
    leader.standardOutput = Pipe()
    leader.standardError = FileHandle.nullDevice
    if (try? leader.run()) != nil {
        let pid = leader.processIdentifier
        t.check("Process makes its child a process group's leader", getpgid(pid) == pid, "pgid \(getpgid(pid)) pid \(pid)")
        leader.terminate()
        leader.waitUntilExit()
    } else {
        t.check("Process makes its child a process group's leader", false, "/bin/sleep did not start")
    }

    // A compiler that hangs: on the timeout it is stopped with the script, and the script's
    // scratch directory goes with it.
    let pidFile = bin.appendingPathComponent("hang.pid").path
    let dirFile = bin.appendingPathComponent("hang.dir").path
    let hang = tool("cc-hangs", "dirname \"$2\" > \(dirFile); echo $$ > \(pidFile); exec /bin/sleep 60")
    let timedOut = runCheckCommand(["/bin/sh", "-c", Toolchain.script, "sh", select, hang, swiftc], timeout: 2)
    t.check("a hung toolchain script times out", timedOut == nil)
    let hung = (try? String(contentsOfFile: pidFile, encoding: .utf8)).flatMap { pid_t($0.trimmingCharacters(in: .whitespacesAndNewlines)) }
    let scratchDir = (try? String(contentsOfFile: dirFile, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines)
    var hungGone = false
    for _ in 0..<20 {
        if let pid = hung, kill(pid, 0) != 0 { hungGone = true; break }
        usleep(100_000)
    }
    t.check("the hung compiler does not outlive the timeout", hungGone, "\(String(describing: hung))")
    t.check("the timed-out script removes its scratch directory",
            scratchDir.map { !$0.isEmpty && !FileManager.default.fileExists(atPath: $0) } ?? false,
            scratchDir ?? "no directory recorded")

    t.check("a script that did not run fails", !toolchainVerdict(nil).ok)
    t.check("an unknown status names itself", toolchainVerdict((7, Data())) == .fail("as runner: the toolchain script exit 7"))
    t.check("a pass with no path is no answer", !toolchainVerdict((0, Data())).ok)

    let version = xcodeVerdict("Xcode 27.0\nBuild version 18A5123b\n", expect.xcode)
    t.check("the xcode check passes only with both",
            xcodeCheckVerdict(version: version, toolchain: ok).ok
                && !xcodeCheckVerdict(version: version, toolchain: v).ok
                && !xcodeCheckVerdict(version: .fail("x"), toolchain: ok).ok)
    t.check("a matching Xcode does not hide a toolchain runner cannot use",
            xcodeCheckVerdict(version: version, toolchain: v) == v)
}
