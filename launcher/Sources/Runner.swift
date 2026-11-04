// Runs one VM until it stops: state lines, signals, and shutdown.

import Darwin
import Foundation
import Virtualization

@MainActor
final class Runner: NSObject {
    let kind: BundleKind
    let vm: VZVirtualMachine
    let files: [FileRef]
    let log: ConsoleLog?
    /// What the child protocol reports, when the supervisor started this run.
    var childStatus: ChildStatus?
    /// Called once the VM runs and has passed the post-start checks: a macOS child connects
    /// its guest agent here.
    var onRunning: ((VZVirtualMachine) -> Void)?

    private var stateObservation: NSKeyValueObservation?
    private var signalSources: [DispatchSourceSignal] = []
    private var shuttingDown = false
    private var stopPendingStart = false
    private var stopIssued = false

    init(kind: BundleKind, config: VZVirtualMachineConfiguration, files: [FileRef], log: ConsoleLog?) {
        self.kind = kind
        self.files = files
        self.log = log
        // The main queue is the VM's queue: every call and callback on the VM and its devices
        // happens there.
        self.vm = VZVirtualMachine(configuration: config)
        super.init()
    }

    func start() {
        vm.delegate = self
        let status = childStatus
        // Read `vm.state`, never `change.newValue`: an NS_ENUM property's change values do not
        // bridge and are always nil.
        stateObservation = vm.observe(\.state, options: stateObservationOptions) { vm, _ in
            say("state=\(stateName(vm.state))")
            switch childStatusEffect(of: vm.state) {
            case .booting?: _ = status?.move(from: .starting, to: .booting)
            case .stopping?: status?.set(.stopping)
            case nil: break
            }
        }
        installSignalHandlers()
        log?.startDraining()  // before the guest can write a byte
        vm.start { [weak self] result in
            guard let self else { return }
            switch result {
            case .success:
                guard self.runningDevicesMatch() else {
                    warn("wall: the running VM has an unexpected device set")
                    self.forceStop(exitCode: 1)
                    return
                }
                // The framework opened its files by path during start. A path that now names
                // another file means one was swapped after the wall check.
                if let changed = changedFile(self.files) {
                    say("event=file-identity-changed path=\(changed.url.path)")
                    warn("wall: \(changed.url.path) is no longer the file the wall checked; stopping")
                    self.forceStop(exitCode: 4)
                    return
                }
                if self.stopPendingStart {
                    self.forceStop(exitCode: 3)
                    return
                }
                self.onRunning?(self.vm)
            case .failure(let error):
                warn("start failed: \(describe(error))")
                exit(1)
            }
        }
    }

    /// The devices the running VM reports, checked again against the wall.
    private func runningDevicesMatch() -> Bool {
        guard vm.networkDevices.count == 1, vm.socketDevices.count == 1,
              vm.directorySharingDevices.isEmpty, vm.consoleDevices.isEmpty,
              vm.usbControllers.isEmpty, vm.memoryBalloonDevices.count == 1 else { return false }
        switch kind {
        case .macOS: return vm.graphicsDevices.count == 1
        case .linux: return vm.graphicsDevices.isEmpty
        }
    }

    private func installSignalHandlers() {
        for (sig, name) in [(SIGINT, "SIGINT"), (SIGTERM, "SIGTERM")] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            source.setEventHandler { [weak self] in
                MainActor.assumeIsolated { self?.beginShutdown(reason: name) }
            }
            source.resume()
            signalSources.append(source)
        }
    }

    /// A macOS guest is force-stopped at once: requestStop() meets the logged-in session's
    /// shutdown dialog, and a per-job clone is thrown away anyway. The Linux guest, whose disk
    /// holds the only copy of the runner image, is asked to stop and given a grace period.
    func beginShutdown(reason: String) {
        if shuttingDown {
            say("shutdown=forced reason=\(reason)-again")
            forceStop(exitCode: 3)
            return
        }
        shuttingDown = true
        childStatus?.set(.stopping)
        say("shutdown=requested reason=\(reason) state=\(stateName(vm.state))")
        if vm.state == .starting {
            stopPendingStart = true  // stop as soon as start completes
            return
        }
        guard kind == .linux, vm.canRequestStop else {
            forceStop(exitCode: 3)
            return
        }
        do {
            try vm.requestStop()
            say("shutdown=guest-asked grace_s=\(Int(linuxStopGrace))")
        } catch {
            warn("requestStop failed: \(describe(error)); stopping")
            forceStop(exitCode: 3)
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + linuxStopGrace) { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.vm.state != .stopped else { return }
                say("shutdown=grace-expired")
                self.forceStop(exitCode: 3)
            }
        }
    }

    /// The guest agent broke its protocol or missed a deadline (Agent.swift has already logged
    /// the code): force-stop, exit 5.
    func stopForAgentError() {
        shuttingDown = true
        childStatus?.set(.stopping)
        forceStop(exitCode: 5)
    }

    /// The JIT runner exited without ever taking a job (Agent.swift has logged it): not a job
    /// that finished, so the child does not report `job_finished`. It stops the VM, exit 6,
    /// which the supervisor reports as `runner_exited_without_job` and keeps the console for.
    /// The guest wrote the runner's tail to its console just before it said so; the stop waits
    /// `noJobConsoleGrace` for those bytes to reach the log.
    func stopForNoJob() {
        guard !shuttingDown else { return }
        shuttingDown = true
        childStatus?.set(.stopping)
        DispatchQueue.main.asyncAfter(deadline: .now() + noJobConsoleGrace) { [weak self] in
            MainActor.assumeIsolated { self?.forceStop(exitCode: noJobExitCode) }
        }
    }

    private func forceStop(exitCode: Int32) {
        if stopIssued { return }  // a second signal or the grace timer while stop() is in flight
        stopIssued = true
        guard vm.canStop else {
            warn("cannot stop in state \(stateName(vm.state))")
            exit(vm.state == .stopped ? exitCode : 1)
        }
        vm.stop { error in
            if let error {
                warn("stop failed: \(describe(error))")
                exit(1)
            }
            say("event=stopped-by-host")
            exit(exitCode)
        }
    }
}

extension Runner: @preconcurrency VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        say("event=guest-stopped")
        exit(0)
    }

    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        say("event=stopped-with-error")
        warn("VM stopped with error: \(describe(error))")
        exit(1)
    }

    func virtualMachine(_ virtualMachine: VZVirtualMachine, networkDevice: VZNetworkDevice,
                        attachmentWasDisconnectedWithError error: Error) {
        say("event=network-disconnected")
        warn("network attachment disconnected: \(describe(error))")
    }
}

/// The child's exit status for a VM stopped because its runner took no job
/// (Supervisor.swift, stopReasonForExit).
let noJobExitCode: Int32 = 6

/// How long a VM whose runner took no job runs on, for the guest's last console lines (the
/// runner's tail) to reach the log.
let noJobConsoleGrace: TimeInterval = 2

func stateName(_ state: VZVirtualMachine.State) -> String {
    switch state {
    case .stopped: "stopped"
    case .running: "running"
    case .paused: "paused"
    case .error: "error"
    case .starting: "starting"
    case .pausing: "pausing"
    case .resuming: "resuming"
    case .stopping: "stopping"
    case .saving: "saving"
    case .restoring: "restoring"
    @unknown default: "unknown(\(state.rawValue))"
    }
}

/// What a framework state does to the child's reported status.
enum ChildStatusEffect: Equatable {
    /// Running is `booting` until the guest agent reports ready (Agent.swift).
    case booting
    case stopping
}

/// How Runner observes the VM's state: changes only, registered before `start`, so none is
/// missed. Never `.initial`: the initial value is the VM before `start`, always `stopped`, and
/// taken as a stop it made every child report `stopping` from birth, so faber stopped each VM
/// it had just started.
let stateObservationOptions: NSKeyValueObservingOptions = [.new]

/// The effect of the VM reaching `state`, or nil for none. Pure.
func childStatusEffect(of state: VZVirtualMachine.State) -> ChildStatusEffect? {
    switch state {
    case .running: return .booting
    case .stopping, .stopped, .error: return .stopping
    default: return nil
    }
}
