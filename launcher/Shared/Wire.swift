// Plumbing every faber-vm wire shares, and the guest agent with it: errors, the peer check,
// timeouts, a writer, and a frame reader that enforces its cap before it buffers more.

import Darwin
import Foundation

/// A refusal or failure: exit 1.
struct Failure: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

/// "Permission denied (errno 13)". Read errno before anything else can change it.
func errnoText(_ code: Int32) -> String {
    "\(String(cString: strerror(code))) (errno \(code))"
}

/// Whether a connection's peer may talk at all. A peer that is not `expected` is closed at
/// once: nothing is read from it and nothing written to it. Pure, so a test can hand it any
/// uid; `peer` is nil when getpeereid(3) failed, which is refused too.
func admitPeer(_ peer: uid_t?, expected: uid_t) -> Bool {
    guard let peer else { return false }
    return peer == expected
}

/// The connected peer's process id, for logs only (it can be reused; never a trust input).
func peerPID(_ fd: Int32) -> pid_t? {
    var pid: pid_t = 0
    var len = socklen_t(MemoryLayout<pid_t>.size)
    return getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &pid, &len) == 0 ? pid : nil
}

/// The connection's peer uid, by getpeereid(3).
func peerUID(_ fd: Int32) -> uid_t? {
    var uid: uid_t = 0
    var gid: gid_t = 0
    return getpeereid(fd, &uid, &gid) == 0 ? uid : nil
}

func setTimeouts(_ fd: Int32, seconds: Int) { setTimeouts(fd, milliseconds: seconds * 1000) }

func setTimeouts(_ fd: Int32, milliseconds: Int) {
    var tv = timeval(tv_sec: milliseconds / 1000, tv_usec: Int32(milliseconds % 1000) * 1000)
    _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
}

/// Write every byte, or fail: a short write past the socket's timeout is a failure.
func writeAll(_ fd: Int32, _ bytes: [UInt8]) throws {
    var done = 0
    while done < bytes.count {
        let n = bytes[done...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        if n < 0 {
            if errno == EINTR { continue }
            throw Failure("write: \(errnoText(errno))")
        }
        done += n
    }
}

/// Reads one frame at a time from a blocking socket. It never asks for more than the cap
/// leaves room for, so an oversized frame is refused at `cap` bytes, not after it has been
/// buffered whole, and it never reads past a frame's LF into the next one's bytes.
struct FrameReader {
    let fd: Int32
    /// The largest frame, its LF included.
    let cap: Int
    private var buffer: [UInt8] = []

    init(fd: Int32, cap: Int = maxFrame) {
        self.fd = fd
        self.cap = cap
    }

    /// The next frame, LF included; nil on a clean EOF before any byte of it. Throws on a
    /// timeout, an EOF mid-frame or an oversized frame.
    mutating func next() throws -> [UInt8]? {
        var chunk = [UInt8](repeating: 0, count: cap)
        while true {
            let room = cap - buffer.count
            guard room > 0 else { throw ProtoError.frameTooLarge }
            // One byte at a time is too slow; a whole chunk could swallow the next frame. The
            // peer is lockstep, so until the LF arrives everything it sent is this frame.
            let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, room) }
            if n < 0 {
                if errno == EINTR { continue }
                throw Failure("read: \(errnoText(errno))")
            }
            if n == 0 {
                if buffer.isEmpty { return nil }
                throw ProtoError.notOneLine
            }
            buffer.append(contentsOf: chunk[0..<n])
            if let lf = buffer.firstIndex(of: 0x0A) {
                // Bytes after the LF are a second frame sent before this one's reply: lockstep
                // is broken, and the frame is refused like two frames on one line.
                guard lf == buffer.count - 1 else { throw ProtoError.notOneLine }
                let frame = buffer
                buffer = []
                return frame
            }
        }
    }
}
