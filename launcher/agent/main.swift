// faber-agent: the guest agent inside a macOS clone (docs/vm-agent-protocol.md).
//
//   faber-agent serve         as root, from the bootstrap LaunchDaemon (launcher/golden/)
//   faber-agent runner-exec   as runner, from the LaunchAgent `serve` bootstraps on begin
//   faber-agent --version

import Darwin
import Foundation

setvbuf(stdout, nil, _IOLBF, 0)
signal(SIGPIPE, SIG_IGN)

let args = Array(CommandLine.arguments.dropFirst())
switch args {
case ["serve"]:
    guard geteuid() == 0 else {
        agentLog("serve runs as root, from the bootstrap")
        exit(1)
    }
    let console = guestConsoleWriter()
    let core = AgentCore(consoleIsRunner: consoleUserIsRunner, probe: sendProbeDatagrams,
                         startRunner: { jit, core in startRunnerInSession(jit, core, console: console) },
                         runChecks: runGuestChecks,
                         reapplyUpdates: { at in
                             console(reapplyUpdatesOff(at: at, run: { runCheckCommand($0, timeout: $1) }))
                         })
    // ADR-0049's Software Update keys, written again before the agent listens (Updates.swift).
    core.serveStarting()
    do {
        try serveAgent(core)
    } catch {
        agentLog("\(error)")
        exit(1)
    }
case ["runner-exec"]:
    exit(runnerExec())
case ["--version"]:
    print("faber-agent \(faberVMVersion) protocol=\(agentProtocol)")
    exit(0)
default:
    agentLog("usage: faber-agent serve | runner-exec | --version")
    exit(2)
}
