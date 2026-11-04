// The supervisor and the child protocol. No VM is ever booted: a child is a shell script
// that sleeps, /bin/sleep itself, or an in-process ChildServer standing in for `run`.

import Darwin
import Foundation
import Virtualization

/// A short scratch directory: a Unix socket path must fit in 103 bytes, and $TMPDIR (on a CI
/// runner above all) can be deeper than that before any suffix. The child sockets live in
/// the state directory, so it is under here too.
private final class ShortScratch {
    let url: URL

    init() throws {
        var template = Array("/tmp/fvm.XXXXXX".utf8CString)
        guard let made = mkdtemp(&template) else { throw Failure("mkdtemp: \(errnoText(errno))") }
        url = try resolvedURL(URL(fileURLWithPath: String(cString: made)))
    }

    func dir(_ name: String, mode: mode_t = 0o700) throws -> URL {
        let d = url.appendingPathComponent(name)
        guard mkdir(d.path, mode) == 0 || errno == EEXIST, chmod(d.path, mode) == 0 else {
            throw Failure("mkdir \(d.path): \(errnoText(errno))")
        }
        return d
    }

    func remove() { try? FileManager.default.removeItem(at: url) }
}

/// A client speaking raw lines, with a receive timeout so a broken supervisor fails a test
/// instead of hanging it.
private final class Client {
    let fd: Int32
    private var reader: FrameReader

    init(_ path: URL) throws {
        fd = try connectUnix(path, timeoutSeconds: 5)
        reader = FrameReader(fd: fd)
    }

    deinit { close(fd) }

    /// Send one line; return the reply line without its LF, or nil on EOF.
    func call(_ line: String) -> String? {
        guard (try? writeAll(fd, Array((line + "\n").utf8))) != nil else { return nil }
        return read()
    }

    func send(_ raw: [UInt8]) { try? writeAll(fd, raw) }

    func read() -> String? {
        guard let reply = try? reader.next() else { return nil }
        return String(decoding: reply.dropLast(), as: UTF8.self)
    }

    /// True when the peer closed and sent nothing at all.
    func closedSilently() -> Bool {
        var byte: UInt8 = 0
        return Darwin.read(fd, &byte, 1) == 0
    }
}

private let helloLine = #"{"id":1,"request":{"hello":{"protocol":\#(supervisorProtocol)}}}"#

/// A supervisor running on a thread of its own, as launchd would run the real one.
private final class Running: @unchecked Sendable {
    let supervisor: Supervisor
    private let done = DispatchSemaphore(value: 0)

    init(_ config: SupervisorConfig) throws {
        supervisor = try Supervisor(config)
        Thread.detachNewThread { [self] in
            supervisor.run()
            done.signal()
        }
    }

    /// Wait for the loop to return; false if it did not within the timeout.
    func finished(within seconds: Double = 10) -> Bool { done.wait(timeout: .now() + seconds) == .success }

    func stop() {
        supervisor.requestExit()
        _ = finished()
    }
}

private struct Dirs {
    let scratch: ShortScratch
    let socket: URL
    let state: URL
    let images: URL
    let tools: URL
    let clones: URL

    init() throws {
        scratch = try ShortScratch()
        socket = try scratch.dir("sock").appendingPathComponent("s")
        state = try scratch.dir("state")
        images = try scratch.dir("images")
        tools = try scratch.dir("tools")
        clones = try scratch.dir("clones")
    }

    func config(peer: uid_t = geteuid(), executable: String = "/usr/bin/true", plan: NetworkPlan? = nil) -> SupervisorConfig {
        SupervisorConfig(socket: socket, peerUID: peer, stateDir: state, imageDir: images, toolsDir: tools,
                         cloneDir: clones, plan: plan, executable: executable, version: "0.0.0-test",
                         handleSignals: false)
    }

    func stateFD() throws -> Int32 { try openStateDirectory(state) }
}

private func list(_ path: URL) -> [VmRecord]? {
    let c = try? Client(path)
    guard c?.call(helloLine) != nil, let line = c?.call(#"{"id":2,"request":{"list":{}}}"#),
          let frame = try? decodeResponseForTest(Array((line + "\n").utf8)), case .vms(let vms) = frame.reply else { return nil }
    return vms
}

private func request(_ path: URL, _ body: String) -> String? {
    let c = try? Client(path)
    guard c?.call(helloLine) != nil else { return nil }
    return c?.call(#"{"id":2,"request":"# + body + "}")
}

/// Poll `list` until `ok` holds, for up to `seconds`: the supervisor learns of exits on its
/// one-second tick.
private func waitForList(_ path: URL, seconds: Double = 6, _ ok: ([VmRecord]) -> Bool) -> [VmRecord]? {
    let deadline = Date().addingTimeInterval(seconds)
    var last: [VmRecord]?
    repeat {
        last = list(path)
        if let last, ok(last) { return last }
        usleep(200_000)
    } while Date() < deadline
    return last
}

/// Kill and reap a child of this process, only while it is still unreaped: a reaped pid may
/// already belong to someone else.
private func reap(_ pid: pid_t) {
    var status: Int32 = 0
    guard pid > 0, waitpid(pid, &status, WNOHANG) == 0 else { return }
    kill(pid, SIGKILL)
    waitpid(pid, &status, 0)
}

/// A child that ignores its arguments and sleeps, keeping the slot lock it inherited.
private func sleeperScript(_ dir: URL, exitCode: Int? = nil) throws -> String {
    let path = dir.appendingPathComponent(exitCode.map { "exit-\($0)" } ?? "sleeper").path
    let body = exitCode.map { "#!/bin/sh\nexit \($0)\n" } ?? "#!/bin/sh\nexec /bin/sleep 30\n"
    try Data(body.utf8).write(to: URL(fileURLWithPath: path))
    chmod(path, 0o700)
    return path
}

private func marker(slot: Int, kind: VmKind = .linux, claim: String, pid: pid_t, startedUs: UInt64,
                    childProtocol v: Int = childProtocol, bundle: String? = nil) -> ClaimMarker {
    ClaimMarker(childProtocol: v, slot: slot, kind: kind, claim: claim, pid: pid, pidStartedUs: startedUs,
                startedUnix: 1_760_700_000, bundle: bundle)
}

private func scratch_write(_ url: URL) throws {
    try Data().write(to: url, options: .withoutOverwriting)
}

private func makeMarkedClone(_ dir: URL, _ name: String, marked: Bool = true) throws {
    let url = dir.appendingPathComponent(name)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
    if marked { try Data(cloneMarkerContents.utf8).write(to: url.appendingPathComponent(cloneMarkerName)) }
}

@MainActor
func testSupervisor(_ t: Harness) {
    testPureDecisions(t)
    testFrameCap(t)
    testMarkerFiles(t)
    testChildServer(t)
    testHandshakeAndPeers(t)
    testAdoption(t)
    testFirstStopReachesAStoppingChild(t)
    testStartAndExit(t)
    testChildRepliesAreChecked(t)
    testAdoptionLoadsEveryMarkerFirst(t)
    testProtectedSet(t)
    testDiskGrowth(t)
}

/// ADR-0061 §4: `start.disk_bytes` grows a stopped Linux VM's disk file, never shrinks it, never
/// past 1 TiB or the volume's free space, never in place on root's image, and the record reports
/// the file's length and held blocks.
@MainActor
private func testDiskGrowth(_ t: Harness) {
    let gib: UInt64 = 1 << 30
    t.check("growing within the free space is allowed", diskGrowthRefusal(current: 64 * gib, target: 96 * gib, free: 40 * gib) == nil)
    t.check("the same size is allowed (nothing to do)", diskGrowthRefusal(current: 64 * gib, target: 64 * gib, free: 0) == nil)
    t.check("a size smaller than the file is refused", diskGrowthRefusal(current: 96 * gib, target: 64 * gib, free: 1 << 50) != nil)
    t.check("a size that is not whole MiB is refused", diskGrowthRefusal(current: 0, target: 64 * gib + 1, free: 1 << 50) != nil)
    t.check("a size above 1 TiB is refused", diskGrowthRefusal(current: 0, target: (1 << 40) + (1 << 20), free: 1 << 50) != nil)
    t.check("growth larger than the free space is refused", diskGrowthRefusal(current: 64 * gib, target: 96 * gib, free: 32 * gib - 1) != nil)
    t.check("decode refuses disk_bytes on a macOS start",
            (try? decodeRequest(Array(#"{"id":1,"request":{"start":{"slot":0,"kind":"macos","claim":"a","image":"b","tools":"t","cpus":1,"memory_mib":1024,"disk_bytes":1048576}}}"#.utf8) + [0x0A])) == nil)
    t.check("decode refuses disk_bytes that is not whole MiB",
            (try? decodeRequest(Array(#"{"id":1,"request":{"start":{"slot":0,"kind":"linux","claim":"a","image":"b","cpus":1,"memory_mib":1024,"disk_bytes":1048577}}}"#.utf8) + [0x0A])) == nil)
    t.check("decode refuses disk_bytes above 1 TiB",
            (try? decodeRequest(Array(#"{"id":1,"request":{"start":{"slot":0,"kind":"linux","claim":"a","image":"b","cpus":1,"memory_mib":1024,"disk_bytes":1099512676352}}}"#.utf8) + [0x0A])) == nil)
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let disk = dirs.scratch.url.appendingPathComponent("disk.raw")
        let fd = open(disk.path, O_CREAT | O_WRONLY, 0o600)
        ftruncate(fd, off_t(1 << 20))
        close(fd)
        t.check("a disk is grown with its inode kept", {
            var before = stat()
            lstat(disk.path, &before)
            let grown = growLinuxDisk(disk, to: 4 << 20)
            var after = stat()
            lstat(disk.path, &after)
            return grown == .grown(from: 1 << 20) && after.st_size == 4 << 20 && after.st_ino == before.st_ino
        }())
        t.check("the same size again changes nothing", growLinuxDisk(disk, to: 4 << 20) == .unchanged)
        if case .refused = growLinuxDisk(disk, to: 2 << 20) {
            t.check("a smaller size is refused and the file kept", diskSize(disk)?.bytes == 4 << 20)
        } else {
            t.check("a smaller size is refused and the file kept", false)
        }
        let link = dirs.scratch.url.appendingPathComponent("link.raw")
        symlink(disk.path, link.path)
        if case .failed = growLinuxDisk(link, to: 8 << 20) {
            t.check("a symlink is never followed", diskSize(disk)?.bytes == 4 << 20)
        } else {
            t.check("a symlink is never followed", false)
        }
        t.check("the record's size is the length, the held bytes from the blocks",
                diskSize(disk).map { $0.bytes == 4 << 20 && $0.allocated < 4 << 20 } ?? false)

        let linux = try dirs.scratch.dir("linux")
        t.check("no bundle: no disk to report", soleLinuxDisk(in: linux) == nil)
        let one = try dirs.scratch.dir("linux/linux")
        try Data().write(to: one.appendingPathComponent("disk.raw"))
        t.check("one bundle: its disk", soleLinuxDisk(in: linux)?.lastPathComponent == "disk.raw")
        let two = try dirs.scratch.dir("linux/other")
        try Data().write(to: two.appendingPathComponent("disk.raw"))
        t.check("two bundles: no guess", soleLinuxDisk(in: linux) == nil)

        // A start with disk_bytes and no --linux-dir would grow root's image in place: refused.
        _ = try dirs.scratch.dir("images/linux-engine")
        let running = try Running(dirs.config())
        defer { running.stop() }
        t.check("disk_bytes on a boot in place is bad_request",
                request(dirs.socket, #"{"start":{"slot":5,"kind":"linux","claim":"a","image":"linux-engine","cpus":1,"memory_mib":1024,"disk_bytes":1048576}}"#)
                    == #"{"id":2,"reply":{"error":{"code":"bad_request"}}}"#)
        t.check("a record with a disk carries both sizes, after started_unix", {
            var w = JSONWriter()
            VmRecord(slot: 14, kind: .linux, claim: "e", state: .inService, stopReason: nil, startedUnix: 1,
                     disk: (bytes: 2 << 20, allocated: 1 << 20)).write(&w)
            return String(decoding: w.bytes, as: UTF8.self).hasSuffix(#""started_unix":1,"disk_bytes":2097152,"disk_allocated_bytes":1048576}"#)
        }())
        t.check("a macOS record may not carry a disk",
                (try? VmRecord(slot: 1, kind: .macos, claim: "m", state: .inService, stopReason: nil, startedUnix: 1,
                               disk: (bytes: 1, allocated: 1)).validate()) == nil)
    } catch {
        t.check("disk growth fixtures", false, "\(error)")
    }
}

/// What the clone sweep may never touch (ADR-0026, AGENTS.md: read labels, never names): the
/// golden images, the tools disks, the claim markers, a Linux bundle even if marked, and any
/// directory without the marker. Only a marked macOS clone no live marker names goes.
@MainActor
private func testProtectedSet(_ t: Harness) {
    func refused(_ clones: String, _ other: String) -> Bool {
        (try? requireSeparateCloneDir(URL(fileURLWithPath: clones), from: [("--image-dir", URL(fileURLWithPath: other))])) == nil
    }
    t.check("a clone dir equal to another dir is refused", refused("/x/images", "/x/images"))
    t.check("a clone dir inside another dir is refused", refused("/x/state/clones", "/x/state"))
    t.check("a clone dir containing another dir is refused", refused("/x", "/x/tools"))
    t.check("a sibling sharing a name prefix is no overlap", !refused("/x/clones", "/x/clone"))
    t.check("disjoint dirs are accepted", !refused("/x/clones", "/y/images"))
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        for (what, dir) in [("state", dirs.state), ("image", dirs.images), ("tools", dirs.tools)] {
            var config = dirs.config()
            config.cloneDir = dir
            t.check("the supervisor refuses a clone dir that is its \(what) dir", (try? Supervisor(config)) == nil)
        }
        let nested = try dirs.scratch.dir("images/clones")
        var inside = dirs.config()
        inside.cloneDir = nested
        t.check("the supervisor refuses a clone dir inside its image dir", (try? Supervisor(inside)) == nil)

        func write(_ dir: URL, _ name: String) throws {
            try Data("x".utf8).write(to: dir.appendingPathComponent(name))
        }
        // A Linux bundle carrying the clone marker: the marker lies, the disk stays.
        try makeMarkedClone(dirs.clones, "linux-marked")
        try write(dirs.clones.appendingPathComponent("linux-marked"), "disk.raw")
        // A macOS bundle without the marker: not faber's clone.
        try makeMarkedClone(dirs.clones, "mac-unmarked", marked: false)
        try write(dirs.clones.appendingPathComponent("mac-unmarked"), "vz-hwmodel")
        // An orphaned marked macOS clone: the one thing the sweep removes.
        try makeMarkedClone(dirs.clones, "mac-orphan")
        try write(dirs.clones.appendingPathComponent("mac-orphan"), "vz-hwmodel")
        // Marked-looking directories in the golden image and tools dirs: never swept.
        try makeMarkedClone(dirs.images, "golden")
        try write(dirs.images.appendingPathComponent("golden"), "vz-hwmodel")
        try makeMarkedClone(dirs.tools, "tools-dir-entry")
        try write(dirs.tools, "tools-1")

        let running = try Running(dirs.config())
        running.stop()
        t.check("the sweep removes an orphaned marked macOS clone", !pathExists(dirs.clones.appendingPathComponent("mac-orphan")))
        t.check("a marked Linux bundle in the clone dir survives the sweep",
                pathExists(dirs.clones.appendingPathComponent("linux-marked/disk.raw")))
        t.check("an unmarked macOS bundle survives the sweep", pathExists(dirs.clones.appendingPathComponent("mac-unmarked/vz-hwmodel")))
        t.check("the golden image dir is never touched", pathExists(dirs.images.appendingPathComponent("golden/vz-hwmodel")))
        t.check("the tools dir is never touched", pathExists(dirs.tools.appendingPathComponent("tools-1"))
            && pathExists(dirs.tools.appendingPathComponent("tools-dir-entry")))
        t.check("deleteClone refuses a marked Linux bundle by name too", !running.supervisor.deleteClone("linux-marked"))
    } catch {
        t.check("protected-set fixtures", false, "\(error)")
    }
}

@MainActor
private func testPureDecisions(_ t: Harness) {
    let me = geteuid()
    t.check("the expected peer is admitted", admitPeer(me, expected: me))
    t.check("another uid is refused", !admitPeer(me &+ 1, expected: me))
    t.check("an unknown peer is refused", !admitPeer(nil, expected: me))

    t.check("asked for: requested, whatever the exit", stopReasonForExit(requested: true, exitCode: 1, everBooted: false) == .requested)
    t.check("exit 4: wall_violation", stopReasonForExit(requested: false, exitCode: 4, everBooted: true) == .wallViolation)
    t.check("exit 5: agent_error", stopReasonForExit(requested: false, exitCode: 5, everBooted: true) == .agentError)
    t.check("exit 5 asked for: requested", stopReasonForExit(requested: true, exitCode: 5, everBooted: true) == .requested)
    t.check("exit 6: runner_exited_without_job",
            stopReasonForExit(requested: false, exitCode: noJobExitCode, everBooted: true) == .runnerExitedWithoutJob)
    t.check("exit 6 asked for: requested", stopReasonForExit(requested: true, exitCode: noJobExitCode, everBooted: true) == .requested)
    t.check("the no-job stop's wire word", StopReason.runnerExitedWithoutJob.rawValue == "runner_exited_without_job")
    t.check("a tools name resolves to <name>.img in the cache",
            toolsDiskURL(toolsDir: URL(fileURLWithPath: "/c/tools"), name: "runner-2.330.0-agent-0.4.0").path == "/c/tools/runner-2.330.0-agent-0.4.0.img")
    t.check("exit 0: guest_shutdown", stopReasonForExit(requested: false, exitCode: 0, everBooted: false) == .guestShutdown)
    t.check("exit 1 before booting: start_failed", stopReasonForExit(requested: false, exitCode: 1, everBooted: false) == .startFailed)
    t.check("exit 1 after booting: launcher_exited", stopReasonForExit(requested: false, exitCode: 1, everBooted: true) == .launcherExited)
    t.check("unknowable: launcher_exited", stopReasonForExit(requested: false, exitCode: nil, everBooted: false) == .launcherExited)

    t.accepts("a stale socket of this account may be removed") {
        try checkStaleSocket(fakeStat(mode: S_IFSOCK | 0o600, uid: me), euid: me, path: "/x")
    }
    t.refuses("a regular file at the socket path is never removed", containing: "not a socket") {
        try checkStaleSocket(fakeStat(mode: S_IFREG | 0o600, uid: me), euid: me, path: "/x")
    }
    t.refuses("another account's socket is never removed", containing: "owned by uid") {
        try checkStaleSocket(fakeStat(mode: S_IFSOCK | 0o600, uid: me &+ 1), euid: me, path: "/x")
    }

    // The child's command line is one run accepts, and carries no secret.
    let start = StartRequest(slot: 3, kind: .macos, claim: "c-1", image: "macos-27", tools: "tools-1", cpus: 2, memoryMiB: 4096)
    let state = URL(fileURLWithPath: "/opt/fvm/state")
    let network = try? VmnetPlan(subnet: IPv4Subnet.parse("192.168.100.0/24"), reservation: IPv4.parse("192.168.100.9"))
    let args = childArguments(executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/c/vm-3"), start: start,
                              stateDir: state, tools: URL(fileURLWithPath: "/t/tools-1"), toolsOwner: 450, network: network)
    t.check("the child runs this binary's run", args.prefix(3) == ["/x/faber-vm", "run", "/c/vm-3"])
    do {
        let o = try RunOptions(parse(Array(args.dropFirst(3)), valued: RunOptions.valued), forRun: true)
        t.check("run reads the child's options", o.claim == "c-1" && o.slotLockFD == 3 && o.memoryBytes == 4096 << 20
            && o.serveSocket?.path == "/opt/fvm/state/slot-3.sock" && o.vmnet?.guest.description == "192.168.100.9"
            && o.toolsDisk?.path == "/t/tools-1" && o.toolsOwner == 450 && o.cpus == 2, "\(args)")
    } catch {
        t.check("run accepts the child's command line", false, "\(error)")
    }
    let noTools = childArguments(executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/c/vm-3"), start: start,
                                 stateDir: state, tools: nil, toolsOwner: 450, network: network)
    t.check("a child with no tools disk gets no --tools-owner", !noTools.contains("--tools-owner"), "\(noTools)")
    t.refuses("--tools-owner without --tools-disk", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--state-dir", "/s", "--network", "shared-nat",
                                  "--tools-owner", "450"], valued: RunOptions.valued), forRun: true)
    }
    t.refuses("--tools-owner outside the supervisor's child", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--state-dir", "/s", "--network", "shared-nat",
                                  "--tools-disk", "/t/x", "--tools-owner", "450"], valued: RunOptions.valued), forRun: true)
    }
    t.refuses("--tools-owner naming root", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--state-dir", "/s", "--network", "shared-nat", "--claim", "a",
                                  "--serve-socket", "/s/slot-1.sock", "--tools-disk", "/t/x", "--tools-owner", "0"],
                                 valued: RunOptions.valued), forRun: true)
    }
    t.refuses("--serve-socket elsewhere than the state dir", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--state-dir", "/s", "--claim", "a", "--serve-socket", "/tmp/x.sock",
                                  "--network", "shared-nat"], valued: RunOptions.valued), forRun: true)
    }
    // A real state directory under macOS's /var symlink: the supervisor passes resolved
    // paths (/private/var/...), and either spelling names the same socket (October 2026).
    do {
        let tmp = canonicalURL(NSTemporaryDirectory()).appendingPathComponent("fvm-serve-\(getpid())")
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        let real = tmp.path
        let other = real.hasPrefix("/private/") ? String(real.dropFirst("/private".count)) : real
        for (dir, sock) in [(real, real), (other, real), (real, other)] {
            let o = try RunOptions(parse(["--slot", "14", "--state-dir", dir, "--claim", "linux-engine",
                                          "--serve-socket", sock + "/slot-14.sock", "--network", "shared-nat"],
                                         valued: RunOptions.valued), forRun: true)
            t.check("a /private spelling of the state dir is the same place (\(dir), \(sock))",
                    o.serveSocket?.path == real + "/slot-14.sock", "\(String(describing: o.serveSocket))")
        }
    } catch {
        t.check("the /private state dir parses", false, "\(error)")
    }
    t.refuses("--claim without --serve-socket", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--state-dir", "/s", "--claim", "a", "--network", "shared-nat"],
                                 valued: RunOptions.valued), forRun: true)
    }
    t.refuses("--memory-mib with --memory-gib", usage: true) {
        _ = try RunOptions(parse(["--slot", "1", "--memory-mib", "2048", "--memory-gib", "2", "--network", "shared-nat"],
                                 valued: RunOptions.valued), forRun: false)
    }
    t.check("--memory-gib still works", (try? RunOptions(parse(["--slot", "1", "--memory-gib", "2", "--network", "shared-nat"],
                                                               valued: RunOptions.valued), forRun: false))?.memoryBytes == 2048 << 20)
}

@MainActor
private func testFrameCap(_ t: Harness) {
    var pair: [Int32] = [-1, -1]
    guard socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0 else {
        t.check("socketpair", false, errnoText(errno))
        return
    }
    defer { close(pair[0]); close(pair[1]) }
    // The writer blocks once the socket buffer is full, so it runs on a thread of its own.
    let writer = pair[1]
    let written = DispatchSemaphore(value: 0)
    Thread.detachNewThread {
        let payload = [UInt8](repeating: 0x61, count: maxFrame) + Array("tail\n".utf8)
        try? writeAll(writer, payload)
        written.signal()
    }
    var reader = FrameReader(fd: pair[0])
    do {
        _ = try reader.next()
        t.check("an oversized frame is refused", false)
    } catch {
        t.check("an oversized frame is refused as too large", error as? ProtoError == .frameTooLarge, "\(error)")
    }
    _ = written.wait(timeout: .now() + 5)
    var rest = [UInt8](repeating: 0, count: 64)
    let n = rest.withUnsafeMutableBytes { read(pair[0], $0.baseAddress, 64) }
    t.check("the reader stopped at MAX_FRAME: what follows is still unread", n == 5 && Array(rest[0..<5]) == Array("tail\n".utf8), "n=\(n)")
}

@MainActor
private func testMarkerFiles(_ t: Harness) {
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let fd = try dirs.stateFD()
        defer { close(fd) }
        var m = marker(slot: 2, kind: .macos, claim: "m-1", pid: 0, startedUs: 0, bundle: "vm-2-x")
        try writeClaimMarker(dirFD: fd, m)
        t.refuses("a second marker for the slot is refused (O_EXCL)", containing: "exists") { try writeClaimMarker(dirFD: fd, m) }
        var st = stat()
        let path = dirs.state.appendingPathComponent("slot-2.claim").path
        t.check("a marker is 0600", lstat(path, &st) == 0 && st.st_mode & 0o777 == 0o600)
        let before = st.st_ino
        m.pid = 4242
        m.pidStartedUs = 99
        try replaceClaimMarker(dirFD: fd, m)
        t.check("the pid update replaces the marker in one rename", lstat(path, &st) == 0 && st.st_ino != before)
        t.check("the replaced marker reads back", readMarkerFile(dirFD: fd, name: "slot-2.claim").flatMap { try? ClaimMarker.decode($0) } == m)
        t.check("no temporary file is left", !pathExists(dirs.state.appendingPathComponent("slot-2.claim.new")))
        try FileManager.default.createSymbolicLink(atPath: dirs.state.appendingPathComponent("slot-5.claim").path, withDestinationPath: path)
        t.check("a symlinked marker is not read", readMarkerFile(dirFD: fd, name: "slot-5.claim") == nil)
        t.check("an absent lock file is a free slot", slotLockIsFree(dirFD: fd, slot: 7))
        let held = try HeldLock.acquire(in: fd, name: "slot-7.lock", displayPath: "slot-7.lock", busy: "busy")
        t.check("a held lock is not free", !slotLockIsFree(dirFD: fd, slot: 7))
        // run's side of the hand-down: the inherited descriptor is accepted as the slot lock.
        t.accepts("run inherits the slot lock the supervisor took") { _ = try Slot(7).inheritLock(fd: held.fd, stateDir: dirs.state) }
        t.refuses("a descriptor that is not the slot's lock is refused", containing: "is not") {
            _ = try Slot(6).inheritLock(fd: held.fd, stateDir: dirs.state)
        }
        close(held.fd)
        t.check("a released lock is free", slotLockIsFree(dirFD: fd, slot: 7))
    } catch {
        t.check("marker files", false, "\(error)")
    }
}

/// A guest agent that is there, for the transitions the absent one never reaches.
private struct PresentAgent: GuestAgent {
    var isPresent: Bool { true }
    func sendProbe() throws {}
    func begin(jitConfig: JitConfig?) throws {}
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func set() { lock.lock(); value = true; lock.unlock() }
    var isSet: Bool { lock.lock(); defer { lock.unlock() }; return value }
}

@MainActor
private func testChildServer(_ t: Harness) {
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let status = ChildStatus()
        let stopped = Flag()
        let server = try ChildServer(path: dirs.state.appendingPathComponent("slot-1.sock"), slot: 1, claim: "cl",
                                     status: status, agent: AbsentGuestAgent()) { stopped.set() }
        var greeted = false
        t.check("the first request must be hello", server.answer(.status, greeted: &greeted) == (.error(.handshakeRequired), false))
        t.check("another child version is refused", server.answer(.hello(childProtocol: 99), greeted: &greeted)
            == (.error(.versionMismatch, childProtocol: childProtocol), false))
        t.check("hello names the slot and claim", server.answer(.hello(childProtocol: childProtocol), greeted: &greeted)
            == (.hello(childProtocol: childProtocol, slot: 1, claim: "cl"), true))
        t.check("a second hello is refused", server.answer(.hello(childProtocol: childProtocol), greeted: &greeted) == (.error(.badRequest), false))
        t.check("status starts at starting", server.answer(.status, greeted: &greeted) == (.status(state: .starting, stopReason: nil), true))
        t.check("running is booting", status.move(from: .starting, to: .booting))
        t.check("no agent: probe is wrong_state", server.answer(.probe, greeted: &greeted) == (.error(.wrongState), true))
        t.check("no agent: begin is wrong_state", server.answer(.begin(jitConfig: JitConfig("ZmFrZQ==")), greeted: &greeted) == (.error(.wrongState), true))
        t.check("stop answers done", server.answer(.stop, greeted: &greeted) == (.done, true) && stopped.isSet)
        t.check("stop moves to stopping", status.state == .stopping)
        status.set(.ready)
        t.check("nothing leaves stopping", status.state == .stopping)
        server.close()

        let s2 = ChildStatus()
        let present = try ChildServer(path: dirs.state.appendingPathComponent("slot-2.sock"), slot: 2, claim: "c2",
                                      status: s2, agent: PresentAgent()) {}
        var g2 = true
        s2.set(.ready)
        t.check("with an agent, probe from ready is probe_sent", present.answer(.probe, greeted: &g2) == (.status(state: .probeSent, stopReason: nil), true))
        t.check("begin from probe_sent is in_service", present.answer(.begin(jitConfig: JitConfig("ZmFrZQ==")), greeted: &g2) == (.status(state: .inService, stopReason: nil), true))
        t.check("probe again is wrong_state", present.answer(.probe, greeted: &g2) == (.error(.wrongState), true))

        // Over the socket: the supervisor's client, and a refusal of the wrong claim.
        present.start()
        let reply = try? callChild(socket: present.path, version: childProtocol, slot: 2, claim: "c2", request: .status)
        t.check("callChild reads the child's state", reply == .status(state: .inService, stopReason: nil), "\(String(describing: reply))")
        t.check("a child naming another claim is refused",
                (try? callChild(socket: present.path, version: 1, slot: 2, claim: "other", request: .status)) == nil)
        t.check("a child naming another slot is refused",
                (try? callChild(socket: present.path, version: 1, slot: 3, claim: "c2", request: .status)) == nil)
        present.close()
    } catch {
        t.check("child server", false, "\(error)")
    }
}

@MainActor
private func testHandshakeAndPeers(_ t: Harness) {
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }

        // Another account's connection: closed with nothing read and nothing written.
        let refusing = try Running(dirs.config(peer: geteuid() &+ 1))
        var st = stat()
        t.check("the socket is 0660", lstat(dirs.socket.path, &st) == 0 && st.st_mode & 0o777 == 0o660)
        if let c = try? Client(dirs.socket) {
            c.send(Array((helloLine + "\n").utf8))
            t.check("a peer of another uid sees EOF and zero bytes", c.closedSilently())
        } else {
            t.check("connect", false)
        }
        refusing.stop()
        t.check("the socket is removed on exit", !pathExists(dirs.socket))

        let running = try Running(dirs.config())
        let c1 = try Client(dirs.socket)
        t.check("a request before hello is handshake_required",
                c1.call(#"{"id":5,"request":{"list":{}}}"#) == #"{"id":5,"reply":{"error":{"code":"handshake_required"}}}"#)
        t.check("... and the connection is closed", c1.closedSilently())

        let c2 = try Client(dirs.socket)
        t.check("hello answers with the version", c2.call(helloLine) == #"{"id":1,"reply":{"hello":{"protocol":\#(supervisorProtocol),"supervisor_version":"0.0.0-test"}}}"#)
        t.check("an empty list", c2.call(#"{"id":2,"request":{"list":{}}}"#) == #"{"id":2,"reply":{"vms":{"vms":[]}}}"#)
        t.check("an unknown claim", c2.call(#"{"id":3,"request":{"stop":{"claim":"nope"}}}"#) == #"{"id":3,"reply":{"error":{"code":"unknown_claim"}}}"#)
        t.check("a second hello is bad_request", c2.call(helloLine) == #"{"id":1,"reply":{"error":{"code":"bad_request"}}}"#)
        t.check("... and closes", c2.closedSilently())

        let c3 = try Client(dirs.socket)
        _ = c3.call(helloLine)
        t.check("a readable bad frame is bad_request", c3.call(#"{"id":9,"request":{"list":{"x":1}}}"#) == #"{"id":9,"reply":{"error":{"code":"bad_request"}}}"#)
        t.check("... and closes", c3.closedSilently())

        let c4 = try Client(dirs.socket)
        _ = c4.call(helloLine)
        c4.send(Array("{\"id\":9,\"request\"\n".utf8))
        t.check("an unreadable frame closes without a reply", c4.closedSilently())

        let c5 = try Client(dirs.socket)
        c5.send([UInt8](repeating: 0x61, count: maxFrame))
        t.check("an oversized frame closes without a reply", c5.closedSilently())

        let c6 = try Client(dirs.socket)
        _ = c6.call(helloLine)
        c6.send(Array(#"{"id":2,"request":{"list":{}}}"#.utf8) + [0x0A] + Array(#"{"id":3,"request":{"list":{}}}"#.utf8) + [0x0A])
        t.check("two frames before a reply close without one (lockstep)", c6.closedSilently())

        let begin = #"{"id":2,"request":{"begin":{"claim":"x","jit_config":"ZmFrZQ=="}}}"#
        t.check("begin for an unknown claim", request(dirs.socket, #"{"begin":{"claim":"x","jit_config":"ZmFrZQ=="}}"#)
            == #"{"id":2,"reply":{"error":{"code":"unknown_claim"}}}"#, begin)

        // A connection cap: with four connections served (each proven by its hello), a fifth
        // is accepted and closed at once.
        usleep(300_000)  // let the supervisor see the connections above close
        do {
            let held = (0..<4).compactMap { _ in try? Client(dirs.socket) }
            t.check("four connections are served", held.count == 4 && held.allSatisfy { $0.call(helloLine) != nil })
            // ARC may release `held` after its last use; it must outlive the fifth connection.
            withExtendedLifetime(held) {
                if let extra = try? Client(dirs.socket) {
                    t.check("a connection past the cap is closed silently", extra.closedSilently())
                }
            }
        }
        usleep(300_000)

        let c7 = try Client(dirs.socket)
        t.check("hello at another version is version_mismatch with ours",
                c7.call(#"{"id":1,"request":{"hello":{"protocol":1}}}"#) == #"{"id":1,"reply":{"error":{"code":"version_mismatch","protocol":\#(supervisorProtocol)}}}"#)
        t.check("after a mismatch, list is closed without a reply", c7.call(#"{"id":2,"request":{"list":{}}}"#) == nil)
        let c8 = try Client(dirs.socket)
        _ = c8.call(#"{"id":1,"request":{"hello":{"protocol":7}}}"#)
        t.check("after a mismatch, exit is still done (the frozen core)", c8.call(#"{"id":2,"request":{"exit":{}}}"#) == #"{"id":2,"reply":{"done":{}}}"#)
        t.check("exit ends the supervisor", running.finished())
    } catch {
        t.check("handshake fixtures", false, "\(error)")
    }
}

/// A child that reports `stopping` by itself and never exits must still get faber's first
/// `stop` (#227); only a stop already requested is answered without asking it again.
@MainActor
private func testFirstStopReachesAStoppingChild(_ t: Harness) {
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let fd = try dirs.stateFD()
        let me = getpid()
        guard let myStart = processInfo(me)?.startedUs else { throw Failure("no start time for this process") }
        let lock = try HeldLock.acquire(in: fd, name: "slot-0.lock", displayPath: "slot-0.lock", busy: "busy")
        let status = ChildStatus()
        status.set(.stopping)
        let asked = Flag()
        let child = try ChildServer(path: dirs.state.appendingPathComponent("slot-0.sock"), slot: 0, claim: "wedged",
                                    status: status, agent: AbsentGuestAgent()) { asked.set() }
        child.start()
        try writeClaimMarker(dirFD: fd, marker(slot: 0, claim: "wedged", pid: me, startedUs: myStart))
        let running = try Running(dirs.config())
        t.check("a child reporting stopping is adopted as stopping", list(dirs.socket)?.first?.state == .stopping)
        t.check("the first stop is answered done",
                request(dirs.socket, #"{"stop":{"claim":"wedged"}}"#) == #"{"id":2,"reply":{"done":{}}}"#)
        t.check("the first stop reached the child that never exits", asked.isSet)
        running.stop()
        child.close()
        close(lock.fd)
    } catch {
        t.check("stopping-child fixtures", false, "\(error)")
    }
}

@MainActor
private func testAdoption(_ t: Harness) {
    var sleepers: [pid_t] = []
    defer { sleepers.forEach(reap) }
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let fd = try dirs.stateFD()
        defer { close(fd) }
        let me = getpid()
        guard let myStart = processInfo(me)?.startedUs else { throw Failure("no start time for this process") }

        // Slot 0: a live child this supervisor speaks to (in-process), in state ready.
        let lock0 = try HeldLock.acquire(in: fd, name: "slot-0.lock", displayPath: "slot-0.lock", busy: "busy")
        let status0 = ChildStatus()
        status0.set(.ready)
        let child0 = try ChildServer(path: dirs.state.appendingPathComponent("slot-0.sock"), slot: 0, claim: "adopt-me",
                                     status: status0, agent: AbsentGuestAgent()) {}
        child0.start()
        try writeClaimMarker(dirFD: fd, marker(slot: 0, claim: "adopt-me", pid: me, startedUs: myStart))

        // Slot 1: a live child at a child_protocol this build does not speak.
        let sleep1 = try spawnChild(executable: "/bin/sleep", arguments: ["sleep", "60"], lockFD: nil)
        sleepers.append(sleep1)
        usleep(50_000)
        let lock1 = try HeldLock.acquire(in: fd, name: "slot-1.lock", displayPath: "slot-1.lock", busy: "busy")
        try writeClaimMarker(dirFD: fd, marker(slot: 1, kind: .macos, claim: "old-child", pid: sleep1,
                                               startedUs: processInfo(sleep1)?.startedUs ?? 0, childProtocol: 99,
                                               bundle: "vm-1-held"))
        try makeMarkedClone(dirs.clones, "vm-1-held")
        t.check("spawnChild puts the child in a process group of its own", getpgid(sleep1) == sleep1)

        // Slot 2: a reused pid (the start time does not match), with a child server that would
        // answer if it were wrongly trusted.
        let lock2 = try HeldLock.acquire(in: fd, name: "slot-2.lock", displayPath: "slot-2.lock", busy: "busy")
        let child2 = try ChildServer(path: dirs.state.appendingPathComponent("slot-2.sock"), slot: 2, claim: "reused",
                                     status: ChildStatus(), agent: AbsentGuestAgent()) {}
        child2.start()
        try writeClaimMarker(dirFD: fd, marker(slot: 2, claim: "reused", pid: me, startedUs: myStart &+ 1))

        // Slot 3: its lock is free, so the VM is gone; its clone is torn down.
        try writeClaimMarker(dirFD: fd, marker(slot: 3, kind: .macos, claim: "gone", pid: 1, startedUs: 1, bundle: "vm-3-dead"))
        try makeMarkedClone(dirs.clones, "vm-3-dead")

        // Slot 4: not a marker (no signature): left alone.
        let notMarker = dirs.state.appendingPathComponent("slot-4.claim")
        try Data("{\"note\":\"not faber's\"}\n".utf8).write(to: notMarker)

        // Clones: an orphan faber made (deleted), and a directory without the marker (kept).
        try makeMarkedClone(dirs.clones, "vm-orphan")
        try makeMarkedClone(dirs.clones, "manual", marked: false)

        let running = try Running(dirs.config())
        let vms = list(dirs.socket) ?? []
        let bySlot = Dictionary(uniqueKeysWithValues: vms.map { ($0.slot, $0) })
        t.check("a live child at a supported version is adopted in its real state", bySlot[0]?.state == .ready && bySlot[0]?.claim == "adopt-me", "\(vms)")
        t.check("an unsupported child_protocol is unmanaged", bySlot[1]?.state == .unmanaged, "\(vms)")
        t.check("a reused pid is never adopted", bySlot[2]?.state == .unmanaged, "\(vms)")
        t.check("a free lock is stopped, launcher_exited", bySlot[3]?.state == .stopped && bySlot[3]?.stopReason == .launcherExited, "\(vms)")
        t.check("a file without the signature is not a VM", bySlot[4] == nil)
        t.check("the gone VM's clone is torn down", !pathExists(dirs.clones.appendingPathComponent("vm-3-dead")))
        t.check("the gone VM's marker is removed", !pathExists(dirs.state.appendingPathComponent("slot-3.claim")))
        t.check("the sweep deletes an orphan clone carrying faber's marker", !pathExists(dirs.clones.appendingPathComponent("vm-orphan")))
        t.check("the sweep keeps a directory without the marker", pathExists(dirs.clones.appendingPathComponent("manual")))
        t.check("the sweep keeps a clone a live marker names", pathExists(dirs.clones.appendingPathComponent("vm-1-held")))
        t.check("the non-marker file is left alone", pathExists(notMarker))

        t.check("an unmanaged VM is never stopped", request(dirs.socket, #"{"stop":{"claim":"old-child"}}"#)
            == #"{"id":2,"reply":{"error":{"code":"unmanaged"}}}"#)
        t.check("probe on an unmanaged VM", request(dirs.socket, #"{"probe":{"claim":"old-child"}}"#)
            == #"{"id":2,"reply":{"error":{"code":"unmanaged"}}}"#)
        t.check("probe on a ready VM with no guest agent is wrong_state",
                request(dirs.socket, #"{"probe":{"claim":"adopt-me"}}"#) == #"{"id":2,"reply":{"error":{"code":"wrong_state"}}}"#)
        t.check("a jit_config for a Linux VM is bad_request",
                request(dirs.socket, #"{"begin":{"claim":"adopt-me","jit_config":"ZmFrZQ=="}}"#) == #"{"id":2,"reply":{"error":{"code":"bad_request"}}}"#)
        t.check("start in an occupied slot is slot_busy",
                request(dirs.socket, #"{"start":{"slot":1,"kind":"linux","claim":"n","image":"x","cpus":1,"memory_mib":1024}}"#)
                    == #"{"id":2,"reply":{"error":{"code":"slot_busy"}}}"#)
        t.check("start with a live claim is claim_exists",
                request(dirs.socket, #"{"start":{"slot":9,"kind":"linux","claim":"adopt-me","image":"x","cpus":1,"memory_mib":1024}}"#)
                    == #"{"id":2,"reply":{"error":{"code":"claim_exists"}}}"#)
        if let line = request(dirs.socket, #"{"adopt":{"claims":["gone","adopt-me","never"]}}"#),
           let frame = try? decodeResponseForTest(Array((line + "\n").utf8)), case .adopted(let found, let unknown) = frame.reply {
            t.check("adopt finds the live claim and reports the rest unknown",
                    found.map(\.claim) == ["adopt-me"] && unknown == ["gone", "never"], line)
        } else {
            t.check("adopt answers adopted", false)
        }
        t.check("stop on the adopted child asks it", request(dirs.socket, #"{"stop":{"claim":"adopt-me"}}"#) == #"{"id":2,"reply":{"done":{}}}"#
            && status0.state == .stopping)
        t.check("stop is idempotent", request(dirs.socket, #"{"stop":{"claim":"adopt-me"}}"#) == #"{"id":2,"reply":{"done":{}}}"#)

        running.stop()
        t.check("exit leaves an unmanaged child running, never signalled", kill(sleep1, 0) == 0 && processInfo(sleep1) != nil)
        t.check("exit keeps the live markers", pathExists(dirs.state.appendingPathComponent("slot-1.claim")))

        // A new supervisor on the same state: still unmanaged. When its process exits and the
        // lock comes free, it is torn down after: stopped, launcher_exited, clone deleted.
        let again = try Running(dirs.config())
        t.check("a restarted supervisor finds the unmanaged child again", list(dirs.socket)?.first { $0.slot == 1 }?.state == .unmanaged)
        close(lock1.fd)
        kill(sleep1, SIGKILL)
        let after = waitForList(dirs.socket) { $0.first { $0.slot == 1 }?.state == .stopped }
        let rec1 = after?.first { $0.slot == 1 }
        t.check("an unmanaged child's exit is torn down after", rec1?.state == .stopped && rec1?.stopReason == .launcherExited, "\(String(describing: after))")
        t.check("its clone is deleted then", !pathExists(dirs.clones.appendingPathComponent("vm-1-held")))
        again.stop()
        child0.close()
        child2.close()
        close(lock0.fd)
        close(lock2.fd)
    } catch {
        t.check("adoption fixtures", false, "\(error)")
    }
}

@MainActor
private func testStartAndExit(_ t: Harness) {
    var children: [pid_t] = []
    defer { children.forEach(reap) }
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let bin = try dirs.scratch.dir("bin")
        let sleeper = try sleeperScript(bin)
        let failing = try sleeperScript(bin, exitCode: 1)
        _ = try dirs.scratch.dir("images/linux-engine")
        _ = try dirs.scratch.dir("images/macos-27")
        let fd = try dirs.stateFD()
        defer { close(fd) }
        let start = { (slot: Int, claim: String) in
            #"{"start":{"slot":\#(slot),"kind":"linux","claim":"\#(claim)","image":"linux-engine","cpus":1,"memory_mib":1024}}"#
        }

        var running = try Running(dirs.config(executable: sleeper))
        t.check("an image not in the store", request(dirs.socket, #"{"start":{"slot":5,"kind":"linux","claim":"a","image":"nope","cpus":1,"memory_mib":1024}}"#)
            == #"{"id":2,"reply":{"error":{"code":"image_not_found"}}}"#)
        t.check("a tools disk not in the cache", request(dirs.socket, #"{"start":{"slot":5,"kind":"macos","claim":"a","image":"macos-27","tools":"nope","cpus":1,"memory_mib":1024}}"#)
            == #"{"id":2,"reply":{"error":{"code":"tools_not_found"}}}"#)
        // faber's cache stores <name>.img (src/vm/tools_disk.rs): a bare <name> is not a disk,
        // and <name>.img is found (the start then fails at the clone: the image is a fixture).
        try scratch_write(dirs.tools.appendingPathComponent("bare"))
        try scratch_write(dirs.tools.appendingPathComponent("cached.img"))
        t.check("a tools file without .img is not the disk", request(dirs.socket, #"{"start":{"slot":5,"kind":"macos","claim":"a","image":"macos-27","tools":"bare","cpus":1,"memory_mib":1024}}"#)
            == #"{"id":2,"reply":{"error":{"code":"tools_not_found"}}}"#)
        t.check("a tools name finds <name>.img", request(dirs.socket, #"{"start":{"slot":5,"kind":"macos","claim":"a","image":"macos-27","tools":"cached","cpus":1,"memory_mib":1024}}"#)
            == #"{"id":2,"reply":{"error":{"code":"start_failed"}}}"#)

        let reply = request(dirs.socket, start(5, "spawned")) ?? ""
        t.check("start answers vm in starting", reply.contains(#""state":"starting""#) && reply.contains(#""claim":"spawned""#), reply)
        let m = readMarkerFile(dirFD: fd, name: "slot-5.claim").flatMap { try? ClaimMarker.decode($0) }
        let pid = m?.pid ?? 0
        if pid > 0 { children.append(pid) }
        t.check("the marker records the child's pid and start time", pid > 0 && m.map { isSameProcess(pid: $0.pid, startedUs: $0.pidStartedUs) } ?? false)
        t.check("the child leads a process group of its own", pid > 0 && getpgid(pid) == pid)
        t.check("the child holds the slot lock it was handed", !slotLockIsFree(dirFD: fd, slot: 5))
        t.check("the slot is busy", request(dirs.socket, start(5, "other")) == #"{"id":2,"reply":{"error":{"code":"slot_busy"}}}"#)
        t.check("the claim is live", request(dirs.socket, start(6, "spawned")) == #"{"id":2,"reply":{"error":{"code":"claim_exists"}}}"#)

        running.stop()
        t.check("exit leaves the spawned child running", pid > 0 && kill(pid, 0) == 0)

        // Restarted on the same state, the child is still there; it serves no child socket,
        // so the handshake fails and it is unmanaged, never signalled.
        running = try Running(dirs.config(executable: sleeper))
        t.check("a restarted supervisor re-finds the child by its marker", list(dirs.socket)?.first { $0.slot == 5 }?.state == .unmanaged)
        t.check("... and has not signalled it", kill(pid, 0) == 0)
        kill(pid, SIGKILL)
        var status: Int32 = 0
        waitpid(pid, &status, 0)
        let gone = waitForList(dirs.socket) { $0.first { $0.slot == 5 }?.state == .stopped }
        t.check("its exit is seen and torn down", gone?.first { $0.slot == 5 }?.stopReason == .launcherExited, "\(String(describing: gone))")
        t.check("the slot can be started again", (request(dirs.socket, start(5, "second")) ?? "").contains(#""state":"starting""#))
        let second = readMarkerFile(dirFD: fd, name: "slot-5.claim").flatMap { try? ClaimMarker.decode($0) }?.pid ?? 0
        if second > 0 { children.append(second) }
        t.check("stop asks the child; not serving yet, it gets SIGTERM", request(dirs.socket, #"{"stop":{"claim":"second"}}"#) == #"{"id":2,"reply":{"done":{}}}"#)
        let stopped = waitForList(dirs.socket) { $0.first { $0.slot == 5 }?.state == .stopped }
        t.check("a stopped child is requested", stopped?.first { $0.slot == 5 }?.stopReason == .requested, "\(String(describing: stopped))")
        t.check("its marker is removed", !pathExists(dirs.state.appendingPathComponent("slot-5.claim")))
        running.stop()

        running = try Running(dirs.config(executable: failing))
        t.check("a child that fails is started", (request(dirs.socket, start(7, "fails")) ?? "").contains("starting"))
        let failed = waitForList(dirs.socket) { $0.first { $0.slot == 7 }?.state == .stopped }
        t.check("exiting 1 before booting is start_failed", failed?.first { $0.slot == 7 }?.stopReason == .startFailed, "\(String(describing: failed))")
        running.stop()

        let plan = try NetworkPlan.parse(Array(#"{"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":9,"addr":"192.168.100.2"}]}]}"#.utf8))
        running = try Running(dirs.config(executable: failing, plan: plan))
        t.check("a slot absent from the plan is start_failed", request(dirs.socket, start(3, "noplan")) == #"{"id":2,"reply":{"error":{"code":"start_failed"}}}"#)
        t.check("a failed start leaves no marker", !pathExists(dirs.state.appendingPathComponent("slot-3.claim")))
        running.stop()
    } catch {
        t.check("start fixtures", false, "\(error)")
    }
}

// MARK: - A child that says whatever the test tells it

/// A fake child that speaks the child protocol's framing but answers each request as `answer`
/// says, including what a real v1 child never sends: the supervisor must not believe it.
private final class RawFakeChild: @unchecked Sendable {
    private let fd: Int32
    private let lock = NSLock()
    private var answer: @Sendable (ChildRequest) -> ChildReply

    init(path: URL, slot: Int, claim: String, version: Int = childProtocol,
         answer: @escaping @Sendable (ChildRequest) -> ChildReply) throws {
        fd = try listenUnix(path, mode: 0o600).fd
        self.answer = answer
        Thread.detachNewThread { [self] in
            while true {
                let conn = accept(fd, nil, nil)
                if conn < 0 { return }
                setTimeouts(conn, seconds: 2)
                var reader = FrameReader(fd: conn)
                while let line = try? reader.next(), let frame = try? decodeChildRequest(line) {
                    let reply: ChildReply
                    if case .hello = frame.request {
                        reply = .hello(childProtocol: version, slot: slot, claim: claim)
                    } else {
                        lock.lock()
                        reply = self.answer(frame.request)
                        lock.unlock()
                    }
                    guard let bytes = try? encodeChildResponse(ChildResponseFrame(id: frame.id, reply: reply)),
                          (try? writeAll(conn, bytes)) != nil else { break }
                }
                close(conn)
            }
        }
    }

    func set(_ answer: @escaping @Sendable (ChildRequest) -> ChildReply) {
        lock.lock()
        self.answer = answer
        lock.unlock()
    }
}

private func status(_ s: VmState, _ reason: StopReason? = nil) -> ChildReply { .status(state: s, stopReason: reason) }

@MainActor
private func testChildRepliesAreChecked(_ t: Harness) {
    // The pure transition table.
    t.check("starting to booting", childStatusMoves(from: .starting, to: .booting))
    t.check("booting to ready", childStatusMoves(from: .booting, to: .ready))
    t.check("in_service to job_finished", childStatusMoves(from: .inService, to: .jobFinished))
    t.check("any state to stopping", VmState.allCases.filter { $0 != .stopped && $0 != .unmanaged }
        .allSatisfy { childStatusMoves(from: $0, to: .stopping) })
    t.check("the same state again", childStatusMoves(from: .ready, to: .ready))
    t.check("never stopped from a child", !VmState.allCases.contains { childStatusMoves(from: $0, to: .stopped) })
    t.check("no regression to starting", !childStatusMoves(from: .booting, to: .starting))
    t.check("no regression ready to booting", !childStatusMoves(from: .ready, to: .booting))
    t.check("status never reaches probe_sent or in_service", !childStatusMoves(from: .ready, to: .probeSent)
        && !childStatusMoves(from: .probeSent, to: .inService) && !childStatusMoves(from: .booting, to: .inService))
    t.check("nothing leaves stopping", !childStatusMoves(from: .stopping, to: .ready))

    // The child's side: what a framework state reports. The VM before `start` is `stopped`,
    // so the observation must never deliver its initial value.
    t.check("the state observation skips the initial value", !stateObservationOptions.contains(.initial))
    t.check("running is booting", childStatusEffect(of: .running) == .booting)
    t.check("stopping, stopped and error stop", [VZVirtualMachine.State.stopping, .stopped, .error]
        .allSatisfy { childStatusEffect(of: $0) == .stopping })
    t.check("starting has no effect", childStatusEffect(of: .starting) == nil)

    var sleepers: [pid_t] = []
    defer { sleepers.forEach(reap) }
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let fd = try dirs.stateFD()
        defer { close(fd) }
        func liveSleeper() throws -> (pid_t, UInt64) {
            let pid = try spawnChild(executable: "/bin/sleep", arguments: ["sleep", "60"], lockFD: nil)
            sleepers.append(pid)
            usleep(50_000)
            return (pid, processInfo(pid)?.startedUs ?? 0)
        }

        // Slot 0 answers `stopped` at adoption: unmanaged, and its clone survives the sweep.
        let (p0, s0) = try liveSleeper()
        let l0 = try HeldLock.acquire(in: fd, name: "slot-0.lock", displayPath: "slot-0.lock", busy: "busy")
        let liar = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-0.sock"), slot: 0, claim: "liar") { _ in
            status(.stopped, .guestShutdown)
        }
        try writeClaimMarker(dirFD: fd, marker(slot: 0, kind: .macos, claim: "liar", pid: p0, startedUs: s0, bundle: "vm-0-live"))
        try makeMarkedClone(dirs.clones, "vm-0-live")

        // Slot 1 is ready; slot 2 is probe_sent (macOS, for begin).
        let (p1, s1) = try liveSleeper()
        let l1 = try HeldLock.acquire(in: fd, name: "slot-1.lock", displayPath: "slot-1.lock", busy: "busy")
        let one = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-1.sock"), slot: 1, claim: "one") { _ in status(.ready) }
        try writeClaimMarker(dirFD: fd, marker(slot: 1, claim: "one", pid: p1, startedUs: s1))
        let (p2, s2) = try liveSleeper()
        let l2 = try HeldLock.acquire(in: fd, name: "slot-2.lock", displayPath: "slot-2.lock", busy: "busy")
        let two = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-2.sock"), slot: 2, claim: "two") { _ in status(.probeSent) }
        try writeClaimMarker(dirFD: fd, marker(slot: 2, kind: .macos, claim: "two", pid: p2, startedUs: s2, bundle: "vm-2-live"))
        try makeMarkedClone(dirs.clones, "vm-2-live")

        // ADR-0049 §7: slot 3 is a macOS child at child protocol 1, slot 4 a Linux child, and
        // slot 5 a macOS child at this build's protocol; all three are probe_sent.
        let (p3, s3) = try liveSleeper()
        let l3 = try HeldLock.acquire(in: fd, name: "slot-3.lock", displayPath: "slot-3.lock", busy: "busy")
        let old = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-3.sock"), slot: 3, claim: "old",
                                   version: 1) { r in if case .verify = r { return status(.verifying) }; return status(.probeSent) }
        try writeClaimMarker(dirFD: fd, marker(slot: 3, kind: .macos, claim: "old", pid: p3, startedUs: s3,
                                               childProtocol: 1, bundle: "vm-3-live"))
        try makeMarkedClone(dirs.clones, "vm-3-live")
        let (p4, s4) = try liveSleeper()
        let l4 = try HeldLock.acquire(in: fd, name: "slot-4.lock", displayPath: "slot-4.lock", busy: "busy")
        let lin = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-4.sock"), slot: 4, claim: "lin") { r in
            if case .verify = r { return status(.verifying) }; return status(.probeSent)
        }
        try writeClaimMarker(dirFD: fd, marker(slot: 4, claim: "lin", pid: p4, startedUs: s4))
        let (p5, s5) = try liveSleeper()
        let l5 = try HeldLock.acquire(in: fd, name: "slot-5.lock", displayPath: "slot-5.lock", busy: "busy")
        let ver = try RawFakeChild(path: dirs.state.appendingPathComponent("slot-5.sock"), slot: 5, claim: "ver") { r in
            if case .verify = r { return status(.verifying) }; return status(.probeSent)
        }
        try writeClaimMarker(dirFD: fd, marker(slot: 5, kind: .macos, claim: "ver", pid: p5, startedUs: s5, bundle: "vm-5-live"))
        try makeMarkedClone(dirs.clones, "vm-5-live")

        let running = try Running(dirs.config())
        let state = { (slot: Int) in list(dirs.socket)?.first { $0.slot == slot }?.state }
        t.check("a child answering stopped at adoption is unmanaged", state(0) == .unmanaged)
        t.check("... and its clone is not deleted", pathExists(dirs.clones.appendingPathComponent("vm-0-live")))
        t.check("... nor its marker", pathExists(dirs.state.appendingPathComponent("slot-0.claim")))
        t.check("ready is adopted", state(1) == .ready)
        t.check("probe_sent is adopted", state(2) == .probeSent)

        one.set { _ in status(.booting) }
        t.check("a regression reported by status is refused: ready kept", state(1) == .ready)
        one.set { _ in status(.stopped, .guestShutdown) }
        t.check("stopped reported at runtime is refused: ready kept", state(1) == .ready)
        one.set { _ in status(.inService) }
        t.check("a skip to in_service by status is refused", state(1) == .ready)

        one.set { r in if case .probe = r { return status(.inService) }; return status(.ready) }
        t.check("probe answered with the wrong state is internal",
                request(dirs.socket, #"{"probe":{"claim":"one"}}"#) == #"{"id":2,"reply":{"error":{"code":"internal"}}}"#)
        t.check("... and the state is kept", state(1) == .ready)
        one.set { r in if case .probe = r { return status(.probeSent) }; return status(.probeSent) }
        t.check("probe answered with probe_sent is accepted",
                (request(dirs.socket, #"{"probe":{"claim":"one"}}"#) ?? "").contains(#""state":"probe_sent""#))

        two.set { r in if case .begin = r { return status(.ready) }; return status(.probeSent) }
        t.check("begin answered with the wrong state is internal",
                request(dirs.socket, #"{"begin":{"claim":"two","jit_config":"ZmFrZQ=="}}"#) == #"{"id":2,"reply":{"error":{"code":"internal"}}}"#)
        t.check("... and the state is kept", state(2) == .probeSent)
        two.set { r in if case .begin = r { return status(.inService) }; return status(.inService) }
        t.check("begin answered with in_service is accepted",
                (request(dirs.socket, #"{"begin":{"claim":"two","jit_config":"ZmFrZQ=="}}"#) ?? "").contains(#""state":"in_service""#))
        two.set { _ in status(.jobFinished) }
        t.check("in_service to job_finished by status is accepted", state(2) == .jobFinished)
        t.check("the live clones are still there", pathExists(dirs.clones.appendingPathComponent("vm-2-live")))
        let verify = { (claim: String) in
            request(dirs.socket, #"{"verify":{"claim":"\#(claim)","expect":{"runtimes":[]}}}"#)
        }
        t.check("verify to a child at protocol 1 is wrong_state, never sent",
                verify("old") == #"{"id":2,"reply":{"error":{"code":"wrong_state"}}}"#)
        t.check("... and it stays probe_sent", state(3) == .probeSent)
        t.check("verify to a Linux VM is bad_request",
                verify("lin") == #"{"id":2,"reply":{"error":{"code":"bad_request"}}}"#)
        t.check("... and it stays probe_sent", state(4) == .probeSent)
        t.check("verify to a current macOS child is verifying", (verify("ver") ?? "").contains(#""state":"verifying""#))
        ver.set { _ in .status(state: .verified, stopReason: nil, failed: [.filevault]) }
        let rec = list(dirs.socket)?.first { $0.slot == 5 }
        t.check("verified carries the child's codes", rec?.state == .verified && rec?.failed == [.filevault], "\(String(describing: rec))")
        ver.set { _ in .status(state: .verified, stopReason: nil, failed: []) }
        t.check("a verdict that changes later is not taken", list(dirs.socket)?.first { $0.slot == 5 }?.failed == [.filevault])
        lin.set { _ in .status(state: .verified, stopReason: nil, failed: []) }
        t.check("a Linux child claiming verified is refused", state(4) == .probeSent)

        running.stop()
        withExtendedLifetime((liar, one, two, old, lin, ver)) {}
        [l0, l1, l2, l3, l4, l5].forEach { close($0.fd) }
    } catch {
        t.check("child reply fixtures", false, "\(error)")
    }
}

@MainActor
private func testAdoptionLoadsEveryMarkerFirst(_ t: Harness) {
    var sleepers: [pid_t] = []
    defer { sleepers.forEach(reap) }
    do {
        let dirs = try Dirs()
        defer { dirs.scratch.remove() }
        let fd = try dirs.stateFD()
        defer { close(fd) }

        // Slot 1 is gone (lock free) and names a clone that slot 6, live, also names. With
        // markers judged one by one, slot 1's teardown would run before slot 6 was loaded.
        try writeClaimMarker(dirFD: fd, marker(slot: 1, kind: .macos, claim: "gone", pid: 1, startedUs: 1, bundle: "vm-shared"))
        let pid = try spawnChild(executable: "/bin/sleep", arguments: ["sleep", "60"], lockFD: nil)
        sleepers.append(pid)
        usleep(50_000)
        let l6 = try HeldLock.acquire(in: fd, name: "slot-6.lock", displayPath: "slot-6.lock", busy: "busy")
        try writeClaimMarker(dirFD: fd, marker(slot: 6, kind: .macos, claim: "live", pid: pid,
                                               startedUs: processInfo(pid)?.startedUs ?? 0, childProtocol: 99, bundle: "vm-shared"))
        try makeMarkedClone(dirs.clones, "vm-shared")

        // Slot 2 is gone too, with a clone of its own; slot 9's marker cannot be read and its
        // lock is held, so no clone may be deleted until it is gone.
        try writeClaimMarker(dirFD: fd, marker(slot: 2, kind: .macos, claim: "gone2", pid: 1, startedUs: 1, bundle: "vm-2-gone"))
        try makeMarkedClone(dirs.clones, "vm-2-gone")
        try makeMarkedClone(dirs.clones, "vm-orphan")
        var l9: HeldLock? = try HeldLock.acquire(in: fd, name: "slot-9.lock", displayPath: "slot-9.lock", busy: "busy")
        try Data((ClaimMarker.signature + ",broken\n").utf8).write(to: dirs.state.appendingPathComponent("slot-9.claim"))

        let running = try Running(dirs.config())
        let vms = list(dirs.socket) ?? []
        t.check("a clone a later live marker names survives an earlier teardown",
                pathExists(dirs.clones.appendingPathComponent("vm-shared")) && vms.first { $0.slot == 1 }?.state == .stopped)
        t.check("no clone is deleted while a marker is unreadable",
                pathExists(dirs.clones.appendingPathComponent("vm-2-gone")) && pathExists(dirs.clones.appendingPathComponent("vm-orphan")))
        t.check("an unreadable marker holds its slot",
                request(dirs.socket, #"{"start":{"slot":9,"kind":"linux","claim":"n","image":"x","cpus":1,"memory_mib":1024}}"#)
                    == #"{"id":2,"reply":{"error":{"code":"slot_busy"}}}"#)
        if let l = l9 { close(l.fd) }
        l9 = nil
        let deadline = Date().addingTimeInterval(6)
        while pathExists(dirs.clones.appendingPathComponent("vm-2-gone")), Date() < deadline { usleep(200_000) }
        t.check("once the unreadable marker is gone, the sweep takes the clones no live marker names",
                !pathExists(dirs.clones.appendingPathComponent("vm-2-gone")) && !pathExists(dirs.clones.appendingPathComponent("vm-orphan")))
        t.check("... and keeps the one a live marker names", pathExists(dirs.clones.appendingPathComponent("vm-shared")))
        running.stop()
        close(l6.fd)
    } catch {
        t.check("adoption ordering fixtures", false, "\(error)")
    }
}

