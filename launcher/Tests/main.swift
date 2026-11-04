// faber-vm's unit tests. Usage: faber-vm-tests <path to faber-vm.entitlements> <tests/fixtures/vm-proto>
// Exit 0 only when no test failed; skipped tests print their reason.

import Darwin
import Foundation
import Virtualization

setvbuf(stdout, nil, _IOLBF, 0)
let harness = Harness()
let entitlementsPath = CommandLine.arguments.dropFirst().first
let fixturesPath = CommandLine.arguments.dropFirst(2).first
// The golden build's script, beside the entitlements file in launcher/.
let provisionPath = entitlementsPath.map {
    URL(fileURLWithPath: $0).deletingLastPathComponent().appendingPathComponent("golden/provision.sh").path
}

print("faber-vm-tests: macos=\(macOSVersion) vz_supported=\(VZVirtualMachine.isSupported) version=\(faberVMVersion)")
let scratch: Scratch
do {
    scratch = try Scratch()
} catch {
    print("FAIL cannot create a temporary directory: \(error)")
    exit(1)
}
testSlots(harness)
testSubnets(harness)
testOptions(harness)
testBundlesAndLocks(harness, scratch)
testEntitlementsFile(harness, path: entitlementsPath)
testVersion(harness)
testWallLinux(harness, scratch)
testWallMac(harness, scratch)
testValidateWallOnly(harness, scratch)
testWallVmnet(harness, scratch)
testOwnership(harness, scratch)
testIdentity(harness, scratch)
testConsoleLog(harness, scratch)
testAncestors(harness, scratch)
testToolsDisk(harness, scratch)
testProtocol(harness)
testProto(harness, fixtures: fixturesPath)
testSupervisor(harness)
testLinuxGuest(harness, scratch)
testAgent(harness, scratch)
testHandoffDiagnostics(harness, scratch)
testRunnerTail(harness, scratch)
testVerify(harness)
testVerifyDiagnostics(harness, scratch)
testUpdates(harness, provision: provisionPath)
scratch.remove()

print("faber-vm-tests: \(harness.passed) passed, \(harness.failed) failed, \(harness.skipped) skipped")
exit(harness.failed == 0 ? 0 : 1)
