// Pure logic: slots and MACs, subnets, options, bundles, locks, the entitlements file, the version.

import Darwin
import Foundation
import Virtualization

@MainActor
func testSlots(_ t: Harness) {
    var seen = Set<String>()
    for i in 0..<Slot.count {
        guard let slot = try? Slot(i) else {
            t.check("slot \(i) accepted", false)
            continue
        }
        let mac = slot.mac
        t.check("slot \(i) MAC is locally administered", mac.isLocallyAdministeredAddress, mac.string)
        t.check("slot \(i) MAC is unicast", mac.isUnicastAddress && !mac.isMulticastAddress && !mac.isBroadcastAddress, mac.string)
        t.check("slot \(i) MAC text matches the framework's", mac.string == slot.macText, "\(mac.string) vs \(slot.macText)")
        t.check("slot \(i) MAC is deterministic", (try? Slot(i))?.macText == slot.macText)
        t.check("slot \(i) octets round-trip", macOctets(mac) == slot.octets)
        seen.insert(slot.macText)
    }
    t.check("slot MACs are unique", seen.count == Slot.count, "\(seen.count) distinct of \(Slot.count)")
    t.check("slot 0 MAC", (try? Slot(0))?.macText == "02:fa:be:00:00:00")
    t.check("slot 15 MAC", (try? Slot(15))?.macText == "02:fa:be:00:00:0f")
    t.refuses("slot 16 refused", usage: true) { _ = try Slot(16) }
    t.refuses("slot -1 refused", usage: true) { _ = try Slot.parse("-1") }
    t.refuses("slot 01 refused (not canonical)", usage: true) { _ = try Slot.parse("01") }
    t.refuses("slot x refused", usage: true) { _ = try Slot.parse("x") }
}

@MainActor
func testSubnets(_ t: Harness) {
    do {
        let s = try IPv4Subnet.parse("192.168.100.0/24")
        t.check("subnet network", s.network.description == "192.168.100.0")
        t.check("gateway is the subnet's second address", s.gateway.description == "192.168.100.1")
        t.check("default reservation is the third address", s.defaultGuest.description == "192.168.100.2")
        t.check("broadcast is the last address", s.broadcast.description == "192.168.100.255")
        t.check("mask", s.maskAddress.description == "255.255.255.0")
        t.accepts("reservation inside the subnet") { try s.checkReservation(IPv4.parse("192.168.100.50")!) }
        t.refuses("reservation at the network address", usage: true) { try s.checkReservation(IPv4.parse("192.168.100.0")!) }
        t.refuses("reservation at the gateway", usage: true) { try s.checkReservation(IPv4.parse("192.168.100.1")!) }
        t.refuses("reservation at the broadcast address", usage: true) { try s.checkReservation(IPv4.parse("192.168.100.255")!) }
        t.refuses("reservation outside the subnet", usage: true) { try s.checkReservation(IPv4.parse("192.168.101.5")!) }
        let plan = try VmnetPlan(subnet: s, reservation: nil)
        t.check("plan defaults the guest to the third address", plan.guest.description == "192.168.100.2")
        let line = plan.line(slot: try Slot(3))
        for part in ["subnet=192.168.100.0/24", "gateway=192.168.100.1", "guest=192.168.100.2",
                     "mac=02:fa:be:00:00:03", "ipv6=link-local-only", "dns_proxy=off"] {
            t.check("network line has \(part)", line.contains(part), line)
        }
    } catch {
        t.check("192.168.100.0/24 parses", false, "\(error)")
    }
    for ok in ["10.20.0.0/16", "172.16.0.0/16", "172.31.255.252/30", "192.168.100.0/30", "10.0.0.0/29"] {
        t.accepts("subnet \(ok) accepted") { _ = try IPv4Subnet.parse(ok) }
    }
    if let tiny = try? IPv4Subnet.parse("192.168.100.0/30") {
        t.check("a /30 still has a guest address", tiny.defaultGuest.description == "192.168.100.2"
            && tiny.broadcast.description == "192.168.100.3")
    }
    let refused = [
        "192.168.100.1/24": "host bits",
        "8.8.8.0/24": "private",
        "100.64.0.0/16": "private",
        "172.32.0.0/16": "private",
        "169.254.0.0/16": "private",
        "10.0.0.0/8": "prefix length",
        "192.168.0.0/15": "prefix length",
        "192.168.100.0/31": "prefix length",
        "192.168.100.0/32": "prefix length",
        "192.168.100.0": "address/prefix",
        "192.168.100.0/024": "address/prefix",
        "192.168.100.0/abc": "address/prefix",
        "fd00::/64": "address/prefix",
    ]
    for (text, why) in refused.sorted(by: { $0.key < $1.key }) {
        t.refuses("subnet \(text) refused", containing: why, usage: true) { _ = try IPv4Subnet.parse(text) }
    }
    t.refuses("plan refuses a reservation at the gateway", usage: true) {
        _ = try VmnetPlan(subnet: IPv4Subnet.parse("192.168.100.0/24"), reservation: IPv4.parse("192.168.100.1"))
    }
}

@MainActor
func testOptions(_ t: Harness) {
    func options(_ args: [String], forRun: Bool = false) throws -> RunOptions {
        try RunOptions(parse(args, valued: RunOptions.valued, switches: forRun ? [] : ["--wall-only"]), forRun: forRun)
    }
    t.accepts("vmnet with a subnet") {
        let o = try options(["--slot", "2", "--subnet", "192.168.100.0/24"])
        guard o.network == .vmnet, o.vmnet?.guest.description == "192.168.100.2" else { throw Failure("wrong plan") }
    }
    t.accepts("--reserve-ip overrides the guest address") {
        let o = try options(["--slot", "2", "--subnet", "192.168.100.0/24", "--reserve-ip", "192.168.100.9"])
        guard o.vmnet?.guest.description == "192.168.100.9" else { throw Failure("wrong reservation") }
    }
    t.refuses("--slot is required", containing: "--slot", usage: true) { _ = try options(["--network", "shared-nat"]) }
    t.refuses("vmnet needs --subnet", containing: "--subnet", usage: true) { _ = try options(["--slot", "0"]) }
    t.refuses("shared-nat refuses --subnet", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat", "--subnet", "192.168.100.0/24"])
    }
    t.refuses("unknown network refused", usage: true) { _ = try options(["--slot", "0", "--network", "bridged"]) }
    t.refuses("--wall-only with vmnet refused", containing: "--wall-only", usage: true) {
        _ = try options(["--slot", "0", "--subnet", "192.168.100.0/24", "--wall-only"])
    }
    t.refuses("run needs --state-dir", containing: "--state-dir", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat"], forRun: true)
    }
    t.refuses("run has no --wall-only", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat", "--state-dir", "/var/empty", "--wall-only"], forRun: true)
    }
    t.refuses("a repeated option refused", usage: true) { _ = try options(["--slot", "0", "--slot", "1"]) }
    t.refuses("an unknown option refused", usage: true) { _ = try options(["--slot", "0", "--gui"]) }
    t.refuses("a bad --disk-cache refused", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat", "--disk-cache", "fast"])
    }
    t.refuses("--console-log-max-mib needs --console-log", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat", "--console-log-max-mib", "8"])
    }
    t.refuses("--console-log-max-mib 0 refused", usage: true) {
        _ = try options(["--slot", "0", "--network", "shared-nat", "--console-log", "/tmp/x.log", "--console-log-max-mib", "0"])
    }
    t.accepts("--console-log-max-mib sets the cap") {
        let o = try options(["--slot", "0", "--network", "shared-nat", "--console-log", "/tmp/x.log", "--console-log-max-mib", "8"])
        guard o.consoleLogMaxMiB == 8 else { throw Failure("cap not set") }
    }
}

@MainActor
func testBundlesAndLocks(_ t: Harness, _ scratch: Scratch) {
    do {
        let ambiguous = try scratch.dir("ambiguous")
        try scratch.sparse(ambiguous.appendingPathComponent("disk.raw"))
        try scratch.write(ambiguous.appendingPathComponent("vz-hwmodel"))
        t.refuses("a bundle with both kinds' files refused", containing: "both") { _ = try VMBundle(url: ambiguous).kind() }

        let empty = try scratch.dir("empty")
        t.refuses("a directory with neither kind's files refused") { _ = try VMBundle(url: empty).kind() }

        let macPartial = try scratch.dir("mac-partial")
        try scratch.write(macPartial.appendingPathComponent("vz-aux"))
        t.refuses("macOS files without vz-hwmodel refused", containing: "vz-hwmodel") { _ = try VMBundle(url: macPartial).kind() }

        let linuxStray = try scratch.dir("linux-stray")
        try scratch.sparse(linuxStray.appendingPathComponent("disk.raw"))
        try scratch.write(linuxStray.appendingPathComponent("vz-identifier"))
        t.refuses("a Linux bundle with a stray vz-identifier refused", containing: "both") { _ = try VMBundle(url: linuxStray).kind() }

        let linux = try scratch.dir("kind-linux")
        try scratch.sparse(linux.appendingPathComponent("disk.raw"))
        t.check("disk.raw makes a Linux bundle", (try? VMBundle(url: linux).kind()) == .linux)

        t.refuses("clone refuses a Linux bundle", containing: "Linux") {
            _ = try clone(VMBundle(url: linux), VMBundle(url: scratch.url.appendingPathComponent("never")), keepIdentifier: false)
        }
        t.check("a refused clone leaves no destination", !pathExists(scratch.url.appendingPathComponent("never")))

        // clonefile itself, on the temporary volume (APFS on every supported Mac).
        let src = try scratch.write(scratch.url.appendingPathComponent("clone-src"), Data("faber".utf8))
        let dst = scratch.url.appendingPathComponent("clone-dst")
        t.accepts("clonefile copies a file") {
            try cloneFile(src, dst)
            guard try Data(contentsOf: dst) == Data("faber".utf8) else { throw Failure("content differs") }
        }
        t.refuses("clonefile refuses an existing destination", containing: "refusing to fall back") { try cloneFile(src, dst) }

        // A golden image in the store is read-only (root:wheel 444); its per-job clone must be
        // writable by this account, or Virtualization cannot open the disk and aux storage.
        let golden = try scratch.write(scratch.url.appendingPathComponent("golden-aux"), Data("aux".utf8))
        _ = chmod(golden.path, 0o444)
        let jobCopy = scratch.url.appendingPathComponent("job-aux")
        t.accepts("a clone of a read-only file is this account's to write") {
            try cloneWritable(golden, jobCopy)
            var st = stat()
            guard lstat(jobCopy.path, &st) == 0 else { throw Failure("no clone") }
            guard st.st_mode & 0o777 == 0o600, st.st_uid == geteuid() else {
                throw Failure("mode \(String(st.st_mode & 0o777, radix: 8)) uid \(st.st_uid)")
            }
            guard access(jobCopy.path, W_OK) == 0 else { throw Failure("not writable") }
        }
        var goldenSt = stat()
        t.check("the source stays read-only", lstat(golden.path, &goldenSt) == 0 && goldenSt.st_mode & 0o777 == 0o444)
        t.refuses("cloneWritable refuses an existing destination", containing: "refusing to fall back") { try cloneWritable(golden, jobCopy) }

        // Bundle files must be regular files, never symlinks.
        let linked = try scratch.dir("linked")
        let outside = try scratch.sparse(scratch.url.appendingPathComponent("outside.raw"))
        try FileManager.default.createSymbolicLink(at: linked.appendingPathComponent("disk.raw"), withDestinationURL: outside)
        t.refuses("a symlinked disk refused", containing: "not a regular file") {
            _ = try regularFile(VMBundle(url: linked).rawDisk)
        }

        // Locks: a second holder is refused, even inside one process (flock is per open file).
        let state = try scratch.dir("state")
        chmod(state.path, 0o700)
        let slot = try Slot(4)
        let first = try slot.lock(stateDir: state)
        t.refuses("a held slot refused", containing: "slot 4 is in use") { _ = try slot.lock(stateDir: state) }
        t.accepts("another slot is free") { close(try Slot(5).lock(stateDir: state).fd) }
        close(first.fd)
        t.accepts("a released slot can be taken again") { close(try slot.lock(stateDir: state).fd) }
        let firstBundle = try VMBundle(url: linux).lock()
        t.refuses("a running bundle refused", containing: "already running") { _ = try VMBundle(url: linux).lock() }
        close(firstBundle.fd)
        let fakeState = scratch.url.appendingPathComponent("state-link")
        try FileManager.default.createSymbolicLink(at: fakeState, withDestinationURL: state)
        t.refuses("a symlinked state dir refused") { _ = try slot.lock(stateDir: fakeState) }
        try FileManager.default.createSymbolicLink(at: state.appendingPathComponent("slot-6.lock"), withDestinationURL: outside)
        t.refuses("a symlinked slot lock refused") { _ = try Slot(6).lock(stateDir: state) }

        // The console log is opened without following a symlink.
        let logLink = scratch.url.appendingPathComponent("log-link")
        try FileManager.default.createSymbolicLink(at: logLink, withDestinationURL: outside)
        t.refuses("a symlinked console log refused", containing: "symlink") { _ = try ConsoleLog.open(logLink, create: true) }
        let absentLog = scratch.url.appendingPathComponent("absent.log")
        t.accepts("validate's console log never creates the file") {
            let log = try ConsoleLog.open(absentLog, create: false)
            defer { log.removeScratch() }
            guard !pathExists(absentLog), log.scratch != nil else { throw Failure("validate created \(absentLog.path)") }
        }
    } catch {
        t.check("bundle fixtures", false, "\(error)")
    }
}

@MainActor
func testEntitlementsFile(_ t: Harness, path: String?) {
    guard let path else {
        t.skip("entitlements file", "no path given (test.sh passes launcher/faber-vm.entitlements)")
        return
    }
    guard let data = FileManager.default.contents(atPath: path), let dict = plist(data) else {
        t.check("entitlements file is a plist dictionary", false, path)
        return
    }
    t.check("entitlements hold exactly one key", dict.keys.sorted() == ["com.apple.security.virtualization"],
            dict.keys.sorted().joined(separator: ","))
    t.check("the virtualization entitlement is true", (dict["com.apple.security.virtualization"] as? Bool) == true)
    t.check("no bridged-networking entitlement", dict["com.apple.vm.networking"] == nil)
}

@MainActor
func testVersion(_ t: Harness) {
    let pattern = #"^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$"#
    t.check("the stamped version is a semver with optional build metadata",
            faberVMVersion.range(of: pattern, options: .regularExpression) != nil, faberVMVersion)
}
