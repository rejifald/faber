// The runner's exit (launcher/agent/Sources/RunnerTail.swift): whether it took a job, read
// from its `_diag`'s names, and the last lines it wrote, read as root and copied to the
// console redacted and bounded. No guest: the files are laid out in a scratch "home" owned by
// this test's own uid, which stands in for runner's.

import Darwin
import Foundation

private let jit = "eyJydW5uZXIiOiJmYWJlci1qb2IiLCJ0b2tlbiI6InNlY3JldC12YWx1ZS1mb3ItdGVzdHMifQ=="

@MainActor
func testRunnerTail(_ t: Harness, _ scratch: Scratch) {
    testRunnerJobWord(t)
    testTailLines(t)
    testTailRedaction(t)
    testTailBlock(t)
    testReadingTails(t, scratch)
}

@MainActor
private func testRunnerJobWord(_ t: Harness) {
    let listener = "Runner_20250101-120000-utc.log"
    let worker = "Worker_20250101-120003-utc.log"
    t.check("a listener log alone is a runner that took no job", runnerJob(diagNames: [listener]) == .notStarted)
    t.check("a worker log is a runner that was handed a job", runnerJob(diagNames: [listener, worker]) == .started)
    t.check("an empty _diag took no job", runnerJob(diagNames: []) == .notStarted)
    t.check("a worker file that is not a log is no job", runnerJob(diagNames: [listener, "Worker_20250101.txt"]) == .notStarted)
    t.check("the newest listener log is the greatest name",
            newestRunnerDiag(["Runner_20250101-110000-utc.log", worker, "Runner_20250101-120000-utc.log", "Runner_x.txt"])
                == "Runner_20250101-120000-utc.log")
    t.check("no listener log, no newest", newestRunnerDiag([worker, "pages"]) == nil)
    t.check("the runner's own log name is shown as it is", isRunnerDiagName(listener))
    t.check("a name the runner did not give is not",
            !isRunnerDiagName("Runner_\(jit).log") && !isRunnerDiagName("Runner_2025010x-120000-utc.log")
                && !isRunnerDiagName("Runner_20250101-120000-utc.log.bak") && !isRunnerDiagName("Runner_20250101_120000-utc.log"))
    let planted = runnerTailBlock(RunnerTailFacts(output: .failure(Failure("none")),
                                                  diag: .success((name: "Runner_\(jit).log", lines: []))))
    t.check("a planted log name is redacted on the console", !planted.joined().contains("eyJydW5u"), "\(planted)")
    t.check("the job words are the protocol's", RunnerJob.started.rawValue == "started" && RunnerJob.notStarted.rawValue == "none")
}

@MainActor
private func testTailLines(_ t: Harness) {
    let text = Array("one\r\ntwo\nthree\n".utf8)
    t.check("lines lose their CR and the last newline", tailLines(text, count: 5, partial: false) == ["one", "two", "three"])
    t.check("only the last lines are kept", tailLines(text, count: 2, partial: false) == ["two", "three"])
    t.check("a read that began mid-file drops its first fragment",
            tailLines(Array("ne\ntwo\nthree".utf8), count: 5, partial: true) == ["two", "three"])
    t.check("an empty file has no lines", tailLines([], count: 5, partial: false).isEmpty)
    t.check("an empty line in the middle is kept", tailLines(Array("a\n\nb\n".utf8), count: 5, partial: false) == ["a", "", "b"])
}

@MainActor
private func testTailRedaction(_ t: Harness) {
    t.check("the runner's own words pass", redactTail("2025-01-01 12:00:01Z: Listening for Jobs") == "2025-01-01 12:00:01Z: Listening for Jobs")
    t.check("a JIT config is redacted wherever it is", !redactTail("config \(jit) here").contains("eyJydW5u"))
    t.check("a JWT's segments are redacted", !redactTail("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJydW5uZXIifQ.c2lnbmF0dXJlc2lnbmF0dXJl").contains("eyJ"))
    t.check("a header cuts the line after its name",
            redactTail("Authorization: Bearer abc") == "Authorization <redacted>", redactTail("Authorization: Bearer abc"))
    t.check("a token is cut whatever its shape", !redactTail("the token is hunter2").contains("hunter2"))
    t.check("a secret word after a non-ASCII char is still cut", !redactTail("\u{130}\u{130} token is hunter2").contains("hunter2"))
    t.check("the earliest secret word decides the cut", redactTail("password then token: x") == "password <redacted>")
    t.check("a lowercase path survives", redactTail("at /private/var/faber/tools/faber-agent") == "at /private/var/faber/tools/faber-agent")
    t.check("a short mixed-case word survives", redactTail("Runner.Listener exited") == "Runner.Listener exited")
}

@MainActor
private func testTailBlock(_ t: Harness) {
    let facts = RunnerTailFacts(output: .success(["√ Connected to GitHub", "Listening for Jobs", "the token: x"]),
                                diag: .success((name: "Runner_20250101-120000-utc.log", lines: ["[INFO Runner] started"])),
                                job: .notStarted)
    let block = runnerTailBlock(facts)
    t.check("the tail starts with its output header", block.first == "FABER-AGENT runner output: the last 3 lines run.sh wrote to stdout and stderr",
            "\(block)")
    t.check("each output line is tagged and sanitized", block.contains("FABER-AGENT runner out| ??? Connected to GitHub"), "\(block)")
    t.check("a line with a secret word is cut", block.contains("FABER-AGENT runner out| the token <redacted>"), "\(block)")
    t.check("the diag header names the file", block.contains("FABER-AGENT runner diag: the last 1 lines of _diag/Runner_20250101-120000-utc.log"),
            "\(block)")
    t.check("every tail line is printable ASCII and bounded",
            block.allSatisfy { $0.utf8.count <= Diagnostics.maxLine && $0.utf8.allSatisfy { (0x20...0x7e).contains($0) } })

    let missing = RunnerTailFacts(output: .failure(Failure("_faber-runner-output.log: No such file or directory")),
                                  diag: .failure(Failure("_diag: No such file or directory")))
    t.check("a missing file says so, and only that",
            runnerTailBlock(missing) == ["FABER-AGENT runner output: none read: _faber-runner-output.log: No such file or directory",
                                         "FABER-AGENT runner diag: none read: _diag: No such file or directory"],
            "\(runnerTailBlock(missing))")

    let long = String(repeating: "x ", count: 300)
    let full = RunnerTailFacts(output: .success(Array(repeating: long, count: 60)),
                               diag: .success((name: "Runner_1.log", lines: Array(repeating: long, count: 60))))
    let capped = runnerTailBlock(full)
    let bytes = capped.reduce(0) { $0 + $1.utf8.count + 1 }
    t.check("the tail is held to its byte cap", bytes <= RunnerTail.maxBytes, "\(bytes)")
    t.check("the tail is held to its line cap", capped.count <= RunnerTail.maxLines, "\(capped.count)")
    t.check("each file gives at most its own lines",
            runnerTailBlock(RunnerTailFacts(output: .success(Array(repeating: "x", count: 60)), diag: .failure(Failure("none"))))
                .filter { $0.hasPrefix("FABER-AGENT runner out|") }.count == RunnerTail.lines)
}

@MainActor
private func testReadingTails(_ t: Harness, _ scratch: Scratch) {
    let uid = getuid()
    do {
        let home = try scratch.dir("runner-tail-home")
        let dir = try scratch.dir("runner-tail-home/\(Guest.runnerDirName)")
        let diag = try scratch.dir("runner-tail-home/\(Guest.runnerDirName)/_diag")
        try scratch.write(dir.appendingPathComponent(RunnerTail.outputName), Data("one\ntwo\nthree\n".utf8))
        try scratch.write(diag.appendingPathComponent("Runner_20250101-110000-utc.log"), Data("old\n".utf8))
        try scratch.write(diag.appendingPathComponent("Runner_20250101-120000-utc.log"), Data("a\nb\n".utf8))

        var facts = readRunnerTails(home: home.path, uid: uid)
        t.check("root reads run.sh's output", (try? facts.output.get()) == ["one", "two", "three"], "\(facts.output)")
        t.check("root reads the newest listener log",
                (try? facts.diag.get().name) == "Runner_20250101-120000-utc.log" && (try? facts.diag.get().lines) == ["a", "b"],
                "\(facts.diag)")
        t.check("no worker log: no job, by root's own reading", facts.job == .notStarted)
        t.check("runner-exec's reading agrees", readRunnerJob(dir: dir.path) == .notStarted)

        try scratch.write(diag.appendingPathComponent("Worker_20250101-120003-utc.log"), Data("w\n".utf8))
        facts = readRunnerTails(home: home.path, uid: uid)
        t.check("a worker log: a job, by root", facts.job == .started)
        t.check("a worker log: a job, by runner-exec", readRunnerJob(dir: dir.path) == .started)

        t.check("a home of another account's is read for nothing",
                readRunnerTails(home: home.path, uid: uid &+ 1).job == nil
                    && (try? readRunnerTails(home: home.path, uid: uid &+ 1).output.get()) == nil)

        // A planted symlink or a second name for a file is never read through.
        let output = dir.appendingPathComponent(RunnerTail.outputName)
        let elsewhere = try scratch.write(scratch.url.appendingPathComponent("runner-tail-elsewhere"), Data("not runner's\n".utf8))
        try FileManager.default.removeItem(at: output)
        try FileManager.default.createSymbolicLink(at: output, withDestinationURL: elsewhere)
        t.check("a symlinked output file is refused", (try? readRunnerTails(home: home.path, uid: uid).output.get()) == nil)
        try FileManager.default.removeItem(at: output)
        guard link(elsewhere.path, output.path) == 0 else { throw Failure("link: \(errnoText(errno))") }
        t.check("a hard-linked output file is refused", (try? readRunnerTails(home: home.path, uid: uid).output.get()) == nil)

        let bare = try scratch.dir("runner-tail-bare/\(Guest.runnerDirName)")
        t.check("no _diag at all: the listener never started, so no job", readRunnerJob(dir: bare.path) == .notStarted)
        let odd = try scratch.dir("runner-tail-odd/\(Guest.runnerDirName)")
        try scratch.write(odd.appendingPathComponent("_diag"), Data("a file\n".utf8))
        t.check("a _diag that is no directory: unknown", readRunnerJob(dir: odd.path) == nil)
    } catch {
        t.check("the runner's files", false, "\(error)")
    }
}
