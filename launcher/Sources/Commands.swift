// The commands: inspect, clone, validate and run, and what validate and run share.

import Darwin
import Foundation
import Virtualization
import vmnet

let usage = """
usage: faber-vm <command> [arguments]

  inspect <bundle>                       read-only report on a macOS or Linux bundle
  clone <src> <dst> [--keep-identifier]  macOS bundles: clonefile(2) disk and vz-aux (never
                                         a full copy), copy vz-hwmodel, write a fresh
                                         vz-identifier (unless --keep-identifier) and the
                                         faber-clone marker. <dst> must not exist.
  validate <bundle> [run options] [--wall-only]
                                         build the exact run configuration, check the wall and
                                         ask the framework to validate it. Writes nothing into
                                         the bundle; with --network vmnet it creates the
                                         network for the check. --wall-only checks the wall
                                         alone: no framework validation, no network, so it
                                         runs where VMs cannot (it needs --network shared-nat).
  run <bundle> [options]                 boot the bundle until its guest stops
      --slot N              required: the concurrency slot 0...15; it fixes the VM's MAC
                            (02:fa:be:00:00:NN)
      --state-dir DIR       required by run: DIR/slot-N.lock is held while the VM lives;
                            DIR must be this account's, mode 0700
      --network MODE        vmnet (default; macOS 26+): a NAT network of this VM's own, with
                            NAT66, router advertisements and the DNS proxy off
                            shared-nat: Apple's shared NAT, the fallback
      --subnet CIDR         vmnet, required: a private IPv4 subnet, /16 to /30
      --reserve-ip ADDR     vmnet: the guest's reserved address (default: the subnet's third)
      --cpus N              vCPUs (default 4)
      --memory-gib N        guest memory in GiB (default 8)
      --memory-mib N        guest memory in MiB, the supervisor's unit (excludes --memory-gib)
      --console-log PATH    a serial port that appends guest output to PATH: a regular file
                            this account owns, with one link (never a symlink)
      --console-log-max-mib N  stop writing the log at N MiB (default 64); the guest is
                            still drained, so it never blocks
      --disk-cache MODE     automatic|cached|uncached (default cached)
      --tools-disk PATH     macOS only: attach PATH read-only as the second disk (the runner
                            and guest agent). It must be a regular file with one link that
                            this account neither owns nor can write, under directories it
                            does not own
      --tools-owner UID     the supervisor's child only: the tools disk's own directory may
                            be UID's (faber's, which builds the disks) instead of root's
      --claim C --serve-socket DIR/slot-N.sock [--slot-lock-fd 3]
                            the supervisor's child: serve the child protocol to this account
                            on <state-dir>/slot-N.sock, holding the slot lock handed down. A
                            macOS child speaks to its guest agent over vsock
                            (docs/vm-agent-protocol.md)
      --agent-ready-timeout SECONDS
                            the guest agent must report ready this long after the VM runs
                            (default 600)
      --docker-socket PATH --docker-peer-uid N
                            Linux child only: at `begin`, listen on PATH (0660, its directory
                            this account's with no group or other write) for uid N alone, and
                            join each client to the guest's Docker relay over vsock
      --docker-vsock-port P the guest's Docker relay (default 2375)
      --probe-vsock-port P  Linux child only: the guest's wall-probe service (default 6101)
  supervise --socket PATH --peer-uid N --state-dir DIR --image-dir DIR --tools-dir DIR
            --clone-dir DIR (--network-plan PATH | --network shared-nat)
            [--linux-dir DIR] [--docker-socket PATH]
                                         the VM supervisor (docs/vm-supervisor-protocol.md):
                                         serves uid N on PATH (0660); exit or SIGTERM leaves
                                         every VM running. --linux-dir: a Linux image X boots
                                         from DIR/X, cloned once from the image's disk.raw
                                         and never recreated. --docker-socket: the Linux
                                         child's bridge (default: beside --socket, docker.sock)
  supervisor-call --socket PATH [--no-hello] < frame
                                         debugging: read one frame from stdin, handshake, send
                                         it, print the replies
  --version                              print the stamped version
  protocol                               print the child protocol version run speaks
  help                                   print this

Bundles: macOS (Lima's layout) disk, vz-aux, vz-hwmodel, vz-identifier. Linux disk.raw (RAW),
efi-vars and machine-id (created by the first run), seed.iso (optional, attached read-only).
The kind comes from the files; a bundle with files of both kinds is refused.
validate and run refuse a bundle unless it and its parent directory are this account's,
with no group or other write.
A macOS disk is opened without synchronization (a per-job clone), disk.raw with full.
run prints key=value lines: faber_vm=<version> protocol=<n>, config, network, wall=ok, state=, event=.
run exit codes: 0 guest shut down; 1 error; 2 usage; 3 force-stopped; 4 a file the VM
was given changed after the wall check (the VM is force-stopped); 5 the guest agent broke
its protocol or missed a deadline (the VM is force-stopped; event=agent-error what=<code>);
6 the JIT runner exited without ever taking a job (the VM is force-stopped; event=runner-exited job=none).
On SIGTERM or SIGINT a macOS guest is force-stopped; a Linux guest is asked to stop and
force-stopped after \(Int(linuxStopGrace)) s, or at a second signal.
"""

let linuxStopGrace: TimeInterval = 60

/// The child protocol `run --serve-socket` speaks to the supervisor that started or adopts it
/// (README.md, "The child protocol"); the supervisor records it in the child's claim marker.
/// A supervisor leaves a child whose protocol it cannot speak to finish, and never stops it
/// (ADR-0026). Bump it on any incompatible change, and keep the old version in
/// supportedChildProtocols for as long as a supported launcher could have started a child on it.
let launcherProtocol = 2

/// What run keeps alive for its whole life: the locks and the vmnet network reference.
@MainActor
enum Held {
    static var locks: [HeldLock] = []
    static var network: vmnet_network_ref?
    static var runner: Runner?
    static var child: ChildServer?
    static var agent: VsockAgentConnector?
}

// MARK: - inspect

func inspect(_ bundle: VMBundle) throws -> [String] {
    let kind = try bundle.kind()
    var lines = ["bundle=\(bundle.url.path) kind=\(kind.rawValue) marked_clone=\(bundle.isMarkedClone)"]
    func describeFile(_ url: URL, absent: String = "absent") -> String {
        guard pathExists(url) else { return absent }
        guard (try? regularFile(url)) != nil else { return "not-a-regular-file(refused)" }
        guard let size = fileSize(url) else { return "unreadable" }
        return "bytes=\(size.bytes) allocated=\(size.allocated)"
    }
    switch kind {
    case .macOS:
        lines.append("disk: \(describeFile(bundle.disk))")
        lines.append("vz-aux: \(describeFile(bundle.aux))")
        if let model = try? bundle.loadHardwareModel() {
            lines.append("vz-hwmodel: valid=true supported_on_this_host=\(model.isSupported)")
        } else {
            lines.append("vz-hwmodel: valid=false")
        }
        if pathExists(bundle.identifierFile) {
            lines.append("vz-identifier: valid=\((try? bundle.loadIdentifier()) != nil)")
        } else {
            lines.append("vz-identifier: absent")
        }
    case .linux:
        lines.append("disk.raw: \(describeFile(bundle.rawDisk))")
        lines.append("efi-vars: \(describeFile(bundle.variableStoreFile, absent: "absent (run creates it)"))")
        if pathExists(bundle.machineIDFile) {
            let valid = (try? regularFile(bundle.machineIDFile)) != nil
                && (try? Data(contentsOf: bundle.machineIDFile)).flatMap(VZGenericMachineIdentifier.init(dataRepresentation:)) != nil
            lines.append("machine-id: valid=\(valid)")
        } else {
            lines.append("machine-id: absent (run creates it)")
        }
        lines.append("seed.iso: \(describeFile(bundle.seed))")
    }
    lines.append("host: vz_supported=\(VZVirtualMachine.isSupported) macos=\(macOSVersion)"
        + " vmnet_networks=\(isVmnetNetworkAPIAvailable ? "available" : "needs-macos-26")"
        + " cpus=\(VZVirtualMachineConfiguration.minimumAllowedCPUCount)...\(VZVirtualMachineConfiguration.maximumAllowedCPUCount)"
        + " memory_mib=\(VZVirtualMachineConfiguration.minimumAllowedMemorySize >> 20)...\(VZVirtualMachineConfiguration.maximumAllowedMemorySize >> 20)")
    return lines
}

var isVmnetNetworkAPIAvailable: Bool {
    if #available(macOS 26, *) { return true }
    return false
}

// MARK: - validate and run

struct Prepared {
    let kind: BundleKind
    let options: RunOptions
    let config: VZVirtualMachineConfiguration
    let networkLine: String
    let identity: LinuxIdentity?
    let log: ConsoleLog?
    /// The files the framework opens by path, for run's re-check once the VM is running.
    let files: [FileRef]

    /// validate only: remove throwaway files.
    func cleanUp() {
        identity?.removeScratch()
        log?.removeScratch()
    }
}

/// Everything validate and run share: parse, detect the bundle's kind, take the locks (run),
/// make the network, build the exact configuration, check the wall and, unless --wall-only,
/// ask the framework to validate it. No VM is created here.
@MainActor
func prepare(_ args: [String], forRun: Bool) throws -> Prepared {
    let parsed = try parse(args, valued: RunOptions.valued, switches: forRun ? [] : ["--wall-only"])
    guard parsed.positional.count == 1 else { throw UsageError("expected exactly one <bundle>") }
    let options = try RunOptions(parsed, forRun: forRun)
    let bundle = try VMBundle.forRun(parsed.positional[0])
    let kind = try bundle.kind()
    var tools: FileRef?
    if let path = options.toolsDisk {
        guard kind == .macOS else {
            throw UsageError("--tools-disk is for macOS bundles; the Linux VM gets no tools disk")
        }
        tools = try requireToolsDisk(path, toolsOwner: options.toolsOwner)
    }
    try checkLinuxOnlyOptions(options, kind: kind)
    if forRun, let socket = options.dockerSocket {
        // Checked again when `begin` binds it; refused here so a bad directory fails the start.
        try requirePrivateDirectory(socket.deletingLastPathComponent())
    }

    if forRun, let stateDir = options.stateDir {
        if let fd = options.slotLockFD {
            Held.locks.append(try options.slot.inheritLock(fd: fd, stateDir: stateDir))
        } else {
            Held.locks.append(try options.slot.lock(stateDir: stateDir))
        }
        Held.locks.append(try bundle.lock())  // before anything is written into the bundle
    } else if let stateDir = options.stateDir {
        close(try openStateDirectory(stateDir))  // validate: checked as run checks it; no lock is taken
    }

    let networkLine: String
    let network: NetworkChoice
    switch options.network {
    case .sharedNAT:
        network = .sharedNAT
        networkLine = sharedNATLine(slot: options.slot)
    case .vmnet:
        guard let plan = options.vmnet else { throw UsageError("--network vmnet needs --subnet") }
        guard #available(macOS 26, *) else {
            throw Failure("--network vmnet needs macOS 26 or later, and this host runs macOS \(macOSVersion); use --network shared-nat")
        }
        let ref = try createVmnetNetwork(plan, mac: options.slot.mac)
        Held.network = ref
        network = .vmnet(ref)
        networkLine = plan.line(slot: options.slot)
    }

    var identity: LinuxIdentity?
    var log: ConsoleLog?
    do {
        if let path = options.consoleLog { log = try ConsoleLog.open(path, create: forRun, maxMiB: options.consoleLogMaxMiB) }
        let config: VZVirtualMachineConfiguration
        let expect: WallExpectation
        switch kind {
        case .macOS:
            (config, expect) = try macConfiguration(bundle, options, network: network, log: log, tools: tools)
        case .linux:
            let loaded = try LinuxIdentity.load(bundle, create: forRun)
            identity = loaded
            for name in loaded.created { warn("created \(name)") }
            (config, expect) = try linuxConfiguration(bundle, options, network: network, identity: loaded, log: log)
        }
        try checkWall(config, expect)
        if !options.wallOnly {
            do {
                try config.validate()
            } catch {
                throw Failure("configuration invalid: \(describe(error))")
            }
        }
        return Prepared(kind: kind, options: options, config: config, networkLine: networkLine,
                        identity: identity, log: log, files: expect.attachedFiles)
    } catch {
        identity?.removeScratch()
        log?.removeScratch()
        throw error
    }
}

@MainActor
func validateCommand(_ args: [String]) throws -> Int32 {
    let prepared = try prepare(args, forRun: false)
    defer { prepared.cleanUp() }
    say(configLine(prepared.kind, prepared.options, prepared.config))
    say(prepared.networkLine)
    say("wall=ok mac=\(prepared.options.slot.macText)")
    say("protocol=\(launcherProtocol)")
    say(prepared.options.wallOnly ? "validate=wall-only" : "validate=ok")
    return 0
}

@MainActor
func runCommand(_ args: [String]) throws -> Never {
    say("faber_vm=\(faberVMVersion) protocol=\(launcherProtocol)")
    let prepared = try prepare(args, forRun: true)
    say(configLine(prepared.kind, prepared.options, prepared.config))
    say(prepared.networkLine)
    say("wall=ok mac=\(prepared.options.slot.macText) shares=0 port_forwards=0")
    let runner = Runner(kind: prepared.kind, config: prepared.config, files: prepared.files, log: prepared.log)
    Held.runner = runner
    if let socket = prepared.options.serveSocket, let claim = prepared.options.claim {
        // The supervisor's child: serve the child protocol before the VM starts, so `status`
        // answers `starting` from the first moment. `stop` does what SIGTERM does.
        let status = ChildStatus()
        runner.childStatus = status
        let agent: GuestAgent
        if prepared.kind == .macOS {
            // The guest agent, over vsock once the VM runs. Any violation stops the VM, exit 5; a
            // runner that exits without taking a job stops it too, exit 6.
            let channel = AgentChannel(status: status, probeTarget: probeTarget(prepared.options),
                                       deadlines: AgentDeadlines(ready: prepared.options.agentReadyTimeout),
                                       onNoJob: {
                                           DispatchQueue.main.async { MainActor.assumeIsolated { Held.runner?.stopForNoJob() } }
                                       }) { _ in
                DispatchQueue.main.async { MainActor.assumeIsolated { Held.runner?.stopForAgentError() } }
            }
            runner.onRunning = { vm in
                guard let device = vm.socketDevices.first as? VZVirtioSocketDevice else {
                    channel.fail(.connectionLost)
                    return
                }
                let connector = VsockAgentConnector(channel: channel, device: device)
                Held.agent = connector
                connector.start()
            }
            agent = channel
        } else {
            // The Linux VM runs Docker Engine, not a JIT runner: its agent is the launcher's
            // own (LinuxGuest.swift), which polls the guest and opens the Docker bridge.
            agent = try makeLinuxGuestAgent(vm: runner.vm, options: prepared.options, status: status)
        }
        let server = try ChildServer(path: socket, slot: prepared.options.slot.index, claim: claim,
                                     status: status, agent: agent) {
            liveAgents.stopAll()  // the Docker socket goes before the VM does
            DispatchQueue.main.async { MainActor.assumeIsolated { Held.runner?.beginShutdown(reason: "supervisor-stop") } }
        }
        Held.child = server
        server.start()
        say("child_protocol=\(childProtocol) socket=\(socket.path)")
    }
    runner.start()
    RunLoop.main.run()  // headless: the main run loop drains the main queue VZ runs on
    exit(1)  // unreachable: the runner exits from a VM callback
}

// MARK: - dispatch

@MainActor
func dispatch(_ argv: [String]) throws -> Int32 {
    guard let command = argv.first else { throw UsageError("missing command") }
    let rest = Array(argv.dropFirst())
    switch command {
    case "help", "--help", "-h":
        say(usage)
        return 0
    case "--version", "-V":
        guard rest.isEmpty else { throw UsageError("--version takes no arguments") }
        say("faber-vm \(faberVMVersion)")
        return 0
    case "protocol":
        guard rest.isEmpty else { throw UsageError("protocol takes no arguments") }
        say(String(launcherProtocol))
        return 0
    case "inspect":
        let parsed = try parse(rest)
        guard parsed.positional.count == 1 else { throw UsageError("inspect: expected <bundle>") }
        try inspect(VMBundle(parsed.positional[0])).forEach(say)
        return 0
    case "clone":
        let parsed = try parse(rest, switches: ["--keep-identifier"])
        guard parsed.positional.count == 2 else { throw UsageError("clone: expected <src> <dst>") }
        try clone(VMBundle(parsed.positional[0]), VMBundle(parsed.positional[1]),
                  keepIdentifier: parsed.switches.contains("--keep-identifier")).forEach(say)
        return 0
    case "validate":
        return try validateCommand(rest)
    case "run":
        try runCommand(rest)
    case "supervise":
        return try superviseCommand(rest)
    case "supervisor-call":
        return try supervisorCallCommand(rest)
    default:
        throw UsageError("unknown command \(command)")
    }
}
