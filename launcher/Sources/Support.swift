// Shared plumbing: output, errors, argument parsing, and file identity.

import Darwin
import Foundation

// MARK: - Errors

// Failure and errnoText are in Shared/Wire.swift.

/// A malformed command line: exit 2, with the usage on stderr.
struct UsageError: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

// MARK: - Output

/// stdout carries the machine-readable key=value lines; main.swift makes it line-buffered.
func say(_ line: String) { print(line) }

/// stderr carries diagnostics for a person.
func warn(_ line: String) {
    FileHandle.standardError.write(Data("faber-vm: \(line)\n".utf8))
}

func describe(_ error: Error) -> String {
    if let failure = error as? Failure { return failure.description }
    let ns = error as NSError
    var parts = ["\(ns.localizedDescription) [\(ns.domain) \(ns.code)]"]
    if let reason = ns.localizedFailureReason, !ns.localizedDescription.contains(reason) {
        parts.append(reason)
    }
    if let underlying = ns.userInfo[NSUnderlyingErrorKey] as? NSError {
        parts.append("underlying: \(underlying.localizedDescription) [\(underlying.domain) \(underlying.code)]")
    }
    return parts.joined(separator: "; ")
}

// MARK: - Arguments

struct Arguments {
    var positional: [String] = []
    var values: [String: String] = [:]
    var switches: Set<String> = []

    func value(_ name: String) -> String? { values[name] }
    func has(_ name: String) -> Bool { values[name] != nil || switches.contains(name) }
}

/// `--name value` and `--name=value`. Every option may appear at most once, and an unknown
/// option is an error, never ignored.
func parse(_ args: [String], valued: Set<String> = [], switches: Set<String> = []) throws -> Arguments {
    var out = Arguments()
    var i = 0
    while i < args.count {
        let arg = args[i]
        if arg.hasPrefix("--") {
            var name = arg
            var value: String?
            if let eq = arg.firstIndex(of: "=") {
                name = String(arg[..<eq])
                value = String(arg[arg.index(after: eq)...])
            }
            guard !out.has(name) else { throw UsageError("\(name) given more than once") }
            if valued.contains(name) {
                if value == nil {
                    i += 1
                    guard i < args.count else { throw UsageError("\(name) needs a value") }
                    value = args[i]
                }
                out.values[name] = value ?? ""
            } else if switches.contains(name), value == nil {
                out.switches.insert(name)
            } else {
                throw UsageError("unknown option \(arg)")
            }
        } else {
            out.positional.append(arg)
        }
        i += 1
    }
    return out
}

// MARK: - Files

/// A file by identity rather than by path: a path can be re-pointed, a device and inode
/// cannot. The wall compares disks this way.
struct FileID: Equatable, CustomStringConvertible {
    let device: dev_t
    let inode: ino_t

    init(_ st: stat) {
        device = st.st_dev
        inode = st.st_ino
    }

    /// The regular file a path names, by lstat(2): nil for a symlink, anything that is not a
    /// regular file, or nothing. Every identity the wall compares comes from here, so a
    /// symlink never stands in for the file it points at.
    static func ofRegularFile(path: String) -> FileID? {
        var st = stat()
        guard lstat(path, &st) == 0, st.st_mode & S_IFMT == S_IFREG else { return nil }
        return FileID(st)
    }

    /// The file an open descriptor refers to.
    static func of(fd: Int32) -> FileID? {
        var st = stat()
        return fstat(fd, &st) == 0 ? FileID(st) : nil
    }

    var description: String { "dev=\(device) ino=\(inode)" }
}

/// A file the VM is given by path, with the identity the wall saw. The framework opens these
/// by path itself (in its own XPC service, at start), so run checks them again once the VM
/// is running.
struct FileRef {
    let url: URL
    let id: FileID
}

/// The first file whose path no longer names the regular file the wall checked, if any.
///
/// Residual: the framework's open happens in another process, so the launcher cannot see
/// which inode it actually opened. A swap made after the wall check and undone before this
/// re-check goes unseen. What stops that swap is that only this account can write the
/// bundle's directory (requirePrivateDirectory); this re-check catches one left in place.
/// The tools disk's directory is the exception: the tools owner writes it too (checkToolsAncestor).
func changedFile(_ refs: [FileRef]) -> FileRef? {
    refs.first { FileID.ofRegularFile(path: $0.url.path) != $0.id }
}

/// True when the path exists in any form, a dangling symlink included.
func pathExists(_ url: URL) -> Bool {
    var st = stat()
    return lstat(url.path, &st) == 0
}

/// The identity of a regular file that is not a symlink. Bundle files must be exactly that,
/// so a bundle cannot point the launcher at a disk outside itself.
func regularFile(_ url: URL) throws -> FileID {
    var st = stat()
    guard lstat(url.path, &st) == 0 else {
        throw Failure("\(url.path): \(errnoText(errno))")
    }
    guard st.st_mode & S_IFMT == S_IFREG else {
        throw Failure("\(url.path): not a regular file (a symlink or other file type is refused)")
    }
    return FileID(st)
}

func regularFileRef(_ url: URL) throws -> FileRef { FileRef(url: url, id: try regularFile(url)) }

func fileSize(_ url: URL) -> (bytes: Int64, allocated: Int64)? {
    var st = stat()
    guard lstat(url.path, &st) == 0 else { return nil }
    return (Int64(st.st_size), Int64(st.st_blocks) * 512)
}

func absoluteURL(_ path: String) -> URL {
    URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL
}

/// `path` with its deepest existing ancestor's symlinks resolved and the rest kept as given:
/// one spelling for one place, whether or not the file itself exists yet. `standardizedFileURL`
/// is not: it drops a leading `/private` only when the result exists, so the existing
/// `/private/var/db/faber-vm/state` standardizes to `/var/...` while a socket not yet bound
/// under it keeps `/private` (observed in practice, October 2026).
func canonicalURL(_ path: String) -> URL {
    let url = URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardized
    var existing = url
    var rest: [String] = []
    while existing.path != "/" {
        if let real = realpath(existing.path, nil) {
            defer { free(real) }
            var out = URL(fileURLWithPath: String(cString: real))
            for part in rest.reversed() { out.appendPathComponent(part) }
            return out
        }
        rest.append(existing.lastPathComponent)
        existing.deleteLastPathComponent()
    }
    return url
}

/// The path with every symlink resolved, so later checks and opens see real directories.
func resolvedURL(_ url: URL) throws -> URL {
    guard let real = realpath(url.path, nil) else { throw Failure("\(url.path): \(errnoText(errno))") }
    defer { free(real) }
    return URL(fileURLWithPath: String(cString: real), isDirectory: true)
}

func plist(_ data: Data) -> [String: Any]? {
    (try? PropertyListSerialization.propertyList(from: data, options: [], format: nil)) as? [String: Any]
}

var macOSVersion: String {
    let v = ProcessInfo.processInfo.operatingSystemVersion
    return "\(v.majorVersion).\(v.minorVersion)" + (v.patchVersion > 0 ? ".\(v.patchVersion)" : "")
}

// MARK: - Ownership

/// A directory only `euid` can change: a real directory (not a symlink), owned by `euid`,
/// with no group or other write. Pure, so a test can hand it another owner.
func checkPrivateDirectory(_ st: stat, euid: uid_t, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFDIR else {
        throw Failure("\(path): not a directory (a symlink is refused)")
    }
    guard st.st_uid == euid else {
        throw Failure("\(path): owned by uid \(st.st_uid), not by this process's uid \(euid)")
    }
    guard st.st_mode & 0o022 == 0 else {
        throw Failure("\(path): writable by group or others (mode \(String(st.st_mode & 0o7777, radix: 8)))")
    }
}

/// A bundle and the directory holding it must both be ones only this account can change,
/// so nobody else can swap a disk between the wall check and the framework's open.
func requirePrivateDirectory(_ url: URL) throws {
    var st = stat()
    guard lstat(url.path, &st) == 0 else { throw Failure("\(url.path): \(errnoText(errno))") }
    try checkPrivateDirectory(st, euid: geteuid(), path: url.path)
}

/// An ancestor of a bundle or state directory: a real directory owned by `euid` or root, with
/// no group or other write unless the sticky bit is set (as on /tmp, where nobody can rename
/// another account's entry). The rule ADR-0025's setup applies to the binary's directories.
/// Pure, so a test can hand it any owner and mode.
func checkAncestor(_ st: stat, euid: uid_t, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFDIR else { throw Failure("\(path): not a directory") }
    guard st.st_uid == euid || st.st_uid == 0 else {
        throw Failure("\(path): owned by uid \(st.st_uid), neither this process's uid \(euid) nor root")
    }
    guard st.st_mode & 0o022 == 0 || st.st_mode & S_ISVTX != 0 else {
        throw Failure("\(path): writable by group or others without the sticky bit (mode \(String(st.st_mode & 0o7777, radix: 8)))")
    }
}

/// Every directory above `start` (exclusive) up to and including /, held to checkAncestor.
/// `start` must already be resolved, so each component is a real directory, not a symlink.
func requireSafeAncestors(above start: URL) throws {
    var dir = start.path
    while dir != "/" {
        dir = (dir as NSString).deletingLastPathComponent
        if dir.isEmpty { dir = "/" }
        var st = stat()
        guard lstat(dir, &st) == 0 else { throw Failure("\(dir): \(errnoText(errno))") }
        try checkAncestor(st, euid: geteuid(), path: dir)
    }
}

/// The state directory holds the slot locks: owned by `euid` and mode exactly 0700. Pure.
func checkStateDirectory(_ st: stat, euid: uid_t, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFDIR else { throw Failure("--state-dir \(path): not a directory") }
    guard st.st_uid == euid else {
        throw Failure("--state-dir \(path): owned by uid \(st.st_uid), not by this process's uid \(euid)")
    }
    guard st.st_mode & 0o777 == 0o700 else {
        throw Failure("--state-dir \(path): mode \(String(st.st_mode & 0o7777, radix: 8)), expected 700")
    }
}

/// A lock or log file this process may trust: a regular file it owns, with one link, so it
/// is not another file's hard-linked alias. Pure.
func checkOwnedFile(_ st: stat, euid: uid_t, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFREG else { throw Failure("\(path): not a regular file") }
    guard st.st_uid == euid else {
        throw Failure("\(path): owned by uid \(st.st_uid), not by this process's uid \(euid)")
    }
    guard st.st_nlink == 1 else { throw Failure("\(path): has \(st.st_nlink) hard links, expected 1") }
}

// MARK: - Locks

/// An exclusive flock(2) held for the life of the process. The kernel drops it when the
/// process exits, however it exits, so a crashed launcher never leaves a stale lock.
struct HeldLock {
    let fd: Int32
    let path: String

    /// Open (creating if absent) `name` inside the directory `dirFD` refers to, without
    /// following a symlink, and take the lock without waiting. The file must be a regular
    /// file this process owns, with one link.
    static func acquire(in dirFD: Int32, name: String, displayPath: String, busy: String) throws -> HeldLock {
        let fd = openat(dirFD, name, O_CREAT | O_RDWR | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw Failure("\(displayPath): \(errnoText(errno))") }
        var st = stat()
        guard fstat(fd, &st) == 0 else {
            let code = errno
            close(fd)
            throw Failure("\(displayPath): \(errnoText(code))")
        }
        do {
            try checkOwnedFile(st, euid: geteuid(), path: displayPath)
        } catch {
            close(fd)
            throw error
        }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            let code = errno
            close(fd)
            if code == EWOULDBLOCK { throw Failure(busy) }
            throw Failure("\(displayPath): flock: \(errnoText(code))")
        }
        return HeldLock(fd: fd, path: displayPath)
    }
}

/// Open a directory for openat(2), refusing a symlink.
func openDirectory(_ url: URL) throws -> Int32 {
    let fd = open(url.path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { throw Failure("\(url.path): not a directory or not openable: \(errnoText(errno))") }
    return fd
}

/// Open the state directory and hold it to checkStateDirectory, by fstat on the open
/// descriptor, so the directory checked is the one the lock is made in.
///
/// Residual: the caller still chooses the path. Setup's fixed layout (a later slice) pins it;
/// until then a caller that is this account could point two runs at two directories it made.
func openStateDirectory(_ given: URL) throws -> Int32 {
    var lst = stat()
    guard lstat(given.path, &lst) == 0, lst.st_mode & S_IFMT == S_IFDIR else {
        throw Failure("--state-dir \(given.path): not a directory (a symlink is refused)")
    }
    let url = try resolvedURL(given)
    try requireSafeAncestors(above: url)
    let fd = try openDirectory(url)
    var st = stat()
    guard fstat(fd, &st) == 0 else {
        let code = errno
        close(fd)
        throw Failure("--state-dir \(url.path): \(errnoText(code))")
    }
    do {
        try checkStateDirectory(st, euid: geteuid(), path: url.path)
    } catch {
        close(fd)
        throw error
    }
    return fd
}

// MARK: - The tools disk

/// A file the launcher's account can read and never write: a regular file with one link,
/// not owned by `euid` (an owner can always chmod its own file back to writable), with no
/// group or other write, and not writable by this process at all (`writableByMe`, from
/// access(W_OK), which also covers ACLs). Pure.
func checkNotWritable(_ st: stat, euid: uid_t, writableByMe: Bool, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFREG else { throw Failure("--tools-disk \(path): not a regular file") }
    guard st.st_nlink == 1 else { throw Failure("--tools-disk \(path): has \(st.st_nlink) hard links, expected 1") }
    guard st.st_uid != euid else {
        throw Failure("--tools-disk \(path): owned by this process's uid \(euid), which could make it writable; it must be faber's or root's")
    }
    guard st.st_mode & 0o022 == 0 else {
        throw Failure("--tools-disk \(path): writable by group or others (mode \(String(st.st_mode & 0o7777, radix: 8)))")
    }
    guard !writableByMe else { throw Failure("--tools-disk \(path): this process can write it") }
}

/// An ancestor of the tools disk: a real directory this account does not own (so it cannot
/// rename another file into the disk's place), with no group or other write unless sticky.
/// Stricter than a bundle's ancestors, which may be this account's own. Pure.
///
/// `toolsOwner` is the account that builds the tools disks (faber's, the supervisor's peer),
/// passed for the disk's own directory only: that directory may be its instead of root's
/// (ADR-0026, amended 2026-10-23). It is never this account. Residual: owning the directory,
/// it can race the check (and the framework's own open, by path, as this account) with a
/// symlink, and so hand a guest a file only this account can read, such as another VM's
/// clone disk, read-only. It gains no reach by that: it already writes the agent every guest
/// runs (the tools disk itself).
func checkToolsAncestor(_ st: stat, euid: uid_t, path: String, toolsOwner: uid_t? = nil) throws {
    if let toolsOwner, toolsOwner != euid, toolsOwner != 0, st.st_uid == toolsOwner {
        guard st.st_mode & S_IFMT == S_IFDIR else { throw Failure("\(path): not a directory") }
        guard st.st_mode & 0o022 == 0 || st.st_mode & S_ISVTX != 0 else {
            throw Failure("\(path): writable by group or others without the sticky bit (mode \(String(st.st_mode & 0o7777, radix: 8)))")
        }
        return
    }
    try checkAncestor(st, euid: euid, path: path)
    guard st.st_uid != euid || path == "/" else {
        throw Failure("--tools-disk: \(path) is owned by this process's uid \(euid), which could swap the disk")
    }
}

/// The directories above a resolved tools disk, nearest first, each with the tools owner it is
/// checked against: the disk's own directory gets `toolsOwner`, every other one nil. Pure.
func toolsAncestors(of path: String, toolsOwner: uid_t?) -> [(path: String, toolsOwner: uid_t?)] {
    var out: [(path: String, toolsOwner: uid_t?)] = []
    var dir = path
    while dir != "/" {
        dir = (dir as NSString).deletingLastPathComponent
        if dir.isEmpty { dir = "/" }
        out.append((dir, out.isEmpty ? toolsOwner : nil))
    }
    return out
}

/// The tools disk as the wall will check it: the given path must not itself be a symlink; the
/// resolved path is held to checkNotWritable and its every ancestor to checkToolsAncestor,
/// the disk's own directory with `toolsOwner` (the supervisor's `--tools-owner`), the rest without.
func requireToolsDisk(_ given: URL, toolsOwner: uid_t? = nil) throws -> FileRef {
    var lst = stat()
    guard lstat(given.path, &lst) == 0 else { throw Failure("--tools-disk \(given.path): \(errnoText(errno))") }
    guard lst.st_mode & S_IFMT == S_IFREG else {
        throw Failure("--tools-disk \(given.path): not a regular file (a symlink is refused)")
    }
    guard let real = realpath(given.path, nil) else { throw Failure("--tools-disk \(given.path): \(errnoText(errno))") }
    let path = String(cString: real)
    free(real)
    var st = stat()
    guard lstat(path, &st) == 0 else { throw Failure("--tools-disk \(path): \(errnoText(errno))") }
    try checkNotWritable(st, euid: geteuid(), writableByMe: access(path, W_OK) == 0, path: path)
    for (dir, owner) in toolsAncestors(of: path, toolsOwner: toolsOwner) {
        var dst = stat()
        guard lstat(dir, &dst) == 0 else { throw Failure("\(dir): \(errnoText(errno))") }
        try checkToolsAncestor(dst, euid: geteuid(), path: dir, toolsOwner: owner)
    }
    return FileRef(url: URL(fileURLWithPath: path), id: FileID(st))
}
