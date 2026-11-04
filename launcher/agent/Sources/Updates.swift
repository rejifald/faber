// Software Update stays off in every clone (ADR-0049, *Software Update is off in the guest*,
// amended 2026-10-26). The golden build writes the keys once (launcher/golden/provision.sh,
// step a), but a clone's boot does not keep them: on a fresh verify clone of an image whose
// disk held all four false, AutomaticCheckEnabled was gone shortly after boot. So the
// agent writes the build's keys again, as root, when `serve` starts and again right before a
// job's hand-off, the same way the build does: through `defaults write`, so cfprefsd owns the
// file, never by editing it under cfprefsd.
//
// The writes are the build's step a, key for key; launcher/Tests/UpdatesTests.swift reads
// provision.sh and holds the two lists equal. A write that fails never stops the agent: the
// result is one line on the guest's console, and the verification still judges the plist.

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
}

/// When the agent writes the keys again.
enum ReapplyMoment: String, Sendable {
    /// `faber-agent serve` starting, before it answers any request.
    case serve
    /// A job's `begin`, before the runner is handed its config.
    case begin
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

/// The verification's evidence line after software_update: what `softwareupdate --schedule`
/// said, its first line bounded. It never changes the check's verdict. Pure.
func scheduleEvidenceLine(_ result: (Int32, Data)?) -> String {
    guard let (code, out) = result else {
        return diagnosticLine("evidence check=software_update schedule=did not run or timed out")
    }
    return diagnosticLine("evidence check=software_update schedule=exit \(code) '\(firstLine(String(decoding: out, as: UTF8.self), max: 100))'")
}
