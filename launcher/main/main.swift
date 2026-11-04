// faber-vm: the launcher faber runs its VMs with (ADR-0026), on Apple's Virtualization
// framework. Swift and Apple's frameworks only (ADR-0026, check 19). All logic is in
// ../Sources; this file is the entry point and nothing else.

import Darwin
import Foundation

setvbuf(stdout, nil, _IOLBF, 0)
do {
    exit(try dispatch(Array(CommandLine.arguments.dropFirst())))
} catch let error as UsageError {
    warn(error.description)
    FileHandle.standardError.write(Data((usage + "\n").utf8))
    exit(2)
} catch {
    warn("error: \(describe(error))")
    exit(1)
}
