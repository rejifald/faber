// The Linux guest agent, the Docker bridge, and the supervisor's Linux bundle. No VM is ever
// booted: a Unix socket stands in for each of the guest's vsock ports.

import Darwin
import Foundation

/// A short directory for Unix sockets: a socket path must fit in 103 bytes.
private final class SocketDir {
    let url: URL

    init() throws {
        var template = Array("/tmp/fvl.XXXXXX".utf8CString)
        guard let made = mkdtemp(&template) else { throw Failure("mkdtemp: \(errnoText(errno))") }
        url = try resolvedURL(URL(fileURLWithPath: String(cString: made)))
    }

    func path(_ name: String) -> URL { url.appendingPathComponent(name) }
    func remove() { try? FileManager.default.removeItem(at: url) }
}

/// The guest's vsock ports, each a Unix socket in a directory: port N is `port-N`.
private struct UnixDialer: GuestDialer {
    let dir: URL

    func dial(port: UInt32, timeoutMilliseconds: Int) throws -> GuestStream {
        let fd = try connectUnix(dir.appendingPathComponent("port-\(port)"), timeoutMilliseconds: timeoutMilliseconds)
        setTimeouts(fd, milliseconds: 0)  // a pumped connection waits as long as its peer does
        return GuestStream(fd: fd, keep: nil) { close(fd) }
    }
}

/// A stand-in for one guest service: accepts on `port-N` and hands each connection to
/// `handle` on a thread of its own, counting them.
private final class FakeService: @unchecked Sendable {
    private let fd: Int32
    private let lock = NSLock()
    private var count = 0

    init(_ dir: SocketDir, port: UInt32, handle: @escaping @Sendable (Int32) -> Void) throws {
        fd = try listenUnix(dir.path("port-\(port)"), mode: 0o600).fd
        Thread.detachNewThread { [self] in
            while true {
                let conn = accept(fd, nil, nil)
                if conn < 0 { return }
                lock.lock()
                count += 1
                lock.unlock()
                Thread.detachNewThread {
                    handle(conn)
                    close(conn)
                }
            }
        }
    }

    var accepted: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }

    func stop() {
        shutdown(fd, SHUT_RDWR)
        close(fd)
    }
}

private func readToEOF(_ fd: Int32, max: Int = 4096) -> [UInt8] {
    var out: [UInt8] = []
    var chunk = [UInt8](repeating: 0, count: 1024)
    while out.count < max {
        let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
        if n < 0, errno == EINTR { continue }
        guard n > 0 else { break }
        out.append(contentsOf: chunk[0..<n])
    }
    return out
}

private func waitUntil(seconds: Double = 5, _ ok: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while Date() < deadline {
        if ok() { return true }
        usleep(20_000)
    }
    return ok()
}

@MainActor
func testLinuxGuest(_ t: Harness, _ scratch: Scratch) {
    testReplyJudges(t)
    testLinuxRunOptions(t)
    testLinuxChildArguments(t)
    testLinuxBundle(t, scratch)
    testBridge(t)
    testLinuxAgent(t)
}

@MainActor
private func testReplyJudges(_ t: Harness) {
    func b(_ s: String) -> [UInt8] { Array(s.utf8) }
    let docker = "HTTP/1.0 200 OK\r\nApi-Version: 1.51\r\nContent-Length: 2\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nOK"
    t.check("ping: Docker's HTTP/1.0 200 with OK is ready", judgePing(b(docker)))
    t.check("ping: HTTP/1.1 200 with OK is ready", judgePing(b("HTTP/1.1 200 OK\r\n\r\nOK")))
    t.check("ping: a bare status line is accepted", judgePing(b("HTTP/1.0 200\r\n\r\nOK")))
    t.check("ping: 500 is not ready", !judgePing(b("HTTP/1.0 500 Internal Server Error\r\n\r\nOK")))
    t.check("ping: 2000 is not 200", !judgePing(b("HTTP/1.0 2000 OK\r\n\r\nOK")))
    t.check("ping: HTTP/2 is refused", !judgePing(b("HTTP/2 200\r\n\r\nOK")))
    t.check("ping: another body is not ready", !judgePing(b("HTTP/1.0 200 OK\r\n\r\nNO")))
    t.check("ping: a body with more than OK is not ready", !judgePing(b("HTTP/1.0 200 OK\r\n\r\nOK\n")))
    t.check("ping: no end of headers is not ready", !judgePing(b("HTTP/1.0 200 OK\r\nOK")))
    t.check("ping: an empty reply is not ready", !judgePing([]))
    let padded = "HTTP/1.0 200 OK\r\nX-Pad: " + String(repeating: "a", count: 500) + "\r\n\r\nOK"
    t.check("ping: more than 512 bytes is refused", padded.utf8.count > 512 && !judgePing(b(padded)))
    t.check("the ping request is HTTP/1.0 to /_ping", pingRequest == b("GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n"))

    t.check("probe: sent 3 is the one good reply", judgeProbeReply(b("sent 3\n")))
    t.check("probe: fewer sent is refused", !judgeProbeReply(b("sent 2\n")))
    t.check("probe: no LF is refused", !judgeProbeReply(b("sent 3")))
    t.check("probe: bytes after the LF are refused", !judgeProbeReply(b("sent 3\nx")))
    t.check("probe: nothing is refused", !judgeProbeReply([]))
    t.check("the probe line names the gateway, port 9, three datagrams",
            probeLine(gateway: IPv4.parse("192.168.100.1")!) == b("probe 192.168.100.1 9 3\n"))
}

@MainActor
private func testLinuxRunOptions(_ t: Harness) {
    let child = ["--slot", "14", "--state-dir", "/s", "--claim", "linux-engine", "--serve-socket", "/s/slot-14.sock",
                 "--network", "shared-nat"]
    func options(_ extra: [String], base: [String] = child) throws -> RunOptions {
        try RunOptions(parse(base + extra, valued: RunOptions.valued), forRun: true)
    }
    t.accepts("the docker options parse together") {
        let o = try options(["--docker-socket", "/r/docker.sock", "--docker-peer-uid", "501",
                             "--docker-vsock-port", "2400", "--probe-vsock-port", "6200"])
        guard o.dockerSocket?.path == "/r/docker.sock", o.dockerPeerUID == 501, o.dockerVsockPort == 2400,
              o.probeVsockPort == 6200 else { throw Failure("\(o)") }
    }
    let plain = try? options([])
    let defaults: Bool = plain?.dockerVsockPort == 2375 && plain?.probeVsockPort == 6101
    t.check("the vsock ports default to 2375 and 6101", defaults && plain?.dockerSocket == nil)
    t.refuses("--docker-socket needs --docker-peer-uid", containing: "go together", usage: true) {
        _ = try options(["--docker-socket", "/r/docker.sock"])
    }
    t.refuses("--docker-peer-uid needs --docker-socket", containing: "go together", usage: true) {
        _ = try options(["--docker-peer-uid", "501"])
    }
    t.refuses("--docker-socket is the child's", containing: "--serve-socket", usage: true) {
        _ = try options(["--docker-socket", "/r/docker.sock", "--docker-peer-uid", "501"],
                        base: ["--slot", "14", "--state-dir", "/s", "--network", "shared-nat"])
    }
    t.refuses("--docker-peer-uid must be a uid", usage: true) {
        _ = try options(["--docker-socket", "/r/docker.sock", "--docker-peer-uid", "-1"])
    }
    t.refuses("--docker-vsock-port needs --docker-socket", usage: true) { _ = try options(["--docker-vsock-port", "2375"]) }
    t.refuses("vsock port 0 is refused", usage: true) { _ = try options(["--probe-vsock-port", "0"]) }
    t.refuses("vsock port any (2^32-1) is refused", usage: true) { _ = try options(["--probe-vsock-port", "4294967295"]) }

    let docker = try? options(["--docker-socket", "/r/docker.sock", "--docker-peer-uid", "501"])
    t.refuses("a macOS bundle refuses the docker options", containing: "Linux VM", usage: true) {
        try checkLinuxOnlyOptions(docker!, kind: .macOS)
    }
    t.refuses("a macOS bundle refuses --probe-vsock-port", usage: true) {
        try checkLinuxOnlyOptions(try options(["--probe-vsock-port", "6101"]), kind: .macOS)
    }
    t.accepts("a Linux bundle takes them") { try checkLinuxOnlyOptions(docker!, kind: .linux) }
    t.accepts("a macOS bundle without them is unchanged") { try checkLinuxOnlyOptions(try options([]), kind: .macOS) }
}

@MainActor
private func testLinuxChildArguments(_ t: Harness) {
    let state = URL(fileURLWithPath: "/v/state")
    let linux = StartRequest(slot: 14, kind: .linux, claim: "linux-engine", image: "linux", tools: nil, cpus: 4, memoryMiB: 8192)
    let mac = StartRequest(slot: 2, kind: .macos, claim: "m", image: "macos-27", tools: "t", cpus: 2, memoryMiB: 4096)
    let docker = (socket: URL(fileURLWithPath: "/v/run/docker.sock"), peerUID: uid_t(480))
    let args = childArguments(executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/v/linux/linux"), start: linux,
                              stateDir: state, tools: nil, network: nil, docker: docker)
    do {
        let o = try RunOptions(parse(Array(args.dropFirst(3)), valued: RunOptions.valued), forRun: true)
        t.check("a Linux child gets the docker socket and its peer", o.dockerSocket?.path == "/v/run/docker.sock"
            && o.dockerPeerUID == 480, "\(args)")
    } catch {
        t.check("run accepts the Linux child's command line", false, "\(error)")
    }
    let macArgs = childArguments(executable: "/x/faber-vm", bundle: URL(fileURLWithPath: "/c/vm-2"), start: mac,
                                 stateDir: state, tools: URL(fileURLWithPath: "/t/t"), network: nil, docker: docker)
    t.check("a macOS child never gets the docker options", !macArgs.contains("--docker-socket") && !macArgs.contains("--docker-peer-uid"))

    // supervise: --docker-socket if given, else docker.sock beside --socket.
    let base = ["--socket", "/v/run/supervisor.sock", "--peer-uid", "480", "--state-dir", "/v/state", "--image-dir", "/i",
                "--tools-dir", "/t", "--clone-dir", "/c", "--network", "shared-nat"]
    do {
        let plain = try supervisorConfig(base)
        t.check("the docker socket defaults to beside the supervisor's", plain.dockerSocketPath.path == "/v/run/docker.sock"
            && plain.linuxDir == nil)
        let given = try supervisorConfig(base + ["--docker-socket", "/w/d.sock", "--linux-dir", "/v/linux"])
        t.check("--docker-socket and --linux-dir are taken", given.dockerSocketPath.path == "/w/d.sock"
            && given.linuxDir?.path == "/v/linux")
    } catch {
        t.check("supervise options", false, "\(error)")
    }
    t.refuses("the docker socket cannot be the supervisor's", usage: true) {
        _ = try supervisorConfig(base + ["--docker-socket", "/v/run/supervisor.sock"])
    }
}

@MainActor
private func testLinuxBundle(_ t: Harness, _ scratch: Scratch) {
    do {
        let root = try scratch.dir("linux-bundle")
        let images = try scratch.dir("linux-bundle/images/linux")
        let linuxDir = try scratch.dir("linux-bundle/linux")
        chmod(linuxDir.path, 0o700)
        let base = images.appendingPathComponent("disk.raw")
        try scratch.write(base, Data("base image".utf8))
        chmod(base.path, 0o444)
        let bundle = linuxDir.appendingPathComponent("linux")

        let made = try prepareLinuxBundle(image: images, bundle: bundle)
        var st = stat()
        let disk = bundle.appendingPathComponent("disk.raw")
        t.check("the first start makes the bundle", made && lstat(bundle.path, &st) == 0
            && st.st_mode & S_IFMT == S_IFDIR && st.st_mode & 0o777 == 0o700)
        t.check("the disk is cloned, this account's and 0600", lstat(disk.path, &st) == 0 && st.st_uid == geteuid()
            && st.st_mode & 0o777 == 0o600 && (try? Data(contentsOf: disk)) == Data("base image".utf8))
        t.check("the base image is untouched", lstat(base.path, &st) == 0 && st.st_mode & 0o777 == 0o444)

        // The VM's writes are the only copy of the runner image: a second start keeps them.
        try Data("runner image".utf8).write(to: disk)
        let before = FileID.ofRegularFile(path: disk.path)
        let again = try prepareLinuxBundle(image: images, bundle: bundle)
        t.check("a bundle that exists is never recreated", !again && FileID.ofRegularFile(path: disk.path) == before
            && (try? Data(contentsOf: disk)) == Data("runner image".utf8))
        try FileManager.default.removeItem(at: base)
        t.check("... even once the base image is gone", (try? prepareLinuxBundle(image: images, bundle: bundle)) == false
            && (try? Data(contentsOf: disk)) == Data("runner image".utf8))

        let missing = linuxDir.appendingPathComponent("other")
        t.refuses("an image without disk.raw is refused") { _ = try prepareLinuxBundle(image: images, bundle: missing) }
        t.check("... and leaves no directory", !pathExists(missing))

        let file = linuxDir.appendingPathComponent("a-file")
        try scratch.write(file, Data("keep".utf8))
        t.refuses("a file at the bundle's path is refused", containing: "not a directory") {
            _ = try prepareLinuxBundle(image: images, bundle: file)
        }
        t.check("... and left alone", (try? Data(contentsOf: file)) == Data("keep".utf8))
        _ = root
    } catch {
        t.check("linux bundle fixtures", false, "\(error)")
    }
}

@MainActor
private func testBridge(_ t: Harness) {
    do {
        let dir = try SocketDir()
        defer { dir.remove() }
        let dialer = UnixDialer(dir: dir.url)
        // Docker's stand-in: read the whole request, then answer after it, as an attached
        // `docker run` reads its output after closing stdin.
        let docker = try FakeService(dir, port: 2375) { conn in
            let got = readToEOF(conn)
            usleep(100_000)
            try? writeAll(conn, Array("reply:".utf8) + got)
        }
        defer { docker.stop() }
        let path = dir.path("docker.sock")
        t.check("the docker socket does not exist before the bridge", !pathExists(path))
        let bridge = try DockerBridge(path: path, peerUID: geteuid(), dialer: dialer, port: 2375, maxConnections: 2)
        bridge.start()
        var st = stat()
        t.check("the docker socket is 0660", lstat(path.path, &st) == 0 && st.st_mode & S_IFMT == S_IFSOCK
            && st.st_mode & 0o777 == 0o660)

        let c = try connectUnix(path, timeoutSeconds: 5)
        try writeAll(c, Array("hello".utf8))
        shutdown(c, SHUT_WR)
        let reply = readToEOF(c)
        close(c)
        t.check("a half-closed client still gets the reply sent after its EOF", reply == Array("reply:hello".utf8),
                String(decoding: reply, as: UTF8.self))
        t.check("the pair is released once both directions end", waitUntil { bridge.activeConnections == 0 })

        // The cap: two held open, the third closed at once, and a slot freed is usable again.
        let held = try FakeService(dir, port: 2400) { conn in _ = readToEOF(conn) }
        defer { held.stop() }
        let capped = try DockerBridge(path: dir.path("capped.sock"), peerUID: geteuid(), dialer: dialer, port: 2400,
                                      maxConnections: 2)
        capped.start()
        let a = try connectUnix(capped.path, timeoutSeconds: 5)
        let b = try connectUnix(capped.path, timeoutSeconds: 5)
        t.check("two connections are joined", waitUntil { held.accepted == 2 && capped.activeConnections == 2 })
        let third = try connectUnix(capped.path, timeoutSeconds: 5)
        var byte: UInt8 = 0
        t.check("a connection over the cap is closed without a byte", read(third, &byte, 1) == 0 && held.accepted == 2)
        close(third)
        close(a)
        t.check("closing one frees its slot", waitUntil { capped.activeConnections == 1 })
        let d = try connectUnix(capped.path, timeoutSeconds: 5)
        t.check("... and the next one is joined", waitUntil { held.accepted == 3 })
        close(b)
        close(d)
        capped.close()

        // Another uid gets nothing, and the guest is never dialled for it.
        let before = docker.accepted
        let refusing = try DockerBridge(path: dir.path("other.sock"), peerUID: geteuid() &+ 1, dialer: dialer, port: 2375)
        refusing.start()
        let other = try connectUnix(refusing.path, timeoutSeconds: 5)
        t.check("another uid is closed before a byte", read(other, &byte, 1) == 0)
        usleep(100_000)
        t.check("... and never reaches the guest", docker.accepted == before)
        close(other)
        refusing.close()
        t.check("close removes the socket it made", !pathExists(refusing.path))

        bridge.close()
        t.check("the bridge's socket is removed at stop", !pathExists(path))
        // A socket put at the path since is not this bridge's, and is left alone.
        let replaced = try DockerBridge(path: path, peerUID: geteuid(), dialer: dialer, port: 2375)
        unlink(path.path)
        let (otherFD, _) = try listenUnix(path, mode: 0o600)
        replaced.close()
        t.check("close leaves a socket it did not make", pathExists(path))
        close(otherFD)
    } catch {
        t.check("bridge fixtures", false, "\(error)")
    }
}

/// A box the fake probe service writes the line it read into.
private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var bytes: [UInt8] = []
    func set(_ b: [UInt8]) { lock.lock(); bytes = b; lock.unlock() }
    var value: [UInt8] { lock.lock(); defer { lock.unlock() }; return bytes }
}

@MainActor
private func testLinuxAgent(_ t: Harness) {
    do {
        let dir = try SocketDir()
        defer { dir.remove() }
        let dialer = UnixDialer(dir: dir.url)
        let status = ChildStatus()
        let socket = dir.path("docker.sock")
        let bridge = DockerBridgeConfig(socket: socket, peerUID: geteuid(), vsockPort: 2375)
        let agent = LinuxGuestAgent(dialer: dialer, status: status, gateway: IPv4.parse("192.168.100.1"), bridge: bridge)

        // Readiness: nothing answers, then Docker does.
        _ = status.move(from: .starting, to: .booting)
        t.check("no relay: not ready", !agent.pingOnce())
        let ping = try FakeService(dir, port: 2375) { conn in
            _ = readReply(conn, max: 512, milliseconds: 1000, stopAtLF: true).count
            try? writeAll(conn, Array("HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK".utf8))
        }
        defer { ping.stop() }
        agent.startReadinessPoll(interval: 0.1)
        t.check("Docker answering /_ping moves booting to ready", waitUntil { status.state == .ready })

        // The probe: the launcher's line, and only `sent 3` is success.
        let seen = Seen()
        let reply = Seen()
        reply.set(Array("sent 3\n".utf8))
        let probe = try FakeService(dir, port: 6101) { conn in
            seen.set(readReply(conn, max: 64, milliseconds: 1000, stopAtLF: true))
            try? writeAll(conn, reply.value)
        }
        defer { probe.stop() }
        t.accepts("the probe succeeds on sent 3") { try agent.sendProbe() }
        t.check("the guest is told the gateway, port 9, three datagrams", seen.value == Array("probe 192.168.100.1 9 3\n".utf8))
        reply.set(Array("sent 2\n".utf8))
        t.refuses("a short count fails the probe", containing: "did not confirm") { try agent.sendProbe() }
        let nat = LinuxGuestAgent(dialer: dialer, status: status, gateway: nil, bridge: bridge)
        t.refuses("no known gateway: no probe", containing: "gateway") { try nat.sendProbe() }

        // begin: no jit_config, then the bridge, once.
        t.refuses("begin with a jit_config is refused", containing: "jit_config") { try agent.begin(jitConfig: JitConfig("ZmFrZQ==")) }
        t.check("... and opens no socket", !pathExists(socket))
        let bare = LinuxGuestAgent(dialer: dialer, status: status, gateway: nil, bridge: nil)
        t.refuses("begin without --docker-socket is refused") { try bare.begin(jitConfig: nil) }
        t.accepts("begin opens the bridge") { try agent.begin(jitConfig: nil) }
        t.check("... at the docker socket", pathExists(socket))
        t.refuses("a second begin is refused") { try agent.begin(jitConfig: nil) }
        agent.stop()
        t.check("stop removes the docker socket", !pathExists(socket))
        t.refuses("no bridge opens after stop") { try agent.begin(jitConfig: nil) }

        // A child whose begin cannot listen answers wrong_state, and stays probe_sent.
        let blocked = dir.path("blocked")
        try Data().write(to: blocked)
        let s2 = ChildStatus()
        let failing = LinuxGuestAgent(dialer: dialer, status: s2, gateway: nil,
                                      bridge: DockerBridgeConfig(socket: blocked, peerUID: geteuid(), vsockPort: 2375))
        let server = try ChildServer(path: dir.path("slot-14.sock"), slot: 14, claim: "linux-engine", status: s2,
                                     agent: failing) {}
        defer { server.close() }
        s2.set(.probeSent)
        var greeted = true
        t.check("a bridge that cannot listen fails begin", server.answer(.begin(jitConfig: nil), greeted: &greeted)
            == (.error(.wrongState), true) && s2.state == .probeSent)
        t.check("... and the file at the path is left", (try? Data(contentsOf: blocked)) == Data())
    } catch {
        t.check("linux agent fixtures", false, "\(error)")
    }
}
