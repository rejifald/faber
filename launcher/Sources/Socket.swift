// Unix stream sockets for the supervisor protocol and the child protocol: where a socket may
// live and who may connect. The peer check, the writer and the frame reader are in
// Shared/Wire.swift, which the guest agent shares.

import Darwin
import Foundation

func unixAddress(_ path: String) throws -> sockaddr_un {
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8)
    let capacity = MemoryLayout.size(ofValue: addr.sun_path)
    guard !bytes.isEmpty, bytes.count < capacity else {
        throw Failure("\(path): a socket path must be 1...\(capacity - 1) bytes")
    }
    withUnsafeMutableBytes(of: &addr.sun_path) { raw in
        raw.copyBytes(from: bytes)
        raw[bytes.count] = 0
    }
    addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
    return addr
}

private func newSocket() throws -> Int32 {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw Failure("socket: \(errnoText(errno))") }
    _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
    var on: Int32 = 1
    _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
    return fd
}

/// What may sit at a socket path before bind: nothing, or a stale socket this account owns,
/// which is removed. Anything else (a file, a symlink, another account's socket) is refused,
/// never removed. Pure, so a test can hand it any stat.
func checkStaleSocket(_ st: stat, euid: uid_t, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFSOCK else {
        throw Failure("\(path) exists and is not a socket; refusing to remove it")
    }
    guard st.st_uid == euid else {
        throw Failure("\(path) is a socket owned by uid \(st.st_uid), not by this process's uid \(euid); refusing to remove it")
    }
}

/// A listening socket at `path`, mode `mode`. The parent directory must be this account's
/// own, with no group or other write, so nobody else can replace the socket; on macOS the
/// socket takes the directory's group, which is how setup lets `_faber` reach the
/// supervisor's (0660) and nobody reach a child's (0600, in the 0700 state directory).
/// Returns the descriptor and the socket's identity, so close removes only this socket.
func listenUnix(_ path: URL, mode: mode_t) throws -> (fd: Int32, id: FileID) {
    try requirePrivateDirectory(path.deletingLastPathComponent())
    var st = stat()
    if lstat(path.path, &st) == 0 {
        try checkStaleSocket(st, euid: geteuid(), path: path.path)
        guard unlink(path.path) == 0 else { throw Failure("\(path.path): unlink: \(errnoText(errno))") }
    }
    var addr = try unixAddress(path.path)
    let fd = try newSocket()
    // The mode is set at bind by the umask, so the socket is never briefly wider than asked.
    let old = umask(0o777 & ~mode)
    let bound = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    let bindErrno = errno
    umask(old)
    guard bound == 0 else {
        close(fd)
        throw Failure("\(path.path): bind: \(errnoText(bindErrno))")
    }
    guard chmod(path.path, mode) == 0, listen(fd, 16) == 0, lstat(path.path, &st) == 0 else {
        let code = errno
        close(fd)
        unlink(path.path)
        throw Failure("\(path.path): \(errnoText(code))")
    }
    return (fd, FileID(st))
}

/// Remove the socket at `path` only if it is still the one this process bound.
func removeSocket(_ path: URL, id: FileID) {
    var st = stat()
    if lstat(path.path, &st) == 0, st.st_mode & S_IFMT == S_IFSOCK, FileID(st) == id {
        unlink(path.path)
    }
}

/// Connect to `path`, with send and receive timeouts so a silent peer cannot hang the caller.
func connectUnix(_ path: URL, timeoutSeconds: Int) throws -> Int32 {
    try connectUnix(path, timeoutMilliseconds: timeoutSeconds * 1000)
}

func connectUnix(_ path: URL, timeoutMilliseconds: Int) throws -> Int32 {
    var addr = try unixAddress(path.path)
    let fd = try newSocket()
    setTimeouts(fd, milliseconds: timeoutMilliseconds)
    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    guard ok == 0 else {
        let code = errno
        close(fd)
        throw Failure("\(path.path): connect: \(errnoText(code))")
    }
    return fd
}
