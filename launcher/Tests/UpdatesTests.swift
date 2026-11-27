// ADR-0049, Software Update is off in the guest (amended 2026-10-26 and 2026-11-20): the agent
// writes the build's keys again when `serve` starts, before a job's `begin`, and whenever its
// watch finds one no longer false; the writes are exactly provision.sh's step a; a write that
// fails is a console line, never a dead agent; and the verification's check writes them once
// more and reads again before it fails, but still fails a Software Update that stays on.

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
    testKeyLost(t)
    testWatch(t)
    testRemoverEvidence(t)
    testSoftwareUpdateCheck(t)
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

    // A verification adds no write through the core: its check writes the keys only when the
    // plist fails it (testSoftwareUpdateCheck).
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

private var allOff: [String: Any] {
    ["AutomaticDownload": false, "CriticalUpdateInstall": false, "ConfigDataInstall": false, "AutomaticCheckEnabled": false]
}

@MainActor
private func testKeyLost(_ t: Harness) {
    t.check("all four false is nothing lost", updateKeyLost(allOff) == nil)
    var gone = allOff
    gone["AutomaticCheckEnabled"] = nil
    t.check("an absent key is lost", updateKeyLost(gone) == "AutomaticCheckEnabled absent", updateKeyLost(gone) ?? "nil")
    var on = allOff
    on["ConfigDataInstall"] = true
    t.check("a true key is lost", updateKeyLost(on) == "ConfigDataInstall=1", updateKeyLost(on) ?? "nil")
    t.check("an unreadable plist is lost", updateKeyLost(nil) == "plist unreadable")
    // The verification also judges AutomaticallyInstallMacOSUpdates, which the agent never
    // writes: the watch leaves it to the verification instead of rewriting forever.
    var macOS = allOff
    macOS["AutomaticallyInstallMacOSUpdates"] = true
    t.check("a key the agent does not write is not the watch's", updateKeyLost(macOS) == nil)
    t.check("but the verification still fails it", !softwareUpdateVerdict(macOS).ok)
}

/// A plist that a test changes, as macOS does under the agent.
private final class FakePlist: @unchecked Sendable {
    private let lock = NSLock()
    private var prefs: [String: Any]?
    init(_ prefs: [String: Any]?) { self.prefs = prefs }
    func read() -> [String: Any]? { lock.lock(); defer { lock.unlock() }; return prefs }
    func set(_ p: [String: Any]?) { lock.lock(); prefs = p; lock.unlock() }
    /// A `defaults write ... -bool false` lands in the fake, as cfprefsd would write it.
    func command(_ argv: [String]) -> (Int32, Data)? {
        if argv.first == UpdatePolicy.defaults, argv.count == 6, argv[2] == UpdatePolicy.softwareUpdateDomain {
            lock.lock()
            var p = prefs ?? [:]
            p[argv[3]] = false
            prefs = p
            lock.unlock()
        }
        return (0, Data())
    }
}

@MainActor
private func testWatch(_ t: Harness) {
    let plist = FakePlist(allOff)
    let commands = Recorded<[String]>()
    let lines = Recorded<String>()
    let log = Data(#"{"processImagePath":"/usr/libexec/x/softwareupdated","eventMessage":"Automatic check reset"}"#.utf8)
    let watch = UpdateWatch(read: { plist.read() }, run: { argv, _ in
        commands.add(argv)
        return argv.first == UpdatePolicy.log ? (0, log) : plist.command(argv)
    }, console: { lines.add($0) })

    watch.tick()
    t.check("keys that are off are left alone", commands.all.isEmpty && lines.all.isEmpty)
    t.check("no restore yet", watch.restores.count == 0 && watch.restores.first == nil)

    // macOS removes AutomaticCheckEnabled after the agent's write, as on the builder host.
    var gone = allOff
    gone["AutomaticCheckEnabled"] = nil
    plist.set(gone)
    watch.tick()
    t.check("a lost key gets every write again, then the log is read",
            commands.all.dropLast().map { $0 } == updatesOffWrites() && commands.all.last?.first == UpdatePolicy.log,
            "\(commands.all)")
    t.check("the log read asks for the guest's own recent log as ndjson",
            commands.all.last.map { $0.contains("--last") && $0.contains("ndjson") && $0.contains(UpdatePolicy.logPredicate) } == true)
    t.check("the key is back", updateKeyLost(plist.read()) == nil)
    t.check("the restore names its key",
            lines.all.first?.hasPrefix("FABER-AGENT updates_lost after=") == true
                && lines.all.first?.hasSuffix("restores=1 why=AutomaticCheckEnabled absent") == true, "\(lines.all)")
    t.check("then the writes' own line", lines.all.dropFirst().first == "FABER-AGENT updates_off at=watch ok=true wrote=5/5")
    t.check("then who logged about it",
            lines.all.dropFirst(2).first == "FABER-AGENT evidence updates_lost=AutomaticCheckEnabled absent log=exit 0 logged=softwareupdated:1",
            "\(lines.all)")
    t.check("the restore is counted", watch.restores.count == 1 && watch.restores.first?.why == "AutomaticCheckEnabled absent")

    let before = lines.all.count
    watch.tick()
    t.check("a key that stays put needs nothing more", lines.all.count == before)

    // A second loss writes again, but the log is read the first time only.
    plist.set(gone)
    let logs = commands.all.filter { $0.first == UpdatePolicy.log }.count
    watch.tick()
    t.check("a later loss is restored too", updateKeyLost(plist.read()) == nil && watch.restores.count == 2)
    t.check("the log is read once", commands.all.filter { $0.first == UpdatePolicy.log }.count == logs)
    t.check("the first loss stays the first", watch.restores.first?.why == "AutomaticCheckEnabled absent")

    // A remover that never stops: every restore happens, but the console is not flooded.
    for _ in 0..<(UpdatePolicy.quietEvery * 2) {
        plist.set(gone)
        watch.tick()
    }
    let restores = watch.restores.count
    let lost = lines.all.filter { $0.contains("updates_lost after=") }.count
    t.check("every loss is restored", restores == 2 + UpdatePolicy.quietEvery * 2)
    t.check("only the first restores and every quietEvery-th are written out",
            lost == UpdatePolicy.loudRestores + restores / UpdatePolicy.quietEvery, "\(lost) of \(restores)")
    t.check("every watch line is bounded and printable",
            lines.all.allSatisfy { $0.utf8.count <= Diagnostics.maxLine && $0.utf8.allSatisfy { (0x20...0x7e).contains($0) } })

    // A write that fails is a line, and the watch keeps going.
    let stuck = FakePlist(gone)
    let broken = UpdateWatch(read: { stuck.read() }, run: { _, _ in nil }, console: { lines.add($0) })
    broken.tick()
    broken.tick()
    t.check("a watch whose writes fail keeps counting", broken.restores.count == 2)
    t.check("and says the writes failed",
            lines.all.contains("FABER-AGENT updates_off at=watch ok=false wrote=0/5 failed=AutomaticDownload did not run or timed out"))
    t.check("and that the log did not run",
            lines.all.contains("FABER-AGENT evidence updates_lost=AutomaticCheckEnabled absent log=did not run or timed out"))
}

@MainActor
private func testRemoverEvidence(_ t: Harness) {
    let ndjson = [
        #"{"processImagePath":"/System/Library/CoreServices/Software Update.app/Contents/Resources/softwareupdated","eventMessage":"BackgroundActions: Automatic check parameters: autoDownload=NO"}"#,
        #"{"processImagePath":"/usr/sbin/cfprefsd","eventMessage":"wrote <private>"}"#,
        "not json at all",
        #"{"processImagePath":"/usr/sbin/cfprefsd","eventMessage":"wrote <private>"}"#,
        #"{"processImagePath":"/usr/libexec/mdmclient","eventMessage":"AutomaticCheckEnabled reset"}"#,
        #"{"processImagePath":"/System/Library/CoreServices/Software Update.app/Contents/Resources/softwareupdated","eventMessage":"\#(String(repeating: "automatic ", count: 40))\u001b"}"#,
        #"{"processImagePath":"/usr/sbin/cfprefsd","eventMessage":"Automatic \nsecond line"}"#,
    ].joined(separator: "\n")
    let out = removerEvidenceLines(lost: "AutomaticCheckEnabled absent", (0, Data(ndjson.utf8)))
    t.check("the first line counts each process, most first, ties in the log's order",
            out.first == "FABER-AGENT evidence updates_lost=AutomaticCheckEnabled absent log=exit 0 logged=cfprefsd:3,softwareupdated:2,mdmclient:1",
            out.first ?? "none")
    t.check("then the newest messages about automatic checks, at most three",
            out.count == 1 + UpdatePolicy.quotedMessages
                && out[1].hasSuffix("said=mdmclient: AutomaticCheckEnabled reset")
                && out[3].hasSuffix("said=cfprefsd: Automatic "), "\(out)")
    t.check("every evidence line is bounded and printable",
            out.allSatisfy { $0.utf8.count <= Diagnostics.maxLine && $0.utf8.allSatisfy { (0x20...0x7e).contains($0) } })
    t.check("a quoted message is cut to its own bound",
            out[2].hasSuffix("said=softwareupdated: " + String(repeating: "automatic ", count: 10)), out[2])
    t.check("an empty log says none",
            removerEvidenceLines(lost: "x", (0, Data())) == ["FABER-AGENT evidence updates_lost=x log=exit 0 logged=none"])

    t.check("the watch evidence without a watch says so",
            watchEvidenceLine(nil) == "FABER-GOLDEN-VERIFY evidence check=software_update watch=not running")
    t.check("a quiet watch says none", watchEvidenceLine((0, nil)) == "FABER-GOLDEN-VERIFY evidence check=software_update watch_restores=0")
    t.check("a watch that restored says how often and what it first found",
            watchEvidenceLine((3, ("AutomaticCheckEnabled absent", 61)))
                == "FABER-GOLDEN-VERIFY evidence check=software_update watch_restores=3 first=AutomaticCheckEnabled absent after=61s")
}

@MainActor
private func testSoftwareUpdateCheck(_ t: Harness) {
    var gone = allOff
    gone["AutomaticCheckEnabled"] = nil
    let wrote = "FABER-AGENT updates_off at=check ok=true wrote=5/5"

    // Off at the first read: nothing written, nothing waited for.
    var steps: [String] = []
    let (pass, none) = softwareUpdateCheck(judge: { steps.append("judge"); return softwareUpdateVerdict(allOff) },
                                           reapply: { steps.append("reapply"); return wrote },
                                           settle: { steps.append("settle") })
    t.check("a plist that is off passes at once", pass.ok && none == nil && steps == ["judge"], "\(steps)")
    t.check("and the verdict is the plist's", pass == softwareUpdateVerdict(allOff))

    // The race: macOS removed the key, the watch had not put it back yet.
    let plist = FakePlist(gone)
    steps = []
    let (raced, line) = softwareUpdateCheck(
        judge: { steps.append("judge"); return softwareUpdateVerdict(plist.read()) },
        reapply: { steps.append("reapply"); return reapplyUpdatesOff(at: .check, run: { argv, _ in plist.command(argv) }) },
        settle: { steps.append("settle") })
    t.check("a lost key is written again, waited on, and read again", steps == ["judge", "reapply", "settle", "judge"], "\(steps)")
    t.check("then passes, naming what the first read found",
            raced == .pass("written again at the check, then off (first read: AutomaticCheckEnabled absent (it defaults to on))"),
            raced.why)
    t.check("and its writes are a line of their own", line == wrote, line ?? "nil")

    // Really on: whatever turned it on does so again before the second read.
    let (on, _) = softwareUpdateCheck(judge: { softwareUpdateVerdict(gone) }, reapply: { wrote }, settle: {})
    t.check("a key that stays lost still fails", on == .fail("AutomaticCheckEnabled absent (it defaults to on) (written again at the check)"),
            on.why)
    // Writes that fail cannot pass it either.
    let (failed, failedLine) = softwareUpdateCheck(judge: { softwareUpdateVerdict(gone) },
                                                   reapply: { reapplyUpdatesOff(at: .check, run: { _, _ in (1, Data()) }) },
                                                   settle: {})
    t.check("failed writes fail the check", !failed.ok && failedLine?.contains("ok=false") == true)
    // A key the agent never writes is not cured by writing.
    var macOS = allOff
    macOS["AutomaticallyInstallMacOSUpdates"] = true
    t.check("automatic macOS installs on still fail",
            !softwareUpdateCheck(judge: { softwareUpdateVerdict(macOS) }, reapply: { wrote }, settle: {}).0.ok)
}
