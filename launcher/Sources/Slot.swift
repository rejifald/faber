// A concurrency slot and its fixed MAC (ADR-0026, "Per job" step 3).
//
// Each slot maps to one MAC, so a stream of jobs reuses a handful of addresses instead of
// exhausting a NAT's DHCP pool, and a VM's DHCP reservation can be made before it boots. The
// bundle's own MAC, if a tool stored one, is never used.
//
// The MACs are 02:fa:be:00:00:NN, NN the slot in hex. The first octet, 0x02, sets the
// locally-administered bit and clears the multicast bit, so the address is a unicast one no
// vendor was ever assigned. Could it collide with a MAC some other VZ tool picked at random?
// VZMACAddress.randomLocallyAdministered() randomizes every bit except those two, measured:
// 46 random bits, so a given random MAC hits one of the 16 slot MACs with probability
// 16 / 2^46, about 2e-13. And on the default network, vmnet, the VM is alone on a network
// its own process created, so no other MAC is on its segment at all; only Apple's shared NAT,
// the fallback, is shared with other tools' VMs. No prefix could make a collision impossible
// there, since the random addresses cover every locally-administered unicast first octet.
//
// `run` holds an exclusive lock on <state-dir>/slot-N.lock for its whole life, so no two VMs
// ever share a MAC even if the caller asks for the same slot twice.

import Darwin
import Foundation
import Virtualization

struct Slot: Equatable, CustomStringConvertible {
    static let count = 16

    let index: Int

    init(_ index: Int) throws {
        guard (0..<Slot.count).contains(index) else {
            throw UsageError("--slot \(index): expected 0...\(Slot.count - 1)")
        }
        self.index = index
    }

    static func parse(_ text: String) throws -> Slot {
        guard let n = Int(text), String(n) == text else {
            throw UsageError("--slot: not a non-negative integer: \(text)")
        }
        return try Slot(n)
    }

    var octets: [UInt8] { [0x02, 0xfa, 0xbe, 0x00, 0x00, UInt8(index)] }

    var macText: String { octets.map { String(format: "%02x", $0) }.joined(separator: ":") }

    var mac: VZMACAddress {
        let o = octets
        return VZMACAddress(ethernetAddress: ether_addr_t(octet: (o[0], o[1], o[2], o[3], o[4], o[5])))
    }

    var lockName: String { "slot-\(index).lock" }

    var description: String { String(index) }

    /// Hold <stateDir>/slot-N.lock for the life of the process.
    func lock(stateDir: URL) throws -> HeldLock {
        let dir = try openStateDirectory(stateDir)
        defer { close(dir) }
        return try HeldLock.acquire(
            in: dir, name: lockName, displayPath: stateDir.appendingPathComponent(lockName).path,
            busy: "slot \(index) is in use by another faber-vm (\(lockName) is locked); two VMs never share a MAC")
    }

    /// Hold the slot lock the supervisor took and handed down as descriptor `fd`. The supervisor
    /// takes it before it writes the claim marker, so from then until this child exits the slot
    /// is never free: a supervisor that crashes in between leaves a held lock, not a VM that
    /// looks gone. The descriptor must be this state directory's slot-N.lock, by identity.
    func inheritLock(fd: Int32, stateDir: URL) throws -> HeldLock {
        let dir = try openStateDirectory(stateDir)
        defer { close(dir) }
        let display = stateDir.appendingPathComponent(lockName).path
        var held = stat()
        var named = stat()
        guard fstat(fd, &held) == 0 else { throw Failure("--slot-lock-fd \(fd): \(errnoText(errno))") }
        try checkOwnedFile(held, euid: geteuid(), path: display)
        guard fstatat(dir, lockName, &named, AT_SYMLINK_NOFOLLOW) == 0, FileID(held) == FileID(named) else {
            throw Failure("--slot-lock-fd \(fd) is not \(display)")
        }
        // Already held through this open file: flock converts it in place and cannot block.
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            throw Failure("slot \(index) is in use by another faber-vm (\(lockName) is locked); two VMs never share a MAC")
        }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        return HeldLock(fd: fd, path: display)
    }
}

func macOctets(_ mac: VZMACAddress) -> [UInt8] {
    let e = mac.ethernetAddress.octet
    return [e.0, e.1, e.2, e.3, e.4, e.5]
}
