// The guest agent's transport for a real VM: the VM's one virtio-vsock device, connected to the
// agent's port from the host once the VM runs (docs/vm-agent-protocol.md, Transport).

import Darwin
import Foundation
import Virtualization

/// Apple's shared NAT puts its gateway, the host's own address on that network, at
/// 192.168.64.1 unless the host was changed. The launcher cannot ask the framework for it, so a
/// shared-NAT VM's probe goes there; on vmnet, the default, the gateway is the subnet's second
/// address, which this launcher set.
let sharedNATGateway = "192.168.64.1"

/// The probe's target for a VM on `options`' network: the slot's gateway.
func probeTarget(_ options: RunOptions) -> String {
    options.vmnet.map { $0.subnet.gateway.description } ?? sharedNATGateway
}

/// Connects an AgentChannel to the guest: VZVirtioSocketDevice.connect(toPort:) on the VM's
/// queue (main), every second until the ready deadline, then hands the connection's
/// descriptor to the channel and keeps the connection object alive with it.
@MainActor
final class VsockAgentConnector {
    let channel: AgentChannel
    private let device: VZVirtioSocketDevice

    init(channel: AgentChannel, device: VZVirtioSocketDevice) {
        self.channel = channel
        self.device = device
    }

    /// The VM is running: start the channel's clock and poll thread, and connect.
    func start() {
        channel.start()
        attempt()
    }

    private func attempt() {
        guard !channel.hasFailed, channel.status.state != .stopping else { return }
        device.connect(toPort: agentPort) { [weak self] result in
            MainActor.assumeIsolated {
                guard let self else { return }
                switch result {
                case .success(let connection):
                    // The connection owns the descriptor and closes it when released, so the
                    // channel keeps it for the VM's life.
                    self.channel.connect(fd: connection.fileDescriptor, owner: connection)
                case .failure:
                    // The agent is not listening yet: the guest is still booting.
                    guard Date() < self.channel.readyDeadline else {
                        self.channel.fail(.timeout)
                        return
                    }
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
                        MainActor.assumeIsolated { self?.attempt() }
                    }
                }
            }
        }
    }
}
