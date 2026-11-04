// The host side of the Linux VM's Docker socket (ADR-0026, "The Linux VM runs Docker Engine
// under the same launcher"): a Unix socket, 0660 in a directory only this account can write,
// that takes the directory's group and so lets `_faber` in, and admits only the one uid
// --docker-peer-uid names. Each client is joined to a fresh vsock connection to the guest's
// relay on port 2375, and the bytes are pumped both ways.
//
// Half-closes are kept, as the guest's relay keeps them: an attached `docker run` or a
// `docker exec` shuts its write side once stdin ends and still reads the output. So an EOF on
// one side becomes shutdown(SHUT_WR) on the other, a pair is closed only when both directions
// have ended, and an error in either direction shuts both down.

import Darwin
import Foundation

final class DockerBridge: @unchecked Sendable {
    static let defaultMaxConnections = 64

    let path: URL
    let allowedUID: uid_t
    let dialer: GuestDialer
    let port: UInt32
    let maxConnections: Int
    private let fd: Int32
    private let socketID: FileID
    private let lock = NSLock()
    private var active = 0
    private var closed = false

    /// Listens at once; the socket exists from here until close.
    init(path: URL, peerUID: uid_t, dialer: GuestDialer, port: UInt32,
         maxConnections: Int = DockerBridge.defaultMaxConnections) throws {
        // listenUnix holds the directory to requirePrivateDirectory and removes only a stale
        // socket this account owns, as for the supervisor's socket.
        (fd, socketID) = try listenUnix(path, mode: 0o660)
        self.path = path
        self.allowedUID = peerUID
        self.dialer = dialer
        self.port = port
        self.maxConnections = maxConnections
        signal(SIGPIPE, SIG_IGN)  // a peer gone mid-write is an error to handle, not a death
    }

    var activeConnections: Int {
        lock.lock()
        defer { lock.unlock() }
        return active
    }

    func start() {
        Thread.detachNewThread { [self] in serve() }
    }

    /// Stop listening and remove the socket, only if it is still the one this bridge bound.
    /// Connections already joined run to their end.
    func close() {
        lock.lock()
        let wasClosed = closed
        closed = true
        lock.unlock()
        guard !wasClosed else { return }
        Darwin.shutdown(fd, SHUT_RDWR)
        Darwin.close(fd)
        removeSocket(path, id: socketID)
    }

    private var isClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return closed
    }

    private func reserve() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard active < maxConnections else { return false }
        active += 1
        return true
    }

    private func release() {
        lock.lock()
        active -= 1
        lock.unlock()
    }

    private func serve() {
        while true {
            let conn = accept(fd, nil, nil)
            if conn < 0 {
                if isClosed { return }
                if errno == EINTR || errno == ECONNABORTED { continue }
                return
            }
            _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
            var on: Int32 = 1
            _ = setsockopt(conn, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            // The peer's uid before anything else, then the cap: either refusal closes the
            // connection without reading or writing a byte.
            guard admitPeer(peerUID(conn), expected: allowedUID), reserve() else {
                Darwin.close(conn)
                continue
            }
            Thread.detachNewThread { [self] in join(conn) }
        }
    }

    private func join(_ client: Int32) {
        guard let upstream = try? dialer.dial(port: port, timeoutMilliseconds: 2000) else {
            Darwin.close(client)
            release()
            return
        }
        let pair = BridgedPair(client: client, upstream: upstream) { [self] in release() }
        Thread.detachNewThread { pair.pump(from: client, to: upstream.fd) }
        pair.pump(from: upstream.fd, to: client)
    }
}

/// One client and its guest connection. Each direction is pumped on a thread of its own; the
/// pair closes both descriptors, and keeps the guest connection's owner alive until then.
final class BridgedPair: @unchecked Sendable {
    let client: Int32
    let upstream: GuestStream
    private let lock = NSLock()
    private var openDirections = 2
    private let onClose: () -> Void

    init(client: Int32, upstream: GuestStream, onClose: @escaping () -> Void) {
        self.client = client
        self.upstream = upstream
        self.onClose = onClose
    }

    func pump(from src: Int32, to dst: Int32) {
        var buffer = [UInt8](repeating: 0, count: 64 << 10)
        while true {
            let n = buffer.withUnsafeMutableBytes { read(src, $0.baseAddress, $0.count) }
            if n < 0, errno == EINTR { continue }
            if n == 0 {
                Darwin.shutdown(dst, SHUT_WR)
                break
            }
            guard n > 0, (try? writeAll(dst, Array(buffer[0..<n]))) != nil else {
                abort()
                break
            }
        }
        directionDone()
    }

    private func abort() {
        Darwin.shutdown(client, SHUT_RDWR)
        Darwin.shutdown(upstream.fd, SHUT_RDWR)
    }

    private func directionDone() {
        lock.lock()
        openDirections -= 1
        let last = openDirections == 0
        lock.unlock()
        guard last else { return }
        Darwin.close(client)
        upstream.close()
        onClose()
    }
}
