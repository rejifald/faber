// The wall (ADR-0026, "The wall is built in, not configured"): a fail-closed allow-list of
// devices, by count and by exact class. The launcher builds a configuration, then checks it
// here against what it meant to build; anything extra, missing, or of another class refuses
// the VM before the framework is ever asked to validate or start it.

import Darwin
import Foundation
import ObjectiveC
import Virtualization
import vmnet

/// The network the launcher built the VM's one NIC for.
enum NetworkChoice {
    /// Apple's shared NAT.
    case sharedNAT
    /// A vmnet network this process created (createVmnetNetwork).
    case vmnet(vmnet_network_ref)

    var mode: NetworkMode {
        switch self {
        case .sharedNAT: .sharedNAT
        case .vmnet: .vmnet
        }
    }

    func attachment() throws -> VZNetworkDeviceAttachment {
        switch self {
        case .sharedNAT:
            return VZNATNetworkDeviceAttachment()
        case .vmnet(let network):
            guard #available(macOS 26, *) else { throw Failure("--network vmnet needs macOS 26 or later") }
            return VZVmnetNetworkDeviceAttachment(network: network)
        }
    }
}

/// What the launcher built, which checkWall holds the configuration to.
struct WallExpectation {
    let kind: BundleKind
    let nic: NetworkChoice
    let mac: [UInt8]
    let disk: FileRef
    /// Linux: the seed, while the bundle has one.
    let seed: FileRef?
    /// macOS: the read-only tools disk (runner and guest agent), when one was given.
    let tools: FileRef?
    /// Linux: the EFI variable store.
    let variableStore: FileRef?
    /// macOS: the auxiliary storage.
    let auxiliaryStorage: FileRef?
    /// The write end of the console pipe, when a serial log was asked for.
    let serialPipe: FileID?

    /// Every file the framework opens by path, which run checks again once the VM is running.
    var attachedFiles: [FileRef] { [disk] + [seed, tools, variableStore, auxiliaryStorage].compactMap { $0 } }
}

/// Exactly this class: a subclass is a different device, so `is` would be too generous.
func isExactly(_ object: AnyObject?, _ cls: AnyClass) -> Bool {
    guard let object else { return false }
    return object_getClass(object) == cls
}

func checkWall(_ config: VZVirtualMachineConfiguration, _ expect: WallExpectation) throws {
    func refuse(_ why: String) -> Failure { Failure("wall: \(why)") }

    // One virtio NIC, on the network the launcher made, with the slot's MAC.
    guard config.networkDevices.count == 1,
          let nic = config.networkDevices.first as? VZVirtioNetworkDeviceConfiguration,
          isExactly(nic, VZVirtioNetworkDeviceConfiguration.self) else {
        throw refuse("expected exactly one virtio network device")
    }
    switch expect.nic {
    case .sharedNAT:
        guard isExactly(nic.attachment, VZNATNetworkDeviceAttachment.self) else {
            throw refuse("the network device is not attached to Apple's shared NAT")
        }
    case .vmnet(let network):
        guard #available(macOS 26, *),
              isExactly(nic.attachment, VZVmnetNetworkDeviceAttachment.self),
              let vmnet = nic.attachment as? VZVmnetNetworkDeviceAttachment,
              vmnet.network == network else {
            throw refuse("the network device is not attached to the vmnet network this process created")
        }
    }
    guard macOctets(nic.macAddress) == expect.mac else {
        throw refuse("the network device's MAC \(nic.macAddress.string) is not the slot's")
    }

    // One vsock device, one entropy device, one traditional balloon.
    guard config.socketDevices.count == 1, isExactly(config.socketDevices.first, VZVirtioSocketDeviceConfiguration.self) else {
        throw refuse("expected exactly one virtio socket device")
    }
    guard config.entropyDevices.count == 1, isExactly(config.entropyDevices.first, VZVirtioEntropyDeviceConfiguration.self) else {
        throw refuse("expected exactly one virtio entropy device")
    }
    guard config.memoryBalloonDevices.count == 1,
          isExactly(config.memoryBalloonDevices.first, VZVirtioTraditionalMemoryBalloonDeviceConfiguration.self) else {
        throw refuse("expected exactly one traditional memory balloon")
    }

    // Nothing that carries data across the wall.
    guard config.directorySharingDevices.isEmpty else { throw refuse("directory sharing configured") }
    guard config.audioDevices.isEmpty else { throw refuse("audio configured") }
    guard config.consoleDevices.isEmpty else { throw refuse("console device (spice/clipboard) configured") }
    guard config.usbControllers.isEmpty else { throw refuse("USB controller configured") }
    if #available(macOS 27, *) {
        guard config.customVirtioDevices.isEmpty else { throw refuse("custom virtio device configured") }
    }

    // At most one serial port, write-only, into the log file the launcher opened.
    guard config.serialPorts.count <= (expect.serialPipe == nil ? 0 : 1) else {
        throw refuse(expect.serialPipe == nil ? "serial port configured" : "more than one serial port configured")
    }
    if let pipeID = expect.serialPipe {
        guard config.serialPorts.count == 1,
              isExactly(config.serialPorts[0], VZVirtioConsoleDeviceSerialPortConfiguration.self),
              let port = config.serialPorts[0].attachment as? VZFileHandleSerialPortAttachment,
              isExactly(port, VZFileHandleSerialPortAttachment.self),
              port.fileHandleForReading == nil,
              let writing = port.fileHandleForWriting,
              isFIFO(writing.fileDescriptor), FileID.of(fd: writing.fileDescriptor) == pipeID else {
            throw refuse("the serial port must write, and only write, into the launcher's console pipe")
        }
    }

    switch expect.kind {
    case .macOS:
        guard let platform = config.platform as? VZMacPlatformConfiguration,
              isExactly(platform, VZMacPlatformConfiguration.self),
              isExactly(config.bootLoader, VZMacOSBootLoader.self) else {
            throw refuse("a macOS bundle boots the macOS platform and boot loader only")
        }
        guard let aux = platform.auxiliaryStorage, let want = expect.auxiliaryStorage,
              FileID.ofRegularFile(path: aux.url.path) == want.id else {
            throw refuse("unexpected auxiliary storage")
        }
        let disks = expect.tools == nil ? 1 : 2
        guard config.storageDevices.count == disks else {
            throw refuse(expect.tools == nil ? "a macOS VM gets exactly one disk" : "a macOS VM gets its disk and its read-only tools disk and nothing else")
        }
        try checkDisk(config.storageDevices[0], expect.disk, readOnly: false, sync: VZDiskImageSynchronizationMode.none)
        if let tools = expect.tools {
            try checkDisk(config.storageDevices[1], tools, readOnly: true, sync: nil)
        }
        guard config.graphicsDevices.count == 1,
              let graphics = config.graphicsDevices.first as? VZMacGraphicsDeviceConfiguration,
              isExactly(graphics, VZMacGraphicsDeviceConfiguration.self), graphics.displays.count == 1,
              config.keyboards.count == 1, isExactly(config.keyboards.first, VZMacKeyboardConfiguration.self),
              config.pointingDevices.count == 1, isExactly(config.pointingDevices.first, VZMacTrackpadConfiguration.self) else {
            throw refuse("a macOS VM gets exactly one Mac display, keyboard and trackpad")
        }
    case .linux:
        guard let platform = config.platform as? VZGenericPlatformConfiguration,
              isExactly(platform, VZGenericPlatformConfiguration.self),
              !platform.isNestedVirtualizationEnabled else {
            throw refuse("a Linux bundle boots the generic platform, without nested virtualization")
        }
        guard let bootLoader = config.bootLoader as? VZEFIBootLoader, isExactly(bootLoader, VZEFIBootLoader.self),
              let store = bootLoader.variableStore, let want = expect.variableStore,
              FileID.ofRegularFile(path: store.url.path) == want.id else {
            throw refuse("a Linux bundle boots by EFI, with its own variable store")
        }
        let disks = expect.seed == nil ? 1 : 2
        guard config.storageDevices.count == disks else {
            throw refuse("a Linux VM gets its disk\(expect.seed == nil ? "" : " and its seed") and nothing else")
        }
        try checkDisk(config.storageDevices[0], expect.disk, readOnly: false, sync: .full)
        if let seed = expect.seed {
            try checkDisk(config.storageDevices[1], seed, readOnly: true, sync: nil)
        }
        guard config.graphicsDevices.isEmpty, config.keyboards.isEmpty, config.pointingDevices.isEmpty else {
            throw refuse("a Linux VM gets no display, keyboard or pointing device")
        }
    }
}

/// A virtio block device on a disk image that is the expected file, by device and inode,
/// with the expected read-only flag and, when given, synchronization mode.
private func checkDisk(_ device: VZStorageDeviceConfiguration, _ want: FileRef, readOnly: Bool,
                       sync: VZDiskImageSynchronizationMode?) throws {
    guard isExactly(device, VZVirtioBlockDeviceConfiguration.self),
          let attachment = device.attachment as? VZDiskImageStorageDeviceAttachment,
          isExactly(attachment, VZDiskImageStorageDeviceAttachment.self) else {
        throw Failure("wall: storage that is not a virtio block device on a disk image")
    }
    let name = attachment.url.lastPathComponent
    guard FileID.ofRegularFile(path: attachment.url.path) == want.id else {
        throw Failure("wall: unexpected disk image \(attachment.url.path)")
    }
    guard attachment.isReadOnly == readOnly else {
        throw Failure("wall: \(name) must be \(readOnly ? "read-only" : "writable")")
    }
    if let sync, attachment.synchronizationMode != sync {
        throw Failure("wall: \(name) must use \(syncName(sync)) synchronization")
    }
}

func isFIFO(_ fd: Int32) -> Bool {
    var st = stat()
    return fstat(fd, &st) == 0 && st.st_mode & S_IFMT == S_IFIFO
}
