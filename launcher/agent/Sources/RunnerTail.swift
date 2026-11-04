// The runner's exit, for the operator (docs/vm-agent-protocol.md, *The runner's exit*).
//
// Two things, kept apart on purpose:
//
// - **Whether the runner ever took a job**, a closed word (RunnerJob) that crosses vsock and
//   decides the job's outcome. runner-exec reads it as runner from the runner's own `_diag`:
//   Runner.Listener starts Runner.Worker only once GitHub has assigned it a job, and the
//   worker's first act is to open `_diag/Worker_<utc>.log`. A listener that never got a job has
//   only `Runner_<utc>.log` files there. Only file names are read, never their bytes.
// - **The last lines the runner wrote**, its own stdout and stderr (which runner-exec sends to
//   a file in the runner's directory) and its newest `_diag/Runner_*.log`, which root copies to
//   the guest's console after the runner exits. They go to the console and nowhere else:
//   nothing parses them, on either side of vsock. So they are bounded and sanitized, and every
//   run that could be a credential (a JIT config, a token, an Authorization header) is redacted
//   before a byte is written.
//
// Root reads those files from a directory runner owns, so it opens every component with
// O_NOFOLLOW from runner's home down, and reads a file only if it is a regular file runner owns
// with one link: a symlink or a hard link to a file root may read and runner may not is refused.

import Darwin
import Foundation

enum RunnerTail {
    /// Where runner-exec sends run.sh's stdout and stderr, in the runner's directory. A file the
    /// tarball never holds.
    static let outputName = "_faber-runner-output.log"
    /// The runner's own log directory, under its root.
    static let diagDir = "_diag"
    /// The most lines copied from each of the two files.
    static let lines = 20
    /// The most bytes read from the end of either file: enough for 20 long lines.
    static let readBytes = 64 * 1024
    /// The most bytes every tail line together may take, headers included.
    static let maxBytes = 8192
    /// The most lines a tail writes: two headers and each file's lines.
    static var maxLines: Int { 2 + 2 * lines }
    /// A run of a token's alphabet at least this long is redacted, unless it is a plain
    /// lowercase word or path (HandoffDiagnostics' rule, with base64url's `-` and `_` and a
    /// shorter run: a GitHub token is 40 bytes of it, a JWT segment longer).
    static let redactRun = 20
    /// A line holding one of these words, in any case, is cut after it: what follows may be a
    /// credential in a shape the run rule misses.
    static let secretWords = ["authorization", "bearer", "token", "jitconfig", "password", "secret", "credential"]
}

// MARK: - Did the runner take a job? (pure)

/// Whether `names`, the entries of the runner's `_diag`, show a job was assigned: a
/// `Worker_*.log` is there. Pure.
func runnerJob(diagNames names: [String]) -> RunnerJob {
    names.contains { $0.hasPrefix("Worker_") && $0.hasSuffix(".log") } ? .started : .notStarted
}

/// The runner's newest listener log among `names`: the greatest `Runner_*.log`, whose name is
/// its UTC start time. Pure.
func newestRunnerDiag(_ names: [String]) -> String? {
    names.filter { $0.hasPrefix("Runner_") && $0.hasSuffix(".log") && !$0.contains("/") }.max()
}

/// Whether `name` is in the shape the runner itself names a listener log,
/// `Runner_<yyyyMMdd>-<HHmmss>-utc.log`, and so may go on the console as it is. Any other name
/// in `_diag` is runner's to choose, and is redacted like a line. Pure.
func isRunnerDiagName(_ name: String) -> Bool {
    let b = Array(name.utf8)
    let prefix = Array("Runner_".utf8)
    let suffix = Array("-utc.log".utf8)
    guard b.count == prefix.count + 15 + suffix.count, Array(b.prefix(prefix.count)) == prefix,
          Array(b.suffix(suffix.count)) == suffix else { return false }
    let stamp = b[prefix.count..<(prefix.count + 15)]
    return stamp.enumerated().allSatisfy { i, c in i == 8 ? c == UInt8(ascii: "-") : (c >= 0x30 && c <= 0x39) }
}

/// runner-exec's answer, as runner: the names in `<dir>/_diag`. No `_diag` at all is a runner
/// that never started its listener, so it took no job; a directory that cannot be read is
/// unknown (nil).
func readRunnerJob(dir: String) -> RunnerJob? {
    guard let names = directoryNames(dir + "/" + RunnerTail.diagDir) else {
        return errno == ENOENT ? .notStarted : nil
    }
    return runnerJob(diagNames: names)
}

/// The names in `path`, or nil with errno set. A symlink is not followed.
private func directoryNames(_ path: String) -> [String]? {
    let fd = open(path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return nil }
    return namesIn(fd)
}

/// The names in the directory `fd`, which this takes over and closes.
private func namesIn(_ fd: Int32) -> [String]? {
    guard let d = fdopendir(fd) else {
        let e = errno
        close(fd)
        errno = e
        return nil
    }
    defer { closedir(d) }
    var names: [String] = []
    while let entry = readdir(d) {
        let name = withUnsafeBytes(of: entry.pointee.d_name) { raw in
            String(decoding: raw.prefix(Int(entry.pointee.d_namlen)), as: UTF8.self)
        }
        if name != ".", name != ".." { names.append(name) }
        if names.count > 4096 { break }
    }
    return names
}

// MARK: - The lines (pure)

/// The last `count` lines of `bytes`, without their CR or LF. When the read began part-way
/// into the file (`partial`), its first line is a fragment and is dropped. Pure.
func tailLines(_ bytes: [UInt8], count: Int, partial: Bool) -> [String] {
    var lines = bytes.split(separator: 0x0A, omittingEmptySubsequences: false).map { Array($0) }
    if partial, !lines.isEmpty { lines.removeFirst() }
    if lines.last?.isEmpty == true { lines.removeLast() }
    return lines.suffix(count).map { line in
        String(decoding: line.last == 0x0D ? Array(line.dropLast()) : line, as: UTF8.self)
    }
}

/// `s` with anything that could be a credential replaced by `<redacted>`: everything after a
/// word of RunnerTail.secretWords, and every run of RunnerTail.redactRun or more bytes of
/// `[A-Za-z0-9+/=_-]` that holds anything but lowercase letters, `/`, `_` and `-`. Pure.
func redactTail(_ s: String) -> String {
    var bytes = Array(s.utf8)
    // ASCII-only lowercasing keeps byte offsets aligned with `bytes` whatever non-ASCII
    // characters precede a secret word (a full Unicode lowercase can change the length).
    let lower = bytes.map { $0 >= 0x41 && $0 <= 0x5A ? $0 + 0x20 : $0 }
    var cut: Int?
    for word in RunnerTail.secretWords {
        let w = Array(word.utf8)
        guard w.count <= lower.count else { continue }
        for i in 0...(lower.count - w.count) where Array(lower[i..<(i + w.count)]) == w {
            cut = min(cut ?? Int.max, i + w.count)
            break
        }
    }
    if let cut, cut < bytes.count { bytes = Array(bytes[..<cut]) + Array(" \(HandoffDiagnostics.redacted)".utf8) }
    func isToken(_ c: UInt8) -> Bool {
        isAlnum(c) || c == UInt8(ascii: "+") || c == UInt8(ascii: "/") || c == UInt8(ascii: "=")
            || c == UInt8(ascii: "_") || c == UInt8(ascii: "-")
    }
    func isPlain(_ c: UInt8) -> Bool {
        (c >= 0x61 && c <= 0x7A) || c == UInt8(ascii: "/") || c == UInt8(ascii: "_") || c == UInt8(ascii: "-")
    }
    var out: [UInt8] = []
    var run: [UInt8] = []
    func flush() {
        let secretLike = run.count >= RunnerTail.redactRun && !run.allSatisfy(isPlain)
        out += secretLike ? Array(HandoffDiagnostics.redacted.utf8) : run
        run.removeAll()
    }
    for c in bytes {
        if isToken(c) { run.append(c) } else {
            flush()
            out.append(c)
        }
    }
    flush()
    return String(decoding: out, as: UTF8.self)
}

/// One tail line: `FABER-AGENT runner <source>| <text>`, redacted, sanitized and bounded. Pure.
func runnerTailLine(_ source: String, _ text: String) -> String {
    diagnosticLine("runner \(source)| \(redactTail(text))", tag: UpdatePolicy.tag)
}

/// What root found of the runner's two files: each one's lines, or why there are none.
struct RunnerTailFacts {
    var output: Result<[String], Failure>
    /// The newest `Runner_*.log`'s name and its lines.
    var diag: Result<(name: String, lines: [String]), Failure>
    /// Root's own reading of `_diag`'s names, for when runner-exec's report says nothing
    /// (it was killed, or could not read the directory). nil when root could not tell either.
    var job: RunnerJob? = nil
}

/// The console lines for `facts`: a header per file, then its lines, at most
/// RunnerTail.maxBytes in all; a line that would pass the cap ends the tail. Pure.
func runnerTailBlock(_ facts: RunnerTailFacts) -> [String] {
    var out: [String] = []
    switch facts.output {
    case .success(let lines):
        out.append(diagnosticLine("runner output: the last \(lines.count) lines run.sh wrote to stdout and stderr",
                                  tag: UpdatePolicy.tag))
        out += lines.suffix(RunnerTail.lines).map { runnerTailLine("out", $0) }
    case .failure(let why):
        out.append(diagnosticLine("runner output: none read: \(redactTail(why.description))", tag: UpdatePolicy.tag))
    }
    switch facts.diag {
    case .success(let (name, lines)):
        let shown = isRunnerDiagName(name) ? name : redactTail(name)
        out.append(diagnosticLine("runner diag: the last \(lines.count) lines of \(RunnerTail.diagDir)/\(shown)",
                                  tag: UpdatePolicy.tag))
        out += lines.suffix(RunnerTail.lines).map { runnerTailLine("diag", $0) }
    case .failure(let why):
        out.append(diagnosticLine("runner diag: none read: \(redactTail(why.description))", tag: UpdatePolicy.tag))
    }
    var total = 0
    var kept: [String] = []
    for line in out {
        total += line.utf8.count + 1
        guard total <= RunnerTail.maxBytes else { break }
        kept.append(line)
    }
    return kept
}

// MARK: - Reading them, as root

/// Read the tails of the runner's two files under `home`/`dir`, as root, through no symlink and
/// only from files `uid` owns.
func readRunnerTails(home: String, dirName: String = Guest.runnerDirName, uid: uid_t) -> RunnerTailFacts {
    guard let homeFD = openOwnedDirectory(AT_FDCWD, home, uid: uid, ownerCheck: false) else {
        let f = Failure("\(home): \(errnoText(errno))")
        return RunnerTailFacts(output: .failure(f), diag: .failure(f))
    }
    defer { close(homeFD) }
    guard let dirFD = openOwnedDirectory(homeFD, dirName, uid: uid) else {
        let f = Failure("\(home)/\(dirName): \(errnoText(errno))")
        return RunnerTailFacts(output: .failure(f), diag: .failure(f))
    }
    defer { close(dirFD) }
    let output = readOwnedTail(dirFD, RunnerTail.outputName, uid: uid)
    let diag: Result<(name: String, lines: [String]), Failure>
    var job: RunnerJob?
    if let diagFD = openOwnedDirectory(dirFD, RunnerTail.diagDir, uid: uid) {
        // namesIn closes the descriptor it is given: hand it a duplicate.
        let names = namesIn(dup(diagFD))
        job = names.map { runnerJob(diagNames: $0) }
        if let newest = newestRunnerDiag(names ?? []) {
            diag = readOwnedTail(diagFD, newest, uid: uid).map { (name: newest, lines: $0) }
        } else {
            diag = .failure(Failure("\(RunnerTail.diagDir) holds no Runner_*.log"))
        }
        close(diagFD)
    } else {
        let e = errno
        job = e == ENOENT ? .notStarted : nil
        diag = .failure(Failure("\(RunnerTail.diagDir): \(errnoText(e))"))
    }
    return RunnerTailFacts(output: output, diag: diag, job: job)
}

/// `name` in `parent` as a directory, never through a symlink, owned by `uid` unless
/// `ownerCheck` is false (runner's home's own parent chain is root's). nil with errno set.
private func openOwnedDirectory(_ parent: Int32, _ name: String, uid: uid_t, ownerCheck: Bool = true) -> Int32? {
    let fd = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return nil }
    var st = stat()
    guard fstat(fd, &st) == 0, !ownerCheck || st.st_uid == uid else {
        close(fd)
        errno = EPERM
        return nil
    }
    return fd
}

/// The last RunnerTail.lines lines of `name` in `dir`: a regular file `uid` owns, with one
/// link, opened through no symlink and without blocking, read at most RunnerTail.readBytes
/// from its end.
private func readOwnedTail(_ dir: Int32, _ name: String, uid: uid_t) -> Result<[String], Failure> {
    let fd = openat(dir, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    guard fd >= 0 else { return .failure(Failure("\(name): \(errnoText(errno))")) }
    defer { close(fd) }
    var st = stat()
    guard fstat(fd, &st) == 0 else { return .failure(Failure("\(name): \(errnoText(errno))")) }
    guard st.st_mode & S_IFMT == S_IFREG, st.st_uid == uid, st.st_nlink == 1 else {
        return .failure(Failure("\(name): not a regular file of runner's with one link"))
    }
    let size = Int(max(0, st.st_size))
    let start = max(0, size - RunnerTail.readBytes)
    var buffer = [UInt8](repeating: 0, count: size - start)
    var got = 0
    while got < buffer.count {
        let n = buffer[got...].withUnsafeMutableBytes { pread(fd, $0.baseAddress, $0.count, off_t(start + got)) }
        if n < 0, errno == EINTR { continue }
        if n <= 0 { break }
        got += n
    }
    return .success(tailLines(Array(buffer[0..<got]), count: RunnerTail.lines, partial: start > 0))
}
