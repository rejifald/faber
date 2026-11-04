// The wall: it accepts exactly what the launcher builds, and refuses each hostile change.
// Building a configuration needs no VM support, so these run on CI's macOS runners.

import Darwin
import Foundation
import Virtualization
import vmnet

/// A Linux bundle in a temporary directory: a sparse disk.raw, an EFI store file and a
/// machine identifier, so LinuxIdentity loads them as run would, without the store-creating API.
@MainActor
func linuxFixture(_ scratch: Scratch, _ name: String, seed: Bool = false) throws -> VMBundle {
    let dir = try scratch.dir(name)
    let bundle = VMBundle(url: dir)
    try scratch.sparse(bundle.rawDisk)
    try scratch.write(bundle.variableStoreFile)
    try scratch.write(bundle.machineIDFile, VZGenericMachineIdentifier().dataRepresentation)
    if seed { try scratch.write(bundle.seed, Data(repeating: 0, count: 4096)) }
    return bundle
}

@MainActor
func sharedNATOptions(slot: Int = 1) throws -> RunOptions {
    try RunOptions(parse(["--slot", String(slot), "--network", "shared-nat", "--cpus", "2", "--memory-gib", "2"],
                         valued: RunOptions.valued), forRun: false)
}

@MainActor
func testWallLinux(_ t: Harness, _ scratch: Scratch) {
    let bundle: VMBundle
    let seeded: VMBundle
    let identity: LinuxIdentity
    let seededIdentity: LinuxIdentity
    let options: RunOptions
    let other: URL
    do {
        bundle = try linuxFixture(scratch, "linux")
        seeded = try linuxFixture(scratch, "linux-seeded", seed: true)
        identity = try LinuxIdentity.load(bundle, create: false)
        seededIdentity = try LinuxIdentity.load(seeded, create: false)
        options = try sharedNATOptions()
        other = try scratch.sparse(scratch.url.appendingPathComponent("other.raw"))
    } catch {
        t.check("Linux fixture", false, "\(error)")
        return
    }

    func build(_ b: VMBundle = bundle, _ id: LinuxIdentity? = nil, log: ConsoleLog? = nil)
        throws -> (VZVirtualMachineConfiguration, WallExpectation) {
        try linuxConfiguration(b, options, network: .sharedNAT, identity: id ?? identity, log: log)
    }

    t.accepts("the wall accepts the Linux configuration the launcher builds") {
        let (config, expect) = try build()
        try checkWall(config, expect)
    }
    t.accepts("the wall accepts a Linux configuration with its read-only seed") {
        let (config, expect) = try build(seeded, seededIdentity)
        guard config.storageDevices.count == 2 else { throw Failure("seed not attached") }
        try checkWall(config, expect)
    }
    t.accepts("the base configuration sets every device as the ADR lists it") {
        let (c, _) = try build()
        guard c.networkDevices.count == 1, c.socketDevices.count == 1, c.entropyDevices.count == 1,
              c.memoryBalloonDevices.count == 1, c.storageDevices.count == 1, c.directorySharingDevices.isEmpty,
              c.audioDevices.isEmpty, c.consoleDevices.isEmpty, c.usbControllers.isEmpty, c.graphicsDevices.isEmpty,
              c.keyboards.isEmpty, c.pointingDevices.isEmpty, c.serialPorts.isEmpty else {
            throw Failure("unexpected device counts")
        }
        guard let disk = c.storageDevices[0].attachment as? VZDiskImageStorageDeviceAttachment,
              disk.synchronizationMode == .full, !disk.isReadOnly else { throw Failure("disk.raw not fully synchronized") }
        guard let nic = c.networkDevices[0] as? VZVirtioNetworkDeviceConfiguration,
              nic.macAddress.string == options.slot.macText else { throw Failure("NIC MAC is not the slot's") }
    }

    let logURL = scratch.url.appendingPathComponent("console.log")
    t.accepts("the wall accepts a serial log") {
        let log = try ConsoleLog.open(logURL, create: true)
        let (config, expect) = try build(log: log)
        try checkWall(config, expect)
    }

    /// Build fresh, apply one hostile change, and require checkWall to refuse it.
    func hostile(_ name: String, containing: String? = nil, b: VMBundle? = nil, id: LinuxIdentity? = nil,
                 withLog: Bool = false, _ mutate: (VZVirtualMachineConfiguration) throws -> Void) {
        t.refuses("wall refuses: \(name)", containing: containing) {
            let log = withLog ? try ConsoleLog.open(logURL, create: true) : nil
            let (config, expect) = try build(b ?? bundle, id, log: log)
            try mutate(config)
            try checkWall(config, expect)
        }
    }

    hostile("a second NIC", containing: "one virtio network device") { c in
        let nic = VZVirtioNetworkDeviceConfiguration()
        nic.attachment = VZNATNetworkDeviceAttachment()
        c.networkDevices.append(nic)
    }
    hostile("no NIC", containing: "one virtio network device") { c in c.networkDevices = [] }
    hostile("a NIC on a file handle (a user-mode stack could reach host loopback)", containing: "shared NAT") { c in
        var fds: [Int32] = [-1, -1]
        guard socketpair(AF_UNIX, SOCK_DGRAM, 0, &fds) == 0 else { throw Failure("socketpair: \(errnoText(errno))") }
        close(fds[1])
        let handle = FileHandle(fileDescriptor: fds[0], closeOnDealloc: true)
        (c.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).attachment =
            VZFileHandleNetworkDeviceAttachment(fileHandle: handle)
    }
    hostile("a NIC with no attachment", containing: "shared NAT") { c in
        (c.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).attachment = nil
    }
    hostile("a random MAC", containing: "MAC") { c in
        (c.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).macAddress = .randomLocallyAdministered()
    }
    hostile("another slot's MAC", containing: "MAC") { c in
        (c.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).macAddress = try Slot(2).mac
    }
    hostile("a directory share", containing: "directory sharing") { c in
        let share = VZVirtioFileSystemDeviceConfiguration(tag: "share")
        share.share = VZSingleDirectoryShare(directory: VZSharedDirectory(url: scratch.url, readOnly: true))
        c.directorySharingDevices = [share]
    }
    hostile("audio", containing: "audio") { c in c.audioDevices = [VZVirtioSoundDeviceConfiguration()] }
    hostile("a USB controller", containing: "USB") { c in c.usbControllers = [VZXHCIControllerConfiguration()] }
    hostile("a console device", containing: "console device") { c in
        c.consoleDevices = [VZVirtioConsoleDeviceConfiguration()]
    }
    hostile("a serial port nobody asked for", containing: "serial port") { c in
        let port = VZVirtioConsoleDeviceSerialPortConfiguration()
        port.attachment = try VZFileSerialPortAttachment(url: logURL, append: true)
        c.serialPorts = [port]
    }
    hostile("two serial ports", containing: "more than one serial port", withLog: true) { c in
        let port = VZVirtioConsoleDeviceSerialPortConfiguration()
        port.attachment = c.serialPorts[0].attachment
        c.serialPorts.append(port)
    }
    hostile("serial to a path attachment the framework opens itself", containing: "console pipe", withLog: true) { c in
        c.serialPorts[0].attachment = try VZFileSerialPortAttachment(url: logURL, append: true)
    }
    hostile("serial to a pipe", containing: "console pipe", withLog: true) { c in
        let pipe = Pipe()
        c.serialPorts[0].attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: nil, fileHandleForWriting: pipe.fileHandleForWriting)
    }
    hostile("serial that also reads from the host", containing: "console pipe", withLog: true) { c in
        let ours = (c.serialPorts[0].attachment as! VZFileHandleSerialPortAttachment).fileHandleForWriting
        c.serialPorts[0].attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: FileHandle(forReadingAtPath: "/dev/null"), fileHandleForWriting: ours)
    }
    hostile("serial straight into a log file, uncapped", containing: "console pipe", withLog: true) { c in
        let elsewhere = try ConsoleLog.open(scratch.url.appendingPathComponent("other.log"), create: true)
        c.serialPorts[0].attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: nil, fileHandleForWriting: elsewhere.log)
    }
    hostile("an extra disk", containing: "nothing else") { c in
        let extra = try VZDiskImageStorageDeviceAttachment(url: other, readOnly: false)
        c.storageDevices.append(VZVirtioBlockDeviceConfiguration(attachment: extra))
    }
    hostile("no disk", containing: "nothing else") { c in c.storageDevices = [] }
    hostile("the disk swapped for another file", containing: "unexpected disk image") { c in
        let swapped = try VZDiskImageStorageDeviceAttachment(url: other, readOnly: false, cachingMode: .cached,
                                                              synchronizationMode: .full)
        c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: swapped)]
    }
    hostile("the disk made read-only", containing: "writable") { c in
        let ro = try VZDiskImageStorageDeviceAttachment(url: bundle.rawDisk, readOnly: true, cachingMode: .cached,
                                                         synchronizationMode: .full)
        c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: ro)]
    }
    for mode in [VZDiskImageSynchronizationMode.fsync, .none] {
        hostile("disk.raw with \(syncName(mode)) synchronization", containing: "full synchronization") { c in
            let weak = try VZDiskImageStorageDeviceAttachment(url: bundle.rawDisk, readOnly: false, cachingMode: .cached,
                                                               synchronizationMode: mode)
            c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: weak)]
        }
    }
    hostile("the disk on USB mass storage", containing: "virtio block") { c in
        let disk = try VZDiskImageStorageDeviceAttachment(url: bundle.rawDisk, readOnly: false, cachingMode: .cached,
                                                           synchronizationMode: .full)
        c.storageDevices = [VZUSBMassStorageDeviceConfiguration(attachment: disk)]
    }
    hostile("the seed made writable", containing: "read-only", b: seeded, id: seededIdentity) { c in
        let seed = try VZDiskImageStorageDeviceAttachment(url: seeded.seed, readOnly: false)
        c.storageDevices[1] = VZVirtioBlockDeviceConfiguration(attachment: seed)
    }
    hostile("the seed dropped while the bundle has one", containing: "nothing else", b: seeded, id: seededIdentity) { c in
        c.storageDevices = [c.storageDevices[0]]
    }
    hostile("nested virtualization on", containing: "nested") { c in
        (c.platform as! VZGenericPlatformConfiguration).isNestedVirtualizationEnabled = true
    }
    hostile("the Mac platform on a Linux bundle", containing: "generic platform") { c in
        c.platform = VZMacPlatformConfiguration()
    }
    hostile("another EFI variable store", containing: "variable store") { c in
        let store = VZEFIVariableStore(url: try scratch.write(scratch.url.appendingPathComponent("evil-efi-vars")))
        (c.bootLoader as! VZEFIBootLoader).variableStore = store
    }
    hostile("a Linux kernel boot loader", containing: "EFI") { c in
        c.bootLoader = VZLinuxBootLoader(kernelURL: other)
    }
    hostile("graphics on Linux", containing: "no display") { c in
        let graphics = VZVirtioGraphicsDeviceConfiguration()
        graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 800, heightInPixels: 600)]
        c.graphicsDevices = [graphics]
    }
    hostile("a keyboard on Linux", containing: "no display") { c in c.keyboards = [VZUSBKeyboardConfiguration()] }
    hostile("a pointing device on Linux", containing: "no display") { c in
        c.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]
    }
    hostile("no vsock", containing: "socket") { c in c.socketDevices = [] }
    hostile("two vsock devices", containing: "socket") { c in
        c.socketDevices = [VZVirtioSocketDeviceConfiguration(), VZVirtioSocketDeviceConfiguration()]
    }
    hostile("no memory balloon", containing: "balloon") { c in c.memoryBalloonDevices = [] }
    hostile("two memory balloons", containing: "balloon") { c in
        c.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration(),
                                  VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]
    }
    hostile("no entropy device", containing: "entropy") { c in c.entropyDevices = [] }
    hostile("two entropy devices", containing: "entropy") { c in
        c.entropyDevices = [VZVirtioEntropyDeviceConfiguration(), VZVirtioEntropyDeviceConfiguration()]
    }
    if #available(macOS 27, *) {
        hostile("a custom virtio device", containing: "custom virtio") { c in
            c.customVirtioDevices = [VZCustomVirtioDeviceConfiguration()]
        }
    } else {
        t.skip("wall refuses: a custom virtio device", "custom virtio devices exist from macOS 27, this host runs \(macOSVersion)")
    }

    t.refuses("wall refuses: a macOS expectation against a Linux configuration", containing: "macOS platform") {
        let (config, expect) = try build()
        try checkWall(config, WallExpectation(kind: .macOS, nic: expect.nic, mac: expect.mac, disk: expect.disk,
                                              seed: nil, tools: nil, variableStore: nil, auxiliaryStorage: expect.disk,
                                              serialPipe: nil))
    }
    t.refuses("wall refuses: a vmnet expectation against a shared-NAT attachment", containing: "vmnet") {
        let (config, expect) = try build()
        // Never dereferenced: the attachment's class is refused before the pointer is compared.
        let notANetwork = OpaquePointer(bitPattern: 0x1)!
        try checkWall(config, WallExpectation(kind: .linux, nic: .vmnet(notANetwork), mac: expect.mac, disk: expect.disk,
                                              seed: nil, tools: nil, variableStore: expect.variableStore, auxiliaryStorage: nil,
                                              serialPipe: nil))
    }
    t.refuses("the builder refuses a symlinked disk.raw", containing: "not a regular file") {
        let linked = try scratch.dir("linux-linked")
        let b = VMBundle(url: linked)
        try FileManager.default.createSymbolicLink(at: b.rawDisk, withDestinationURL: other)
        try scratch.write(b.variableStoreFile)
        _ = try linuxConfiguration(b, options, network: .sharedNAT, identity: try LinuxIdentity.load(b, create: false), log: nil)
    }
}

@MainActor
func testWallMac(_ t: Harness, _ scratch: Scratch) {
    // A real macOS bundle needs a hardware model from a restore image, which CI does not have;
    // the platform here has none, which the wall does not look at. Everything else is real.
    let disk: URL, aux: URL, otherAux: URL, options: RunOptions
    do {
        let dir = try scratch.dir("macos")
        disk = try scratch.sparse(dir.appendingPathComponent("disk"))
        aux = try scratch.write(dir.appendingPathComponent("vz-aux"))
        otherAux = try scratch.write(scratch.url.appendingPathComponent("other-aux"))
        options = try sharedNATOptions(slot: 7)
    } catch {
        t.check("macOS fixture", false, "\(error)")
        return
    }
    func build() throws -> (VZVirtualMachineConfiguration, WallExpectation) {
        try macConfiguration(platform: VZMacPlatformConfiguration(), disk: disk, aux: aux, options,
                             network: .sharedNAT, log: nil)
    }
    t.accepts("the wall accepts the macOS configuration the launcher builds") {
        let (config, expect) = try build()
        guard let attachment = config.storageDevices.first?.attachment as? VZDiskImageStorageDeviceAttachment,
              attachment.synchronizationMode == .none else { throw Failure("a clone's disk must use no synchronization") }
        try checkWall(config, expect)
    }
    func hostile(_ name: String, containing: String? = nil, _ mutate: (VZVirtualMachineConfiguration) throws -> Void) {
        t.refuses("wall refuses (macOS): \(name)", containing: containing) {
            let (config, expect) = try build()
            try mutate(config)
            try checkWall(config, expect)
        }
    }
    hostile("a fully synchronized clone disk", containing: "none synchronization") { c in
        let a = try VZDiskImageStorageDeviceAttachment(url: disk, readOnly: false, cachingMode: .cached, synchronizationMode: .full)
        c.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: a)]
    }
    hostile("a second disk", containing: "exactly one disk") { c in
        c.storageDevices.append(c.storageDevices[0])
    }
    hostile("two displays", containing: "Mac display") { c in
        (c.graphicsDevices[0] as! VZMacGraphicsDeviceConfiguration).displays.append(
            VZMacGraphicsDisplayConfiguration(widthInPixels: 800, heightInPixels: 600, pixelsPerInch: 80))
    }
    hostile("no trackpad", containing: "trackpad") { c in c.pointingDevices = [] }
    hostile("no keyboard", containing: "keyboard") { c in c.keyboards = [] }
    hostile("another auxiliary storage", containing: "auxiliary") { c in
        (c.platform as! VZMacPlatformConfiguration).auxiliaryStorage = VZMacAuxiliaryStorage(url: otherAux)
    }
    hostile("the generic platform", containing: "macOS platform") { c in c.platform = VZGenericPlatformConfiguration() }
    hostile("audio", containing: "audio") { c in c.audioDevices = [VZVirtioSoundDeviceConfiguration()] }
    t.refuses("wall refuses: a Linux expectation against a macOS configuration", containing: "generic platform") {
        let (config, expect) = try build()
        try checkWall(config, WallExpectation(kind: .linux, nic: expect.nic, mac: expect.mac, disk: expect.disk,
                                              seed: nil, tools: nil, variableStore: expect.auxiliaryStorage, auxiliaryStorage: nil,
                                              serialPipe: nil))
    }
}

/// validate --wall-only, end to end through prepare(): it must accept, and write nothing into
/// the bundle.
@MainActor
func testValidateWallOnly(_ t: Harness, _ scratch: Scratch) {
    do {
        let bundle = try linuxFixture(scratch, "validate")
        let before = try FileManager.default.contentsOfDirectory(atPath: bundle.url.path).sorted()
        let prepared = try prepare([bundle.url.path, "--slot", "3", "--network", "shared-nat",
                                    "--cpus", "2", "--memory-gib", "2", "--wall-only"], forRun: false)
        prepared.cleanUp()
        let after = try FileManager.default.contentsOfDirectory(atPath: bundle.url.path).sorted()
        t.check("validate --wall-only writes nothing into the bundle", before == after, "\(before) -> \(after)")
        t.check("validate --wall-only prints the shared-NAT line",
                prepared.networkLine.contains("mode=shared-nat") && prepared.networkLine.contains("mac=02:fa:be:00:00:03"))
    } catch {
        t.check("validate --wall-only on a Linux fixture", false, "\(error)")
    }

    // A bundle with no efi-vars or machine-id yet: validate uses throwaway ones.
    do {
        let dir = try scratch.dir("validate-fresh")
        let bundle = VMBundle(url: dir)
        try scratch.sparse(bundle.rawDisk)
        let prepared: Prepared
        do {
            prepared = try prepare([dir.path, "--slot", "3", "--network", "shared-nat", "--cpus", "2",
                                    "--memory-gib", "2", "--wall-only"], forRun: false)
        } catch {
            t.skip("validate --wall-only on a fresh Linux bundle", "cannot create a throwaway EFI store here: \(error)")
            return
        }
        let scratchDir = prepared.identity?.scratch
        prepared.cleanUp()
        t.check("validate creates neither efi-vars nor machine-id",
                !pathExists(bundle.variableStoreFile) && !pathExists(bundle.machineIDFile))
        t.check("validate removes its throwaway EFI store", scratchDir.map { !pathExists($0) } ?? false)
    } catch {
        t.check("fresh Linux fixture", false, "\(error)")
    }
}

/// Opt-in: these create vmnet networks, a host-side change (InternetSharing), so they run only
/// with FABER_VM_TEST_VMNET=1, on macOS 26 or later.
@MainActor
func testWallVmnet(_ t: Harness, _ scratch: Scratch) {
    let name = "wall on a vmnet network"
    guard ProcessInfo.processInfo.environment["FABER_VM_TEST_VMNET"] == "1" else {
        t.skip(name, "creates vmnet networks; set FABER_VM_TEST_VMNET=1 to run")
        return
    }
    guard #available(macOS 26, *) else {
        t.skip(name, "vmnet networks need macOS 26, this host runs \(macOSVersion)")
        return
    }
    do {
        let bundle = try linuxFixture(scratch, "linux-vmnet")
        let identity = try LinuxIdentity.load(bundle, create: false)
        let options = try RunOptions(parse(["--slot", "9", "--subnet", "192.168.100.0/24", "--cpus", "2", "--memory-gib", "2"],
                                           valued: RunOptions.valued), forRun: false)
        let mine: vmnet_network_ref
        let theirs: vmnet_network_ref
        do {
            mine = try createVmnetNetwork(options.vmnet!, mac: options.slot.mac)
            theirs = try createVmnetNetwork(VmnetPlan(subnet: IPv4Subnet.parse("192.168.101.0/24"), reservation: nil),
                                            mac: options.slot.mac)
        } catch {
            t.skip(name, "this host cannot create vmnet networks: \(error)")
            return
        }
        t.accepts("the wall accepts the NIC on this process's vmnet network") {
            let (config, expect) = try linuxConfiguration(bundle, options, network: .vmnet(mine), identity: identity, log: nil)
            try checkWall(config, expect)
        }
        t.refuses("the wall refuses the NIC on another vmnet network", containing: "vmnet") {
            let (config, expect) = try linuxConfiguration(bundle, options, network: .vmnet(mine), identity: identity, log: nil)
            (config.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).attachment =
                VZVmnetNetworkDeviceAttachment(network: theirs)
            try checkWall(config, expect)
        }
        t.refuses("the wall refuses shared NAT where vmnet was built", containing: "vmnet") {
            let (config, expect) = try linuxConfiguration(bundle, options, network: .vmnet(mine), identity: identity, log: nil)
            (config.networkDevices[0] as! VZVirtioNetworkDeviceConfiguration).attachment = VZNATNetworkDeviceAttachment()
            try checkWall(config, expect)
        }
    } catch {
        t.check(name, false, "\(error)")
    }
}
