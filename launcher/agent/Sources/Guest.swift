// faber-agent's fixed facts and its pure pieces: the paths, the tools disk's manifest, the
// LaunchAgent plist, and every command line and environment it builds. Pure, so the tests
// (launcher/Tests/AgentTests.swift) hold them to the rule that matters most: the JIT config is
// on no argv and in no file, only in run.sh's environment.

import Darwin
import Foundation

enum Guest {
    /// The account job code runs as, with GUI auto-login (ADR-0026, The golden image).
    static let runnerUser = "runner"
    /// Where the bootstrap mounts the tools disk, read-only (launcher/golden/bootstrap.sh).
    static let toolsMount = "/private/var/faber/tools"
    static let agentBinary = toolsMount + "/faber-agent"
    static let manifestPath = toolsMount + "/manifest.json"
    /// Root's directory for the hand-off: the LaunchAgent plist and the one-shot socket.
    static let stateRoot = "/private/var/faber"
    static let handoffDir = stateRoot + "/handoff"
    static let handoffSocket = handoffDir + "/runner.sock"
    static let runnerLabel = "io.github.rejifald.faber.runner"
    static let plistPath = handoffDir + "/" + runnerLabel + ".plist"
    /// The fresh directory in runner's home the runner is unpacked into. One clone, one job:
    /// it must not exist.
    static let runnerDirName = "faber-runner"
    static let locale = "en_US.UTF-8"
    static let launchctl = "/bin/launchctl"
    static let tar = "/usr/bin/tar"
    /// The largest manifest.json accepted. faber's is under 400 bytes.
    static let maxManifest = 4096
    /// The fixed PATH of every process the agent starts. run.sh's adds Homebrew's prefix, where
    /// the golden image's release-job tools live.
    static let systemPath = "/usr/bin:/bin:/usr/sbin:/sbin"
    static let runnerPath = "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:" + systemPath
}

/// "faber-agent: <line>" on stderr: launchd's log for the daemon. Never a JIT config, never a
/// byte the launcher sent.
func agentLog(_ line: String) {
    FileHandle.standardError.write(Data("faber-agent: \(line)\n".utf8))
}

// MARK: - The manifest

/// What the tools disk's manifest.json records (src/vm/tools_disk.rs, `Manifest`).
struct GuestManifest: Equatable {
    let runnerVersion: String
    let runnerSHA256: String
    let agentVersion: String
    let agentSHA256: String

    /// The runner tarball's file name on the disk: actions-runner-osx-arm64-<version>.tar.gz.
    var tarball: String { "actions-runner-osx-arm64-\(runnerVersion).tar.gz" }
}

/// `N.N.N`: three decimal components, 1 to 9 digits each, no leading zeros.
func isRunnerVersion(_ s: String) -> Bool {
    let parts = s.split(separator: ".", omittingEmptySubsequences: false)
    guard parts.count == 3 else { return false }
    return parts.allSatisfy { p in
        let b = Array(p.utf8)
        guard (1...9).contains(b.count), b.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }) else { return false }
        return b.count == 1 || b[0] != 0x30
    }
}

/// 64 lowercase hex digits.
func isSHA256Hex(_ s: String) -> Bool {
    let b = Array(s.utf8)
    return b.count == 64 && b.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x61 && $0 <= 0x66) }
}

/// The manifest, read strictly: at most Guest.maxManifest bytes, a closed object, every field
/// in its grammar. faber writes it pretty-printed, which the reader's whitespace allows.
func parseManifest(_ bytes: [UInt8]) throws -> GuestManifest {
    guard bytes.count <= Guest.maxManifest else { throw ProtoError.frameTooLarge }
    guard String(validating: bytes, as: UTF8.self) != nil else { throw ProtoError.notUTF8 }
    let f = try Fields(StrictJSONReader.parse(bytes), "manifest",
                       required: ["runner_version", "runner_sha256", "agent_version", "agent_sha256"])
    return GuestManifest(
        runnerVersion: try f.string("runner_version", isRunnerVersion),
        runnerSHA256: try f.string("runner_sha256", isSHA256Hex),
        agentVersion: try f.string("agent_version", isSupervisorVersion),
        agentSHA256: try f.string("agent_sha256", isSHA256Hex))
}

// MARK: - What the agent starts

/// The LaunchAgent that runs `runner-exec` in runner's GUI session. It holds no secret: the
/// JIT config reaches `runner-exec` over the hand-off socket. Root writes it, 0644.
func launchAgentPlist(program: String = Guest.agentBinary) throws -> Data {
    let plist: [String: Any] = [
        "Label": Guest.runnerLabel,
        "ProgramArguments": [program, "runner-exec"],
        "EnvironmentVariables": ["LANG": Guest.locale, "LC_ALL": Guest.locale, "PATH": Guest.systemPath],
        "LimitLoadToSessionType": "Aqua",
        "RunAtLoad": true,
        "KeepAlive": false,
        "ProcessType": "Interactive",
    ]
    return try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
}

/// `launchctl bootstrap gui/<uid> <plist>`: a fixed argv; never the JIT config.
func launchctlBootstrapArgv(uid: uid_t, plist: String = Guest.plistPath) -> [String] {
    [Guest.launchctl, "bootstrap", "gui/\(uid)", plist]
}

/// `tar -xzf <tarball> -C <dir>`.
func tarArgv(tarball: String, into dir: String) -> [String] {
    [Guest.tar, "-xzf", tarball, "-C", dir]
}

/// run.sh takes no arguments: the config is in its environment, the runner's own fallback for
/// `--jitconfig`.
func runnerArgv(dir: String) -> [String] {
    [dir + "/run.sh"]
}

/// run.sh's whole environment. The JIT config is here and nowhere else.
func runnerEnvironment(jit: JitConfig, home: String, user: String, tmpdir: String?) -> [String] {
    var env = [
        "ACTIONS_RUNNER_INPUT_JITCONFIG=" + jit.expose(),
        "LANG=" + Guest.locale,
        "LC_ALL=" + Guest.locale,
        "HOME=" + home,
        "USER=" + user,
        "LOGNAME=" + user,
        "PATH=" + Guest.runnerPath,
    ]
    if let tmpdir, tmpdir.hasPrefix("/"), !tmpdir.utf8.contains(where: { $0 < 0x20 }) { env.append("TMPDIR=" + tmpdir) }
    return env
}

/// The environment of every other process the agent starts.
let fixedEnvironment = ["PATH=" + Guest.systemPath, "LANG=" + Guest.locale, "LC_ALL=" + Guest.locale]

// MARK: - Spawning

/// How a process ended.
enum GuestExit: Equatable {
    case code(Int)
    case signal(Int)
    case unknown
}

/// Decode a waitpid status: Swift does not import the WIFEXITED/WTERMSIG macros. Pure.
func guestExit(status: Int32) -> GuestExit {
    let low = status & 0x7f
    if low == 0 { return .code(Int((status >> 8) & 0xff)) }
    if low != 0x7f, (1...31).contains(low) { return .signal(Int(low)) }
    return .unknown
}

/// posix_spawn `argv[0]` with `argv` and `env`, in `cwd` if given: stdin /dev/null, stdout and
/// stderr inherited, or both sent to `output` when given, every other descriptor closed, every
/// signal at its default.
func guestSpawn(_ argv: [String], env: [String], cwd: String? = nil, output: Int32? = nil) throws -> pid_t {
    var attr: posix_spawnattr_t?
    guard posix_spawnattr_init(&attr) == 0 else { throw Failure("posix_spawnattr_init failed") }
    defer { posix_spawnattr_destroy(&attr) }
    var actions: posix_spawn_file_actions_t?
    guard posix_spawn_file_actions_init(&actions) == 0 else { throw Failure("posix_spawn_file_actions_init failed") }
    defer { posix_spawn_file_actions_destroy(&actions) }
    var defaults: sigset_t = 0
    for sig in 1..<NSIG where sig != SIGKILL && sig != SIGSTOP { defaults |= sigset_t(1) << sigset_t(sig - 1) }
    var none: sigset_t = 0
    var steps: [Int32] = [
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK)),
        posix_spawnattr_setsigdefault(&attr, &defaults),
        posix_spawnattr_setsigmask(&attr, &none),
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0),
    ]
    if let output {
        steps.append(posix_spawn_file_actions_adddup2(&actions, output, 1))
        steps.append(posix_spawn_file_actions_adddup2(&actions, output, 2))
    } else {
        steps.append(posix_spawn_file_actions_addinherit_np(&actions, 1))
        steps.append(posix_spawn_file_actions_addinherit_np(&actions, 2))
    }
    if let cwd { steps.append(posix_spawn_file_actions_addchdir_np(&actions, cwd)) }
    if let bad = steps.first(where: { $0 != 0 }) { throw Failure("posix_spawn setup: \(errnoText(bad))") }
    let cArgv = argv.map { strdup($0) } + [nil]
    let cEnv = env.map { strdup($0) } + [nil]
    defer {
        cArgv.forEach { free($0) }
        cEnv.forEach { free($0) }
    }
    var pid: pid_t = 0
    // The program is argv[0], a fixed path: never a lookup.
    let rc = posix_spawn(&pid, argv[0], &actions, &attr, cArgv, cEnv)
    guard rc == 0 else { throw Failure("posix_spawn \(argv[0]): \(errnoText(rc))") }
    return pid
}

/// Wait for `pid` and decode how it ended.
func guestWait(_ pid: pid_t) -> GuestExit {
    var status: Int32 = 0
    while true {
        let r = waitpid(pid, &status, 0)
        if r == pid { return guestExit(status: status) }
        if r < 0, errno == EINTR { continue }
        return .unknown
    }
}

// MARK: - Root's directories

/// A directory only root can change: create it 0755 if absent, then refuse anything but a
/// real directory owned by root with no group or other write. Pure check in checkRootDirectory.
func requireRootDirectory(_ path: String) throws {
    if mkdir(path, 0o755) != 0, errno != EEXIST { throw Failure("\(path): mkdir: \(errnoText(errno))") }
    var st = stat()
    guard lstat(path, &st) == 0 else { throw Failure("\(path): \(errnoText(errno))") }
    try checkRootDirectory(st, path: path)
}

func checkRootDirectory(_ st: stat, path: String) throws {
    guard st.st_mode & S_IFMT == S_IFDIR else { throw Failure("\(path): not a directory (a symlink is refused)") }
    guard st.st_uid == 0 else { throw Failure("\(path): not owned by root") }
    guard st.st_mode & 0o022 == 0 else { throw Failure("\(path): writable by group or others") }
}
