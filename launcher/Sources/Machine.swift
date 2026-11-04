// The VM configuration: the options run and validate share, and the builders that turn a
// bundle into exactly the devices ADR-0026 allows. checkWall (Wall.swift) refuses anything else.

import Darwin
import Foundation
import Virtualization

// MARK: - Options

struct RunOptions {
    var slot: Slot
    var stateDir: URL?
    var network: NetworkMode = .vmnet
    var vmnet: VmnetPlan?
    var cpus = 4
    var memoryGiB = 8.0
    /// --memory-mib, the unit the supervisor protocol speaks; it and --memory-gib exclude each other.
    var memoryMiB: Int?
    var consoleLog: URL?
    var toolsDisk: URL?
    /// `--tools-owner`: the supervisor's peer, whose directory may hold the tools disk.
    var toolsOwner: uid_t?
    var consoleLogMaxMiB = defaultConsoleLogMaxMiB
    var cachingMode: VZDiskImageCachingMode = .cached
    var wallOnly = false
    /// The supervisor's child: the claim this VM carries, the socket it serves the child
    /// protocol on (README.md), and the slot lock the supervisor took and handed down.
    var claim: String?
    var serveSocket: URL?
    var slotLockFD: Int32?
    /// The Linux child's Docker bridge (DockerBridge.swift): the host socket `begin` opens, the
    /// one uid it admits, and the guest's vsock ports for Docker's relay and the wall probe.
    var dockerSocket: URL?
    var dockerPeerUID: uid_t?
    var dockerVsockPort: UInt32 = 2375
    var probeVsockPort: UInt32 = 6101
    var dockerVsockPortGiven = false
    var probeVsockPortGiven = false
    /// How long a macOS child waits for its guest agent to report ready once the VM runs.
    var agentReadyTimeout: TimeInterval = 600

    static let valued: Set<String> = ["--slot", "--state-dir", "--network", "--subnet", "--reserve-ip",
                                      "--cpus", "--memory-gib", "--memory-mib", "--console-log", "--console-log-max-mib",
                                      "--disk-cache", "--tools-disk", "--tools-owner", "--claim", "--serve-socket", "--slot-lock-fd",
                                      "--agent-ready-timeout",
                                      "--docker-socket", "--docker-peer-uid", "--docker-vsock-port", "--probe-vsock-port"]

    /// `forRun` makes --state-dir required and --wall-only unknown; validate takes the same
    /// options as run, so a caller can validate the exact command line it is about to run.
    init(_ args: Arguments, forRun: Bool) throws {
        guard let slotText = args.value("--slot") else { throw UsageError("--slot N is required") }
        slot = try Slot.parse(slotText)

        if let dir = args.value("--state-dir") {
            stateDir = absoluteURL(dir)
        } else if forRun {
            throw UsageError("--state-dir DIR is required: run holds DIR/slot-N.lock while the VM lives")
        }

        if let text = args.value("--network") { network = try NetworkMode.parse(text) }
        let reservation: IPv4?
        if let text = args.value("--reserve-ip") {
            guard let address = IPv4.parse(text) else { throw UsageError("--reserve-ip: not an IPv4 address: \(text)") }
            reservation = address
        } else {
            reservation = nil
        }
        switch network {
        case .vmnet:
            guard let text = args.value("--subnet") else {
                throw UsageError("--network vmnet needs --subnet CIDR, a private IPv4 subnet for this VM alone")
            }
            vmnet = try VmnetPlan(subnet: IPv4Subnet.parse(text), reservation: reservation)
        case .sharedNAT:
            if args.has("--subnet") || args.has("--reserve-ip") {
                throw UsageError("--subnet and --reserve-ip are for --network vmnet; Apple's shared NAT picks its own")
            }
        }

        if let v = args.value("--cpus") {
            guard let n = Int(v), n > 0 else { throw UsageError("--cpus: not a positive integer: \(v)") }
            cpus = n
        }
        if let v = args.value("--memory-gib") {
            guard let g = Double(v), g.isFinite, g > 0 else { throw UsageError("--memory-gib: not a positive number: \(v)") }
            memoryGiB = g
        }
        if let v = args.value("--memory-mib") {
            guard !args.has("--memory-gib") else { throw UsageError("--memory-mib and --memory-gib exclude each other") }
            guard let n = Int(v), String(n) == v, n > 0 else { throw UsageError("--memory-mib: not a positive integer: \(v)") }
            memoryMiB = n
        }
        if let path = args.value("--console-log") { consoleLog = absoluteURL(path) }
        if let path = args.value("--tools-disk") { toolsDisk = absoluteURL(path) }
        // The supervisor's alone: its child gets its peer's uid with a tools disk, so a disk in
        // the directory faber builds them in passes (ADR-0026). A run by hand never needs it.
        if let v = args.value("--tools-owner") {
            guard toolsDisk != nil else { throw UsageError("--tools-owner needs --tools-disk") }
            guard args.has("--serve-socket") else { throw UsageError("--tools-owner is the supervisor's child's, and needs --serve-socket") }
            guard let uid = uid_t(v), String(uid) == v, uid != 0 else { throw UsageError("--tools-owner: not a non-root uid: \(v)") }
            toolsOwner = uid
        }
        if let v = args.value("--console-log-max-mib") {
            guard consoleLog != nil else { throw UsageError("--console-log-max-mib needs --console-log") }
            guard let n = Int(v), (1...65536).contains(n) else {
                throw UsageError("--console-log-max-mib: expected 1...65536, got \(v)")
            }
            consoleLogMaxMiB = n
        }
        switch args.value("--disk-cache") ?? "cached" {
        case "automatic": cachingMode = .automatic
        case "cached": cachingMode = .cached
        case "uncached": cachingMode = .uncached
        case let other: throw UsageError("--disk-cache: expected automatic|cached|uncached, got \(other)")
        }
        wallOnly = args.switches.contains("--wall-only")
        if let text = args.value("--claim") {
            guard isClaim(text) else { throw UsageError("--claim: not a claim id (1-64 of [A-Za-z0-9._-], first alphanumeric)") }
            claim = text
        }
        if let path = args.value("--serve-socket") {
            // The child serves only where the supervisor looks for it: <state-dir>/slot-N.sock.
            guard forRun, let stateDir, claim != nil else {
                throw UsageError("--serve-socket is run's, and needs --state-dir and --claim")
            }
            // Compared by their canonical spellings: `/var` and `/private/var` are one place.
            let want = canonicalURL(stateDir.appendingPathComponent("slot-\(slot.index).sock").path)
            guard canonicalURL(path) == want else { throw UsageError("--serve-socket must be \(want.path)") }
            serveSocket = want
        } else if claim != nil {
            throw UsageError("--claim needs --serve-socket")
        }
        if let v = args.value("--slot-lock-fd") {
            guard forRun, let n = Int32(v), String(n) == v, n > 2 else {
                throw UsageError("--slot-lock-fd: run's, and a descriptor above 2")
            }
            slotLockFD = n
        }
        // The bridge is opened by `begin`, which only the child protocol sends, so its options
        // are the child's, and the socket and its one peer come together.
        switch (args.value("--docker-socket"), args.value("--docker-peer-uid")) {
        case (nil, nil):
            break
        case (let path?, let uidText?):
            guard serveSocket != nil else { throw UsageError("--docker-socket is the supervisor's child's, and needs --serve-socket") }
            guard path.hasPrefix("/") else { throw UsageError("--docker-socket: expected an absolute path, got \(path)") }
            guard let uid = uid_t(uidText), String(uid) == uidText else { throw UsageError("--docker-peer-uid: not a uid: \(uidText)") }
            dockerSocket = absoluteURL(path)
            dockerPeerUID = uid
        default:
            throw UsageError("--docker-socket and --docker-peer-uid go together")
        }
        func vsockPort(_ name: String) throws -> UInt32? {
            guard let v = args.value(name) else { return nil }
            guard let n = UInt32(v), String(n) == v, n > 0, n < UInt32.max else {
                throw UsageError("\(name): expected a vsock port 1...\(UInt32.max - 1), got \(v)")
            }
            return n
        }
        if let n = try vsockPort("--docker-vsock-port") {
            guard dockerSocket != nil else { throw UsageError("--docker-vsock-port needs --docker-socket") }
            dockerVsockPort = n
            dockerVsockPortGiven = true
        }
        if let n = try vsockPort("--probe-vsock-port") {
            guard serveSocket != nil else { throw UsageError("--probe-vsock-port is the supervisor's child's, and needs --serve-socket") }
            probeVsockPort = n
            probeVsockPortGiven = true
        }
        if let v = args.value("--agent-ready-timeout") {
            guard let n = Int(v), String(n) == v, (1...86_400).contains(n) else {
                throw UsageError("--agent-ready-timeout: expected 1...86400 seconds, got \(v)")
            }
            agentReadyTimeout = TimeInterval(n)
        }
        if wallOnly, network == .vmnet {
            // The wall checks that the NIC is attached to the network this process created, and
            // --wall-only never creates one, so there is nothing true it could check.
            throw UsageError("--wall-only cannot check a vmnet attachment without creating the network; use --network shared-nat with --wall-only, or validate without --wall-only")
        }
    }

    var memoryBytes: UInt64 {
        if let memoryMiB { return UInt64(memoryMiB) << 20 }
        return UInt64((memoryGiB * 1024).rounded(.down)) << 20  // whole MiB
    }

    var cachingName: String {
        switch cachingMode {
        case .automatic: "automatic"
        case .cached: "cached"
        case .uncached: "uncached"
        @unknown default: "unknown(\(cachingMode.rawValue))"
        }
    }
}

func syncName(_ mode: VZDiskImageSynchronizationMode) -> String {
    switch mode {
    case .full: "full"
    case .fsync: "fsync"
    case .none: "none"
    @unknown default: "unknown(\(mode.rawValue))"
    }
}

// MARK: - The serial log

let defaultConsoleLogMaxMiB = 64

/// The guest's console output. The serial port writes into a pipe, never into the log file
/// itself, and a thread of the launcher's drains the pipe into the log, up to a cap: a guest
/// that floods its console cannot fill the host's disk, and the drain keeps reading past the
/// cap so the guest never blocks on a full pipe.
///
/// The log is opened O_NOFOLLOW (a symlink planted at the path is refused), O_APPEND and
/// O_NONBLOCK (a FIFO planted there is refused rather than waited on), and must be a regular
/// file this process owns with one link (a hard link to another file is refused). The port is
/// write-only, so nothing flows from the host into the guest.
final class ConsoleLog: @unchecked Sendable {
    let log: FileHandle
    let path: URL
    let scratch: URL?
    /// The pipe: the serial port gets `pipeWrite`; the drain reads `pipeRead`.
    let pipeRead: FileHandle
    let pipeWrite: FileHandle
    /// The pipe's write end, which the wall checks the serial port against.
    let pipeID: FileID
    let maxBytes: Int64
    private let startSize: Int64

    private init(log: FileHandle, path: URL, scratch: URL?, pipeRead: FileHandle, pipeWrite: FileHandle,
                 pipeID: FileID, maxBytes: Int64, startSize: Int64) {
        self.log = log
        self.path = path
        self.scratch = scratch
        self.pipeRead = pipeRead
        self.pipeWrite = pipeWrite
        self.pipeID = pipeID
        self.maxBytes = maxBytes
        self.startSize = startSize
    }

    /// `create`: run creates the log if absent. validate writes nothing: it opens an existing log
    /// without writing to it, or, without one, a throwaway file in a temporary directory.
    static func open(_ url: URL, create: Bool, maxMiB: Int = defaultConsoleLogMaxMiB) throws -> ConsoleLog {
        var flags = O_WRONLY | O_APPEND | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC
        if create { flags |= O_CREAT }
        var fd = Darwin.open(url.path, flags, 0o600)
        var scratch: URL?
        if fd < 0, errno == ENOENT, !create {
            let dir = FileManager.default.temporaryDirectory
                .appendingPathComponent("faber-vm-validate-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false,
                                                    attributes: [.posixPermissions: 0o700])
            scratch = dir
            fd = Darwin.open(dir.appendingPathComponent("console.log").path,
                             O_WRONLY | O_APPEND | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        }
        func fail(_ why: String) -> Failure {
            if fd >= 0 { Darwin.close(fd) }
            if let scratch { try? FileManager.default.removeItem(at: scratch) }
            return Failure("--console-log \(url.path): \(why)")
        }
        guard fd >= 0 else {
            let code = errno
            switch code {
            case ELOOP: throw fail("is a symlink; refusing to follow it")
            case ENXIO: throw fail("is a FIFO; refusing it")
            default: throw fail(errnoText(code))
            }
        }
        var st = stat()
        guard fstat(fd, &st) == 0 else { throw fail(errnoText(errno)) }
        do {
            try checkOwnedFile(st, euid: geteuid(), path: url.path)
        } catch {
            throw fail("\(error)")
        }
        _ = fcntl(fd, F_SETFL, O_WRONLY | O_APPEND)  // O_NONBLOCK was for the open only

        var ends: [Int32] = [-1, -1]
        guard pipe(&ends) == 0 else { throw fail("pipe: \(errnoText(errno))") }
        for end in ends { _ = fcntl(end, F_SETFD, FD_CLOEXEC) }
        guard let pipeID = FileID.of(fd: ends[1]) else {
            close(ends[0]); close(ends[1])
            throw fail("fstat pipe: \(errnoText(errno))")
        }
        return ConsoleLog(log: FileHandle(fileDescriptor: fd, closeOnDealloc: true), path: url, scratch: scratch,
                          pipeRead: FileHandle(fileDescriptor: ends[0], closeOnDealloc: true),
                          pipeWrite: FileHandle(fileDescriptor: ends[1], closeOnDealloc: true),
                          pipeID: pipeID, maxBytes: Int64(maxMiB) << 20, startSize: Int64(st.st_size))
    }

    /// run only: drain the pipe into the log on a thread of its own, for the life of the process.
    func startDraining() {
        let readFD = pipeRead.fileDescriptor
        let logFD = log.fileDescriptor
        let (start, cap, path) = (startSize, maxBytes, self.path.path)
        let thread = Thread {
            _ = drainConsole(from: readFD, to: logFD, size: start, maxBytes: cap) {
                say("event=console-log-capped max_mib=\(cap >> 20) path=\(path)")
            }
        }
        thread.name = "faber-vm console drain"
        thread.start()
    }

    func removeScratch() {
        if let scratch { try? FileManager.default.removeItem(at: scratch) }
    }
}

/// Copy `readFD` to `logFD` until end of file, while the log is under `maxBytes`; past the cap,
/// keep reading and discard, so the writer never blocks. `onCap` runs once, when the cap is
/// reached. Returns the log's size as this drain leaves it.
func drainConsole(from readFD: Int32, to logFD: Int32, size start: Int64, maxBytes: Int64,
                  onCap: () -> Void) -> Int64 {
    var size = start
    var capped = false
    if size >= maxBytes {
        capped = true
        onCap()
    }
    var buffer = [UInt8](repeating: 0, count: 64 << 10)
    while true {
        let n = buffer.withUnsafeMutableBytes { read(readFD, $0.baseAddress, $0.count) }
        if n < 0, errno == EINTR { continue }
        guard n > 0 else { return size }
        guard !capped else { continue }
        let allowed = Int(min(Int64(n), maxBytes - size))
        var offset = 0
        while offset < allowed {
            let w = buffer.withUnsafeBytes { write(logFD, $0.baseAddress! + offset, allowed - offset) }
            if w < 0, errno == EINTR { continue }
            guard w > 0 else { break }  // a log that cannot be written is dropped, never waited on
            offset += w
        }
        size += Int64(offset)
        if size >= maxBytes || offset < allowed {
            capped = true
            onCap()
        }
    }
}


// MARK: - Configuration

let displayWidth = 1920, displayHeight = 1080, displayPPI = 80

/// What every VM gets, whatever its guest: CPUs and memory, its one NIC with the slot's MAC,
/// one vsock device, entropy, a memory balloon, and optionally a serial port that writes to a
/// log file. Every other device list is set empty here, so each absence is deliberate rather
/// than a default someone can flip; the kind-specific builders add only their own devices.
func baseConfiguration(_ options: RunOptions, network: NetworkChoice,
                       log: ConsoleLog?) throws -> VZVirtualMachineConfiguration {
    let cpuRange = VZVirtualMachineConfiguration.minimumAllowedCPUCount...VZVirtualMachineConfiguration.maximumAllowedCPUCount
    guard cpuRange.contains(options.cpus) else {
        throw Failure("--cpus \(options.cpus) is outside the framework's allowed range \(cpuRange.lowerBound)...\(cpuRange.upperBound)")
    }
    let minMemory = VZVirtualMachineConfiguration.minimumAllowedMemorySize
    let maxMemory = VZVirtualMachineConfiguration.maximumAllowedMemorySize
    guard (minMemory...maxMemory).contains(options.memoryBytes) else {
        throw Failure("guest memory \(options.memoryBytes >> 20) MiB is outside the framework's allowed range \(minMemory >> 20)...\(maxMemory >> 20) MiB")
    }

    let config = VZVirtualMachineConfiguration()
    config.cpuCount = options.cpus
    config.memorySize = options.memoryBytes

    let nic = VZVirtioNetworkDeviceConfiguration()
    nic.attachment = try network.attachment()
    nic.macAddress = options.slot.mac
    config.networkDevices = [nic]
    config.socketDevices = [VZVirtioSocketDeviceConfiguration()]  // the control channel: the host connects to the guest agent, and listens on nothing
    config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
    config.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]

    config.storageDevices = []
    config.directorySharingDevices = []
    config.audioDevices = []
    config.consoleDevices = []  // a clipboard (spice) agent would live on a console port
    config.usbControllers = []
    config.graphicsDevices = []
    config.keyboards = []
    config.pointingDevices = []
    config.serialPorts = []
    if #available(macOS 27, *) {
        config.customVirtioDevices = []
        config.label = "faber-vm slot \(options.slot)"
    }

    if let log {
        let port = VZVirtioConsoleDeviceSerialPortConfiguration()
        port.attachment = VZFileHandleSerialPortAttachment(fileHandleForReading: nil, fileHandleForWriting: log.pipeWrite)
        config.serialPorts = [port]
    }
    return config
}

/// A macOS guest: the Mac platform and boot loader, its one disk with no synchronization (a
/// per-job clone is thrown away after one job), and the display, keyboard and trackpad its
/// desktop session needs. The platform comes in already loaded, so a test can build one
/// without a hardware model.
func macConfiguration(platform: VZMacPlatformConfiguration, disk: URL, aux: URL, _ options: RunOptions,
                      network: NetworkChoice, log: ConsoleLog?, tools: FileRef? = nil) throws -> (VZVirtualMachineConfiguration, WallExpectation) {
    let diskID = try regularFileRef(disk)
    let auxID = try regularFileRef(aux)
    let config = try baseConfiguration(options, network: network, log: log)

    platform.auxiliaryStorage = VZMacAuxiliaryStorage(url: aux)
    config.platform = platform
    config.bootLoader = VZMacOSBootLoader()

    let attachment = try VZDiskImageStorageDeviceAttachment(
        url: disk, readOnly: false, cachingMode: options.cachingMode, synchronizationMode: .none)
    var storage: [VZStorageDeviceConfiguration] = [VZVirtioBlockDeviceConfiguration(attachment: attachment)]
    if let tools {
        // The runner and guest agent, read-only: faber builds and caches it outside the bundle.
        let toolsDisk = try VZDiskImageStorageDeviceAttachment(url: tools.url, readOnly: true)
        storage.append(VZVirtioBlockDeviceConfiguration(attachment: toolsDisk))
    }
    config.storageDevices = storage

    let graphics = VZMacGraphicsDeviceConfiguration()
    graphics.displays = [VZMacGraphicsDisplayConfiguration(
        widthInPixels: displayWidth, heightInPixels: displayHeight, pixelsPerInch: displayPPI)]
    config.graphicsDevices = [graphics]
    config.keyboards = [VZMacKeyboardConfiguration()]
    config.pointingDevices = [VZMacTrackpadConfiguration()]

    let expect = WallExpectation(kind: .macOS, nic: network, mac: options.slot.octets,
                                 disk: diskID, seed: nil, tools: tools, variableStore: nil, auxiliaryStorage: auxID,
                                 serialPipe: log?.pipeID)
    return (config, expect)
}

func macConfiguration(_ bundle: VMBundle, _ options: RunOptions, network: NetworkChoice,
                      log: ConsoleLog?, tools: FileRef? = nil) throws -> (VZVirtualMachineConfiguration, WallExpectation) {
    let platform = VZMacPlatformConfiguration()
    platform.hardwareModel = try bundle.loadHardwareModel()
    platform.machineIdentifier = try bundle.loadIdentifier()
    return try macConfiguration(platform: platform, disk: bundle.disk, aux: bundle.aux, options, network: network, log: log, tools: tools)
}

/// The Linux guest: the generic platform without nested virtualization, EFI with its variable
/// store, disk.raw fully synchronized (it holds the only copy of the local-only runner image),
/// and the read-only seed while the bundle has one. No display, keyboard or pointing device.
func linuxConfiguration(_ bundle: VMBundle, _ options: RunOptions, network: NetworkChoice,
                        identity: LinuxIdentity, log: ConsoleLog?) throws -> (VZVirtualMachineConfiguration, WallExpectation) {
    let diskID = try regularFileRef(bundle.rawDisk)
    let seedID = pathExists(bundle.seed) ? try regularFileRef(bundle.seed) : nil
    let storeID = try regularFileRef(identity.variableStoreFile)
    let config = try baseConfiguration(options, network: network, log: log)

    let platform = VZGenericPlatformConfiguration()
    platform.machineIdentifier = identity.machineIdentifier
    platform.isNestedVirtualizationEnabled = false
    config.platform = platform
    let bootLoader = VZEFIBootLoader()
    bootLoader.variableStore = identity.variableStore
    config.bootLoader = bootLoader

    let disk = try VZDiskImageStorageDeviceAttachment(
        url: bundle.rawDisk, readOnly: false, cachingMode: options.cachingMode, synchronizationMode: .full)
    var storage: [VZStorageDeviceConfiguration] = [VZVirtioBlockDeviceConfiguration(attachment: disk)]
    if seedID != nil {
        let seed = try VZDiskImageStorageDeviceAttachment(url: bundle.seed, readOnly: true)
        storage.append(VZVirtioBlockDeviceConfiguration(attachment: seed))
    }
    config.storageDevices = storage

    let expect = WallExpectation(kind: .linux, nic: network, mac: options.slot.octets,
                                 disk: diskID, seed: seedID, tools: nil, variableStore: storeID, auxiliaryStorage: nil,
                                 serialPipe: log?.pipeID)
    return (config, expect)
}

// MARK: - Summary

func configLine(_ kind: BundleKind, _ options: RunOptions, _ config: VZVirtualMachineConfiguration) -> String {
    var line = "config kind=\(kind.rawValue) cpus=\(config.cpuCount) memory_mib=\(config.memorySize >> 20)"
        + " disks=\(config.storageDevices.count) disk_cache=\(options.cachingName)"
        + " disk_sync=\(kind == .linux ? "full" : "none")"
        + " nics=\(config.networkDevices.count) vsock=\(config.socketDevices.count)"
        + " entropy=\(config.entropyDevices.count) balloon=\(config.memoryBalloonDevices.count)"
        + " graphics=\(config.graphicsDevices.count) shares=0 audio=0 usb=0 console_devices=0"
        + " serial_log=\(options.consoleLog?.path ?? "none")"
    line += " tools_disk=\(options.toolsDisk?.path ?? "none")"
    if kind == .macOS { line += " display=\(displayWidth)x\(displayHeight)@\(displayPPI)ppi" }
    return line
}
