// The hand-off's console lines (docs/vm-agent-protocol.md, *The hand-off's console lines*): one
// line per step of starting the runner, so a job VM that stops with `start_failed` says which
// step failed and the one fact that decided it. The same channel and shape as the
// verification's lines (Checks.swift): `FABER-AGENT handoff step=<name> ok=<bool> why=<fact>`,
// printable ASCII, at most Diagnostics.maxLine bytes, at most Diagnostics.maxLines lines.
//
// Root writes every line. `runner-exec` runs as `runner` and cannot open the root-only console,
// so it reports its steps to root on the hand-off connection it already holds, as a closed
// vocabulary (HandoffStep and one integer); root formats the line from those words and from
// paths it knows itself, never from a byte `runner-exec` chose.
//
// No line carries the JIT config: it is on no argv, in no file and in no command's output this
// file reads. As a second guard, every line passes through redactSecrets, which removes any
// long run of the config's alphabet that is not a plain lowercase path.

import Darwin
import Foundation
import SystemConfiguration

enum HandoffDiagnostics {
    /// A run of the JIT config's alphabet ([A-Za-z0-9+/=]) at least this long is redacted when
    /// it holds anything but lowercase letters and `/`. The config is base64 of 1 to 8 192
    /// bytes, so any 24 bytes of it hold a digit, a capital, `+` or `=` all but surely (the
    /// chance they do not is (27/64)^24, under one in 10^9); a path such as
    /// /private/var/faber/tools/actions never does, and is kept.
    static let redactRun = 24
    static let redacted = "<redacted>"
}

/// `s` with every run that could be part of a JIT config replaced (HandoffDiagnostics). Pure.
func redactSecrets(_ s: String) -> String {
    func isB64(_ c: UInt8) -> Bool { isAlnum(c) || c == UInt8(ascii: "+") || c == UInt8(ascii: "/") || c == UInt8(ascii: "=") }
    func isPathLike(_ c: UInt8) -> Bool { (c >= 0x61 && c <= 0x7A) || c == UInt8(ascii: "/") }
    var out: [UInt8] = []
    var run: [UInt8] = []
    func flush() {
        let secretLike = run.count >= HandoffDiagnostics.redactRun && !run.allSatisfy(isPathLike)
        out += secretLike ? Array(HandoffDiagnostics.redacted.utf8) : run
        run.removeAll()
    }
    for c in s.utf8 {
        if isB64(c) { run.append(c) } else {
            flush()
            out.append(c)
        }
    }
    flush()
    return String(decoding: out, as: UTF8.self)
}

/// `FABER-AGENT handoff step=<step> ok=<bool> why=<why>`, redacted, sanitized and bounded. Pure.
func handoffLine(_ step: String, ok: Bool, _ why: String) -> String {
    diagnosticLine("handoff step=\(step) ok=\(ok) why=\(redactSecrets(why))", tag: UpdatePolicy.tag)
}

/// How a command ended, for a line: `exit N`, or `did not run or timed out`. Pure.
func commandEnd(_ result: (Int32, Data)?) -> String {
    result.map { "exit \($0.0)" } ?? "did not run or timed out"
}

/// An errno as errnoText says it: `Permission denied (errno 13)`. Pure.
func errnoFact(_ e: Int) -> String {
    errnoText(Int32(e))
}

/// Root's words for one of runner-exec's steps: the step's object, from paths root knows, and
/// the integer runner-exec reported. Pure.
func runnerStepWhy(_ r: HandoffStepResult, tarball: String, dir: String) -> String {
    let fact: String? = r.exitCode.map { "exit \($0)" } ?? r.signal.map { "signal \($0)" } ?? r.errno.map(errnoFact)
    let object: String
    switch r.step {
    case .configRead: object = "runner-exec reading the config from root"
    case .manifest: object = Guest.manifestPath
    case .mkdir: object = dir
    case .extract: object = "tar -xzf \(tarball) -C \(dir)"
    case .spawn: object = dir + "/run.sh"
    }
    return fact.map { "\(object): \($0)" } ?? object
}

// MARK: - Can runner reach the tools disk? (pure)

/// One path's owner and mode, as lstat gave them.
struct PathFacts: Equatable {
    let path: String
    let uid: uid_t
    let gid: gid_t
    let mode: mode_t
    let isDirectory: Bool
}

/// What runner needs of the last path in a chain.
enum PathNeed: String {
    case execute
    case read
}

/// Whether an account (`uid`, in `groups`) may do `bit` (4 read, 2 write, 1 execute or search)
/// on a path, by its mode bits alone: the owner's when it owns it, else the group's when it is
/// in it, else other's, as the kernel checks them. Root is never asked. Pure.
func modeAllows(_ f: PathFacts, uid: uid_t, groups: Set<gid_t>, bit: mode_t) -> Bool {
    let shift: mode_t = f.uid == uid ? 6 : groups.contains(f.gid) ? 3 : 0
    return (f.mode >> shift) & bit != 0
}

/// The first path of `chain` (from `/` down) runner may not pass, and why; nil when it may
/// search every directory and do `need` to the last. Pure.
func runnerDenied(_ chain: [PathFacts], need: PathNeed, uid: uid_t, groups: Set<gid_t>) -> String? {
    for (i, f) in chain.enumerated() {
        let last = i == chain.count - 1
        let (bit, verb): (mode_t, String) = !last || f.isDirectory ? (1, "search") : need == .read ? (4, "read") : (1, "execute")
        if !modeAllows(f, uid: uid, groups: groups, bit: bit) {
            return "\(f.path) mode \(String(f.mode & 0o7777, radix: 8)) uid \(f.uid) gid \(f.gid): runner cannot \(verb) it"
        }
    }
    return nil
}

/// Every prefix of an absolute `path`, `/` first. Pure.
func pathPrefixes(_ path: String) -> [String] {
    var out = ["/"]
    var at = ""
    for part in path.split(separator: "/") {
        at += "/" + part
        out.append(at)
    }
    return out
}

/// From `launchctl print gui/<uid>/<label>`, the few facts that say why a service did not run:
/// its state, its runs, its last exit code and signal. Pure.
func launchdServiceFacts(_ output: String) -> String {
    let wanted = ["state", "runs", "last exit code", "last terminating signal"]
    var facts: [String] = []
    for line in output.split(whereSeparator: \.isNewline) {
        let parts = line.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
        guard parts.count == 2, wanted.contains(parts[0]), !facts.contains(where: { $0.hasPrefix(parts[0] + "=") }) else { continue }
        facts.append("\(parts[0])=\(parts[1].prefix(40))")
    }
    return facts.isEmpty ? "no service facts" : facts.joined(separator: ", ")
}

// MARK: - The guest's facts

/// The console user and its uid, if any.
func consoleUser() -> (name: String, uid: uid_t)? {
    var uid: uid_t = 0
    var gid: gid_t = 0
    guard let name = SCDynamicStoreCopyConsoleUser(nil, &uid, &gid) as String? else { return nil }
    return (name, uid)
}

/// lstat of every prefix of `path`, or the first that could not be read and why.
func pathChain(_ path: String) -> Result<[PathFacts], Failure> {
    var chain: [PathFacts] = []
    for p in pathPrefixes(path) {
        var st = stat()
        guard lstat(p, &st) == 0 else { return .failure(Failure("\(p): \(errnoText(errno))")) }
        chain.append(PathFacts(path: p, uid: st.st_uid, gid: st.st_gid, mode: st.st_mode,
                               isDirectory: st.st_mode & S_IFMT == S_IFDIR))
    }
    return .success(chain)
}

/// The groups `name` is in, its primary `gid` included.
func accountGroups(_ name: String, gid: gid_t) -> Set<gid_t> {
    var count: Int32 = 64
    var groups = [Int32](repeating: 0, count: Int(count))
    if getgrouplist(name, Int32(bitPattern: gid), &groups, &count) != 0 { return [gid] }
    return Set(groups.prefix(Int(count)).map { gid_t(bitPattern: $0) }).union([gid])
}

/// Whether `uid` can run the agent from the tools disk and read what runner-exec reads there:
/// nil when it can, else the first path that stops it.
func toolsDiskDenied(uid: uid_t, groups: Set<gid_t>, tarball: String?) -> String? {
    var needs: [(String, PathNeed)] = [(Guest.agentBinary, .execute), (Guest.manifestPath, .read)]
    if let tarball { needs.append((tarball, .read)) }
    for (path, need) in needs {
        switch pathChain(path) {
        case .failure(let f): return f.description
        case .success(let chain):
            if let denied = runnerDenied(chain, need: need, uid: uid, groups: groups) { return denied }
        }
    }
    return nil
}
