// A VM's IPv4 subnet, and the addresses in it that vmnet hands out.
//
// vmnet keeps three addresses of a subnet for itself: the first (the network's), the second
// (the gateway, the host's side) and the last (broadcast). It writes no DHCP lease file, so
// the only guest address the launcher can know is the one it reserved for the slot's MAC:
// the subnet's third address unless --reserve-ip names another.

import Darwin
import Foundation

/// An IPv4 address in host byte order; `inAddr` converts for the C APIs.
struct IPv4: Equatable, Comparable, CustomStringConvertible {
    let value: UInt32

    init(_ value: UInt32) { self.value = value }

    static func parse(_ text: String) -> IPv4? {
        var address = in_addr()
        guard inet_pton(AF_INET, text, &address) == 1 else { return nil }
        return IPv4(UInt32(bigEndian: address.s_addr))
    }

    var inAddr: in_addr { in_addr(s_addr: value.bigEndian) }

    var description: String {
        "\(value >> 24).\((value >> 16) & 0xff).\((value >> 8) & 0xff).\(value & 0xff)"
    }

    static func < (a: IPv4, b: IPv4) -> Bool { a.value < b.value }
}

struct IPv4Subnet: Equatable, CustomStringConvertible {
    /// A per-VM network is a /16 at the widest and a /30 at the narrowest, which still leaves
    /// one address (the third) for the guest.
    static let prefixRange = 16...30

    /// RFC 1918. A per-VM network on a public range would shadow real addresses for the host.
    static let privateRanges: [(IPv4, Int)] = [
        (IPv4(0x0A00_0000), 8),   // 10.0.0.0/8
        (IPv4(0xAC10_0000), 12),  // 172.16.0.0/12
        (IPv4(0xC0A8_0000), 16),  // 192.168.0.0/16
    ]

    let network: IPv4
    let prefix: Int

    var mask: UInt32 { prefix == 0 ? 0 : UInt32.max << (32 - prefix) }
    var maskAddress: IPv4 { IPv4(mask) }
    var gateway: IPv4 { IPv4(network.value + 1) }
    var defaultGuest: IPv4 { IPv4(network.value + 2) }
    var broadcast: IPv4 { IPv4(network.value | ~mask) }

    var description: String { "\(network)/\(prefix)" }

    func contains(_ address: IPv4) -> Bool { address.value & mask == network.value }

    /// "a.b.c.d/n": no host bits set, n in prefixRange, inside one RFC 1918 range.
    static func parse(_ text: String) throws -> IPv4Subnet {
        let parts = text.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, let address = IPv4.parse(String(parts[0])),
              let prefix = Int(parts[1]), String(prefix) == parts[1], (0...32).contains(prefix) else {
            throw UsageError("--subnet: expected an IPv4 subnet as address/prefix-length, got \(text)")
        }
        guard prefixRange.contains(prefix) else {
            throw UsageError("--subnet \(text): the prefix length must be \(prefixRange.lowerBound)...\(prefixRange.upperBound)")
        }
        let subnet = IPv4Subnet(network: address, prefix: prefix)
        guard address.value & ~subnet.mask == 0 else {
            throw UsageError("--subnet \(text) has host bits set; the subnet is \(IPv4(address.value & subnet.mask))/\(prefix)")
        }
        guard privateRanges.contains(where: { range, length in
            prefix >= length && IPv4Subnet(network: range, prefix: length).contains(address)
        }) else {
            throw UsageError("--subnet \(text) is not inside a private range (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)")
        }
        return subnet
    }

    /// An address vmnet can reserve for a guest: inside the subnet, and not one of the three
    /// it keeps (first, gateway, last).
    func checkReservation(_ address: IPv4) throws {
        guard contains(address) else {
            throw UsageError("--reserve-ip \(address) is outside --subnet \(self)")
        }
        guard address != network, address != gateway, address != broadcast else {
            throw UsageError("--reserve-ip \(address): vmnet keeps a subnet's first (\(network)), second (the gateway, \(gateway)) and last (\(broadcast)) addresses")
        }
    }
}
