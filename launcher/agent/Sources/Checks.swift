// The golden image's checks (ADR-0049 §7), run by `faber-agent serve` as root on a fresh clone
// when the launcher sends `verify`. Each comes back to the launcher as a fixed code, pass or
// fail; nothing the commands print ever crosses vsock. The pure judges are at the top so the
// tests (launcher/Tests/VerifyTests.swift) can hold them to what each check means; the facts
// they judge are gathered below.
//
// Each check also writes one diagnostic line to the guest's console, `/dev/cu.virtio`: which
// check, pass or fail, and the one fact that decided it (an account, a key and its value, an
// exit status). That is the operator's channel, the same one the golden build reports on
// (launcher/golden/provision.sh): the session supervisor keeps the console in its private
// state directory, readable by root and its own account only, and never sends it to `_faber`.
// The lines are printable ASCII, at most Diagnostics.maxLine bytes each and
// Diagnostics.maxLines per verification, and they never carry a secret: no hash, no
// password, no command's raw output beyond a bounded first line of a tool that prints none.

import Darwin
import Foundation
import SystemConfiguration

enum Checks {
    /// The one login shell that cannot log in: the build sets the Setup Assistant admin's to it
    /// (launcher/golden/provision.sh). `/sbin/nologin` is not one: ADR-0051's spikes found that
    /// pam_opendirectory refuses only `/usr/bin/false`, and a GUI login passes with nologin.
    static let lockedShell = "/usr/bin/false"
    /// `launchctl print`'s exit status for a service that is not loaded.
    static let launchctlNotFound: Int32 = 113
    /// The Software Update settings the build turns off, each of which must be present and false.
    static let updateKeys = ["AutomaticDownload", "CriticalUpdateInstall", "ConfigDataInstall", "AutomaticCheckEnabled"]
    /// The one that may also be absent, since macOS leaves it unset until asked.
    static let updateOptionalKeys = ["AutomaticallyInstallMacOSUpdates"]
    static let softwareUpdatePlist = "/Library/Preferences/com.apple.SoftwareUpdate.plist"
    static let homebrewPrefix = "/opt/homebrew"
    /// The admin group's name and gid on macOS.
    static let adminGroup = "admin"
    static let adminGID: gid_t = 80
    /// The longest a check command may run, and the most of its output read.
    static let commandTimeout: TimeInterval = 150
    static let maxOutput = 1 << 20
    /// How long a command's output reader may run on after the command exits: part of
    /// every command's true bound, which deadlines must count.
    static let readerGrace: TimeInterval = 5
    /// How long a timed-out command's process group has, after SIGTERM, to exit before SIGKILL:
    /// part of a timed-out command's true bound.
    static let killGrace: TimeInterval = 5
}

/// The diagnostic lines on the guest's console.
enum Diagnostics {
    /// Every line starts with it, as the golden build's start with FABER-GOLDEN-BUILD.
    static let tag = "FABER-GOLDEN-VERIFY"
    /// The most bytes of one line, its newline not counted (provision.sh's own bound).
    static let maxLine = 199
    /// The most lines one verification writes: a start, a line per check, an end, and room.
    static let maxLines = 32
    /// The console the launcher's serial port reads.
    static let console = "/dev/cu.virtio"
}

// MARK: - The diagnostic lines (pure)

/// One check's answer and the fact that decided it. `why` goes to the console only.
struct CheckVerdict: Equatable {
    let ok: Bool
    let why: String
    static func pass(_ why: String) -> CheckVerdict { CheckVerdict(ok: true, why: why) }
    static func fail(_ why: String) -> CheckVerdict { CheckVerdict(ok: false, why: why) }
}

/// `s` as printable ASCII: every byte outside 0x20...0x7e, a newline or a multi-byte
/// character's byte included, becomes `?`, so a line can neither break nor carry an escape.
func diagnosticText(_ s: String) -> String {
    String(decoding: s.utf8.map { (0x20...0x7e).contains($0) ? $0 : UInt8(ascii: "?") }, as: UTF8.self)
}

/// A diagnostic line: the tag, then `body` sanitized, cut to Diagnostics.maxLine bytes.
func diagnosticLine(_ body: String, tag: String = Diagnostics.tag) -> String {
    let line = tag + " " + diagnosticText(body)
    return String(decoding: line.utf8.prefix(Diagnostics.maxLine), as: UTF8.self)
}

/// `FABER-GOLDEN-VERIFY check=<code> ok=<true|false> why=<fact>`.
func checkLine(_ code: CheckCode, _ verdict: CheckVerdict) -> String {
    diagnosticLine("check=\(code.rawValue) ok=\(verdict.ok) why=\(verdict.why)")
}

/// The last line: the failed codes, or `none`.
func doneLine(_ failed: [CheckCode]) -> String {
    diagnosticLine("done failed=\(failed.isEmpty ? "none" : failed.map(\.rawValue).joined(separator: ","))")
}

/// At most Diagnostics.maxLines lines to `write`; the rest are dropped. A line is written as
/// given: build it with diagnosticLine, which bounds and sanitizes it.
final class DiagnosticSink: @unchecked Sendable {
    private let lock = NSLock()
    private var written = 0
    private let limit: Int
    private let write: (String) -> Void

    init(write: @escaping (String) -> Void, limit: Int = Diagnostics.maxLines) {
        self.write = write
        self.limit = limit
    }

    func emit(_ line: String) {
        lock.lock()
        defer { lock.unlock() }
        guard written < limit else { return }
        written += 1
        write(line)
    }

    /// A sink to the same console with a budget of its own: the runner's tail
    /// (RunnerTail.swift) is not counted against the hand-off's lines.
    func sibling(limit: Int) -> DiagnosticSink {
        DiagnosticSink(write: write, limit: limit)
    }
}

/// A short first line of a tool's output, for a tool whose output holds nothing secret.
func firstLine(_ output: String, max: Int = 60) -> String {
    let line = output.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
    return String(line.prefix(max))
}

// MARK: - The judges (pure)

/// `xcodebuild -version` printed `expect`'s version and build: its first line is
/// `Xcode <version>`, a later one `Build version <build>`. With nothing expected, only that it
/// ran (the caller checks the exit status).
func xcodeMatches(_ output: String, _ expect: XcodeExpect?) -> Bool {
    guard let expect else { return true }
    let lines = output.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
    return lines.first == "Xcode \(expect.version)" && lines.contains("Build version \(expect.build)")
}

/// xcodeMatches, with what it saw.
func xcodeVerdict(_ output: String, _ expect: XcodeExpect?) -> CheckVerdict {
    let lines = output.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
    let got = "got '\(lines.first.map { String($0.prefix(48)) } ?? "")' '\(lines.first(where: { $0.hasPrefix("Build version ") }).map { String($0.prefix(40)) } ?? "")'"
    guard let expect else { return .pass("\(got), nothing expected") }
    return xcodeMatches(output, expect)
        ? .pass(got)
        : .fail("\(got) want 'Xcode \(expect.version)' 'Build version \(expect.build)'")
}

/// The developer tools as `runner` sees them (ADR-0049 §7, `xcode`). Root bypasses every
/// permission bit, so an Xcode root can run is no proof that a job can: an image can pass
/// `xcodebuild -version` as root while every job's `/usr/bin/git` and `cc` say "No developer
/// tools were found". So the check runs the shims a job runs, as `runner`, and links a program.
enum Toolchain {
    static let xcodeSelect = "/usr/bin/xcode-select"
    static let cc = "/usr/bin/cc"
    static let swiftc = "/usr/bin/swiftc"
    /// How long the whole script may run: a cold swiftc on a fresh clone takes seconds.
    static let timeout: TimeInterval = 120

    /// Run as `runner` with `sh -c script sh <xcode-select> <cc> <swiftc>`. It prints the
    /// developer directory, then compiles, links and runs a C and a Swift program in a fresh
    /// directory of its own, which it removes. Each way to fail has its own exit status
    /// (toolchainVerdict). The tools come in as arguments so a test can hand it fakes.
    static let script = """
        set -u
        PATH=/usr/bin:/bin:/usr/sbin:/sbin
        export PATH
        dev=$("$1" -p 2>/dev/null) || exit 20
        printf '%s\\n' "$dev"
        [ -d "$dev" ] && [ -x "$dev" ] && [ -r "$dev" ] || exit 21
        dir=$(mktemp -d /private/tmp/faber-verify-toolchain.XXXXXX) || exit 22
        trap 'rm -rf "$dir"' EXIT
        trap 'exit 27' HUP INT TERM
        printf 'int main(void) { return 0; }\\n' > "$dir/t.c" || exit 22
        printf 'print("faber")\\n' > "$dir/t.swift" || exit 22
        "$2" -o "$dir/c" "$dir/t.c" > /dev/null 2>&1 || exit 23
        "$dir/c" || exit 24
        "$3" -o "$dir/s" "$dir/t.swift" > /dev/null 2>&1 || exit 25
        [ "$("$dir/s")" = faber ] || exit 26
        """

    /// What each of the script's exit statuses means, for the console line.
    static let failures: [Int32: String] = [
        20: "xcode-select -p does not resolve",
        21: "the developer directory is not one runner can enter and read",
        22: "no scratch directory in /private/tmp",
        23: "cc cannot compile and link a C program",
        24: "the C program cc linked does not run",
        25: "swiftc cannot compile and link a Swift program",
        26: "the Swift program swiftc linked does not run",
        27: "the toolchain script was stopped",
    ]
}

/// The toolchain script's verdict from its exit status (nil: it did not run or timed out) and
/// its output, whose first line is the developer directory.
func toolchainVerdict(_ result: (Int32, Data)?) -> CheckVerdict {
    guard let (code, out) = result else { return .fail("as runner: the toolchain script did not run or timed out") }
    let dev = firstLine(String(decoding: out, as: UTF8.self), max: 80)
    guard code == 0 else {
        let what = Toolchain.failures[code] ?? "the toolchain script exit \(code)"
        return .fail(dev.isEmpty ? "as runner: \(what)" : "as runner: \(what) (\(dev))")
    }
    // A path xcode-select resolved and the script entered; anything else is not an answer.
    guard dev.hasPrefix("/") else { return .fail("as runner: xcode-select -p printed no path") }
    return .pass("as runner: cc and swiftc link in \(dev)")
}

/// The `xcode` check: `xcodebuild -version` as runner matches (xcodeVerdict), and runner's
/// toolchain links (toolchainVerdict). The first failure decides the line.
func xcodeCheckVerdict(version: CheckVerdict, toolchain: CheckVerdict) -> CheckVerdict {
    guard version.ok else { return version }
    guard toolchain.ok else { return toolchain }
    return .pass("\(version.why); \(toolchain.why)")
}

/// Every expected runtime is in `simctl list runtimes -j`'s output, with its platform, version
/// and build, and available.
func runtimesReady(_ json: Data, _ expect: [RuntimeExpect]) -> Bool {
    runtimesVerdict(json, expect).ok
}

/// runtimesReady, naming the first runtime that is missing or not available.
func runtimesVerdict(_ json: Data, _ expect: [RuntimeExpect]) -> CheckVerdict {
    guard !expect.isEmpty else { return .pass("no runtimes expected") }
    guard let top = try? JSONSerialization.jsonObject(with: json) as? [String: Any],
          let list = top["runtimes"] as? [[String: Any]] else { return .fail("simctl output is not a runtimes list") }
    for want in expect {
        let name = "\(want.platform) \(want.version) (\(want.build))"
        let found = list.first { r in
            (r["platform"] as? String) == want.platform && (r["version"] as? String) == want.version
                && (r["buildversion"] as? String) == want.build
        }
        guard let found else { return .fail("runtime \(name) missing") }
        guard (found["isAvailable"] as? Bool) == true else { return .fail("runtime \(name) not available") }
    }
    return .pass("\(expect.count) runtime(s) ready")
}

/// Every automatic Software Update setting is off in `prefs`, the system's SoftwareUpdate
/// preferences: the build's four keys present and false, and automatic macOS installs not on.
func softwareUpdateOff(_ prefs: [String: Any]?) -> Bool {
    softwareUpdateVerdict(prefs).ok
}

/// softwareUpdateOff, naming the first key that decided a failure, and its value. `nil` is a
/// plist the caller could not read; it says why itself.
func softwareUpdateVerdict(_ prefs: [String: Any]?) -> CheckVerdict {
    guard let prefs else { return .fail("plist unreadable") }
    /// A value as the line shows it: a number as itself (a bool is 0 or 1), anything else by type.
    func shown(_ v: Any) -> String {
        if let n = v as? NSNumber { return n.stringValue }
        return "a \(type(of: v))"
    }
    for k in Checks.updateKeys {
        guard let v = prefs[k] else { return .fail("\(k) absent (it defaults to on)") }
        guard let n = v as? NSNumber, !n.boolValue else { return .fail("\(k)=\(shown(v))") }
    }
    for k in Checks.updateOptionalKeys {
        if let v = prefs[k], (v as? NSNumber)?.boolValue != false { return .fail("\(k)=\(shown(v))") }
    }
    let optional = Checks.updateOptionalKeys.map { k in prefs[k] == nil ? "\(k) absent" : "\(k)=0" }
    return .pass((Checks.updateKeys.map { "\($0)=0" } + optional).joined(separator: " "))
}

/// A user's MobileMeAccounts preferences name a signed-in Apple ID.
func appleIDSignedIn(_ prefs: [String: Any]?) -> Bool {
    guard let accounts = prefs?["Accounts"] as? [Any] else { return false }
    return !accounts.isEmpty
}

/// `fdesetup status` says FileVault is off.
func fileVaultOff(_ output: String) -> Bool {
    output.contains("FileVault is Off")
}

/// An admin account cannot log in: its shell is `/usr/bin/false`. Root is judged apart.
func adminLocked(shell: String) -> Bool { shell == Checks.lockedShell }

/// Root's directory record (`dscl . -read /Users/root`) cannot log in: it has no
/// authentication authority (dsenableroot never ran), or one that holds `;DisabledUser;`,
/// which disables the account whatever else it lists. That is the build's own rule
/// (`provision.sh` sets `;DisabledUser;`). An empty record is no answer.
func rootLocked(_ record: String) -> Bool {
    let lines = record.split(whereSeparator: \.isNewline)
    guard lines.contains(where: { $0.hasPrefix("RecordName:") }) else { return false }
    guard let start = lines.firstIndex(where: { $0.hasPrefix("AuthenticationAuthority:") }) else { return true }
    // The attribute's values: the rest of its line, then any continuation lines (" value").
    var values = String(lines[start].dropFirst("AuthenticationAuthority:".count))
    for line in lines[(start + 1)...] {
        guard line.first == " " else { break }
        values += String(line)
    }
    return values.contains(";DisabledUser;")
}

/// One group's directory record, the attributes the admins check reads.
struct GroupRecord: Equatable {
    var names: [String]
    var guid: String?
    /// GroupMembership: short names.
    var members: [String]
    /// NestedGroups: the GeneratedUIDs of groups whose members are members too.
    var nested: [String]
    /// GroupMembers: the users' GeneratedUIDs. opendirectoryd keeps membership here as well as
    /// in GroupMembership, and a user listed only here is a member all the same.
    var memberGUIDs: [String] = []
}

/// A `dscl -plist . -readall` listing: an array of records, each attribute an array of strings
/// under `dsAttrTypeStandard:<name>`, an absent attribute empty. nil when it is not that shape.
private func directoryRecords(_ plist: Data) -> [(String) -> [String]?]? {
    guard let list = (try? PropertyListSerialization.propertyList(from: plist, format: nil)) as? [Any] else { return nil }
    var out: [(String) -> [String]?] = []
    for item in list {
        guard let dict = item as? [String: Any] else { return nil }
        out.append { name in
            guard let v = dict["dsAttrTypeStandard:" + name] ?? dict[name] else { return [] }
            return v as? [String]
        }
    }
    return out
}

/// `dscl -plist . -readall /Groups RecordName GeneratedUID GroupMembership GroupMembers
/// NestedGroups`. nil when it is not that shape: the admins check then cannot judge, and fails.
func parseGroupRecords(_ plist: Data) -> [GroupRecord]? {
    guard let records = directoryRecords(plist) else { return nil }
    var out: [GroupRecord] = []
    for values in records {
        guard let names = values("RecordName"), let guid = values("GeneratedUID"),
              let members = values("GroupMembership"), let nested = values("NestedGroups"),
              let memberGUIDs = values("GroupMembers") else { return nil }
        out.append(GroupRecord(names: names, guid: guid.first, members: members, nested: nested, memberGUIDs: memberGUIDs))
    }
    return out
}

/// `dscl -plist . -readall /Users RecordName GeneratedUID`: each user's GeneratedUID to its
/// name (uppercased, as dscl writes them). nil when it is not that shape.
func parseUserGUIDs(_ plist: Data) -> [String: String]? {
    guard let records = directoryRecords(plist) else { return nil }
    var out: [String: String] = [:]
    for values in records {
        guard let names = values("RecordName"), let guids = values("GeneratedUID") else { return nil }
        if let name = names.first, let guid = guids.first { out[guid.uppercased()] = name }
    }
    return out
}

/// The admin group's members from the directory: its GroupMembership, its GroupMembers resolved
/// through `users` (GeneratedUID to name), and, transitively, those of every group nested in it
/// by GeneratedUID. `unresolved` names a nested group's GUID no group carries; `unknownMembers`
/// a member's GUID no user carries. nil when there is no admin group.
func adminMembers(_ groups: [GroupRecord], users: [String: String] = [:])
    -> (members: [String], unresolved: [String], unknownMembers: [String])? {
    guard let admin = groups.first(where: { $0.names.contains(Checks.adminGroup) }) else { return nil }
    var members: [String] = []
    var unresolved: [String] = []
    var unknownMembers: [String] = []
    var seen: Set<String> = admin.guid.map { [$0] } ?? []
    var queue = [admin]
    while !queue.isEmpty {
        let g = queue.removeFirst()
        for m in g.members where !members.contains(m) { members.append(m) }
        for guid in g.memberGUIDs {
            if let m = users[guid.uppercased()] {
                if !members.contains(m) { members.append(m) }
            } else if !unknownMembers.contains(guid) {
                unknownMembers.append(guid)
            }
        }
        for guid in g.nested where !seen.contains(guid) {
            seen.insert(guid)
            if let inner = groups.first(where: { $0.guid == guid }) { queue.append(inner) } else { unresolved.append(guid) }
        }
    }
    return (members, unresolved, unknownMembers)
}

/// Every admin is locked: root's record has no usable authentication authority, and every
/// other admin, a member of the admin group (direct or nested) or an account whose primary
/// group is admin's, has the shell `/usr/bin/false`. An admin it cannot judge fails: a group
/// listing it could not read, a nested group it cannot find, a member with no account.
///
/// - groups: the parsed group records, nil when dscl failed (`groupsFailure` says how);
/// - users: every user's GeneratedUID to its name, for GroupMembers; nil when dscl failed
///   (`usersFailure` says how), which fails the check;
/// - primaryAdmins: accounts whose primary gid is admin's;
/// - shells: every account's login shell, by name;
/// - rootRecord: `dscl . -read /Users/root`, nil when it failed (`rootFailure` says how).
///
/// It never shows root's record: as root, dscl prints its ShadowHashData.
func adminsVerdict(groups: [GroupRecord]?, groupsFailure: String = "dscl readall /Groups failed",
                   users: [String: String]? = [:], usersFailure: String = "dscl readall /Users failed",
                   primaryAdmins: [String], shells: [String: String],
                   rootRecord: String?, rootFailure: String = "dscl read /Users/root failed") -> CheckVerdict {
    guard let rootRecord else { return .fail(rootFailure) }
    guard rootLocked(rootRecord) else {
        return .fail(rootRecord.isEmpty || !rootRecord.contains("RecordName:")
            ? "root record empty" : "root has an AuthenticationAuthority without ;DisabledUser;")
    }
    guard let groups else { return .fail(groupsFailure) }
    guard let users else { return .fail(usersFailure) }
    guard let (members, unresolved, unknownMembers) = adminMembers(groups, users: users) else {
        return .fail("no admin group in the directory")
    }
    if let guid = unresolved.first { return .fail("admin nests group \(guid), which is not in the directory") }
    if let guid = unknownMembers.first { return .fail("an admin group lists member \(guid), which is no user in the directory") }
    var admins = members
    for name in primaryAdmins where !admins.contains(name) { admins.append(name) }
    admins = admins.filter { $0 != "root" }
    for name in admins {
        guard let shell = shells[name] else { return .fail("admin \(name) has no account record") }
        guard adminLocked(shell: shell) else { return .fail("admin \(name) shell \(shell)") }
    }
    return .pass("root locked; \(admins.count) other admin(s) shell \(Checks.lockedShell): \(admins.joined(separator: " "))")
}

/// `dsmemberutil checkmembership`'s verdict: true for a member (nested groups included), false
/// for a non-member, nil for anything else, which the caller counts as failed.
func memberVerdict(_ output: String) -> Bool? {
    let text = output.trimmingCharacters(in: .whitespacesAndNewlines)
    switch text {
    case "user is a member of the group": return true
    case "user is not a member of the group": return false
    default: return nil
    }
}

/// `sudo -l -U <user>` (as root) said the user may run nothing with sudo.
func sudoDenied(_ output: String, user: String) -> Bool {
    let lines = output.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
    return lines.count == 1 && lines[0].hasPrefix("User \(user) is not allowed to run sudo on ")
}

/// Remote Login is off: `launchctl print-disabled system` lists sshd as disabled; when it does
/// not list sshd at all, `launchctl print system/com.openssh.sshd` must say it is not loaded
/// (exit 113). Any other answer, an error included, is on.
func sshdOff(disabledList: String, printStatus: Int32?) -> Bool {
    sshdVerdict(disabledList: disabledList, printStatus: printStatus).ok
}

func sshdVerdict(disabledList: String, printStatus: Int32?) -> CheckVerdict {
    for line in disabledList.split(whereSeparator: \.isNewline) where line.contains("\"com.openssh.sshd\"") {
        let value = line.split(separator: ">").last.map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
        let shown = "sshd listed as \(String(value.prefix(16)))"
        return value == "disabled" || value == "true" ? .pass(shown) : .fail(shown)
    }
    guard let printStatus else { return .fail("sshd not listed; launchctl print did not run") }
    let shown = "sshd not listed; launchctl print exit \(printStatus)"
    return printStatus == Checks.launchctlNotFound ? .pass(shown) : .fail(shown)
}

/// The console user is `runner`, with the uid `runner` has in the directory, and not root.
func consoleUserVerdict(name: String?, uid: uid_t, runnerUID: uid_t?) -> CheckVerdict {
    guard let name else { return .fail("no console user") }
    guard name == Guest.runnerUser else { return .fail("console user is \(name)") }
    guard let runnerUID else { return .fail("console user is runner, but runner has no account record") }
    guard uid == runnerUID, uid != 0 else { return .fail("console uid \(uid), runner's uid \(runnerUID)") }
    return .pass("console user runner uid \(uid)")
}

/// The failed checks, in CheckCode's order, from a pass/fail per check. A check with no
/// answer failed: the verification fails closed.
func failedChecks(_ results: [CheckCode: Bool]) -> [CheckCode] {
    CheckCode.allCases.filter { results[$0] != true }
}

// MARK: - The facts (the guest)

/// Run `argv` with a fixed, empty environment but PATH, no stdin, and a deadline; its exit status
/// and at most Checks.maxOutput bytes of stdout, or nil when it could not run or timed out.
/// `withStderr` reads stderr into the same output, for a command (sudo) that may say its answer
/// there.
func runCheckCommand(_ argv: [String], timeout: TimeInterval = Checks.commandTimeout,
                     withStderr: Bool = false) -> (Int32, Data)? {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: argv[0])
    p.arguments = Array(argv.dropFirst())
    p.environment = ["PATH": Guest.systemPath, "LANG": Guest.locale, "LC_ALL": Guest.locale]
    p.standardInput = FileHandle.nullDevice
    let out = Pipe()
    p.standardOutput = out
    p.standardError = withStderr ? out : FileHandle.nullDevice
    let done = DispatchSemaphore(value: 0)
    p.terminationHandler = { _ in done.signal() }
    do { try p.run() } catch { return nil }
    let box = OutputBox()
    let reader = DispatchSemaphore(value: 0)
    let handle = out.fileHandleForReading
    DispatchQueue.global().async {
        while box.count <= Checks.maxOutput {
            let chunk = handle.availableData
            if chunk.isEmpty { break }
            box.append(chunk)
        }
        reader.signal()
    }
    guard done.wait(timeout: .now() + timeout) == .success else {
        // Process starts the command in a process group of its own, which what it starts
        // inherits. sudo forwards SIGTERM to its command but cannot forward SIGKILL, so the
        // group first gets a grace period to exit (the toolchain script removes its scratch
        // directory) before whatever is left in it is killed. Apple does not document the
        // group, so with none (ESRCH) the command itself is signalled rather than nothing.
        var target = -p.processIdentifier
        if kill(target, SIGTERM) == -1, errno == ESRCH {
            target = p.processIdentifier
            kill(target, SIGTERM)
        }
        let exited = done.wait(timeout: .now() + Checks.killGrace) == .success
        // A group's survivors still need the SIGKILL. A bare pid does not once the command
        // has exited: Process has reaped it, so the pid may already be another process's.
        if target < 0 || !exited { kill(target, SIGKILL) }
        return nil
    }
    _ = reader.wait(timeout: .now() + Checks.readerGrace)
    return (p.terminationStatus, box.data.prefix(Checks.maxOutput))
}

/// `argv` as `runner`, through sudo from root (which needs no password and asks for none),
/// with runner's home: the account and home a job's tools see.
func asRunner(_ argv: [String]) -> [String] {
    ["/usr/bin/sudo", "-n", "-H", "-u", Guest.runnerUser, "--"] + argv
}

/// A command's stdout, filled on a reader thread.
private final class OutputBox: @unchecked Sendable {
    private let lock = NSLock()
    private var bytes = Data()
    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return bytes.count
    }
    var data: Data {
        lock.lock()
        defer { lock.unlock() }
        return bytes
    }
    func append(_ d: Data) {
        lock.lock()
        bytes.append(d)
        lock.unlock()
    }
}

/// How a command ended, for a line: `exit N`, or `did not run or timed out`.
private func ended(_ result: (Int32, Data)?) -> String {
    result.map { "exit \($0.0)" } ?? "did not run or timed out"
}

private func plist(_ path: String) -> [String: Any]? {
    NSDictionary(contentsOfFile: path) as? [String: Any]
}

/// Why a plist root reads could not be read: absent, or present and not a dictionary.
private func plistFailure(_ path: String) -> String {
    var st = stat()
    guard lstat(path, &st) == 0 else { return "\(path) unreadable: \(errnoText(errno))" }
    guard st.st_mode & S_IFMT == S_IFREG else { return "\(path) is not a regular file" }
    return "\(path) (\(st.st_size) bytes) is not a readable dictionary"
}

/// Every account: name, uid, primary gid, shell and home.
private func accounts() -> [(name: String, uid: uid_t, gid: gid_t, shell: String, home: String)] {
    var out: [(String, uid_t, gid_t, String, String)] = []
    setpwent()
    while let pw = getpwent() {
        out.append((String(cString: pw.pointee.pw_name), pw.pointee.pw_uid, pw.pointee.pw_gid,
                    String(cString: pw.pointee.pw_shell), String(cString: pw.pointee.pw_dir)))
    }
    endpwent()
    return out.map { (name: $0.0, uid: $0.1, gid: $0.2, shell: $0.3, home: $0.4) }
}

/// Write each line to the guest's console, which root locks to itself first (as
/// provision-wrapper.sh does), so no other account in the guest can forge a line; and to the
/// agent's own log. Each line waits at most 1 s in writeAll; a line the console cannot take in
/// that time is dropped.
func guestConsoleWriter(path: String = Diagnostics.console) -> @Sendable (String) -> Void {
    var st = stat()
    var fd: Int32 = -1
    if lstat(path, &st) == 0, st.st_mode & S_IFMT == S_IFCHR {
        _ = chmod(path, 0o600)
        fd = open(path, O_WRONLY | O_NOCTTY | O_NONBLOCK | O_CLOEXEC)
    }
    return { [fd] line in
        agentLog(line)
        guard fd >= 0 else { return }
        // The newline first, so a line is never glued to other console output. The console is
        // non-blocking: a burst of lines (the runner's tail) waits for room, a second at most,
        // rather than losing bytes to EAGAIN.
        let bytes = Array(("\n" + line + "\n").utf8)
        try? writeAll(fd, bytes, until: Date().addingTimeInterval(1))
    }
}

/// Run every check against `expect`, each line to the guest's console. Slow: it runs
/// xcodebuild and simctl.
func runGuestChecks(_ expect: VerifyExpect) -> [CheckCode] {
    checkGuest(expect, sink: DiagnosticSink(write: guestConsoleWriter()))
}

func checkGuest(_ expect: VerifyExpect, sink: DiagnosticSink) -> [CheckCode] {
    var r: [CheckCode: Bool] = [:]
    func record(_ code: CheckCode, _ verdict: CheckVerdict) {
        r[code] = verdict.ok
        sink.emit(checkLine(code, verdict))
    }
    sink.emit(diagnosticLine("start checks=\(CheckCode.allCases.count)"))
    let all = accounts()
    let runner = all.first(where: { $0.name == Guest.runnerUser })

    var cuid: uid_t = 0
    var cgid: gid_t = 0
    let console = SCDynamicStoreCopyConsoleUser(nil, &cuid, &cgid) as String?
    record(.consoleUser, consoleUserVerdict(name: console, uid: cuid, runnerUID: runner?.uid))

    // Every command error counts as failed: a check passes only on a plain, expected answer.
    if let runner {
        // Not an admin, and no sudo rights of any kind (a sudoers entry is root too).
        let verdict: CheckVerdict
        if runner.gid == Checks.adminGID {
            verdict = .fail("runner's primary group is admin")
        } else {
            let member = runCheckCommand(["/usr/bin/dsmemberutil", "checkmembership", "-U", runner.name, "-G", Checks.adminGroup])
            let isAdmin = member.flatMap { $0.0 == 0 ? memberVerdict(String(decoding: $0.1, as: UTF8.self)) : nil }
            let sudo = runCheckCommand(["/usr/bin/sudo", "-n", "-l", "-U", Guest.runnerUser], withStderr: true)
            let denied = sudo.map { sudoDenied(String(decoding: $0.1, as: UTF8.self), user: Guest.runnerUser) }
            switch (isAdmin, denied) {
            case (nil, _): verdict = .fail("dsmemberutil for runner: no plain answer (\(ended(member)))")
            case (true?, _): verdict = .fail("runner is in admin (dsmemberutil)")
            case (false?, nil): verdict = .fail("sudo -l for runner \(ended(sudo))")
            case (false?, false?): verdict = .fail("sudo -l lists rights for runner (\(ended(sudo)))")
            case (false?, true?): verdict = .pass("runner not in admin; sudo -l denies it")
            }
        }
        record(.runnerNotAdmin, verdict)
    } else {
        record(.runnerNotAdmin, .fail("no runner account"))
    }

    // Root, by its own record; every other admin, by the admin group's members in the
    // directory (nested groups included) and the accounts whose primary group is admin's.
    let rootRead = runCheckCommand(["/usr/bin/dscl", ".", "-read", "/Users/root"])
    let groupsRead = runCheckCommand(["/usr/bin/dscl", "-plist", ".", "-readall", "/Groups",
                                      "RecordName", "GeneratedUID", "GroupMembership", "GroupMembers", "NestedGroups"])
    // Names and GeneratedUIDs only: never a user's authentication attributes.
    let usersRead = runCheckCommand(["/usr/bin/dscl", "-plist", ".", "-readall", "/Users", "RecordName", "GeneratedUID"])
    var shells: [String: String] = [:]
    for a in all where shells[a.name] == nil { shells[a.name] = a.shell }
    record(.adminsLocked, adminsVerdict(
        groups: groupsRead.flatMap { $0.0 == 0 ? parseGroupRecords($0.1) : nil },
        groupsFailure: groupsRead.map { $0.0 == 0 ? "dscl readall /Groups: unexpected output" : "dscl readall /Groups exit \($0.0)" }
            ?? "dscl readall /Groups did not run or timed out",
        users: usersRead.flatMap { $0.0 == 0 ? parseUserGUIDs($0.1) : nil },
        usersFailure: usersRead.map { $0.0 == 0 ? "dscl readall /Users: unexpected output" : "dscl readall /Users exit \($0.0)" }
            ?? "dscl readall /Users did not run or timed out",
        primaryAdmins: all.filter { $0.gid == Checks.adminGID }.map(\.name),
        shells: shells,
        rootRecord: rootRead.flatMap { $0.0 == 0 ? String(decoding: $0.1, as: UTF8.self) : nil },
        rootFailure: "dscl read /Users/root \(ended(rootRead))"))

    let disabled = runCheckCommand(["/bin/launchctl", "print-disabled", "system"])
    if let (code, out) = disabled, code == 0 {
        let loaded = runCheckCommand(["/bin/launchctl", "print", "system/com.openssh.sshd"]).map(\.0)
        record(.remoteLogin, sshdVerdict(disabledList: String(decoding: out, as: UTF8.self), printStatus: loaded))
    } else {
        record(.remoteLogin, .fail("launchctl print-disabled system \(ended(disabled))"))
    }

    let fde = runCheckCommand(["/usr/bin/fdesetup", "status"])
    if let (code, out) = fde {
        let said = firstLine(String(decoding: out, as: UTF8.self))
        record(.filevault, code == 0 && fileVaultOff(String(decoding: out, as: UTF8.self))
            ? .pass(said) : .fail("fdesetup exit \(code): \(said)"))
    } else {
        record(.filevault, .fail("fdesetup \(ended(fde))"))
    }

    let homes = all.filter { $0.home.hasPrefix("/Users/") }
    if let signedIn = homes.first(where: { appleIDSignedIn(plist($0.home + "/Library/Preferences/MobileMeAccounts.plist")) }) {
        record(.appleID, .fail("an Apple ID is signed in for \(signedIn.name)"))
    } else {
        record(.appleID, .pass("no Apple ID in \(homes.count) home(s)"))
    }

    if let prefs = plist(Checks.softwareUpdatePlist) {
        record(.softwareUpdate, softwareUpdateVerdict(prefs))
    } else {
        record(.softwareUpdate, .fail(plistFailure(Checks.softwareUpdatePlist)))
    }
    // Evidence only, on a line of its own (the check's line has no room left for it): what
    // softwareupdate itself says about the schedule. The verdict above is the plist's alone.
    sink.emit(scheduleEvidenceLine(runCheckCommand([UpdatePolicy.softwareupdate, "--schedule"],
                                                   timeout: UpdatePolicy.scheduleTimeout, withStderr: true)))

    // As runner, the account a job runs as: root reads an Xcode runner may not (Toolchain).
    if runner != nil {
        let xcode = runCheckCommand(asRunner(["/usr/bin/xcodebuild", "-version"]))
        let version: CheckVerdict
        if let (code, out) = xcode, code == 0 {
            version = xcodeVerdict(String(decoding: out, as: UTF8.self), expect.xcode)
        } else {
            version = .fail("xcodebuild -version as runner \(ended(xcode))")
        }
        let toolchain = runCheckCommand(asRunner(["/bin/sh", "-c", Toolchain.script, "sh",
                                                  Toolchain.xcodeSelect, Toolchain.cc, Toolchain.swiftc]),
                                        timeout: Toolchain.timeout)
        record(.xcode, xcodeCheckVerdict(version: version, toolchain: toolchainVerdict(toolchain)))
    } else {
        record(.xcode, .fail("no runner account"))
    }

    if expect.runtimes.isEmpty {
        record(.runtimes, .pass("no runtimes expected"))
    } else {
        let simctl = runCheckCommand(["/usr/bin/xcrun", "simctl", "list", "runtimes", "-j"])
        if let (code, out) = simctl, code == 0 {
            record(.runtimes, runtimesVerdict(out, expect.runtimes))
        } else {
            record(.runtimes, .fail("simctl list runtimes \(ended(simctl))"))
        }
    }

    if let runner {
        var st = stat()
        let verdict: CheckVerdict
        if stat(Checks.homebrewPrefix, &st) != 0 {
            verdict = .fail("\(Checks.homebrewPrefix): \(errnoText(errno))")
        } else if st.st_uid != runner.uid {
            verdict = .fail("\(Checks.homebrewPrefix) owner uid \(st.st_uid), runner's uid \(runner.uid)")
        } else if stat(Checks.homebrewPrefix + "/bin/brew", &st) != 0 {
            verdict = .fail("\(Checks.homebrewPrefix)/bin/brew: \(errnoText(errno))")
        } else {
            verdict = .pass("\(Checks.homebrewPrefix) owned by runner uid \(runner.uid)")
        }
        record(.brewOwner, verdict)
    } else {
        record(.brewOwner, .fail("no runner account"))
    }

    let failed = failedChecks(r)
    sink.emit(doneLine(failed))
    return failed
}
