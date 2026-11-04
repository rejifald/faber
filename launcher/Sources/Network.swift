// The VM's one network device, on one of two networks:
//   vmnet       the default, macOS 26 and later: a vmnet network this process creates for
//               its VM alone (VZVmnetNetworkDeviceAttachment), in shared (NAT) mode, on the
//               subnet the caller names, with NAT66, router advertisements and the gateway's
//               DNS proxy off, and a DHCP reservation for the slot's MAC. The guest then has
//               only link-local IPv6. The DNS proxy is off because the gateway answers with the
//               host's own view of names (#76); guests use a public resolver instead.
//   shared-nat  Apple's shared NAT (VZNATNetworkDeviceAttachment): ADR-0026's fallback where a
//               per-VM network cannot be made. Every VM on the host shares it, vmnet.h says its
//               guests can reach each other, and its IPv6 and DNS proxy cannot be turned off, so
//               the host's pf anchor alone must block them.
// vmnet's port-forwarding API is never called, and no attachment that hands the guest's
// frames to this process (a file handle) or to a host interface (bridged) is ever built.

import Darwin
import Foundation
import Virtualization
import vmnet

enum NetworkMode: String {
    case vmnet
    case sharedNAT = "shared-nat"

    static func parse(_ text: String) throws -> NetworkMode {
        guard let mode = NetworkMode(rawValue: text) else {
            throw UsageError("--network: expected vmnet|shared-nat, got \(text)")
        }
        return mode
    }
}

/// The per-VM network the caller asked for, checked before anything is created.
struct VmnetPlan: Equatable {
    let subnet: IPv4Subnet
    let guest: IPv4

    init(subnet: IPv4Subnet, reservation: IPv4?) throws {
        let guest = reservation ?? subnet.defaultGuest
        try subnet.checkReservation(guest)
        self.subnet = subnet
        self.guest = guest
    }

    /// The line the pf-anchor side reads: what this VM's network is, and what it is not.
    func line(slot: Slot) -> String {
        "network mode=vmnet subnet=\(subnet) gateway=\(subnet.gateway) guest=\(guest)"
            + " mac=\(slot.macText) slot=\(slot) ipv6=link-local-only nat66=off"
            + " router_advertisement=off dns_proxy=off port_forwards=0"
    }
}

func sharedNATLine(slot: Slot) -> String {
    // Apple's NAT picks its own subnet (192.168.64.0/24 unless the host changed it) and serves
    // IPv6 and DNS on its gateway; none of that is the launcher's to set.
    "network mode=shared-nat subnet=host-default gateway=host-default guest=dhcp"
        + " mac=\(slot.macText) slot=\(slot) ipv6=not-disabled dns_proxy=apple-default"
        + " isolation=shared port_forwards=0"
}

/// vmnet_return_t by name and number: "VMNET_INVALID_ACCESS(1005)".
func vmnetStatusText(_ status: vmnet_return_t) -> String {
    let name: String
    switch status {
    case .VMNET_SUCCESS: name = "VMNET_SUCCESS"
    case .VMNET_FAILURE: name = "VMNET_FAILURE"
    case .VMNET_MEM_FAILURE: name = "VMNET_MEM_FAILURE"
    case .VMNET_INVALID_ARGUMENT: name = "VMNET_INVALID_ARGUMENT"
    case .VMNET_SETUP_INCOMPLETE: name = "VMNET_SETUP_INCOMPLETE"
    case .VMNET_INVALID_ACCESS: name = "VMNET_INVALID_ACCESS"
    case .VMNET_PACKET_TOO_BIG: name = "VMNET_PACKET_TOO_BIG"
    case .VMNET_BUFFER_EXHAUSTED: name = "VMNET_BUFFER_EXHAUSTED"
    case .VMNET_TOO_MANY_PACKETS: name = "VMNET_TOO_MANY_PACKETS"
    case .VMNET_SHARING_SERVICE_BUSY: name = "VMNET_SHARING_SERVICE_BUSY"
    case .VMNET_NOT_AUTHORIZED: name = "VMNET_NOT_AUTHORIZED"
    @unknown default: name = "unknown"
    }
    return "\(name)(\(status.rawValue))"
}

/// Create the vmnet network for one VM. vmnet ties the network's reservations (its subnet
/// and the DHCP reservation) to the returned reference, and a VM may only use a network its
/// own process created, so the caller keeps the reference for the life of the process.
@available(macOS 26, *)
func createVmnetNetwork(_ plan: VmnetPlan, mac: VZMACAddress) throws -> vmnet_network_ref {
    func fail(_ step: String, _ status: vmnet_return_t) -> Failure {
        Failure("--network vmnet: \(step) returned \(vmnetStatusText(status))")
    }
    var status = vmnet_return_t.VMNET_SUCCESS
    guard let configuration = vmnet_network_configuration_create(.VMNET_SHARED_MODE, &status) else {
        throw fail("vmnet_network_configuration_create", status)
    }
    vmnet_network_configuration_disable_nat66(configuration)
    vmnet_network_configuration_disable_router_advertisement(configuration)
    vmnet_network_configuration_disable_dns_proxy(configuration)

    // vmnet wants the gateway's address here, the subnet's second, although the header calls
    // it the subnet address. Given the network's own address vmnet_network_create still
    // succeeds, but the VM's interface fails to start (#76).
    var gateway = plan.subnet.gateway.inAddr
    var mask = plan.subnet.maskAddress.inAddr
    status = vmnet_network_configuration_set_ipv4_subnet(configuration, &gateway, &mask)
    guard status == .VMNET_SUCCESS else { throw fail("vmnet_network_configuration_set_ipv4_subnet", status) }

    var client = mac.ethernetAddress
    var guest = plan.guest.inAddr
    status = vmnet_network_configuration_add_dhcp_reservation(configuration, &client, &guest)
    guard status == .VMNET_SUCCESS else { throw fail("vmnet_network_configuration_add_dhcp_reservation", status) }

    status = .VMNET_SUCCESS
    guard let network = vmnet_network_create(configuration, &status) else {
        throw fail("vmnet_network_create", status)
    }

    // Read the subnet back and refuse a network that is not the one asked for: the pf anchor
    // is rendered from the line this launcher prints, so it must be the truth.
    var reported = in_addr()
    var reportedMask = in_addr()
    vmnet_network_get_ipv4_subnet(network, &reported, &reportedMask)
    let reportedPrefix = UInt32(bigEndian: reportedMask.s_addr).nonzeroBitCount
    let reportedNetwork = IPv4(UInt32(bigEndian: reported.s_addr) & UInt32(bigEndian: reportedMask.s_addr))
    guard reportedPrefix == plan.subnet.prefix, reportedNetwork == plan.subnet.network else {
        throw Failure("--network vmnet: vmnet made \(reportedNetwork)/\(reportedPrefix), not the requested \(plan.subnet)")
    }
    return network
}
