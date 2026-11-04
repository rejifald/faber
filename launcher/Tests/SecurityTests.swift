// The review fixes: who may change a bundle, the state directory and its locks, file identity
// by lstat and its re-check after start, and the console log's open checks and cap.

import Darwin
import Foundation
import Virtualization

/// A stat with only the fields the pure checks read.
func fakeStat(mode: mode_t, uid: uid_t, nlink: nlink_t = 1) -> stat {
    var st = stat()
    st.st_mode = mode
    st.st_uid = uid
    st.st_nlink = nlink
    return st
}

@MainActor
func testOwnership(_ t: Harness, _ scratch: Scratch) {
    let me = geteuid()
    let other: uid_t = me == 0 ? 501 : 0
    do {
        // (a) The bundle and its parent.
        let parent = try scratch.dir("owned")
        chmod(parent.path, 0o755)
        let good = try linuxFixture(scratch, "owned/good")
        chmod(good.url.path, 0o755)
        t.accepts("a bundle only this account can change is accepted") { _ = try VMBundle.forRun(good.url.path) }
        t.check("forRun resolves symlinks in the path", (try? VMBundle.forRun(good.url.path))?.url.path.hasPrefix("/private/") ?? false,
                (try? VMBundle.forRun(good.url.path))?.url.path ?? "")

        let groupWritable = try linuxFixture(scratch, "owned/group-writable")
        chmod(groupWritable.url.path, 0o775)
        t.refuses("a group-writable bundle is refused", containing: "writable by group") {
            _ = try VMBundle.forRun(groupWritable.url.path)
        }
        let open = try scratch.dir("open")
        chmod(open.path, 0o777)
        let inOpen = try linuxFixture(scratch, "open/bundle")
        chmod(inOpen.url.path, 0o755)
        t.refuses("a bundle in a world-writable directory is refused", containing: "writable by group or others") {
            _ = try VMBundle.forRun(inOpen.url.path)
        }
        chmod(open.path, 0o755)
        t.refuses("a bundle owned by another account is refused", containing: "owned by uid") {
            try checkPrivateDirectory(fakeStat(mode: S_IFDIR | 0o755, uid: other), euid: me, path: "/x")
        }
        t.refuses("a symlink in place of a directory is refused", containing: "not a directory") {
            try checkPrivateDirectory(fakeStat(mode: S_IFLNK | 0o755, uid: me), euid: me, path: "/x")
        }
        t.accepts("a 0700 directory of this account is private") {
            try checkPrivateDirectory(fakeStat(mode: S_IFDIR | 0o700, uid: me), euid: me, path: "/x")
        }

        // The state directory: this account's, mode exactly 0700.
        let state = try scratch.dir("state-checks")
        chmod(state.path, 0o755)
        t.refuses("a 0755 state dir is refused", containing: "expected 700") { _ = try Slot(8).lock(stateDir: state) }
        chmod(state.path, 0o700)
        t.accepts("a 0700 state dir is accepted") { close(try Slot(8).lock(stateDir: state).fd) }
        t.refuses("a state dir owned by another account is refused", containing: "owned by uid") {
            try checkStateDirectory(fakeStat(mode: S_IFDIR | 0o700, uid: other), euid: me, path: "/x")
        }
        // The lock file: regular, this account's, one link.
        let target = try scratch.write(scratch.url.appendingPathComponent("lock-target"))
        try FileManager.default.linkItem(at: target, to: state.appendingPathComponent("slot-9.lock"))
        t.refuses("a hard-linked slot lock is refused", containing: "hard links") { _ = try Slot(9).lock(stateDir: state) }
        t.refuses("a lock owned by another account is refused", containing: "owned by uid") {
            try checkOwnedFile(fakeStat(mode: S_IFREG | 0o600, uid: other), euid: me, path: "/x")
        }
        t.refuses("a lock that is not a regular file is refused", containing: "regular") {
            try checkOwnedFile(fakeStat(mode: S_IFDIR | 0o700, uid: me), euid: me, path: "/x")
        }
    } catch {
        t.check("ownership fixtures", false, "\(error)")
    }
}

@MainActor
func testIdentity(_ t: Harness, _ scratch: Scratch) {
    do {
        let dir = try scratch.dir("identity")
        let a = try scratch.write(dir.appendingPathComponent("a"), Data("a".utf8))
        let b = try scratch.write(dir.appendingPathComponent("b"), Data("b".utf8))
        let link = dir.appendingPathComponent("link")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: a)
        t.check("identity of a regular file", FileID.ofRegularFile(path: a.path) != nil)
        t.check("identity is stable", FileID.ofRegularFile(path: a.path) == FileID.ofRegularFile(path: a.path))
        t.check("two files differ", FileID.ofRegularFile(path: a.path) != FileID.ofRegularFile(path: b.path))
        t.check("a symlink has no identity (lstat, never followed)", FileID.ofRegularFile(path: link.path) == nil)
        t.check("a directory has no identity", FileID.ofRegularFile(path: dir.path) == nil)
        t.check("a missing file has no identity", FileID.ofRegularFile(path: dir.appendingPathComponent("none").path) == nil)

        let refs = [try regularFileRef(a), try regularFileRef(b)]
        t.check("nothing changed", changedFile(refs) == nil)
        // Swap: another file renamed over b's path, as an attacker would between check and open.
        let c = try scratch.write(dir.appendingPathComponent("c"), Data("c".utf8))
        guard rename(c.path, b.path) == 0 else { throw Failure("rename: \(errnoText(errno))") }
        t.check("a file renamed over a checked path is caught", changedFile(refs)?.url == b)
        guard unlink(a.path) == 0 else { throw Failure("unlink: \(errnoText(errno))") }
        try FileManager.default.createSymbolicLink(at: a, withDestinationURL: b)
        t.check("a checked file replaced by a symlink is caught", changedFile(refs)?.url == a)
    } catch {
        t.check("identity fixtures", false, "\(error)")
    }
}

@MainActor
func testConsoleLog(_ t: Harness, _ scratch: Scratch) {
    do {
        let dir = try scratch.dir("console")
        let target = try scratch.write(dir.appendingPathComponent("target.log"))
        let hard = dir.appendingPathComponent("hard.log")
        try FileManager.default.linkItem(at: target, to: hard)
        t.refuses("a hard-linked console log is refused", containing: "hard links") { _ = try ConsoleLog.open(hard, create: true) }
        let fifo = dir.appendingPathComponent("fifo.log")
        guard mkfifo(fifo.path, 0o600) == 0 else { throw Failure("mkfifo: \(errnoText(errno))") }
        t.refuses("a FIFO console log is refused without blocking", containing: "FIFO") { _ = try ConsoleLog.open(fifo, create: true) }
        t.refuses("a directory as console log is refused") { _ = try ConsoleLog.open(dir, create: true) }
        t.refuses("a console log owned by another account is refused", containing: "owned by uid") {
            try checkOwnedFile(fakeStat(mode: S_IFREG | 0o600, uid: geteuid() == 0 ? 501 : 0), euid: geteuid(), path: "/x")
        }
        t.accepts("the serial port writes into a pipe, not the log") {
            let log = try ConsoleLog.open(dir.appendingPathComponent("ok.log"), create: true)
            guard isFIFO(log.pipeWrite.fileDescriptor), !isFIFO(log.log.fileDescriptor) else { throw Failure("wrong ends") }
        }

        // The drain stops writing at the cap and keeps reading, so the writer never blocks.
        func drained(_ name: String, input: Int, start: Int64 = 0, cap: Int64) throws -> (size: Int64, caps: Int, file: Int64) {
            let url = dir.appendingPathComponent(name)
            let logFD = open(url.path, O_WRONLY | O_APPEND | O_CREAT | O_EXCL, 0o600)
            guard logFD >= 0 else { throw Failure("open: \(errnoText(errno))") }
            defer { close(logFD) }
            var ends: [Int32] = [-1, -1]
            guard pipe(&ends) == 0 else { throw Failure("pipe") }
            let bytes = [UInt8](repeating: 0x41, count: input)
            _ = bytes.withUnsafeBytes { write(ends[1], $0.baseAddress, input) }
            close(ends[1])
            var caps = 0
            let size = drainConsole(from: ends[0], to: logFD, size: start, maxBytes: cap) { caps += 1 }
            close(ends[0])
            return (size, caps, fileSize(url)?.bytes ?? -1)
        }
        let over = try drained("over.log", input: 3000, cap: 1024)
        t.check("the drain stops at the cap", over.size == 1024 && over.file == 1024, "\(over)")
        t.check("the cap event fires once", over.caps == 1, "\(over.caps)")
        let under = try drained("under.log", input: 1000, cap: 1024)
        t.check("under the cap everything is written", under.file == 1000 && under.caps == 0, "\(under)")
        let full = try drained("full.log", input: 500, start: 2048, cap: 1024)
        t.check("a log already past the cap gets nothing", full.file == 0 && full.caps == 1, "\(full)")
        // More than a pipe buffer: the drain must keep reading past the cap, or this write blocks.
        var ends: [Int32] = [-1, -1]
        guard pipe(&ends) == 0 else { throw Failure("pipe") }
        let sink = open("/dev/null", O_WRONLY)
        let readFD = ends[0]
        let done = DispatchSemaphore(value: 0)
        Thread {
            _ = drainConsole(from: readFD, to: sink, size: 0, maxBytes: 1024) {}
            done.signal()
        }.start()
        let chunk = [UInt8](repeating: 0x42, count: 64 << 10)
        var total = 0
        for _ in 0..<32 { total += chunk.withUnsafeBytes { write(ends[1], $0.baseAddress, chunk.count) } }
        close(ends[1])
        t.check("a guest writing 2 MiB past a 1 KiB cap never blocks", done.wait(timeout: .now() + 10) == .success
            && total == 32 << 16, "\(total)")
        close(ends[0])
        close(sink)
    } catch {
        t.check("console log fixtures", false, "\(error)")
    }
}

@MainActor
func testAncestors(_ t: Harness, _ scratch: Scratch) {
    let me = geteuid()
    let other: uid_t = me == 0 ? 501 : 0
    let stranger: uid_t = 4242
    t.accepts("a root-owned 0755 ancestor is safe") {
        try checkAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: 0), euid: me, path: "/x")
    }
    t.accepts("an ancestor of this account is safe") {
        try checkAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: me), euid: me, path: "/x")
    }
    t.accepts("a sticky world-writable ancestor (like /tmp) is safe") {
        try checkAncestor(fakeStat(mode: S_IFDIR | 0o1777, uid: 0), euid: me, path: "/x")
    }
    t.refuses("an ancestor of a third account is refused", containing: "neither") {
        try checkAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: stranger == me ? other : stranger), euid: me, path: "/x")
    }
    t.refuses("a group-writable ancestor without the sticky bit is refused", containing: "sticky") {
        try checkAncestor(fakeStat(mode: S_IFDIR | 0o775, uid: 0), euid: me, path: "/x")
    }
    t.refuses("a non-directory ancestor is refused", containing: "not a directory") {
        try checkAncestor(fakeStat(mode: S_IFLNK | 0o755, uid: 0), euid: me, path: "/x")
    }
    do {
        // Real directories: the temporary volume's own ancestors are root's or this account's.
        let real = try resolvedURL(scratch.url)
        t.accepts("the temporary directory's real ancestors are all safe") { try requireSafeAncestors(above: real) }

        let top = try scratch.dir("anc-open")
        let mid = try scratch.dir("anc-open/mid")
        let bundle = try linuxFixture(scratch, "anc-open/mid/bundle")
        for dir in [top, mid, bundle.url] { chmod(dir.path, 0o755) }
        t.accepts("a bundle under safe ancestors is accepted") { _ = try VMBundle.forRun(bundle.url.path) }
        chmod(top.path, 0o777)
        t.refuses("a bundle under a world-writable grandparent is refused", containing: "sticky") {
            _ = try VMBundle.forRun(bundle.url.path)
        }
        chmod(top.path, 0o1777)
        t.accepts("a sticky world-writable grandparent is accepted") { _ = try VMBundle.forRun(bundle.url.path) }

        let state = try scratch.dir("anc-open/mid/state")
        chmod(state.path, 0o700)
        chmod(top.path, 0o777)
        t.refuses("a state dir under a world-writable ancestor is refused", containing: "sticky") {
            _ = try Slot(10).lock(stateDir: state)
        }
        chmod(top.path, 0o755)
        t.accepts("a state dir under safe ancestors is accepted") { close(try Slot(10).lock(stateDir: state).fd) }
    } catch {
        t.check("ancestor fixtures", false, "\(error)")
    }
}

/// The macOS tools disk (ADR-0026 amendment): one read-only second disk the launcher's account
/// cannot write. An accepted one must be some other account's file under directories this one
/// does not own, and VZ wants a whole number of 512-byte sectors: a root-owned file in
/// /usr/bin on the sealed system volume stands in, attached read-only and never written.
@MainActor
func testToolsDisk(_ t: Harness, _ scratch: Scratch) {
    let me = geteuid()
    let systemFile = URL(fileURLWithPath: "/System/Library/CoreServices/SystemVersion.plist")
    do {
        let dir = try scratch.dir("macos-tools")
        let disk = try scratch.sparse(dir.appendingPathComponent("disk"))
        let aux = try scratch.write(dir.appendingPathComponent("vz-aux"))
        let mine = try scratch.sparse(scratch.url.appendingPathComponent("my-tools.img"))
        let options = try sharedNATOptions(slot: 11)

        // The checks on the file itself.
        t.refuses("a tools disk this account owns is refused", containing: "owned by this process") { _ = try requireToolsDisk(mine) }
        let link = scratch.url.appendingPathComponent("tools-link")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: systemFile)
        t.refuses("a symlinked tools disk is refused", containing: "symlink") { _ = try requireToolsDisk(link) }
        t.refuses("a group-writable tools disk is refused", containing: "group or others") {
            try checkNotWritable(fakeStat(mode: S_IFREG | 0o664, uid: 0), euid: me, writableByMe: false, path: "/x")
        }
        t.refuses("a tools disk this process can write (ACL) is refused", containing: "can write") {
            try checkNotWritable(fakeStat(mode: S_IFREG | 0o444, uid: 0), euid: me, writableByMe: true, path: "/x")
        }
        t.refuses("a hard-linked tools disk is refused", containing: "hard links") {
            try checkNotWritable(fakeStat(mode: S_IFREG | 0o444, uid: 0, nlink: 2), euid: me, writableByMe: false, path: "/x")
        }
        t.refuses("a tools disk under a directory this account owns is refused", containing: "could swap") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: me), euid: me, path: "/x")
        }
        t.refuses("a tools disk under a world-writable directory is refused", containing: "sticky") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o777, uid: 0), euid: me, path: "/x")
        }
        // The supervisor's --tools-owner: faber builds the disks in a directory of its own
        // (setup: _faber 755), which the launcher's account must accept (October 2026, #76).
        let faber = me &+ 1
        t.refuses("another account's directory is refused without --tools-owner", containing: "neither this process") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: faber), euid: me, path: "/x")
        }
        t.accepts("the tools owner's directory is accepted") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: faber), euid: me, path: "/x", toolsOwner: faber)
        }
        t.refuses("the tools owner's group-writable directory is refused", containing: "sticky") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o775, uid: faber), euid: me, path: "/x", toolsOwner: faber)
        }
        t.refuses("the tools owner's non-directory is refused", containing: "not a directory") {
            try checkToolsAncestor(fakeStat(mode: S_IFREG | 0o755, uid: faber), euid: me, path: "/x", toolsOwner: faber)
        }
        t.refuses("a third account's directory is refused even with --tools-owner", containing: "neither this process") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: faber &+ 1), euid: me, path: "/x", toolsOwner: faber)
        }
        let walk = toolsAncestors(of: "/var/db/faber/tools-disks/runner.img", toolsOwner: faber)
        t.check("only the disk's own directory is checked against the tools owner",
                walk.map(\.path) == ["/var/db/faber/tools-disks", "/var/db/faber", "/var/db", "/var", "/"]
                    && walk.first?.toolsOwner == faber && walk.dropFirst().allSatisfy { $0.toolsOwner == nil },
                "\(walk)")
        t.check("without --tools-owner no ancestor is", toolsAncestors(of: "/t/x.img", toolsOwner: nil).allSatisfy { $0.toolsOwner == nil })
        t.refuses("a grandparent owned by the tools owner is refused", containing: "neither this process") {
            for (path, owner) in toolsAncestors(of: "/a/b/x.img", toolsOwner: faber) where path == "/a" {
                try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: faber), euid: me, path: path, toolsOwner: owner)
            }
        }
        t.refuses("--tools-owner naming this account never exempts its own directory", containing: "could swap") {
            try checkToolsAncestor(fakeStat(mode: S_IFDIR | 0o755, uid: me), euid: me, path: "/x", toolsOwner: me)
        }

        let candidates = (try? FileManager.default.contentsOfDirectory(atPath: "/usr/bin"))?.sorted() ?? []
        let standIn = candidates.lazy.map { URL(fileURLWithPath: "/usr/bin/\($0)") }.first { url in
            guard let size = fileSize(url)?.bytes, size > 0, size % 512 == 0 else { return false }
            return (try? requireToolsDisk(url)) != nil
        }
        guard me != 0, let standIn, let tools = try? requireToolsDisk(standIn) else {
            t.skip("tools disk accepted", "no root-owned, sector-sized read-only stand-in in /usr/bin for this uid")
            return
        }
        func build(_ tools: FileRef?) throws -> (VZVirtualMachineConfiguration, WallExpectation) {
            try macConfiguration(platform: VZMacPlatformConfiguration(), disk: disk, aux: aux, options,
                                 network: .sharedNAT, log: nil, tools: tools)
        }
        t.accepts("the wall accepts a macOS clone with its read-only tools disk") {
            let (config, expect) = try build(tools)
            guard config.storageDevices.count == 2,
                  (config.storageDevices[1].attachment as? VZDiskImageStorageDeviceAttachment)?.isReadOnly == true else {
                throw Failure("tools disk not attached read-only")
            }
            try checkWall(config, expect)
        }
        t.check("the tools disk is in the post-start re-check", (try? build(tools))?.1.attachedFiles.contains { $0.id == tools.id } ?? false)
        t.accepts("a clone without a tools disk still passes") {
            let (config, expect) = try build(nil)
            try checkWall(config, expect)
        }
        t.refuses("wall refuses (macOS): the tools disk attached writable", containing: "read-only") {
            // The wall's own check, apart from requireToolsDisk: an expectation on a file the
            // framework can open writable, then the same file attached read-write.
            let (config, expect) = try build(try regularFileRef(mine))
            let rw = try VZDiskImageStorageDeviceAttachment(url: mine, readOnly: false)
            config.storageDevices[1] = VZVirtioBlockDeviceConfiguration(attachment: rw)
            try checkWall(config, expect)
        }
        t.refuses("wall refuses (macOS): a writable second disk without a tools disk", containing: "exactly one disk") {
            let (config, expect) = try build(nil)
            let rw = try VZDiskImageStorageDeviceAttachment(url: mine, readOnly: false)
            config.storageDevices.append(VZVirtioBlockDeviceConfiguration(attachment: rw))
            try checkWall(config, expect)
        }
        t.refuses("wall refuses (macOS): the tools disk swapped for the launcher's own file", containing: "unexpected disk image") {
            let (config, expect) = try build(tools)
            let ro = try VZDiskImageStorageDeviceAttachment(url: mine, readOnly: true)
            config.storageDevices[1] = VZVirtioBlockDeviceConfiguration(attachment: ro)
            try checkWall(config, expect)
        }
        t.refuses("wall refuses (macOS): a third disk", containing: "nothing else") {
            let (config, expect) = try build(tools)
            let extra = try VZDiskImageStorageDeviceAttachment(url: standIn, readOnly: true)
            config.storageDevices.append(VZVirtioBlockDeviceConfiguration(attachment: extra))
            try checkWall(config, expect)
        }
        // For a Linux bundle --tools-disk is a usage error, before any file is looked at.
        let linux = try linuxFixture(scratch, "linux-tools")
        t.refuses("--tools-disk is refused for a Linux bundle", containing: "macOS bundles", usage: true) {
            _ = try prepare([linux.url.path, "--slot", "0", "--network", "shared-nat", "--tools-disk", systemFile.path,
                             "--wall-only"], forRun: false)
        }
        // The Linux seed is the other read-only disk: already covered by "the seed made writable".
    } catch {
        t.check("tools disk fixtures", false, "\(error)")
    }
}

@MainActor
func testProtocol(_ t: Harness) {
    t.check("the launcher protocol is 2", launcherProtocol == 2)
    t.accepts("faber-vm protocol prints nothing else and exits 0") {
        guard try dispatch(["protocol"]) == 0 else { throw Failure("non-zero") }
    }
    t.refuses("protocol takes no arguments", usage: true) { _ = try dispatch(["protocol", "2"]) }
}
