// The hand-off's console lines (launcher/agent/Sources/HandoffDiagnostics.swift) and the
// supervisor keeping a failed job's console (launcher/Sources/Supervisor.swift). No guest: the
// pure pieces are driven with the facts a guest would give them.

import Darwin
import Foundation

private let secret = "eyJydW5uZXIiOiJmYWJlci1qb2IiLCJ0b2tlbiI6InNlY3JldC12YWx1ZS1mb3ItdGVzdHMifQ=="

@MainActor
func testHandoffDiagnostics(_ t: Harness, _ scratch: Scratch) {
    testHandoffLines(t)
    testToolsReach(t)
    testHandoffOnThisHost(t)
    testFailedJobLogs(t, scratch)
}

@MainActor
private func testHandoffLines(_ t: Harness) {
    let line = handoffLine("bootstrap", ok: false, "launchctl bootstrap gui/502: exit 5 'Bootstrap failed: 5: Input/output error'")
    t.check("a hand-off line has the agent's tag, its step and verdict",
            line == "FABER-AGENT handoff step=bootstrap ok=false why=launchctl bootstrap gui/502: exit 5 'Bootstrap failed: 5: Input/output error'",
            line)
    let long = handoffLine("connect", ok: false, String(repeating: "x ", count: 300) + "\n\u{1b}[2J")
    t.check("a hand-off line is bounded and printable ASCII",
            long.utf8.count <= Diagnostics.maxLine && long.utf8.allSatisfy { (0x20...0x7e).contains($0) }, long)
    let leaked = handoffLine("run", ok: false, "env ACTIONS_RUNNER_INPUT_JITCONFIG=\(secret) said no")
    t.check("a JIT config is redacted wherever it would appear", !leaked.contains("eyJydW5u") && leaked.contains("<redacted>"), leaked)
    t.check("a run of the config's alphabet split by '/' is redacted too",
            !redactSecrets("a " + secret.prefix(20) + "/" + secret.dropFirst(20) + " b").contains("eyJy"))
    let paths = "tar -xzf /private/var/faber/tools/actions-runner-osx-arm64-2.338.1.tar.gz -C /Users/runner/faber-runner"
    t.check("paths and launchd's facts are not redacted", redactSecrets(paths) == paths, redactSecrets(paths))

    // runner-exec's steps, in root's words: the object from root's own paths, the integer.
    let dir = "/Users/runner/faber-runner"
    let tarball = "/private/var/faber/tools/actions-runner-osx-arm64-2.338.1.tar.gz"
    t.check("an extract that failed names tar's exit and where",
            runnerStepWhy(HandoffStepResult(step: .extract, ok: false, exitCode: 2), tarball: tarball, dir: dir)
                == "tar -xzf \(tarball) -C \(dir): exit 2")
    t.check("a mkdir that failed names its errno",
            runnerStepWhy(HandoffStepResult(step: .mkdir, ok: false, errno: Int(EEXIST)), tarball: tarball, dir: dir)
                == "\(dir): File exists (errno 17)")
    t.check("a manifest read as runner names its errno",
            runnerStepWhy(failedStep(.manifest, errno: EACCES), tarball: tarball, dir: dir)
                == "\(Guest.manifestPath): Permission denied (errno 13)")
    t.check("a step that went well names its object only",
            runnerStepWhy(HandoffStepResult(step: .configRead, ok: true), tarball: tarball, dir: dir)
                == "runner-exec reading the config from root")
    t.check("a spawn killed by a signal says so",
            runnerStepWhy(failedStep(.spawn, exit: .signal(9)), tarball: tarball, dir: dir) == "\(dir)/run.sh: signal 9")
    t.check("an integer outside the hand-off's ranges is left out, never clamped",
            failedStep(.extract, errno: 300, exit: .code(300)) == HandoffStepResult(step: .extract, ok: false))

    // launchd's words for a service that did not run.
    let printed = """
        gui/502/io.github.rejifald.faber.runner = {
        \tactive count = 0
        \tpath = /private/var/faber/handoff/io.github.rejifald.faber.runner.plist
        \tstate = not running
        \tprogram = /private/var/faber/tools/faber-agent
        \truns = 1
        \tlast exit code = 78: EX_CONFIG
        \tlast terminating signal = Killed: 9
        \tenvironment = {
        \t\tPATH => /usr/bin:/bin:/usr/sbin:/sbin
        \t}
        \tstate = running
        }
        """
    t.check("launchd's service facts are its state, runs and last exit, each once",
            launchdServiceFacts(printed) == "state=not running, runs=1, last exit code=78: EX_CONFIG, last terminating signal=Killed: 9",
            launchdServiceFacts(printed))
    t.check("output with no facts says so", launchdServiceFacts("Could not find service") == "no service facts")
}

@MainActor
private func testToolsReach(_ t: Harness) {
    t.check("a path's prefixes start at /", pathPrefixes("/private/var/faber/tools/faber-agent")
        == ["/", "/private", "/private/var", "/private/var/faber", "/private/var/faber/tools", "/private/var/faber/tools/faber-agent"])
    let runner: uid_t = 502
    let groups: Set<gid_t> = [20, 12, 61]
    func dir(_ p: String, _ uid: uid_t = 0, _ gid: gid_t = 0, mode: mode_t) -> PathFacts {
        PathFacts(path: p, uid: uid, gid: gid, mode: S_IFDIR | mode, isDirectory: true)
    }
    func file(_ p: String, _ uid: uid_t = 0, _ gid: gid_t = 0, mode: mode_t) -> PathFacts {
        PathFacts(path: p, uid: uid, gid: gid, mode: S_IFREG | mode, isDirectory: false)
    }
    let above = [dir("/", mode: 0o755), dir("/private", mode: 0o755), dir("/private/var", mode: 0o755),
                 dir("/private/var/faber", mode: 0o755)]
    // The failure this guards against: hdiutil, run as _faber, stamped _faber's uid and gid on the
    // volume and gave its root the 0700 of the staging folder; the daemon's umask left the files 0640.
    let built = above + [dir("/private/var/faber/tools", 299, 299, mode: 0o700), file("/private/var/faber/tools/faber-agent", 299, 299, mode: 0o555)]
    t.check("a tools disk whose root is 0700 and another uid's stops runner at the root",
            runnerDenied(built, need: .execute, uid: runner, groups: groups)
                == "/private/var/faber/tools mode 700 uid 299 gid 299: runner cannot search it")
    let fixed = above + [dir("/private/var/faber/tools", 299, 299, mode: 0o755)]
    t.check("a searchable root lets runner run the agent",
            runnerDenied(fixed + [file("/private/var/faber/tools/faber-agent", 299, 299, mode: 0o555)], need: .execute, uid: runner, groups: groups) == nil)
    t.check("a tarball readable by its owner and group only stops runner",
            runnerDenied(fixed + [file("/private/var/faber/tools/t.tar.gz", 299, 299, mode: 0o640)], need: .read, uid: runner, groups: groups)
                == "/private/var/faber/tools/t.tar.gz mode 640 uid 299 gid 299: runner cannot read it")
    t.check("a 0444 tarball is read", runnerDenied(fixed + [file("/private/var/faber/tools/t.tar.gz", 299, 299, mode: 0o444)],
                                                   need: .read, uid: runner, groups: groups) == nil)
    t.check("the owner's bits decide for the owner, even when other's would allow",
            !modeAllows(file("/f", runner, 20, mode: 0o044), uid: runner, groups: groups, bit: 4))
    t.check("the group's bits decide for a member",
            modeAllows(file("/f", 0, 20, mode: 0o040), uid: runner, groups: groups, bit: 4)
                && !modeAllows(file("/f", 0, 20, mode: 0o004), uid: runner, groups: groups, bit: 4))
    t.check("a file that is not executable stops launchd",
            runnerDenied(fixed + [file("/private/var/faber/tools/faber-agent", 299, 299, mode: 0o444)], need: .execute, uid: runner, groups: groups)
                == "/private/var/faber/tools/faber-agent mode 444 uid 299 gid 299: runner cannot execute it")
}

/// root's side of the hand-off on this host, which has no guest: it stops at the first step
/// the host cannot give it (no `runner` console user, or a state directory only root may
/// make), says so in one line, and never reaches launchctl bootstrap or the config.
@MainActor
private func testHandoffOnThisHost(_ t: Harness) {
    let lines = LinesBox()
    let ran = LinesBox()
    let core = AgentCore(consoleIsRunner: { false }, probe: { _, _, c in c }, startRunner: { _, _ in })
    let threw: Bool
    do {
        try runHandoff(JitConfig(secret)!, core, DiagnosticSink(write: { lines.add($0) }), run: { argv, _ in
            ran.add(argv.joined(separator: " "))
            return (0, Data())
        })
        threw = false
    } catch {
        threw = true
    }
    let all = lines.all
    t.check("the hand-off stops on a host that is no guest", threw, "\(all)")
    t.check("its last line names the step that failed",
            all.last.map { $0.hasPrefix("FABER-AGENT handoff step=") && $0.contains(" ok=false why=") } == true, "\(all)")
    t.check("every line is a bounded hand-off line",
            !all.isEmpty && all.allSatisfy { $0.hasPrefix("FABER-AGENT handoff step=") && $0.utf8.count <= Diagnostics.maxLine })
    t.check("no line carries the config", !all.contains { $0.contains("eyJydW5u") })
    t.check("nothing is bootstrapped", !ran.all.contains { $0.contains("bootstrap") }, "\(ran.all)")
}

private final class LinesBox: @unchecked Sendable {
    private let lock = NSLock()
    private var lines: [String] = []
    func add(_ l: String) { lock.lock(); lines.append(l); lock.unlock() }
    var all: [String] { lock.lock(); defer { lock.unlock() }; return lines }
}

@MainActor
private func testFailedJobLogs(_ t: Harness, _ scratch: Scratch) {
    t.check("a verification's log is kept whatever its end",
            StopReason.allCases.allSatisfy { consoleLogDisposition(verifyRan: true, jobFinished: false, reason: $0) == .keepVerify })
    t.check("a job that finished has its log removed",
            StopReason.allCases.allSatisfy { consoleLogDisposition(verifyRan: false, jobFinished: true, reason: $0) == .remove })
    t.check("a stop faber asked for removes the log",
            consoleLogDisposition(verifyRan: false, jobFinished: false, reason: .requested) == .remove)
    for reason in StopReason.allCases where reason != .requested {
        t.check("a job VM stopped by \(reason.rawValue) keeps its log",
                consoleLogDisposition(verifyRan: false, jobFinished: false, reason: reason) == .keepFailedJob)
    }
    t.check("a failed job's names", KeptConsoleLog.failedJob.latest(image: "xcode-27.1") == "job-xcode-27.1-failed-latest.log"
        && KeptConsoleLog.failedJob.archived(image: "xcode-27.1", stamp: 7) == "job-xcode-27.1-failed-7.log")
    let names = ["job-img-failed-1.log", "job-img-failed-2.log", "job-img-failed-latest.log", "verify-img-1.log",
                 "job-img-x-failed-1.log", "job-img-failed-.log"]
    t.check("pruning a failed job's logs never touches a verification's or another image's",
            keptLogsToPrune(names, prefix: KeptConsoleLog.failedJob.prefix(image: "img"), keep: 2) == ["job-img-failed-1.log"],
            "\(keptLogsToPrune(names, prefix: KeptConsoleLog.failedJob.prefix(image: "img"), keep: 2))")

    do {
        let dir = try scratch.dir("failed-job-logs")
        let fd = open(dir.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        defer { close(fd) }
        func put(_ name: String, _ text: String) throws { try scratch.write(dir.appendingPathComponent(name), Data(text.utf8)) }
        func read(_ name: String) -> String? { try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8) }
        try put("verify-img-latest.log", "a verification\n")
        for n in 1...7 {
            try put("console-slot3-img.log", "job \(n)\n")
            var times = [timespec(tv_sec: 2_000_000 + n, tv_nsec: 0), timespec(tv_sec: 2_000_000 + n, tv_nsec: 0)]
            _ = utimensat(fd, "job-img-failed-latest.log", &times, AT_SYMLINK_NOFOLLOW)
            keepConsoleLog(.failedJob, dirFD: fd, dir: dir, slot: 3)
        }
        let kept = ((try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []).filter { $0.hasPrefix("job-img-failed-") }
        var st = stat()
        t.check("a failed job's console is kept, rotated, the last \(verifyLogsKept) only",
                kept.count == verifyLogsKept && read("job-img-failed-latest.log") == "job 7\n"
                    && read("job-img-failed-2000007.log") == "job 6\n", "\(kept.sorted())")
        t.check("the kept log is 0600", stat(dir.appendingPathComponent("job-img-failed-latest.log").path, &st) == 0
            && st.st_mode & 0o777 == 0o600)
        t.check("a verification's log is left as it was", read("verify-img-latest.log") == "a verification\n")
    } catch {
        t.check("the failed job's log files", false, "\(error)")
    }
}
