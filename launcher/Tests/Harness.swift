// A tiny assertion harness: no XCTest, so the tests build with swiftc alone, like the launcher.

import Darwin
import Foundation

@MainActor
final class Harness {
    private(set) var passed = 0
    private(set) var failed = 0
    private(set) var skipped = 0

    func check(_ name: String, _ ok: Bool, _ detail: @autoclosure () -> String = "") {
        if ok {
            passed += 1
        } else {
            failed += 1
            let more = detail()
            print("FAIL \(name)" + (more.isEmpty ? "" : ": \(more)"))
        }
    }

    /// The body must succeed.
    func accepts(_ name: String, _ body: () throws -> Void) {
        do {
            try body()
            passed += 1
        } catch {
            failed += 1
            print("FAIL \(name): unexpected error: \(error)")
        }
    }

    /// The body must throw a Failure or UsageError, and its message must contain `containing`.
    func refuses(_ name: String, containing: String? = nil, usage: Bool? = nil, _ body: () throws -> Void) {
        do {
            try body()
            failed += 1
            print("FAIL \(name): accepted, expected a refusal")
        } catch {
            let text = "\(error)"
            let isUsage = error is UsageError
            if !(error is Failure || isUsage) {
                failed += 1
                print("FAIL \(name): refused with an unexpected error type: \(text)")
            } else if let usage, usage != isUsage {
                failed += 1
                print("FAIL \(name): expected a \(usage ? "usage error" : "failure"), got: \(text)")
            } else if let containing, !text.contains(containing) {
                failed += 1
                print("FAIL \(name): refusal does not mention '\(containing)': \(text)")
            } else {
                passed += 1
            }
        }
    }

    func skip(_ name: String, _ reason: String) {
        skipped += 1
        print("SKIP \(name): \(reason)")
    }
}

/// A temporary directory removed when the tests end.
final class Scratch {
    let url: URL

    init() throws {
        url = FileManager.default.temporaryDirectory.appendingPathComponent("faber-vm-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
    }

    func dir(_ name: String) throws -> URL {
        let dir = url.appendingPathComponent(name)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// A sparse file of `size` bytes: no blocks are written.
    @discardableResult
    func sparse(_ url: URL, size: off_t = 64 << 20) throws -> URL {
        let fd = open(url.path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw Failure("\(url.path): \(errnoText(errno))") }
        defer { close(fd) }
        guard ftruncate(fd, size) == 0 else { throw Failure("ftruncate \(url.path): \(errnoText(errno))") }
        return url
    }

    @discardableResult
    func write(_ url: URL, _ data: Data = Data()) throws -> URL {
        try data.write(to: url, options: .withoutOverwriting)
        return url
    }

    func remove() { try? FileManager.default.removeItem(at: url) }
}
