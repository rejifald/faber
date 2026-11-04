// A bundle is a directory holding one VM. Its kind is read from the files in it, never from
// its name:
//   macOS (Lima's layout): disk, vz-aux, vz-hwmodel, vz-identifier
//   Linux: disk.raw (RAW), efi-vars and machine-id (both created by the first run), and
//          seed.iso, a cloud-init seed attached read-only while it exists
// The launcher adds faber-vm.lock to a bundle it runs, and faber-clone to a clone it makes.
// Every one of these must be a regular file: a symlink is refused, so a bundle cannot point
// the launcher at a disk that lives somewhere else.

import Darwin
import Foundation
import Virtualization

enum BundleKind: String {
    case macOS = "macos"
    case linux
}

/// The marker `clone` writes, so a sweep can remove the clones faber made and nothing else
/// (AGENTS.md: read labels, never names).
let cloneMarkerName = "faber-clone"
let cloneMarkerContents = "org.faber.kind=vm-clone\n"

struct VMBundle {
    let url: URL

    init(_ path: String) { url = absoluteURL(path) }
    init(url: URL) { self.url = url.standardizedFileURL }
    /// A path realpath(3) already resolved: kept as is, since standardizing would put the
    /// /var and /tmp symlinks back.
    private init(resolved: URL) { url = resolved }

    /// The bundle validate and run use: its path resolved, and it and the directory holding it
    /// changeable by this account alone, so no other account can swap a file between the wall
    /// check and the framework opening it by path. Every directory above them up to / must be
    /// this account's or root's, without group or other write unless sticky. A golden image
    /// is root's and is only ever read by clone, which does not ask this.
    static func forRun(_ path: String) throws -> VMBundle {
        let url = try resolvedURL(absoluteURL(path))
        try requirePrivateDirectory(url)
        try requirePrivateDirectory(url.deletingLastPathComponent())
        try requireSafeAncestors(above: url.deletingLastPathComponent())
        return VMBundle(resolved: url)
    }

    // macOS
    var disk: URL { file("disk") }
    var aux: URL { file("vz-aux") }
    var hardwareModelFile: URL { file("vz-hwmodel") }
    var identifierFile: URL { file("vz-identifier") }

    // Linux
    var rawDisk: URL { file("disk.raw") }
    var variableStoreFile: URL { file("efi-vars") }
    var machineIDFile: URL { file("machine-id") }
    var seed: URL { file("seed.iso") }

    // Both
    var lockName: String { "faber-vm.lock" }
    var marker: URL { file(cloneMarkerName) }

    func file(_ name: String) -> URL { url.appendingPathComponent(name) }

    /// vz-hwmodel marks a macOS bundle and disk.raw a Linux one. Both, or neither, is refused:
    /// a bundle carrying any file of the other kind is ambiguous.
    func kind() throws -> BundleKind {
        let mac = [hardwareModelFile, aux, identifierFile].contains(where: pathExists)
        let linux = [rawDisk, variableStoreFile, machineIDFile, seed].contains(where: pathExists)
        switch (mac, linux) {
        case (true, false):
            guard pathExists(hardwareModelFile) else {
                throw Failure("\(url.path) has macOS bundle files but no vz-hwmodel")
            }
            return .macOS
        case (false, true):
            guard pathExists(rawDisk) else { throw Failure("\(url.path) has Linux bundle files but no disk.raw") }
            return .linux
        case (true, true):
            throw Failure("\(url.path) has both macOS (vz-*) and Linux (disk.raw, efi-vars, machine-id, seed.iso) files; refusing to guess its kind")
        case (false, false):
            throw Failure("\(url.path) is not a bundle: it has neither vz-hwmodel (macOS) nor disk.raw (Linux)")
        }
    }

    var isMarkedClone: Bool {
        guard (try? regularFile(marker)) != nil,
              let text = try? String(contentsOf: marker, encoding: .utf8) else { return false }
        return text == cloneMarkerContents
    }

    func loadHardwareModel() throws -> VZMacHardwareModel {
        _ = try regularFile(hardwareModelFile)
        let data = try Data(contentsOf: hardwareModelFile)
        guard let model = VZMacHardwareModel(dataRepresentation: data) else {
            throw Failure("\(hardwareModelFile.path): not a VZMacHardwareModel data representation")
        }
        return model
    }

    func loadIdentifier() throws -> VZMacMachineIdentifier {
        _ = try regularFile(identifierFile)
        let data = try Data(contentsOf: identifierFile)
        guard let identifier = VZMacMachineIdentifier(dataRepresentation: data) else {
            throw Failure("\(identifierFile.path): not a VZMacMachineIdentifier data representation")
        }
        return identifier
    }

    /// Hold the bundle's lock for the life of the process, so one bundle never boots twice.
    func lock() throws -> HeldLock {
        let dir = try openDirectory(url)
        defer { close(dir) }
        return try HeldLock.acquire(
            in: dir, name: lockName, displayPath: file(lockName).path,
            busy: "\(url.path) is already running under another faber-vm (\(lockName) is locked)")
    }
}

// MARK: - Linux identity

/// The machine identifier and EFI variable store of a Linux bundle. `run` creates both files
/// on the first boot and loads them afterwards; a malformed one is an error, never replaced.
/// `validate` writes nothing into the bundle: without machine-id it uses a throwaway
/// identifier, and without efi-vars a throwaway store in a temporary directory it removes.
struct LinuxIdentity {
    let machineIdentifier: VZGenericMachineIdentifier
    let variableStore: VZEFIVariableStore
    /// The file the store lives in, which the wall checks by identity.
    let variableStoreFile: URL
    let created: [String]
    let scratch: URL?

    static func load(_ bundle: VMBundle, create: Bool) throws -> LinuxIdentity {
        var created: [String] = []

        let identifier: VZGenericMachineIdentifier
        if pathExists(bundle.machineIDFile) {
            _ = try regularFile(bundle.machineIDFile)
            let data = try Data(contentsOf: bundle.machineIDFile)
            guard let loaded = VZGenericMachineIdentifier(dataRepresentation: data) else {
                throw Failure("\(bundle.machineIDFile.path): not a VZGenericMachineIdentifier data representation")
            }
            identifier = loaded
        } else {
            identifier = VZGenericMachineIdentifier()
            if create {
                // .withoutOverwriting is O_EXCL: it refuses a file, or a symlink, put there since.
                try identifier.dataRepresentation.write(to: bundle.machineIDFile, options: .withoutOverwriting)
                created.append(bundle.machineIDFile.lastPathComponent)
            }
        }

        if pathExists(bundle.variableStoreFile) {
            _ = try regularFile(bundle.variableStoreFile)
            return LinuxIdentity(machineIdentifier: identifier,
                                 variableStore: VZEFIVariableStore(url: bundle.variableStoreFile),
                                 variableStoreFile: bundle.variableStoreFile, created: created, scratch: nil)
        }
        if create {
            let store = try VZEFIVariableStore(creatingVariableStoreAt: bundle.variableStoreFile)
            created.append(bundle.variableStoreFile.lastPathComponent)
            return LinuxIdentity(machineIdentifier: identifier, variableStore: store,
                                 variableStoreFile: bundle.variableStoreFile, created: created, scratch: nil)
        }
        let scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("faber-vm-validate-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: false,
                                                attributes: [.posixPermissions: 0o700])
        do {
            let file = scratch.appendingPathComponent("efi-vars")
            let store = try VZEFIVariableStore(creatingVariableStoreAt: file)
            return LinuxIdentity(machineIdentifier: identifier, variableStore: store,
                                 variableStoreFile: file, created: created, scratch: scratch)
        } catch {
            try? FileManager.default.removeItem(at: scratch)
            throw error
        }
    }

    /// validate only: remove the throwaway store's directory.
    func removeScratch() {
        if let scratch { try? FileManager.default.removeItem(at: scratch) }
    }
}

// MARK: - clone

func cloneFile(_ src: URL, _ dst: URL, flags: Int32 = CLONE_NOFOLLOW) throws {
    guard clonefile(src.path, dst.path, UInt32(flags)) == 0 else {
        let code = errno
        let why: String
        switch code {
        case EXDEV: why = "source and destination are on different volumes; clones cannot cross volumes"
        case ENOTSUP: why = "the filesystem does not support clones (APFS needed)"
        default: why = String(cString: strerror(code))
        }
        throw Failure("clonefile \(src.path) -> \(dst.path): \(why) (errno \(code)); refusing to fall back to a full copy")
    }
}

/// clonefile(2) `src` to `dst`, then make the copy this account's to write (0600). clonefile
/// keeps the source's mode, and the image store's golden images are read-only (root:wheel 444,
/// ADR-0049), so a clone of one would be 0444 too: Virtualization opens the disk and the
/// auxiliary storage read-write, and refused every job VM with "Failed to open auxiliary
/// storage ... Permission denied" (October 2026, #76). The clone is in a directory only this
/// account enters, so the mode is the only thing to change.
func cloneWritable(_ src: URL, _ dst: URL) throws {
    try cloneFile(src, dst)
    guard chmod(dst.path, 0o600) == 0 else {
        let code = errno
        throw Failure("chmod 0600 \(dst.path): \(errnoText(code))")
    }
}

/// A per-job copy of a macOS bundle: clonefile(2) for the disk and auxiliary storage, never a
/// full copy; the hardware model copied; a fresh machine identifier unless `keepIdentifier`;
/// and faber's marker. The Linux VM is one long-lived bundle per host and is never cloned per
/// job: the supervisor makes it once (prepareLinuxBundle).
func clone(_ src: VMBundle, _ dst: VMBundle, keepIdentifier: Bool) throws -> [String] {
    guard try src.kind() == .macOS else {
        throw Failure("clone: \(src.url.path) is a Linux bundle; clone makes per-job copies of macOS bundles only")
    }
    _ = try regularFile(src.disk)
    _ = try regularFile(src.aux)
    _ = try src.loadHardwareModel()  // refuse to clone a bundle that would never boot
    let srcIdentifier = keepIdentifier ? try src.loadIdentifier() : nil
    guard src.url != dst.url else { throw Failure("clone: source and destination are the same bundle") }
    guard !pathExists(dst.url) else { throw Failure("clone: \(dst.url.path) already exists; refusing to overwrite") }

    try FileManager.default.createDirectory(at: dst.url, withIntermediateDirectories: false,
                                            attributes: [.posixPermissions: 0o700])
    var finished = false
    defer { if !finished { try? FileManager.default.removeItem(at: dst.url) } }  // only the dir just made

    // The marker goes first, so even a clone left half-made by a crash is one the sweep removes.
    try Data(cloneMarkerContents.utf8).write(to: dst.marker, options: .withoutOverwriting)
    try cloneWritable(src.disk, dst.disk)
    try cloneWritable(src.aux, dst.aux)
    try Data(contentsOf: src.hardwareModelFile).write(to: dst.hardwareModelFile, options: .withoutOverwriting)
    let identifier = srcIdentifier ?? VZMacMachineIdentifier()
    try identifier.dataRepresentation.write(to: dst.identifierFile, options: .withoutOverwriting)
    finished = true

    var lines = ["cloned src=\(src.url.path) dst=\(dst.url.path) identifier=\(keepIdentifier ? "kept" : "new")",
                 "marker=\(cloneMarkerName) kind=vm-clone"]
    if keepIdentifier {
        lines.append("note=same-identifier-as-source")
    }
    return lines
}
