// Software Update stays off in every clone (ADR-0049, *Software Update is off in the guest*,
// amended 2026-10-26 and 2026-11-20). The golden build writes the keys once
// (launcher/golden/provision.sh, step a), but a clone does not keep them: macOS removes
// AutomaticCheckEnabled again after boot, after the agent's write at `serve` (a verify clone
// showed `updates_off at=serve ok=true wrote=5/5`, then the key absent at the check). Absent, it
// defaults to on. The other three keys stay false.
//
// So the agent does not write the keys at fixed moments only. It writes them when `serve`
// starts and before a job's hand-off, as before, and in between it watches the plist the
// verification judges, once a second for the life of the clone, and writes the keys again
// whenever one of them is no longer false. The first time it has to, it also says who was
// talking about Software Update in the guest's log just then, so the console names the
// remover (UpdateWatch, removerEvidenceLines).
//
// Every write goes through `defaults write`, as the build's do, so cfprefsd owns the file;
// the agent never edits it under cfprefsd. The writes are the build's step a, key for key;
// launcher/Tests/UpdatesTests.swift reads provision.sh and holds the two lists equal. A write
// that fails never stops the agent: the result is a console line, and the verification still
// judges the plist (Checks.swift, softwareUpdateCheck).

import Foundation

enum UpdatePolicy {
    static let defaults = "/usr/bin/defaults"
    /// The domains as provision.sh names them: the path of the plist without its extension.
    static let softwareUpdateDomain = "/Library/Preferences/com.apple.SoftwareUpdate"
    static let commerceDomain = "/Library/Preferences/com.apple.commerce"
    static let commerceKey = "AutoUpdate"
    /// The longest one write may take; with `Checks.readerGrace` after it, five writes take
    /// at most 50 s, which the launcher's begin-to-running deadline
    /// (launcher/Sources/Agent.swift, `started`) counts. A write takes well under a second.
    static let writeTimeout: TimeInterval = 5
    /// `softwareupdate --schedule`, read for the verification's evidence only.
    static let softwareupdate = "/usr/sbin/softwareupdate"
    static let scheduleTimeout: TimeInterval = 20
    /// The console tag of the agent's own lines, apart from a verification's.
    static let tag = "FABER-AGENT"
    /// How often the watch reads the plist. A read is one small file; a key macOS removes is
    /// back within about this long.
    static let watchInterval: TimeInterval = 1
    /// How long the verification waits, after writing the keys again at the check, before it
    /// reads the plist a second time: longer than one watch interval and one round of writes.
    static let settle: TimeInterval = 3
    /// The watch's restores that each get their lines; after them, every `quietEvery`th.
    static let loudRestores = 5
    static let quietEvery = 100
    /// The guest's own log around the first restore: who logged about Software Update.
    static let log = "/usr/bin/log"
    static let logWindow = "30s"
    static let logTimeout: TimeInterval = 30
    /// Software Update's daemons and frameworks, and any message naming it or its keys; never
    /// `log` itself, whose own start line carries this predicate.
    static let logPredicate = "(process == \"softwareupdated\" OR subsystem BEGINSWITH \"com.apple.SoftwareUpdate\""
        + " OR subsystem BEGINSWITH \"com.apple.MobileSoftwareUpdate\" OR eventMessage CONTAINS[c] \"SoftwareUpdate\""
        + " OR eventMessage CONTAINS[c] \"AutomaticCheck\") AND NOT process == \"log\""
    /// The most messages the evidence quotes, and the most of each.
    static let quotedMessages = 3
    static let quotedLength = 100
}

/// When the agent writes the keys again.
enum ReapplyMoment: String, Sendable {
    /// `faber-agent serve` starting, before it answers any request.
    case serve
    /// A job's `begin`, before the runner is handed its config.
    case begin
    /// The watch found a key that is no longer false.
    case watch
    /// The verification's software_update check found the plist not off, and writes the keys
    /// once more before it reads the plist again.
    case check
}

/// Runs `argv` with a deadline: its exit status and output, or nil when it did not run or timed
/// out. The guest's is runCheckCommand; a test passes its own.
typealias GuestCommand = @Sendable (_ argv: [String], _ timeout: TimeInterval) -> (Int32, Data)?

/// Every write, in provision.sh's order: the four Software Update keys false, then the App
/// Store's AutoUpdate false. Pure.
func updatesOffWrites() -> [[String]] {
    Checks.updateKeys.map { [UpdatePolicy.defaults, "write", UpdatePolicy.softwareUpdateDomain, $0, "-bool", "false"] }
        + [[UpdatePolicy.defaults, "write", UpdatePolicy.commerceDomain, UpdatePolicy.commerceKey, "-bool", "false"]]
}

/// Make every write, each whatever the others did, and say how it went in one console line:
/// `FABER-AGENT updates_off at=<moment> ok=<bool> wrote=<n>/<total>`, then the first failure's
/// key and how its write ended. Never throws: the agent serves on either way.
func reapplyUpdatesOff(at moment: ReapplyMoment, run: GuestCommand) -> String {
    let writes = updatesOffWrites()
    var failures: [String] = []
    for argv in writes {
        let result = run(argv, UpdatePolicy.writeTimeout)
        guard let (code, _) = result else {
            failures.append("\(argv[3]) did not run or timed out")
            continue
        }
        if code != 0 { failures.append("\(argv[3]) exit \(code)") }
    }
    var body = "updates_off at=\(moment.rawValue) ok=\(failures.isEmpty) wrote=\(writes.count - failures.count)/\(writes.count)"
    if let first = failures.first { body += " failed=\(first)" }
    return diagnosticLine(body, tag: UpdatePolicy.tag)
}

/// The first of the keys the agent writes that is not false in `prefs`, the system's
/// SoftwareUpdate preferences, and its value; nil when all are false. This is what the watch
/// restores: the verification's own judge (softwareUpdateVerdict) is stricter, and a key the
/// agent does not write is never rewritten in a loop that cannot fix it. Pure.
func updateKeyLost(_ prefs: [String: Any]?) -> String? {
    guard let prefs else { return "plist unreadable" }
    for k in Checks.updateKeys {
        guard let v = prefs[k] else { return "\(k) absent" }
        guard let n = v as? NSNumber, !n.boolValue else { return "\(k)=\((v as? NSNumber)?.stringValue ?? "a \(type(of: v))")" }
    }
    return nil
}

/// The watch: reads the SoftwareUpdate plist every UpdatePolicy.watchInterval, for the life
/// of `serve`, and writes the keys again whenever one is no longer false. The guest's facts
/// (the read, the commands, the console, the clock) come in as closures, so a test can drive it.
final class UpdateWatch: @unchecked Sendable {
    private let lock = NSLock()
    private var restoreCount = 0
    private var first: (why: String, after: Int)?
    private let read: @Sendable () -> [String: Any]?
    private let run: GuestCommand
    private let console: @Sendable (String) -> Void
    private let now: @Sendable () -> Date
    private let since: Date

    init(read: @escaping @Sendable () -> [String: Any]?, run: @escaping GuestCommand,
         console: @escaping @Sendable (String) -> Void, now: @escaping @Sendable () -> Date = { Date() }) {
        self.read = read
        self.run = run
        self.console = console
        self.now = now
        self.since = now()
    }

    /// How many times the watch has written the keys again, and the first loss it saw (which
    /// key, and how many seconds after the watch started).
    var restores: (count: Int, first: (why: String, after: Int)?) {
        lock.lock()
        defer { lock.unlock() }
        return (restoreCount, first)
    }

    /// One look. When every key is off, nothing. Otherwise the keys are written again at once,
    /// then the lines: `updates_lost`, the writes' own line, and, the first time only, who
    /// logged about Software Update just before (removerEvidenceLines). After
    /// UpdatePolicy.loudRestores, only every UpdatePolicy.quietEvery-th restore is written out.
    func tick() {
        guard let why = updateKeyLost(read()) else { return }
        let wrote = reapplyUpdatesOff(at: .watch, run: run)
        lock.lock()
        restoreCount += 1
        let n = restoreCount
        let after = max(0, Int(now().timeIntervalSince(since)))
        if first == nil { first = (why, after) }
        lock.unlock()
        guard n <= UpdatePolicy.loudRestores || n % UpdatePolicy.quietEvery == 0 else { return }
        console(diagnosticLine("updates_lost after=\(after)s restores=\(n) why=\(why)", tag: UpdatePolicy.tag))
        console(wrote)
        if n == 1 {
            let log = run([UpdatePolicy.log, "show", "--last", UpdatePolicy.logWindow, "--style", "ndjson",
                           "--predicate", UpdatePolicy.logPredicate], UpdatePolicy.logTimeout)
            removerEvidenceLines(lost: why, log).forEach(console)
        }
    }

    /// Look forever, on a thread of its own.
    func start() {
        Thread.detachNewThread { [self] in
            while true {
                tick()
                Thread.sleep(forTimeInterval: UpdatePolicy.watchInterval)
            }
        }
    }
}

/// The evidence of the first restore, from `log show --style ndjson` over the window before it:
/// one line counting the messages per process (most first), then the newest messages that
/// mention automatic checking, each bounded, at most UpdatePolicy.quotedMessages. The log's
/// own dynamic values show as `<private>` to a reader without private data, which this is.
/// A log that did not run is one line saying so. Pure.
func removerEvidenceLines(lost: String, _ result: (Int32, Data)?) -> [String] {
    func line(_ body: String) -> String { diagnosticLine("evidence updates_lost=\(lost) " + body, tag: UpdatePolicy.tag) }
    guard let (code, out) = result else { return [line("log=did not run or timed out")] }
    var counts: [String: Int] = [:]
    var order: [String] = []
    var quoted: [(String, String)] = []
    for raw in out.split(separator: UInt8(ascii: "\n")) {
        guard let entry = (try? JSONSerialization.jsonObject(with: Data(raw))) as? [String: Any] else { continue }
        let path = entry["processImagePath"] as? String ?? "?"
        let process = path.split(separator: "/").last.map(String.init) ?? path
        if counts[process] == nil { order.append(process) }
        counts[process, default: 0] += 1
        if let message = entry["eventMessage"] as? String,
           message.range(of: "automatic", options: .caseInsensitive) != nil
            || message.range(of: "AutomaticCheck") != nil {
            quoted.append((process, message))
        }
    }
    // Most messages first; a tie keeps the order the log first named them in.
    let ranked = order.enumerated()
        .sorted { (counts[$0.element]!, -$0.offset) > (counts[$1.element]!, -$1.offset) }
        .map(\.element)
    let logged = ranked.isEmpty ? "none" : ranked.map { "\($0):\(counts[$0]!)" }.joined(separator: ",")
    var lines = [line("log=exit \(code) logged=\(logged)")]
    for (process, message) in quoted.suffix(UpdatePolicy.quotedMessages) {
        lines.append(line("said=\(process): \(firstLine(message, max: UpdatePolicy.quotedLength))"))
    }
    return lines
}

/// The verification's evidence line after software_update: what `softwareupdate --schedule`
/// said, its first line bounded. It never changes the check's verdict. Pure.
func scheduleEvidenceLine(_ result: (Int32, Data)?) -> String {
    guard let (code, out) = result else {
        return diagnosticLine("evidence check=software_update schedule=did not run or timed out")
    }
    return diagnosticLine("evidence check=software_update schedule=exit \(code) '\(firstLine(String(decoding: out, as: UTF8.self), max: 100))'")
}

/// The verification's evidence line on the watch: how often it has had to write the keys
/// again since `serve` started, and its first loss. It never changes the check's verdict. Pure.
func watchEvidenceLine(_ restores: (count: Int, first: (why: String, after: Int)?)?) -> String {
    guard let restores else { return diagnosticLine("evidence check=software_update watch=not running") }
    guard let first = restores.first else { return diagnosticLine("evidence check=software_update watch_restores=0") }
    return diagnosticLine("evidence check=software_update watch_restores=\(restores.count) first=\(first.why) after=\(first.after)s")
}
