// ADR-0049, Software Update is off in the guest (amended 2026-10-26): the agent writes the
// build's keys again when `serve` starts and before a job's `begin`, the writes are exactly
// provision.sh's step a, and a write that fails is a console line, never a dead agent.

import Darwin
import Foundation

/// A thread-safe recorder.
private final class Recorded<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [T] = []
    func add(_ x: T) { lock.lock(); items.append(x); lock.unlock() }
    var all: [T] { lock.lock(); defer { lock.unlock() }; return items }
}

/// provision.sh's step a, its `defaults` commands expanded: the section between its `# --- a.`
/// header and the next header, the `for k in ...; do` loop unrolled. nil when the section is
/// missing or holds a command this reader does not know, so a reshaped step a fails the parity
/// test instead of passing it by reading nothing.
func provisionStepAWrites(_ text: String) -> [[String]]? {
    let lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    guard let start = lines.firstIndex(where: { $0.hasPrefix("# --- a. ") }),
          let end = lines[(start + 1)...].firstIndex(where: { $0.hasPrefix("# --- ") }) else { return nil }
    var out: [[String]] = []
    var loop: [String]?
    for raw in lines[(start + 1)..<end] {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if line.isEmpty || line.hasPrefix("#") || line.hasPrefix("begin ") { continue }
        if line.hasPrefix("for k in "), line.hasSuffix("; do"), loop == nil {
            loop = line.dropFirst("for k in ".count).dropLast("; do".count).split(separator: " ").map(String.init)
            continue
        }
        if line == "done", loop != nil {
            loop = nil
            continue
        }
        let words = line.split(separator: " ").map(String.init)
        guard words.first == "defaults" else { return nil }
        if let keys = loop {
            for k in keys { out.append(words.map { $0 == "\"$k\"" ? k : $0 }) }
        } else {
            out.append(words)
        }
    }
    return loop == nil ? out : nil
}

@MainActor
func testUpdates(_ t: Harness, provision: String?) {
    testUpdatesParity(t, provision: provision)
    testReapply(t)
    testReapplyInTheCore(t)
    testScheduleEvidence(t)
}

@MainActor
private func testUpdatesParity(_ t: Harness, provision: String?) {
    // The path comes from test.sh's own arguments; a missing file fails, never skips.
    guard let provision, let text = try? String(contentsOfFile: provision, encoding: .utf8) else {
        t.check("provision.sh is readable for the key parity", false, provision ?? "no path")
        return
    }
    guard let build = provisionStepAWrites(text) else {
        t.check("provision.sh's step a parses", false)
        return
    }
    let agent = updatesOffWrites().map { ["defaults"] + $0.dropFirst() }
    t.check("the agent's writes are provision.sh's step a, key for key and in order", build == agent,
            "provision \(build) agent \(agent)")
    t.check("step a turns off every key the verification judges",
            Set(Checks.updateKeys).isSubset(of: Set(build.compactMap { $0.count > 3 ? $0[3] : nil })))
    t.check("every write is a -bool false", updatesOffWrites().allSatisfy { Array($0.suffix(2)) == ["-bool", "false"] })
    t.check("the agent names defaults by its absolute path", updatesOffWrites().allSatisfy { $0[0] == "/usr/bin/defaults" })
    t.check("the verification reads the domain the writes go to",
            Checks.softwareUpdatePlist == UpdatePolicy.softwareUpdateDomain + ".plist")

    // The reader itself: a reshaped step a is refused, not read as empty.
    t.check("a step a with another command is refused",
            provisionStepAWrites("# --- a. x\nrm -f /x\n# --- b. y\n") == nil)
    t.check("no step a is refused", provisionStepAWrites("# --- b. y\n") == nil)
    t.check("an unclosed loop is refused",
            provisionStepAWrites("# --- a. x\nfor k in A B; do\n  defaults write d \"$k\" -bool false\n# --- b. y\n") == nil)
}

@MainActor
private func testReapply(_ t: Harness) {
    let calls = Recorded<([String], TimeInterval)>()
    let ok = reapplyUpdatesOff(at: .serve, run: { argv, timeout in
        calls.add((argv, timeout))
        return (0, Data())
    })
    t.check("every write runs, in order", calls.all.map(\.0) == updatesOffWrites())
    t.check("each write has its deadline", calls.all.allSatisfy { $0.1 == UpdatePolicy.writeTimeout })
    t.check("a clean re-apply says so", ok == "FABER-AGENT updates_off at=serve ok=true wrote=5/5", ok)

    let tried = Recorded<String>()
    let bad = reapplyUpdatesOff(at: .begin, run: { argv, _ in
        tried.add(argv[3])
        switch argv[3] {
        case "CriticalUpdateInstall": return (1, Data("Could not write domain".utf8))
        case "AutoUpdate": return nil
        default: return (0, Data())
        }
    })
    t.check("a failed write does not stop the others", tried.all == updatesOffWrites().map { $0[3] })
    t.check("a failed re-apply is a line naming the first failure",
            bad == "FABER-AGENT updates_off at=begin ok=false wrote=3/5 failed=CriticalUpdateInstall exit 1", bad)
    t.check("the line never carries what the command printed", !bad.contains("Could not"))
    let none = reapplyUpdatesOff(at: .serve, run: { _, _ in nil })
    t.check("nothing that runs is ok=false, not a crash",
            none == "FABER-AGENT updates_off at=serve ok=false wrote=0/5 failed=AutomaticDownload did not run or timed out", none)
}

/// The core writes the keys at serve start and before the hand-off, and a failed write stops
/// neither.
@MainActor
private func testReapplyInTheCore(_ t: Harness) {
    let events = Recorded<String>()
    let lines = Recorded<String>()
    let core = AgentCore(
        consoleIsRunner: { true }, probe: { _, _, n in n },
        startRunner: { _, _ in events.add("start") },
        reapplyUpdates: { at in
            events.add("reapply \(at.rawValue)")
            // Every write fails: the agent logs it and carries on.
            lines.add(reapplyUpdatesOff(at: at, run: { _, _ in (1, Data()) }))
        })
    t.check("nothing is written before serve starts", events.all.isEmpty)
    core.serveStarting()
    t.check("serve start writes the keys once", events.all == ["reapply serve"])
    var greeted = false
    _ = core.answer(.hello(protocol: agentProtocol), greeted: &greeted)
    t.check("a failed write at serve start still leaves the agent ready",
            core.answer(.status, greeted: &greeted) == (.status(AgentStatus(.ready)), true))
    _ = core.answer(.probe(addr: "192.168.100.1", port: 9, count: 1), greeted: &greeted)
    let jit = JitConfig("c2VjcmV0LWppdC1jb25maWctZm9yLXRlc3Rz")!
    t.check("begin is accepted at once", core.answer(.begin(jitConfig: jit), greeted: &greeted) == (.accepted, true))
    let deadline = Date().addingTimeInterval(3)
    while events.all.count < 3, Date() < deadline { usleep(20_000) }
    t.check("begin writes the keys again, before the hand-off", events.all == ["reapply serve", "reapply begin", "start"],
            "\(events.all)")
    t.check("each failed re-apply was a line", lines.all.count == 2 && lines.all.allSatisfy { $0.contains("ok=false") })

    // A verification writes nothing: it judges the plist as the clone left it.
    let verifying = Recorded<String>()
    let vcore = AgentCore(consoleIsRunner: { true }, probe: { _, _, n in n }, startRunner: { _, _ in },
                          runChecks: { _ in [] }, reapplyUpdates: { at in verifying.add(at.rawValue) })
    vcore.serveStarting()
    var vg = false
    _ = vcore.answer(.hello(protocol: agentProtocol), greeted: &vg)
    _ = vcore.answer(.probe(addr: "192.168.100.1", port: 9, count: 1), greeted: &vg)
    _ = vcore.answer(.verify(VerifyExpect(xcode: nil, runtimes: [])), greeted: &vg)
    usleep(100_000)
    t.check("verify adds no write of its own", verifying.all == ["serve"], "\(verifying.all)")
}

@MainActor
private func testScheduleEvidence(_ t: Harness) {
    let said = scheduleEvidenceLine((0, Data("Automatic checking for updates is turned off.\nmore\n".utf8)))
    t.check("the evidence line carries softwareupdate's first line",
            said == "FABER-GOLDEN-VERIFY evidence check=software_update schedule=exit 0 'Automatic checking for updates is turned off.'", said)
    t.check("it is not a check line", !said.contains(" ok="))
    t.check("a softwareupdate that did not run says so",
            scheduleEvidenceLine(nil) == "FABER-GOLDEN-VERIFY evidence check=software_update schedule=did not run or timed out")
    let long = scheduleEvidenceLine((0, Data((String(repeating: "x", count: 500) + "\u{1b}").utf8)))
    t.check("the evidence line is bounded and printable",
            long.utf8.count <= Diagnostics.maxLine && long.utf8.allSatisfy { (0x20...0x7e).contains($0) })
}
