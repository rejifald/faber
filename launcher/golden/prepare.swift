// faber-golden-prep: the operator's tool for building a golden macOS image (ADR-0026, *The
// golden image*). launcher/golden/build.sh compiles it; `faber golden prepare` drives it; nobody else runs it.
//
// It does the two things the launcher deliberately cannot: install macOS from a restore image
// into a new bundle, and boot that bundle in a window so the operator can do the steps that
// need a desktop. It is never on a job's run path, so it adds neither a window nor an
// installer to faber-vm. Swift and Apple's frameworks only (ADR-0026, check 19).
//
//   latest                       ask Virtualization for the newest restore image this host
//                                supports; print its url, build and os version
//   verify <ipsw>                load a downloaded restore image and check this host can
//                                install it; print its build and os version
//   create <bundle> <ipsw> --disk-gib N [--cpus N] [--memory-gib N]
//                                make a new macOS bundle (disk, vz-aux, vz-hwmodel,
//                                vz-identifier; the launcher's layout) and install into it.
//                                <bundle> must not exist; on failure it is removed again
//   boot <bundle> [--staging IMG] [--cpus N] [--memory-gib N]
//                                boot the bundle in a window with NO network device at all, and
//                                IMG attached read-only as a second disk. Exits 0 once the guest
//                                shuts itself down. Closing the window or SIGTERM asks the
//                                guest to shut down; a second one forces it off (exit 3)
//
//   --version                    print the stamped version
//
// Every line it prints is key=value, so `faber golden prepare` can read it.

import AppKit
import Darwin
import Foundation
import Virtualization

setvbuf(stdout, nil, _IOLBF, 0)

func say(_ line: String) { print(line) }
func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("faber-golden-prep: \(message)\n".utf8))
    exit(1)
}
func describe(_ error: Error) -> String {
    let e = error as NSError
    return "\(e.domain) \(e.code): \(e.localizedDescription)"
}

// The display the launcher gives a macOS guest (launcher/Sources/Machine.swift), so the image
// is prepared at the resolution its clones will run at.
/// "unstamped" in the tree; build.sh replaces it in the copy it compiles.
let prepVersion = "unstamped"

let displayWidth = 1920, displayHeight = 1080, displayPPI = 80

struct Options {
    var positional: [String] = []
    var values: [String: String] = [:]
}

func parse(_ args: ArraySlice<String>, valued: Set<String>) -> Options {
    var options = Options()
    var it = args.makeIterator()
    while let arg = it.next() {
        if arg.hasPrefix("--") {
            guard valued.contains(arg) else { fail("unknown option \(arg)") }
            guard let value = it.next() else { fail("\(arg) needs a value") }
            options.values[arg] = value
        } else {
            options.positional.append(arg)
        }
    }
    return options
}

func intOption(_ options: Options, _ name: String, default fallback: Int, range: ClosedRange<Int>) -> Int {
    guard let text = options.values[name] else { return fallback }
    guard let n = Int(text), range.contains(n) else { fail("\(name) must be a whole number in \(range)") }
    return n
}

/// The bundle's files, by the names the launcher reads (launcher/Sources/Bundle.swift).
struct Bundle {
    let url: URL
    init(_ path: String) { url = URL(fileURLWithPath: path).standardizedFileURL }
    var disk: URL { url.appendingPathComponent("disk") }
    var aux: URL { url.appendingPathComponent("vz-aux") }
    var hardwareModel: URL { url.appendingPathComponent("vz-hwmodel") }
    var identifier: URL { url.appendingPathComponent("vz-identifier") }
}

/// The platform of an existing bundle: its hardware model, identifier and auxiliary storage.
func loadPlatform(_ bundle: Bundle) -> VZMacPlatformConfiguration {
    guard let modelData = try? Data(contentsOf: bundle.hardwareModel),
          let model = VZMacHardwareModel(dataRepresentation: modelData) else {
        fail("\(bundle.hardwareModel.path): not a VZMacHardwareModel")
    }
    guard model.isSupported else { fail("this host cannot run the bundle's hardware model") }
    guard let idData = try? Data(contentsOf: bundle.identifier),
          let identifier = VZMacMachineIdentifier(dataRepresentation: idData) else {
        fail("\(bundle.identifier.path): not a VZMacMachineIdentifier")
    }
    let platform = VZMacPlatformConfiguration()
    platform.hardwareModel = model
    platform.machineIdentifier = identifier
    platform.auxiliaryStorage = VZMacAuxiliaryStorage(url: bundle.aux)
    return platform
}

/// A preparation VM: the bundle's disk, an optional read-only staging disk, and the display,
/// keyboard and trackpad. No network device (ADR-0049: the window VM is outside the wall, so
/// it has nothing to reach), no shared folder, audio, USB or serial port.
func configuration(_ bundle: Bundle, platform: VZMacPlatformConfiguration, cpus: Int, memoryGiB: Int,
                   staging: URL?) -> VZVirtualMachineConfiguration {
    let config = VZVirtualMachineConfiguration()
    config.platform = platform
    config.bootLoader = VZMacOSBootLoader()
    config.cpuCount = cpus
    config.memorySize = UInt64(memoryGiB) << 30

    var storage: [VZStorageDeviceConfiguration] = []
    do {
        let disk = try VZDiskImageStorageDeviceAttachment(url: bundle.disk, readOnly: false)
        storage.append(VZVirtioBlockDeviceConfiguration(attachment: disk))
        if let staging {
            let image = try VZDiskImageStorageDeviceAttachment(url: staging, readOnly: true)
            storage.append(VZVirtioBlockDeviceConfiguration(attachment: image))
        }
    } catch {
        fail("cannot attach a disk: \(describe(error))")
    }
    config.storageDevices = storage

    config.networkDevices = []

    let graphics = VZMacGraphicsDeviceConfiguration()
    graphics.displays = [VZMacGraphicsDisplayConfiguration(
        widthInPixels: displayWidth, heightInPixels: displayHeight, pixelsPerInch: displayPPI)]
    config.graphicsDevices = [graphics]
    config.keyboards = [VZMacKeyboardConfiguration()]
    config.pointingDevices = [VZMacTrackpadConfiguration()]
    config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
    config.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]

    do {
        try config.validate()
    } catch {
        fail("the VM configuration is invalid: \(describe(error))")
    }
    return config
}

func clamp(cpus: Int, memoryGiB: Int, _ requirements: VZMacOSConfigurationRequirements?) -> (Int, Int) {
    var c = cpus, m = memoryGiB
    if let r = requirements {
        c = max(c, r.minimumSupportedCPUCount)
        m = max(m, Int((r.minimumSupportedMemorySize + (1 << 30) - 1) >> 30))
    }
    c = min(c, VZVirtualMachineConfiguration.maximumAllowedCPUCount)
    m = min(m, Int(VZVirtualMachineConfiguration.maximumAllowedMemorySize >> 30))
    return (c, m)
}

/// Hands a framework callback's result to the main queue. Virtualization calls back on a queue
/// of its own and the result is used only on main after that, never on both at once.
struct Unsent<T>: @unchecked Sendable {
    let value: T
    init(_ value: T) { self.value = value }
}

func loadRestoreImage(_ path: String, then body: @escaping @MainActor (VZMacOSRestoreImage) -> Void) {
    let url = URL(fileURLWithPath: path)
    VZMacOSRestoreImage.load(from: url) { result in
        let box = Unsent(result)
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                switch box.value {
                case .success(let image): body(image)
                case .failure(let error): fail("\(path) is not a restore image this host can read: \(describe(error))")
                }
            }
        }
    }
}

func report(_ image: VZMacOSRestoreImage) -> VZMacOSConfigurationRequirements {
    let v = image.operatingSystemVersion
    say("build=\(image.buildVersion)")
    say("os_version=\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)")
    guard let requirements = image.mostFeaturefulSupportedConfiguration else {
        fail("this host cannot install build \(image.buildVersion): no supported configuration")
    }
    guard requirements.hardwareModel.isSupported else {
        fail("this host does not support build \(image.buildVersion)'s hardware model")
    }
    say("min_cpus=\(requirements.minimumSupportedCPUCount)")
    say("min_memory_mib=\(requirements.minimumSupportedMemorySize >> 20)")
    return requirements
}

// MARK: - commands

@MainActor
func latest() {
    VZMacOSRestoreImage.fetchLatestSupported { result in
        let box = Unsent(result)
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                switch box.value {
                case .success(let image):
                    say("url=\(image.url.absoluteString)")
                    _ = report(image)
                    exit(0)
                case .failure(let error):
                    fail("cannot ask Apple for the latest supported restore image: \(describe(error))")
                }
            }
        }
    }
}

@MainActor
func verify(_ path: String) {
    loadRestoreImage(path) { image in
        _ = report(image)
        exit(0)
    }
}

/// Kept alive for the life of the process: the installer and its progress observation.
@MainActor
enum Held {
    static var installer: VZMacOSInstaller?
    static var observation: NSKeyValueObservation?
    static var booter: Booter?
    static var lastStep = -1
}

/// Remove a half-made bundle, then fail.
func abandonBundle(_ url: URL, _ message: String) -> Never {
    try? FileManager.default.removeItem(at: url)
    fail("\(message); removed \(url.path)")
}

@MainActor
func create(_ bundle: Bundle, ipsw: String, diskGiB: Int, cpus: Int, memoryGiB: Int) {
    let fm = FileManager.default
    guard !fm.fileExists(atPath: bundle.url.path) else { fail("\(bundle.url.path) already exists; refusing to overwrite it") }

    loadRestoreImage(ipsw) { image in
        let requirements = report(image)
        let (c, m) = clamp(cpus: cpus, memoryGiB: memoryGiB, requirements)

        // From here on, any failure removes the half-made bundle: it is large and useless.
        let abandon = { @Sendable (message: String) -> Never in abandonBundle(bundle.url, message) }
        do {
            try fm.createDirectory(at: bundle.url, withIntermediateDirectories: false,
                                   attributes: [.posixPermissions: 0o700])
            try requirements.hardwareModel.dataRepresentation.write(to: bundle.hardwareModel)
            try VZMacMachineIdentifier().dataRepresentation.write(to: bundle.identifier)
            _ = try VZMacAuxiliaryStorage(creatingStorageAt: bundle.aux, hardwareModel: requirements.hardwareModel)
            // A sparse raw disk: it takes only the blocks the guest writes.
            guard fm.createFile(atPath: bundle.disk.path, contents: nil, attributes: [.posixPermissions: 0o600]) else {
                abandon("cannot create \(bundle.disk.path)")
            }
            let handle = try FileHandle(forWritingTo: bundle.disk)
            try handle.truncate(atOffset: UInt64(diskGiB) << 30)
            try handle.close()
        } catch {
            abandon("cannot create the bundle: \(describe(error))")
        }

        let platform = loadPlatform(bundle)
        let config = configuration(bundle, platform: platform, cpus: c, memoryGiB: m, staging: nil)
        let vm = VZVirtualMachine(configuration: config)
        let installer = VZMacOSInstaller(virtualMachine: vm, restoringFromImageAt: URL(fileURLWithPath: ipsw))
        Held.installer = installer
        say("install=started disk_gib=\(diskGiB) cpus=\(c) memory_gib=\(m)")
        Held.observation = installer.progress.observe(\.fractionCompleted, options: [.new]) { progress, _ in
            let step = Int(progress.fractionCompleted * 20)  // every 5 %
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    guard step > Held.lastStep else { return }
                    Held.lastStep = step
                    say("install_percent=\(step * 5)")
                }
            }
        }
        installer.install { result in
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    switch result {
                    case .success:
                        say("install=done")
                        exit(0)
                    case .failure(let error):
                        abandon("the install failed: \(describe(error))")
                    }
                }
            }
        }
    }
}

/// The preparation boot: one window, the guest's own shutdown ends the process.
@MainActor
final class Booter: NSObject, VZVirtualMachineDelegate, NSWindowDelegate {
    let vm: VZVirtualMachine
    let title: String
    var window: NSWindow?
    var askedToStop = false
    var signalSources: [DispatchSourceSignal] = []

    init(_ config: VZVirtualMachineConfiguration, title: String) {
        vm = VZVirtualMachine(configuration: config)
        self.title = title
        super.init()
    }

    func start() {
        vm.delegate = self
        for sig in [SIGINT, SIGTERM] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            source.setEventHandler { MainActor.assumeIsolated { Held.booter?.stop(reason: sig == SIGINT ? "SIGINT" : "SIGTERM") } }
            source.resume()
            signalSources.append(source)
        }

        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let view = VZVirtualMachineView()
        view.virtualMachine = vm
        view.capturesSystemKeys = true
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1440, height: 810),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable],
                              backing: .buffered, defer: false)
        window.title = title
        window.contentView = view
        window.contentAspectRatio = NSSize(width: displayWidth, height: displayHeight)
        window.delegate = self
        window.center()
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(view)
        self.window = window

        vm.start { result in
            MainActor.assumeIsolated {
                switch result {
                case .success: say("state=running pid=\(getpid())")
                case .failure(let error): fail("the VM did not start: \(describe(error))")
                }
            }
        }
        app.activate()
        app.run()
    }

    /// First time: ask the guest to shut down (it may show its own dialog). Second time: off.
    func stop(reason: String) {
        if askedToStop || !vm.canRequestStop {
            say("shutdown=forced reason=\(reason)")
            vm.stop { _ in exit(3) }
            return
        }
        askedToStop = true
        do {
            try vm.requestStop()
            say("shutdown=guest-asked reason=\(reason)")
        } catch {
            say("shutdown=forced reason=\(reason)")
            vm.stop { _ in exit(3) }
        }
    }

    nonisolated func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        print("state=stopped")
        exit(0)
    }

    nonisolated func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        fail("the VM stopped with an error: \(describe(error))")
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        stop(reason: "window-closed")
        return false  // the window goes with the process, once the guest has stopped
    }
}

@MainActor
func boot(_ bundle: Bundle, staging: String?, cpus: Int, memoryGiB: Int) {
    let platform = loadPlatform(bundle)
    let stagingURL = staging.map { URL(fileURLWithPath: $0) }
    let config = configuration(bundle, platform: platform, cpus: cpus, memoryGiB: memoryGiB, staging: stagingURL)
    let booter = Booter(config, title: "faber golden image: \(bundle.url.lastPathComponent)")
    Held.booter = booter
    booter.start()
}

// MARK: - main

let args = CommandLine.arguments
if args.count == 2, args[1] == "--version" {
    print("faber-golden-prep \(prepVersion)")
    exit(0)
}
guard args.count >= 2 else { fail("usage: faber-golden-prep --version | latest | verify <ipsw> | create <bundle> <ipsw> --disk-gib N | boot <bundle>") }
let sizing: Set<String> = ["--cpus", "--memory-gib"]

MainActor.assumeIsolated {
    switch args[1] {
    case "latest":
        guard args.count == 2 else { fail("latest takes no arguments") }
        latest()
    case "verify":
        let o = parse(args.dropFirst(2), valued: [])
        guard o.positional.count == 1 else { fail("verify: expected <ipsw>") }
        verify(o.positional[0])
    case "create":
        let o = parse(args.dropFirst(2), valued: sizing.union(["--disk-gib"]))
        guard o.positional.count == 2 else { fail("create: expected <bundle> <ipsw>") }
        guard o.values["--disk-gib"] != nil else { fail("create: --disk-gib is required") }
        create(Bundle(o.positional[0]), ipsw: o.positional[1],
               diskGiB: intOption(o, "--disk-gib", default: 0, range: 64...2048),
               cpus: intOption(o, "--cpus", default: 4, range: 1...64),
               memoryGiB: intOption(o, "--memory-gib", default: 8, range: 4...256))
    case "boot":
        let o = parse(args.dropFirst(2), valued: sizing.union(["--staging"]))
        guard o.positional.count == 1 else { fail("boot: expected <bundle>") }
        boot(Bundle(o.positional[0]), staging: o.values["--staging"],
             cpus: intOption(o, "--cpus", default: 4, range: 1...64),
             memoryGiB: intOption(o, "--memory-gib", default: 8, range: 4...256))
    default:
        fail("unknown command \(args[1])")
    }
}
RunLoop.main.run()
