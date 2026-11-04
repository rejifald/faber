// The Linux guest agent (ADR-0026 slice 7): the long-lived Linux VM has no agent of its own.
// Its readiness is Docker answering `GET /_ping` through the guest's relay on vsock 2375, its
// wall probe is one line to the guest's probe service on vsock 6101, and `begin` opens the
// host-side Docker socket bridge (DockerBridge.swift). The launcher picks the probe's target,
// never the guest: the slot's gateway, UDP port 9, three datagrams, as slice 5's macOS probe.
//
// Nothing the guest sends is logged, and nothing beyond the two replies judged here is parsed.

import Darwin
import Foundation
import Virtualization

// MARK: - Guest connections

/// One stream connection into the guest. `keep` holds whatever owns the descriptor (a
/// VZVirtioSocketConnection) for as long as the stream lives; `release` closes it, once.
final class GuestStream: @unchecked Sendable {
    let fd: Int32
    private let lock = NSLock()
    private var keep: AnyObject?
    private var release: (() -> Void)?

    init(fd: Int32, keep: AnyObject?, release: @escaping () -> Void) {
        self.fd = fd
        self.keep = keep
        self.release = release
        var on: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
    }

    func close() {
        lock.lock()
        let r = release
        release = nil
        keep = nil
        lock.unlock()
        r?()
    }

    deinit { close() }
}

/// Opens connections to the guest's vsock ports: the VM's socket device in run, a Unix
/// socket standing in for it in the tests.
protocol GuestDialer: Sendable {
    func dial(port: UInt32, timeoutMilliseconds: Int) throws -> GuestStream
}

/// A non-Sendable framework object, touched only on the main queue (the VM's queue).
private final class MainQueueBox<T: AnyObject>: @unchecked Sendable {
    let value: T
    init(_ value: T) { self.value = value }
}

/// The result of one connect(toPort:), handed from the VM's queue to the waiting thread. A
/// connection that arrives after the waiter gave up is closed at once.
private final class DialResult: @unchecked Sendable {
    private let lock = NSLock()
    private let done = DispatchSemaphore(value: 0)
    private var connection: VZVirtioSocketConnection?
    private var failure: String?
    private var abandoned = false

    func finish(_ result: Result<VZVirtioSocketConnection, Error>) {
        lock.lock()
        defer { lock.unlock(); done.signal() }
        switch result {
        case .success(let c):
            if abandoned { c.close() } else { connection = c }
        case .failure(let e):
            failure = describe(e)
        }
    }

    func wait(milliseconds: Int) throws -> VZVirtioSocketConnection {
        let timedOut = done.wait(timeout: .now() + .milliseconds(milliseconds)) == .timedOut
        lock.lock()
        defer { lock.unlock() }
        if let connection { return connection }
        if timedOut { abandoned = true }
        throw Failure(failure.map { "vsock connect: \($0)" } ?? "vsock connect: timed out")
    }
}

/// The VM's one virtio socket device. The framework requires connect(toPort:) on the VM's
/// queue, which is the main queue, so a dial is posted there and waited for on the caller's
/// thread; it must never be called from the main thread itself.
final class VMDialer: GuestDialer, @unchecked Sendable {
    private let device: MainQueueBox<VZVirtioSocketDevice>

    @MainActor
    init?(_ vm: VZVirtualMachine) {
        guard let device = vm.socketDevices.first as? VZVirtioSocketDevice else { return nil }
        self.device = MainQueueBox(device)
    }

    func dial(port: UInt32, timeoutMilliseconds: Int) throws -> GuestStream {
        precondition(!Thread.isMainThread, "a vsock dial waits for the main queue")
        let result = DialResult()
        let device = self.device
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                device.value.connect(toPort: port) { result.finish($0) }
            }
        }
        let connection = MainQueueBox(try result.wait(milliseconds: timeoutMilliseconds))
        return GuestStream(fd: connection.value.fileDescriptor, keep: connection) {
            DispatchQueue.main.async { connection.value.close() }
        }
    }
}

// MARK: - Reading a short reply

/// Read from `fd` until EOF, a LF if `stopAtLF`, more than `max` bytes, or `milliseconds`
/// pass; returns what arrived, at most `max + 1` bytes so an oversized reply shows as one.
/// poll(2) rather than socket timeouts, so any descriptor the framework hands over works.
func readReply(_ fd: Int32, max: Int, milliseconds: Int, stopAtLF: Bool) -> [UInt8] {
    var out: [UInt8] = []
    let deadline = Date().addingTimeInterval(Double(milliseconds) / 1000)
    var chunk = [UInt8](repeating: 0, count: max + 1)
    while out.count <= max {
        let left = Int(deadline.timeIntervalSinceNow * 1000)
        guard left > 0 else { break }
        var p = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
        let r = poll(&p, 1, Int32(left))
        if r < 0, errno == EINTR { continue }
        guard r > 0 else { break }
        let room = max + 1 - out.count
        let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, room) }
        if n < 0, errno == EINTR { continue }
        guard n > 0 else { break }
        out.append(contentsOf: chunk[0..<n])
        if stopAtLF, out.contains(0x0A) { break }
    }
    return out
}

// MARK: - The two replies

let pingRequest = Array("GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n".utf8)
let maxPingReply = 512
let maxProbeReply = 32

/// Docker's answer to `GET /_ping`: at most 512 bytes, a status line of exactly HTTP/1.0 or
/// HTTP/1.1 and 200, and the body `OK`. Anything else is not ready. Pure.
func judgePing(_ reply: [UInt8]) -> Bool {
    guard reply.count <= maxPingReply, let end = firstIndex(of: [0x0D, 0x0A, 0x0D, 0x0A], in: reply) else { return false }
    let head = Array(reply[..<end])
    let status = String(decoding: head[..<(firstIndex(of: [0x0D, 0x0A], in: head) ?? head.count)], as: UTF8.self)
    guard ["HTTP/1.0 200", "HTTP/1.1 200"].contains(where: { status == $0 || status.hasPrefix($0 + " ") }) else { return false }
    return Array(reply[(end + 4)...]) == Array("OK".utf8)
}

private func firstIndex(of needle: [UInt8], in hay: [UInt8]) -> Int? {
    guard hay.count >= needle.count else { return nil }
    return (0...(hay.count - needle.count)).first { Array(hay[$0..<$0 + needle.count]) == needle }
}

/// The line the guest's probe service reads: the target and the count are the launcher's.
func probeLine(gateway: IPv4, port: Int = 9, count: Int = 3) -> [UInt8] {
    Array("probe \(gateway) \(port) \(count)\n".utf8)
}

/// The probe service's reply: exactly `sent <count>` and a LF. Pure.
func judgeProbeReply(_ reply: [UInt8], count: Int = 3) -> Bool {
    reply == Array("sent \(count)\n".utf8)
}

// MARK: - The agent

/// What `begin` opens: the host socket faber's Docker calls use, and the one uid let in.
struct DockerBridgeConfig {
    let socket: URL
    let peerUID: uid_t
    let vsockPort: UInt32
}

final class LinuxGuestAgent: GuestAgent, @unchecked Sendable {
    let dialer: GuestDialer
    let status: ChildStatus
    /// The host's address on this VM's network; nil on Apple's shared NAT, where the
    /// launcher does not know it, so the probe is refused.
    let gateway: IPv4?
    let dockerPort: UInt32
    let probePort: UInt32
    let bridgeConfig: DockerBridgeConfig?
    private let lock = NSLock()
    private var bridge: DockerBridge?
    private var stopped = false

    static let dialTimeoutMs = 1000
    static let pingTimeoutMs = 1000
    static let probeTimeoutMs = 1500

    init(dialer: GuestDialer, status: ChildStatus, gateway: IPv4?, dockerPort: UInt32 = 2375,
         probePort: UInt32 = 6101, bridge: DockerBridgeConfig?) {
        self.dialer = dialer
        self.status = status
        self.gateway = gateway
        self.dockerPort = dockerPort
        self.probePort = probePort
        self.bridgeConfig = bridge
    }

    var isPresent: Bool { true }

    /// Whether Docker answers /_ping through the guest's relay now.
    func pingOnce() -> Bool {
        guard let stream = try? dialer.dial(port: dockerPort, timeoutMilliseconds: Self.dialTimeoutMs) else { return false }
        defer { stream.close() }
        guard (try? writeAll(stream.fd, pingRequest)) != nil else { return false }
        return judgePing(readReply(stream.fd, max: maxPingReply, milliseconds: Self.pingTimeoutMs, stopAtLF: false))
    }

    /// Poll every `interval` seconds while the VM is starting or booting, and move it from
    /// booting to ready the first time Docker answers. Ends once the VM is past booting.
    func startReadinessPoll(interval: TimeInterval = 2) {
        Thread.detachNewThread { [self] in
            while true {
                switch status.state {
                case .starting: break
                case .booting:
                    if pingOnce() {
                        if status.move(from: .booting, to: .ready) { say("event=guest-ready") }
                        return
                    }
                default: return
                }
                Thread.sleep(forTimeInterval: interval)
            }
        }
    }

    func sendProbe() throws {
        guard let gateway else { throw Failure("probe: the gateway is unknown on this network") }
        let stream = try dialer.dial(port: probePort, timeoutMilliseconds: Self.dialTimeoutMs)
        defer { stream.close() }
        try writeAll(stream.fd, probeLine(gateway: gateway))
        guard judgeProbeReply(readReply(stream.fd, max: maxProbeReply, milliseconds: Self.probeTimeoutMs, stopAtLF: true)) else {
            throw Failure("probe: the guest did not confirm it")
        }
    }

    /// The Linux VM takes no JIT configuration: it runs no runner itself.
    func begin(jitConfig: JitConfig?) throws {
        guard jitConfig == nil else { throw Failure("begin: the Linux VM takes no jit_config") }
        guard let config = bridgeConfig else { throw Failure("begin: run was given no --docker-socket") }
        lock.lock()
        defer { lock.unlock() }
        guard !stopped, bridge == nil else { throw Failure("begin: the bridge is already open or stopped") }
        let b = try DockerBridge(path: config.socket, peerUID: config.peerUID, dialer: dialer, port: config.vsockPort)
        b.start()
        bridge = b
        say("event=docker-bridge socket=\(config.socket.path)")
    }

    /// Close the bridge and remove its socket; no bridge opens after this.
    func stop() {
        lock.lock()
        let b = bridge
        bridge = nil
        stopped = true
        lock.unlock()
        b?.close()
    }
}

// MARK: - run's side

/// The agents of this process, so every exit path closes their bridges.
final class LiveAgents: @unchecked Sendable {
    private let lock = NSLock()
    private var agents: [LinuxGuestAgent] = []

    func add(_ agent: LinuxGuestAgent) {
        lock.lock()
        agents.append(agent)
        lock.unlock()
    }

    func stopAll() {
        lock.lock()
        let all = agents
        lock.unlock()
        all.forEach { $0.stop() }
    }
}

let liveAgents = LiveAgents()

/// The agent for a Linux child: dials the VM's socket device, polls for readiness, and
/// removes the Docker socket however the process exits.
@MainActor
func makeLinuxGuestAgent(vm: VZVirtualMachine, options: RunOptions, status: ChildStatus) throws -> LinuxGuestAgent {
    guard let dialer = VMDialer(vm) else { throw Failure("the VM has no virtio socket device") }
    let bridge = options.dockerSocket.map {
        DockerBridgeConfig(socket: $0, peerUID: options.dockerPeerUID ?? 0, vsockPort: options.dockerVsockPort)
    }
    let agent = LinuxGuestAgent(dialer: dialer, status: status, gateway: options.vmnet?.subnet.gateway,
                                dockerPort: options.dockerVsockPort, probePort: options.probeVsockPort, bridge: bridge)
    liveAgents.add(agent)
    atexit { liveAgents.stopAll() }
    agent.startReadinessPoll()
    return agent
}

/// --docker-* and --probe-vsock-port are the Linux VM's; a macOS bundle refuses them. Pure.
func checkLinuxOnlyOptions(_ options: RunOptions, kind: BundleKind) throws {
    guard kind == .macOS else { return }
    if options.dockerSocket != nil || options.dockerVsockPortGiven || options.probeVsockPortGiven {
        throw UsageError("--docker-socket, --docker-peer-uid, --docker-vsock-port and --probe-vsock-port are for the Linux VM; a macOS bundle runs no Docker")
    }
}
