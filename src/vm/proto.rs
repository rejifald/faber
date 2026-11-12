//! The `_faber` side of the VM supervisor protocol: `docs/vm-supervisor-protocol.md`.
//!
//! The supervisor handles traffic from guests where job code is root, and `_faber`
//! holds the GitHub credential (ADR-0025). So `_faber` reads this wire the way
//! [`crate::untrusted`] reads Docker: only a fixed vocabulary, never a guest's bytes.
//!
//! - **Strict.** Every struct is `deny_unknown_fields`; an unknown field, a missing
//!   one, a repeated key, an explicit `null` or an unknown enum string is refused.
//! - **Bounded.** A frame is at most [`MAX_FRAME`] bytes, and every list, string and
//!   number has a range the types below enforce.
//! - **Validated where it is read.** The newtypes check their range or grammar as
//!   they deserialize, so a bad value never becomes a `Slot` or a `ClaimId`. The
//!   rules that span fields (a `tools` disk only on macOS, a `stop_reason` only on
//!   a stopped VM, no repeated slot in a list) run in the `decode_*` functions.
//! - **Silent about the peer.** [`ProtoError`] names a category and, for a parse
//!   failure, a line and column. It never quotes the frame: serde's own messages
//!   repeat field names and values, which here come from the other side of the wall.
//!
//! This module does no I/O. It turns bytes into typed frames and back; the socket,
//! the handshake loop and the client live elsewhere. [`decode_request`] and
//! [`encode_response`] are the supervisor's directions, here for the golden
//! fixtures in `tests/fixtures/vm-proto/` and as a reference for the Swift side.

use std::collections::HashSet;
use std::fmt;

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

/// The one protocol version this build speaks. There is no negotiation. Version 3
/// added the stop reason [`StopReason::RunnerExitedWithoutJob`]; version 4 added
/// `start.disk_bytes` and a Linux record's disk size (ADR-0061 §4).
pub const PROTOCOL_VERSION: u16 = 4;
/// The largest frame, its LF included.
pub const MAX_FRAME: usize = 16384;
/// How many launcher slots exist (`0..SLOTS`).
pub const SLOTS: u8 = 16;
/// The longest claim id.
pub const MAX_CLAIM_LEN: usize = 64;
/// The longest image or tools-disk name.
pub const MAX_NAME_LEN: usize = 64;
/// The longest `jit_config`.
pub const MAX_JIT_CONFIG_LEN: usize = 8192;
/// The longest `supervisor_version`.
pub const MAX_VERSION_LEN: usize = 64;
/// The fewest and most vCPUs a `start` may ask for.
pub const CPUS_MIN: u8 = 1;
pub const CPUS_MAX: u8 = 64;
/// The least and most memory, in MiB, a `start` may ask for.
pub const MEMORY_MIB_MIN: u32 = 1024;
pub const MEMORY_MIB_MAX: u32 = 262_144;
/// The largest disk a `start` may grow a Linux VM's disk to: 1 TiB, the
/// supervisor's own bound whatever faber asks (ADR-0061 §4).
pub const DISK_BYTES_MAX: u64 = 1 << 40;
/// A `start.disk_bytes` is a whole number of MiB.
pub const DISK_BYTES_ALIGN: u64 = 1 << 20;
/// The largest size a record reports for a disk file or the blocks it holds:
/// twice [`DISK_BYTES_MAX`], room for a filesystem's own rounding.
pub const DISK_REPORT_MAX: u64 = 2 * DISK_BYTES_MAX;
/// The latest timestamp accepted, in seconds: the year 2100.
pub const MAX_UNIX: u64 = 4_102_444_800;
/// The most claims or records any list carries.
pub const MAX_LIST: usize = 16;
/// The most simulator runtimes a `verify` expects (ADR-0049 §7): the golden
/// build's own cap on runtime facts.
pub const MAX_RUNTIMES: usize = 64;

/// Why a frame was refused. Every variant is a fixed category: none carries, and
/// none prints, a byte from the peer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    #[error("frame is larger than the protocol allows")]
    FrameTooLarge,
    #[error("frame is not exactly one LF-terminated line")]
    NotOneLine,
    #[error("frame is not UTF-8")]
    NotUtf8,
    #[error("frame does not match the protocol (line {line}, column {column})")]
    Malformed { line: usize, column: usize },
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("supervisor speaks protocol {theirs}")]
    VersionMismatch { theirs: u16 },
    #[error("reply does not fit the request")]
    WrongReply,
    #[error("response id does not match the request")]
    IdMismatch,
    #[error("frame could not be encoded")]
    Encode,
}

// ---------------------------------------------------------------------------
// Validated newtypes
// ---------------------------------------------------------------------------

macro_rules! int_newtype {
    ($(#[$meta:meta])* $name:ident, $inner:ty, $inner_str:literal, $what:literal, $lo:expr, $hi:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = $inner_str, into = $inner_str)]
        pub struct $name($inner);

        impl $name {
            pub fn get(self) -> $inner {
                self.0
            }
        }

        impl TryFrom<$inner> for $name {
            type Error = ProtoError;
            fn try_from(v: $inner) -> Result<Self, ProtoError> {
                if ($lo..=$hi).contains(&v) {
                    Ok(Self(v))
                } else {
                    Err(ProtoError::Invalid($what))
                }
            }
        }

        impl From<$name> for $inner {
            fn from(v: $name) -> $inner {
                v.0
            }
        }
    };
}

int_newtype!(
    /// A request id, chosen by the client: 1 to 4 294 967 295.
    FrameId,
    u32,
    "u32",
    "frame id",
    1,
    u32::MAX
);
int_newtype!(
    /// A protocol version: 1 to 65 535.
    ProtocolVersion,
    u16,
    "u16",
    "protocol version",
    1,
    u16::MAX
);
int_newtype!(
    /// A launcher slot: 0 to 15.
    Slot,
    u8,
    "u8",
    "slot",
    0,
    SLOTS - 1
);
int_newtype!(
    /// A vCPU count: 1 to 64.
    Cpus,
    u8,
    "u8",
    "cpus",
    CPUS_MIN,
    CPUS_MAX
);
int_newtype!(
    /// A memory size in MiB: 1 024 to 262 144.
    MemoryMib,
    u32,
    "u32",
    "memory_mib",
    MEMORY_MIB_MIN,
    MEMORY_MIB_MAX
);
int_newtype!(
    /// The size a Linux VM's disk is grown to before it boots: 1 MiB to 1 TiB, a
    /// whole number of MiB ([`Request::validate`] checks the multiple).
    DiskBytes,
    u64,
    "u64",
    "disk_bytes",
    DISK_BYTES_ALIGN,
    DISK_BYTES_MAX
);
int_newtype!(
    /// A time in seconds since the epoch: 0 to the year 2100.
    UnixSecs,
    u64,
    "u64",
    "timestamp",
    0,
    MAX_UNIX
);

fn grammar(s: &str, max: usize, first: fn(u8) -> bool, rest: fn(u8) -> bool) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= max && first(b[0]) && b[1..].iter().all(|&c| rest(c))
}

macro_rules! text_newtype {
    ($(#[$meta:meta])* $name:ident, $what:literal, $max:expr, $first:expr, $rest:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProtoError;
            fn try_from(s: String) -> Result<Self, ProtoError> {
                if grammar(&s, $max, $first, $rest) {
                    Ok(Self(s))
                } else {
                    Err(ProtoError::Invalid($what))
                }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ProtoError;
            fn try_from(s: &str) -> Result<Self, ProtoError> {
                Self::try_from(s.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
    };
}

fn alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}
fn name_first(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

text_newtype!(
    /// faber's own claim id: first `[A-Za-z0-9]`, then `[A-Za-z0-9._-]`, 1 to 64 bytes.
    ClaimId,
    "claim",
    MAX_CLAIM_LEN,
    alnum,
    |c| alnum(c) || matches!(c, b'.' | b'_' | b'-')
);
text_newtype!(
    /// A name in the image store or the tools-disk cache, never a path: first
    /// `[a-z0-9]`, then `[a-z0-9._-]`, 1 to 64 bytes.
    ImageName,
    "image name",
    MAX_NAME_LEN,
    name_first,
    |c| name_first(c) || matches!(c, b'.' | b'_' | b'-')
);
text_newtype!(
    /// The supervisor's version: 1 to 64 bytes of `[0-9A-Za-z.+-]`.
    SupervisorVersion,
    "supervisor version",
    MAX_VERSION_LEN,
    |c| alnum(c) || matches!(c, b'.' | b'+' | b'-'),
    |c| alnum(c) || matches!(c, b'.' | b'+' | b'-')
);
text_newtype!(
    /// Xcode's version line as `xcodebuild -version` prints it after `Xcode `:
    /// first a digit, then `[A-Za-z0-9. ]`, 1 to 32 bytes (`27.0`, `27.0 Beta 2`).
    XcodeVersion,
    "xcode version",
    32,
    |c| c.is_ascii_digit(),
    |c| alnum(c) || matches!(c, b'.' | b' ')
);
text_newtype!(
    /// A build token (Xcode's or a runtime's): ASCII letters and digits, at most 16
    /// bytes. A `verify` holds each to at least 3 ([`Request::validate`]), as the
    /// golden build reads them.
    BuildToken,
    "build",
    16,
    alnum,
    alnum
);
text_newtype!(
    /// A simulator runtime's platform, `iOS`, `watchOS`, …: 1 to 16 ASCII letters.
    RuntimePlatform,
    "runtime platform",
    16,
    |c| c.is_ascii_alphabetic(),
    |c| c.is_ascii_alphabetic()
);
text_newtype!(
    /// A runtime's version: digits and dots, first a digit, 1 to 16 bytes.
    RuntimeVersion,
    "runtime version",
    16,
    |c| c.is_ascii_digit(),
    |c| c.is_ascii_digit() || c == b'.'
);

/// A GitHub JIT runner configuration: a credential. Its `Debug` never prints the
/// value and it has no `Display`; keep it out of logs, argv and files.
///
/// 1 to 8 192 bytes of the base64 alphabet `[A-Za-z0-9+/=]`.
#[derive(PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct JitConfig(String);

impl JitConfig {
    /// The secret itself, for the one place that hands it on.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for JitConfig {
    type Error = ProtoError;
    fn try_from(s: String) -> Result<Self, ProtoError> {
        let ok = !s.is_empty()
            && s.len() <= MAX_JIT_CONFIG_LEN
            && s.bytes()
                .all(|c| alnum(c) || matches!(c, b'+' | b'/' | b'='));
        if ok {
            Ok(Self(s))
        } else {
            Err(ProtoError::Invalid("jit_config"))
        }
    }
}

impl TryFrom<&str> for JitConfig {
    type Error = ProtoError;
    fn try_from(s: &str) -> Result<Self, ProtoError> {
        Self::try_from(s.to_owned())
    }
}

impl Serialize for JitConfig {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl fmt::Debug for JitConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JitConfig(<redacted, {} bytes>)", self.0.len())
    }
}

/// An optional field that, when present, must not be `null`. serde's default for
/// `Option` reads `null` as absent; this protocol leaves an absent field out.
fn non_null<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

// ---------------------------------------------------------------------------
// Closed sets
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmKind {
    Macos,
    Linux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Starting,
    Booting,
    Ready,
    ProbeSent,
    InService,
    JobFinished,
    /// `verify` was sent: the guest agent is running the image's checks.
    Verifying,
    /// The checks are done; the record carries the failed ones in `failed`.
    Verified,
    Stopping,
    Stopped,
    Unmanaged,
}

/// One check of ADR-0049 §7 the guest agent runs on a fresh clone. The closed set
/// `failed` carries: a check the guest names is one of these or the frame is
/// refused. The wall proof is not here: faber reads it from the anchor's
/// counters, never from the guest ([`crate::vm::verify::Failure::Wall`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The console user is `runner`.
    ConsoleUser,
    /// `runner` is not an admin.
    RunnerNotAdmin,
    /// Every admin account cannot log in.
    AdminsLocked,
    /// Remote Login is off.
    RemoteLogin,
    /// FileVault is off.
    Filevault,
    /// No Apple ID is signed in.
    AppleId,
    /// Every automatic Software Update setting is off.
    SoftwareUpdate,
    /// As `runner`: `xcodebuild -version` matches the manifest, `xcode-select -p`
    /// resolves, and `cc` and `swiftc` link a program that runs.
    Xcode,
    /// Every runtime the manifest names is ready.
    Runtimes,
    /// `runner` owns the Homebrew prefix.
    BrewOwner,
}

impl Check {
    /// Every check, in the order the agent runs and reports them.
    pub const ALL: [Check; 10] = [
        Check::ConsoleUser,
        Check::RunnerNotAdmin,
        Check::AdminsLocked,
        Check::RemoteLogin,
        Check::Filevault,
        Check::AppleId,
        Check::SoftwareUpdate,
        Check::Xcode,
        Check::Runtimes,
        Check::BrewOwner,
    ];

    /// The wire's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Check::ConsoleUser => "console_user",
            Check::RunnerNotAdmin => "runner_not_admin",
            Check::AdminsLocked => "admins_locked",
            Check::RemoteLogin => "remote_login",
            Check::Filevault => "filevault",
            Check::AppleId => "apple_id",
            Check::SoftwareUpdate => "software_update",
            Check::Xcode => "xcode",
            Check::Runtimes => "runtimes",
            Check::BrewOwner => "brew_owner",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Requested,
    GuestShutdown,
    StartFailed,
    WallViolation,
    AgentError,
    LauncherExited,
    /// The JIT runner exited without ever being handed a job, by the guest
    /// agent's closed word (the launcher child's exit 6). Its console is kept.
    RunnerExitedWithoutJob,
}

impl StopReason {
    /// The wire's word for it, as the supervisor sends it.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Requested => "requested",
            StopReason::GuestShutdown => "guest_shutdown",
            StopReason::StartFailed => "start_failed",
            StopReason::WallViolation => "wall_violation",
            StopReason::AgentError => "agent_error",
            StopReason::LauncherExited => "launcher_exited",
            StopReason::RunnerExitedWithoutJob => "runner_exited_without_job",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    HandshakeRequired,
    BadRequest,
    SlotBusy,
    ClaimExists,
    UnknownClaim,
    WrongState,
    Unmanaged,
    ImageNotFound,
    ToolsNotFound,
    Capacity,
    StartFailed,
    Internal,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// The payload of an operation with no arguments: exactly `{}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: ProtocolVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub slot: Slot,
    pub kind: VmKind,
    pub claim: ClaimId,
    pub image: ImageName,
    /// Required for macOS, forbidden for Linux.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub tools: Option<ImageName>,
    pub cpus: Cpus,
    pub memory_mib: MemoryMib,
    /// Linux only, optional (v4): grow the stopped VM's disk file to this size
    /// before it boots. Never shrinks it (ADR-0061 §4).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub disk_bytes: Option<DiskBytes>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub claim: ClaimId,
}

/// `jit_config` is required for a macOS VM and forbidden for a Linux one, but the
/// request does not say which kind the claim is. So the type leaves it optional and
/// the supervisor checks it against the VM, answering `bad_request` on a mismatch.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Begin {
    pub claim: ClaimId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub jit_config: Option<JitConfig>,
}

/// The Xcode a `verify` expects, from the image's manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XcodeExpect {
    pub version: XcodeVersion,
    pub build: BuildToken,
}

/// One simulator runtime a `verify` expects ready, from the image's manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExpect {
    pub platform: RuntimePlatform,
    pub version: RuntimeVersion,
    pub build: BuildToken,
}

/// What the guest's checks are held to (ADR-0049 §7): faber sends expectations
/// in, from the manifest it hashed, and gets fixed codes out. A bundle without a
/// manifest expects no particular Xcode (only that `xcodebuild -version` runs)
/// and no runtimes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub xcode: Option<XcodeExpect>,
    pub runtimes: Vec<RuntimeExpect>,
}

/// `verify`: run the image's checks in a proven VM instead of a job. Valid in
/// `probe_sent`, as `begin` is, and only for a macOS VM. No JIT config exists for
/// such a VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    pub claim: ClaimId,
    pub expect: Expect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub claim: ClaimId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adopt {
    pub claims: Vec<ClaimId>,
}

/// One request: externally tagged, so it is `{"list":{}}` and never a bare string.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello(Hello),
    Start(Start),
    Probe(Probe),
    Begin(Begin),
    Verify(Verify),
    Stop(Stop),
    List(Empty),
    Adopt(Adopt),
    Exit(Empty),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFrame {
    pub id: FrameId,
    pub request: Request,
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    pub protocol: ProtocolVersion,
    pub supervisor_version: SupervisorVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmRecord {
    pub slot: Slot,
    pub kind: VmKind,
    pub claim: ClaimId,
    pub state: VmState,
    /// Present exactly when `state` is `stopped`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub stop_reason: Option<StopReason>,
    /// Present exactly when `state` is `verified`: the checks that failed, each
    /// once, in [`Check::ALL`]'s order. Empty is a pass.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub failed: Option<Vec<Check>>,
    pub started_unix: UnixSecs,
    /// A Linux VM's disk file: its length, and the bytes it holds (v4, ADR-0061
    /// §4). Both or neither, and never on macOS; absent when the supervisor cannot
    /// tell which file the VM boots from.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub disk_bytes: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub disk_allocated_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmList {
    pub vms: Vec<VmRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adopted {
    pub found: Vec<VmRecord>,
    pub unknown: Vec<ClaimId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorReply {
    pub code: ErrorCode,
    /// Present exactly when `code` is `version_mismatch`: the one version the
    /// supervisor speaks.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_null"
    )]
    pub protocol: Option<ProtocolVersion>,
}

/// One reply: externally tagged. `vm` carries a VM record directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Hello(HelloReply),
    Vm(VmRecord),
    Vms(VmList),
    Adopted(Adopted),
    Done(Empty),
    Error(ErrorReply),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseFrame {
    pub id: FrameId,
    pub reply: Reply,
}

// ---------------------------------------------------------------------------
// Cross-field validation
// ---------------------------------------------------------------------------

impl Request {
    pub fn validate(&self) -> Result<(), ProtoError> {
        match self {
            Request::Start(s) => {
                match (s.kind, &s.tools) {
                    (VmKind::Macos, Some(_)) | (VmKind::Linux, None) => {}
                    _ => return Err(ProtoError::Invalid("start: tools")),
                }
                match s.disk_bytes {
                    Some(_) if s.kind != VmKind::Linux => {
                        Err(ProtoError::Invalid("start: disk_bytes on macos"))
                    }
                    Some(d) if d.get() % DISK_BYTES_ALIGN != 0 => {
                        Err(ProtoError::Invalid("start: disk_bytes not whole MiB"))
                    }
                    _ => Ok(()),
                }
            }
            Request::Adopt(a) => {
                if a.claims.is_empty() || a.claims.len() > MAX_LIST {
                    return Err(ProtoError::Invalid("adopt: claims count"));
                }
                if has_duplicates(a.claims.iter().map(ClaimId::as_str)) {
                    return Err(ProtoError::Invalid("adopt: duplicate claim"));
                }
                Ok(())
            }
            Request::Verify(v) => {
                if v.expect.runtimes.len() > MAX_RUNTIMES {
                    return Err(ProtoError::Invalid("verify: runtimes count"));
                }
                if v.expect
                    .xcode
                    .as_ref()
                    .is_some_and(|x| x.build.as_str().len() < 3)
                    || v.expect.runtimes.iter().any(|r| r.build.as_str().len() < 3)
                {
                    return Err(ProtoError::Invalid("verify: build"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl RequestFrame {
    pub fn validate(&self) -> Result<(), ProtoError> {
        self.request.validate()
    }
}

impl VmRecord {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if (self.state == VmState::Stopped) != self.stop_reason.is_some() {
            return Err(ProtoError::Invalid("vm: stop_reason"));
        }
        if (self.state == VmState::Verified) != self.failed.is_some() {
            return Err(ProtoError::Invalid("vm: failed"));
        }
        if let Some(failed) = &self.failed {
            // Each check once, in the agent's order: a strictly increasing list.
            if !failed.windows(2).all(|w| w[0] < w[1]) {
                return Err(ProtoError::Invalid("vm: failed order"));
            }
            if self.kind != VmKind::Macos {
                return Err(ProtoError::Invalid("vm: failed on linux"));
            }
        }
        match (self.disk_bytes, self.disk_allocated_bytes) {
            (None, None) => {}
            (Some(len), Some(held)) => {
                if self.kind != VmKind::Linux {
                    return Err(ProtoError::Invalid("vm: disk on macos"));
                }
                if len > DISK_REPORT_MAX || held > DISK_REPORT_MAX {
                    return Err(ProtoError::Invalid("vm: disk size"));
                }
            }
            _ => return Err(ProtoError::Invalid("vm: disk fields")),
        }
        Ok(())
    }
}

fn has_duplicates<'a>(items: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    items.into_iter().any(|i| !seen.insert(i))
}

impl Reply {
    pub fn validate(&self) -> Result<(), ProtoError> {
        match self {
            Reply::Vm(v) => v.validate(),
            Reply::Vms(l) => {
                if l.vms.len() > MAX_LIST {
                    return Err(ProtoError::Invalid("vms: count"));
                }
                for v in &l.vms {
                    v.validate()?;
                }
                if !l.vms.windows(2).all(|w| w[0].slot < w[1].slot) {
                    return Err(ProtoError::Invalid("vms: slot order"));
                }
                if has_duplicates(l.vms.iter().map(|v| v.claim.as_str())) {
                    return Err(ProtoError::Invalid("vms: duplicate claim"));
                }
                Ok(())
            }
            Reply::Adopted(a) => {
                if a.found.len() > MAX_LIST || a.unknown.len() > MAX_LIST {
                    return Err(ProtoError::Invalid("adopted: count"));
                }
                for v in &a.found {
                    v.validate()?;
                }
                let slots: Vec<_> = a.found.iter().map(|v| v.slot).collect();
                if (1..slots.len()).any(|i| slots[..i].contains(&slots[i])) {
                    return Err(ProtoError::Invalid("adopted: duplicate slot"));
                }
                let claims = a
                    .found
                    .iter()
                    .map(|v| v.claim.as_str())
                    .chain(a.unknown.iter().map(ClaimId::as_str));
                if has_duplicates(claims) {
                    return Err(ProtoError::Invalid("adopted: duplicate claim"));
                }
                Ok(())
            }
            Reply::Error(e) => {
                if (e.code == ErrorCode::VersionMismatch) != e.protocol.is_some() {
                    return Err(ProtoError::Invalid("error: protocol"));
                }
                Ok(())
            }
            Reply::Hello(_) | Reply::Done(_) => Ok(()),
        }
    }
}

impl ResponseFrame {
    pub fn validate(&self) -> Result<(), ProtoError> {
        self.reply.validate()
    }
}

// ---------------------------------------------------------------------------
// Encode and decode
// ---------------------------------------------------------------------------

fn terminate(mut body: Vec<u8>) -> Result<Vec<u8>, ProtoError> {
    body.push(b'\n');
    if body.len() > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge);
    }
    Ok(body)
}

fn encode<T: Serialize>(frame: &T) -> Result<Vec<u8>, ProtoError> {
    terminate(serde_json::to_vec(frame).map_err(|_| ProtoError::Encode)?)
}

/// Check the framing and parse the one JSON value. The error is a category and a
/// position, never serde's message, which would quote the peer's bytes.
fn parse<T: DeserializeOwned>(line: &[u8]) -> Result<T, ProtoError> {
    if line.len() > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge);
    }
    let body = match line.split_last() {
        Some((b'\n', body)) => body,
        _ => return Err(ProtoError::NotOneLine),
    };
    if body.iter().any(|&c| c == b'\n' || c == b'\r') {
        return Err(ProtoError::NotOneLine);
    }
    let text = std::str::from_utf8(body).map_err(|_| ProtoError::NotUtf8)?;
    let edge = |c: Option<u8>| c.is_some_and(|c| c.is_ascii_whitespace());
    if edge(body.first().copied()) || edge(body.last().copied()) {
        return Err(ProtoError::Invalid("whitespace around frame"));
    }
    let malformed = |e: serde_json::Error| ProtoError::Malformed {
        line: e.line(),
        column: e.column(),
    };
    // The typed pass refuses duplicate keys, nulls and unknown fields. It also
    // accepts a struct written as a JSON array, so a second pass checks the shape.
    let typed = serde_json::from_str(text).map_err(malformed)?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(malformed)?;
    if !value.is_object() {
        return Err(ProtoError::Invalid("array where an object belongs"));
    }
    check_shape(&value, false)?;
    Ok(typed)
}

/// Every protocol object is a JSON object. An array is allowed only as the value of
/// the list fields, and never inside another array.
fn check_shape(v: &serde_json::Value, array_ok: bool) -> Result<(), ProtoError> {
    use serde_json::Value;
    const BAD: ProtoError = ProtoError::Invalid("array where an object belongs");
    match v {
        Value::Object(m) => m.iter().try_for_each(|(k, x)| {
            check_shape(
                x,
                matches!(
                    k.as_str(),
                    "claims" | "vms" | "found" | "unknown" | "runtimes" | "failed"
                ),
            )
        }),
        Value::Array(a) if array_ok => a.iter().try_for_each(|x| {
            if x.is_array() {
                Err(BAD)
            } else {
                check_shape(x, false)
            }
        }),
        Value::Array(_) => Err(BAD),
        _ => Ok(()),
    }
}

/// Encode a request: compact JSON and one LF. Refuses an invalid or oversized frame.
pub fn encode_request(frame: &RequestFrame) -> Result<Vec<u8>, ProtoError> {
    frame.validate()?;
    encode(frame)
}

/// Decode and validate one response line, LF included.
pub fn decode_response(line: &[u8]) -> Result<ResponseFrame, ProtoError> {
    let frame: ResponseFrame = parse(line)?;
    frame.validate()?;
    Ok(frame)
}

/// The supervisor's direction, for the fixtures and as a reference. `_faber`
/// itself never decodes a request.
pub fn decode_request(line: &[u8]) -> Result<RequestFrame, ProtoError> {
    let frame: RequestFrame = parse(line)?;
    frame.validate()?;
    Ok(frame)
}

/// The supervisor's direction, for the fixtures and as a reference.
pub fn encode_response(frame: &ResponseFrame) -> Result<Vec<u8>, ProtoError> {
    frame.validate()?;
    encode(frame)
}

// ---------------------------------------------------------------------------
// Does the reply fit
// ---------------------------------------------------------------------------

/// The response carries the id that was sent.
pub fn check_id(sent: FrameId, got: &ResponseFrame) -> Result<(), ProtoError> {
    if sent == got.id {
        Ok(())
    } else {
        Err(ProtoError::IdMismatch)
    }
}

/// Whether the `adopted` reply holds exactly the claims asked: each one once,
/// either found or unknown, and nothing else.
pub fn check_adopted(asked: &[ClaimId], reply: &Adopted) -> Result<(), ProtoError> {
    let asked_set: HashSet<&str> = asked.iter().map(ClaimId::as_str).collect();
    let got: Vec<&str> = reply
        .found
        .iter()
        .map(|v| v.claim.as_str())
        .chain(reply.unknown.iter().map(ClaimId::as_str))
        .collect();
    let got_set: HashSet<&str> = got.iter().copied().collect();
    if asked_set.len() == asked.len() && got_set.len() == got.len() && got_set == asked_set {
        Ok(())
    } else {
        Err(ProtoError::WrongReply)
    }
}

/// Whether `reply` is a legal answer to `request`. Assumes both are validated.
pub fn check_reply(request: &Request, reply: &Reply) -> Result<(), ProtoError> {
    let vm_is = |v: &VmRecord, claim: &ClaimId| v.claim == *claim;
    let fits = match (request, reply) {
        (Request::Hello(h), Reply::Hello(r)) => {
            r.protocol.get() == PROTOCOL_VERSION && r.protocol == h.protocol
        }
        (Request::Start(s), Reply::Vm(v)) => {
            v.slot == s.slot && v.kind == s.kind && vm_is(v, &s.claim)
        }
        (Request::Probe(p), Reply::Vm(v)) => vm_is(v, &p.claim) && v.state == VmState::ProbeSent,
        (Request::Begin(b), Reply::Vm(v)) => vm_is(v, &b.claim) && v.state == VmState::InService,
        (Request::Verify(r), Reply::Vm(v)) => {
            vm_is(v, &r.claim) && v.kind == VmKind::Macos && v.state == VmState::Verifying
        }
        (Request::List(_), Reply::Vms(_)) => true,
        (Request::Adopt(a), Reply::Adopted(r)) => return check_adopted(&a.claims, r),
        (Request::Stop(_) | Request::Exit(_), Reply::Done(_)) => true,
        (req, Reply::Error(e)) => {
            e.code != ErrorCode::VersionMismatch || matches!(req, Request::Hello(_))
        }
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err(ProtoError::WrongReply)
    }
}

/// The handshake verdict: the supervisor's version, only if it speaks exactly
/// [`PROTOCOL_VERSION`]. No negotiation and no fallback.
pub fn check_hello(reply: &Reply) -> Result<&SupervisorVersion, ProtoError> {
    match reply {
        Reply::Hello(h) if h.protocol.get() == PROTOCOL_VERSION => Ok(&h.supervisor_version),
        Reply::Hello(h) => Err(ProtoError::VersionMismatch {
            theirs: h.protocol.get(),
        }),
        Reply::Error(ErrorReply {
            code: ErrorCode::VersionMismatch,
            protocol: Some(p),
        }) => Err(ProtoError::VersionMismatch { theirs: p.get() }),
        _ => Err(ProtoError::WrongReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(sub: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vm-proto")
            .join(sub)
    }

    fn files(sub: &str) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<_> = std::fs::read_dir(dir(sub))
            .unwrap_or_else(|_| panic!("missing fixture dir {sub}"))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                )
            })
            .collect();
        out.sort();
        assert!(!out.is_empty(), "no fixtures in {sub}");
        out
    }

    fn claim(s: &str) -> ClaimId {
        ClaimId::try_from(s).unwrap()
    }
    fn name(s: &str) -> ImageName {
        ImageName::try_from(s).unwrap()
    }
    fn id(n: u32) -> FrameId {
        FrameId::try_from(n).unwrap()
    }
    fn record(slot: u8, state: VmState, reason: Option<StopReason>) -> VmRecord {
        VmRecord {
            slot: Slot::try_from(slot).unwrap(),
            kind: VmKind::Macos,
            claim: claim(&format!("c{slot}")),
            state,
            stop_reason: reason,
            failed: None,
            started_unix: UnixSecs::try_from(1_760_700_000).unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        }
    }
    fn start_macos() -> Start {
        Start {
            slot: Slot::try_from(0).unwrap(),
            kind: VmKind::Macos,
            claim: claim("c0"),
            image: name("macos-27"),
            tools: Some(name("tools-1")),
            cpus: Cpus::try_from(4).unwrap(),
            memory_mib: MemoryMib::try_from(8192).unwrap(),
            disk_bytes: None,
        }
    }

    #[test]
    fn valid_requests_round_trip_byte_exact() {
        for (n, bytes) in files("requests/valid") {
            let f = decode_request(&bytes).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!(encode_request(&f).unwrap(), bytes, "{n}");
        }
    }

    #[test]
    fn valid_responses_round_trip_byte_exact() {
        for (n, bytes) in files("responses/valid") {
            let f = decode_response(&bytes).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!(encode_response(&f).unwrap(), bytes, "{n}");
        }
    }

    #[test]
    fn invalid_requests_are_refused() {
        for (n, bytes) in files("requests/invalid") {
            assert!(decode_request(&bytes).is_err(), "{n} was accepted");
        }
    }

    #[test]
    fn invalid_responses_are_refused() {
        for (n, bytes) in files("responses/invalid") {
            assert!(decode_response(&bytes).is_err(), "{n} was accepted");
        }
    }

    #[test]
    fn refused_hello_decodes_but_fails_the_handshake() {
        for (n, bytes) in files("responses/refused-hello") {
            let f = decode_response(&bytes).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert!(check_hello(&f.reply).is_err(), "{n} passed the handshake");
        }
        let ok = std::fs::read(dir("responses/valid/hello.json")).unwrap();
        let f = decode_response(&ok).unwrap();
        assert_eq!(check_hello(&f.reply).unwrap().as_str(), "0.4.0");
    }

    #[test]
    fn check_hello_reports_their_version() {
        let f = decode_response(
            &std::fs::read(dir("responses/valid/error-version-mismatch.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            check_hello(&f.reply),
            Err(ProtoError::VersionMismatch { theirs: 1 })
        );
    }

    /// ADR-0049 §7: the check codes are a closed set, written as the docs and the
    /// launcher spell them, and `ALL` is their order (the order `failed` keeps).
    #[test]
    fn the_check_codes_are_closed_and_ordered() {
        let mut sorted = Check::ALL;
        sorted.sort();
        assert_eq!(sorted, Check::ALL);
        for c in Check::ALL {
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                serde_json::Value::String(c.as_str().into())
            );
        }
        let names: Vec<&str> = Check::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            [
                "console_user",
                "runner_not_admin",
                "admins_locked",
                "remote_login",
                "filevault",
                "apple_id",
                "software_update",
                "xcode",
                "runtimes",
                "brew_owner"
            ]
        );
        let verify = Request::Verify(Verify {
            claim: claim("c0"),
            expect: Expect {
                xcode: None,
                runtimes: vec![],
            },
        });
        let mut v = record(0, VmState::Verifying, None);
        assert!(check_reply(&verify, &Reply::Vm(v.clone())).is_ok());
        v.state = VmState::InService;
        assert!(check_reply(&verify, &Reply::Vm(v.clone())).is_err());
        v.state = VmState::Verifying;
        v.kind = VmKind::Linux;
        assert!(check_reply(&verify, &Reply::Vm(v)).is_err());
    }

    #[test]
    fn every_request_variant_round_trips() {
        let reqs = vec![
            Request::Hello(Hello {
                protocol: ProtocolVersion::try_from(1).unwrap(),
            }),
            Request::Start(start_macos()),
            Request::Start(Start {
                kind: VmKind::Linux,
                tools: None,
                ..start_macos()
            }),
            Request::Probe(Probe { claim: claim("c0") }),
            Request::Begin(Begin {
                claim: claim("c0"),
                jit_config: Some(JitConfig::try_from("ZmFrZQ==").unwrap()),
            }),
            Request::Begin(Begin {
                claim: claim("c0"),
                jit_config: None,
            }),
            Request::Stop(Stop { claim: claim("c0") }),
            Request::List(Empty {}),
            Request::Adopt(Adopt {
                claims: vec![claim("a"), claim("b")],
            }),
            Request::Exit(Empty {}),
        ];
        for request in reqs {
            let f = RequestFrame { id: id(9), request };
            let bytes = encode_request(&f).unwrap();
            assert_eq!(decode_request(&bytes).unwrap(), f);
        }
    }

    #[test]
    fn every_reply_variant_round_trips() {
        let replies = vec![
            Reply::Hello(HelloReply {
                protocol: ProtocolVersion::try_from(1).unwrap(),
                supervisor_version: SupervisorVersion::try_from("0.4.0+x").unwrap(),
            }),
            Reply::Vm(record(0, VmState::Ready, None)),
            Reply::Vm(record(0, VmState::Stopped, Some(StopReason::Requested))),
            Reply::Vms(VmList { vms: vec![] }),
            Reply::Vms(VmList {
                vms: vec![
                    record(0, VmState::Ready, None),
                    record(3, VmState::Unmanaged, None),
                ],
            }),
            Reply::Adopted(Adopted {
                found: vec![record(1, VmState::InService, None)],
                unknown: vec![claim("gone")],
            }),
            Reply::Done(Empty {}),
            Reply::Error(ErrorReply {
                code: ErrorCode::SlotBusy,
                protocol: None,
            }),
            Reply::Error(ErrorReply {
                code: ErrorCode::VersionMismatch,
                protocol: Some(ProtocolVersion::try_from(2).unwrap()),
            }),
        ];
        for reply in replies {
            let f = ResponseFrame { id: id(9), reply };
            let bytes = encode_response(&f).unwrap();
            assert_eq!(decode_response(&bytes).unwrap(), f);
        }
    }

    #[test]
    fn encoders_refuse_semantically_invalid_frames() {
        let bad = RequestFrame {
            id: id(1),
            request: Request::Start(Start {
                tools: None,
                ..start_macos()
            }),
        };
        assert!(encode_request(&bad).is_err());
        let bad = ResponseFrame {
            id: id(1),
            reply: Reply::Vm(record(0, VmState::Stopped, None)),
        };
        assert!(encode_response(&bad).is_err());
    }

    #[test]
    fn strictness_details() {
        let ok = |s: &str| decode_request(format!("{s}\n").as_bytes());
        assert!(ok(r#"{"id":1,"request":{"list":{}}}"#).is_ok());
        assert!(ok(r#"{"id":1,"request":{"list":{"x":1}}}"#).is_err());
        assert!(ok(r#"{"id":1,"request":"list"}"#).is_err());
        // two operations in one request
        assert!(ok(r#"{"id":1,"request":{"list":{},"exit":{}}}"#).is_err());
        // duplicate keys
        assert!(ok(r#"{"id":1,"id":2,"request":{"list":{}}}"#).is_err());
        assert!(ok(r#"{"id":1,"request":{"stop":{"claim":"a","claim":"b"}}}"#).is_err());
        // numbers: floats, exponents, negatives, out of range
        for id in ["1.0", "1e0", "-1", "0", "4294967296", "\"1\""] {
            assert!(
                ok(&format!(r#"{{"id":{id},"request":{{"list":{{}}}}}}"#)).is_err(),
                "{id}"
            );
        }
        assert!(ok(r#"{"id":4294967295,"request":{"list":{}}}"#).is_ok());
        // null for an optional field
        let s = r#"{"id":1,"request":{"start":{"slot":0,"kind":"linux","claim":"a","image":"b","tools":null,"cpus":1,"memory_mib":1024}}}"#;
        assert!(ok(s).is_err());
        // protocol bounds
        for p in ["0", "65536", "-1", "1.0"] {
            assert!(ok(&format!(
                r#"{{"id":1,"request":{{"hello":{{"protocol":{p}}}}}}}"#
            ))
            .is_err());
        }
        assert!(ok(r#"{"id":1,"request":{"hello":{"protocol":65535}}}"#).is_ok());
    }

    #[test]
    fn grammar_boundaries() {
        assert!(ClaimId::try_from("a".repeat(64)).is_ok());
        assert!(ClaimId::try_from("a".repeat(65)).is_err());
        assert!(ClaimId::try_from("").is_err());
        assert!(ClaimId::try_from("-a").is_err());
        assert!(ClaimId::try_from(".a").is_err());
        assert!(ClaimId::try_from("A1._-").is_ok());
        assert!(ClaimId::try_from("a/b").is_err());
        assert!(ImageName::try_from("Mac").is_err());
        assert!(ImageName::try_from("../x").is_err());
        assert!(ImageName::try_from("a.b_c-d").is_ok());
        assert!(SupervisorVersion::try_from("0.4.0+g1-x").is_ok());
        assert!(SupervisorVersion::try_from("0.4_0").is_err());
        assert!(JitConfig::try_from("a".repeat(8192)).is_ok());
        assert!(JitConfig::try_from("a".repeat(8193)).is_err());
        assert!(JitConfig::try_from("ab-c").is_err());
        assert!(JitConfig::try_from("").is_err());
        assert!(Slot::try_from(15).is_ok() && Slot::try_from(16).is_err());
        assert!(Cpus::try_from(0).is_err() && Cpus::try_from(64).is_ok());
        assert!(Cpus::try_from(65).is_err());
        assert!(MemoryMib::try_from(1023).is_err() && MemoryMib::try_from(1024).is_ok());
        assert!(MemoryMib::try_from(262_144).is_ok() && MemoryMib::try_from(262_145).is_err());
        assert!(UnixSecs::try_from(MAX_UNIX).is_ok() && UnixSecs::try_from(MAX_UNIX + 1).is_err());
    }

    #[test]
    fn framing_rules() {
        let good = b"{\"id\":1,\"request\":{\"list\":{}}}\n";
        assert!(decode_request(good).is_ok());
        assert_eq!(
            decode_request(&good[..good.len() - 1]),
            Err(ProtoError::NotOneLine)
        );
        assert_eq!(decode_request(b""), Err(ProtoError::NotOneLine));
        let crlf = b"{\"id\":1,\"request\":{\"list\":{}}}\r\n";
        assert_eq!(decode_request(crlf), Err(ProtoError::NotOneLine));
        let two = b"{\"id\":1,\"request\":{\"list\":{}}}\n{\"id\":2,\"request\":{\"list\":{}}}\n";
        assert_eq!(decode_request(two), Err(ProtoError::NotOneLine));
        assert!(decode_request(b" {\"id\":1,\"request\":{\"list\":{}}}\n").is_err());
        assert!(decode_request(b"{\"id\":1,\"request\":{\"list\":{}}} \n").is_err());
        assert_eq!(decode_request(b"\xff\n"), Err(ProtoError::NotUtf8));
        let big = vec![b'a'; MAX_FRAME];
        let mut line = big.clone();
        line.push(b'\n');
        assert_eq!(decode_request(&line), Err(ProtoError::FrameTooLarge));
        assert_eq!(terminate(big), Err(ProtoError::FrameTooLarge));
        assert!(terminate(vec![b'a'; MAX_FRAME - 1]).is_ok());
    }

    #[test]
    fn largest_valid_frames_fit() {
        // Nothing validated can exceed MAX_FRAME, so the encoder's size check
        // cannot be reached with real values; assert the headroom instead.
        let begin = RequestFrame {
            id: id(u32::MAX),
            request: Request::Begin(Begin {
                claim: claim(&"a".repeat(64)),
                jit_config: Some(JitConfig::try_from("A".repeat(MAX_JIT_CONFIG_LEN)).unwrap()),
            }),
        };
        assert!(encode_request(&begin).unwrap().len() < MAX_FRAME);
        let adopt = RequestFrame {
            id: id(u32::MAX),
            request: Request::Adopt(Adopt {
                claims: (0..16)
                    .map(|i| claim(&format!("{i:x}{}", "a".repeat(63))))
                    .collect(),
            }),
        };
        assert!(encode_request(&adopt).unwrap().len() < MAX_FRAME);
        let vms = (0..16)
            .map(|i| VmRecord {
                claim: claim(&format!("{i:x}{}", "a".repeat(63))),
                ..record(i, VmState::Stopped, Some(StopReason::LauncherExited))
            })
            .collect::<Vec<_>>();
        let resp = ResponseFrame {
            id: id(u32::MAX),
            reply: Reply::Vms(VmList { vms }),
        };
        assert!(encode_response(&resp).unwrap().len() < MAX_FRAME);
    }

    #[test]
    fn list_and_record_semantics() {
        let list = |vms: Vec<VmRecord>| Reply::Vms(VmList { vms }).validate();
        assert!(list(vec![
            record(0, VmState::Ready, None),
            record(1, VmState::Ready, None)
        ])
        .is_ok());
        assert!(list(vec![
            record(1, VmState::Ready, None),
            record(0, VmState::Ready, None)
        ])
        .is_err());
        assert!(list(vec![
            record(1, VmState::Ready, None),
            record(1, VmState::Ready, None)
        ])
        .is_err());
        let mut dup = record(1, VmState::Ready, None);
        dup.claim = claim("c0");
        assert!(list(vec![record(0, VmState::Ready, None), dup]).is_err());
        assert!(list(
            (0..17)
                .map(|i| record(i % 16, VmState::Ready, None))
                .collect()
        )
        .is_err());
        assert!(record(0, VmState::Ready, Some(StopReason::Requested))
            .validate()
            .is_err());
        assert!(record(0, VmState::Stopped, None).validate().is_err());
        assert!(Reply::Error(ErrorReply {
            code: ErrorCode::Internal,
            protocol: Some(ProtocolVersion::try_from(1).unwrap())
        })
        .validate()
        .is_err());
        assert!(Reply::Error(ErrorReply {
            code: ErrorCode::VersionMismatch,
            protocol: None
        })
        .validate()
        .is_err());
        let adopted = |found: Vec<VmRecord>, unknown: Vec<ClaimId>| {
            Reply::Adopted(Adopted { found, unknown }).validate()
        };
        assert!(adopted(vec![record(0, VmState::Ready, None)], vec![claim("x")]).is_ok());
        assert!(adopted(vec![record(0, VmState::Ready, None)], vec![claim("c0")]).is_err());
        assert!(adopted(vec![], vec![claim("x"), claim("x")]).is_err());
        let a = |claims: Vec<ClaimId>| Request::Adopt(Adopt { claims }).validate();
        assert!(a(vec![]).is_err());
        assert!(a(vec![claim("a"), claim("a")]).is_err());
        assert!(a((0..17).map(|i| claim(&format!("a{i}"))).collect()).is_err());
        assert!(a((0..16).map(|i| claim(&format!("a{i}"))).collect()).is_ok());
    }

    #[test]
    fn check_reply_matches_requests() {
        let stop = Request::Stop(Stop { claim: claim("c0") });
        let vms = Reply::Vms(VmList { vms: vec![] });
        assert_eq!(check_reply(&stop, &vms), Err(ProtoError::WrongReply));
        assert!(check_reply(&stop, &Reply::Done(Empty {})).is_ok());
        assert!(check_reply(&Request::Exit(Empty {}), &Reply::Done(Empty {})).is_ok());
        assert!(check_reply(&Request::List(Empty {}), &vms).is_ok());

        let adopt = Request::Adopt(Adopt {
            claims: vec![claim("c0"), claim("gone")],
        });
        let good = Reply::Adopted(Adopted {
            found: vec![record(0, VmState::Ready, None)],
            unknown: vec![claim("gone")],
        });
        assert!(check_reply(&adopt, &good).is_ok());
        for bad in [
            Adopted {
                found: vec![record(0, VmState::Ready, None)],
                unknown: vec![],
            },
            Adopted {
                found: vec![],
                unknown: vec![claim("c0"), claim("gone"), claim("extra")],
            },
            Adopted {
                found: vec![record(0, VmState::Ready, None)],
                unknown: vec![claim("other")],
            },
        ] {
            assert_eq!(
                check_reply(&adopt, &Reply::Adopted(bad)),
                Err(ProtoError::WrongReply)
            );
        }

        let start = Request::Start(start_macos());
        assert!(check_reply(&start, &Reply::Vm(record(0, VmState::Starting, None))).is_ok());
        let mut other = record(0, VmState::Starting, None);
        other.claim = claim("other");
        assert!(check_reply(&start, &Reply::Vm(other)).is_err());
        let mut other = record(0, VmState::Starting, None);
        other.kind = VmKind::Linux;
        assert!(check_reply(&start, &Reply::Vm(other)).is_err());
        assert!(check_reply(&start, &Reply::Vm(record(1, VmState::Starting, None))).is_err());

        let probe = Request::Probe(Probe { claim: claim("c0") });
        assert!(check_reply(&probe, &Reply::Vm(record(0, VmState::ProbeSent, None))).is_ok());
        assert!(check_reply(&probe, &Reply::Vm(record(0, VmState::Ready, None))).is_err());
        let begin = Request::Begin(Begin {
            claim: claim("c0"),
            jit_config: None,
        });
        assert!(check_reply(&begin, &Reply::Vm(record(0, VmState::InService, None))).is_ok());
        assert!(check_reply(&begin, &Reply::Vm(record(2, VmState::InService, None))).is_err());

        let hello = Request::Hello(Hello {
            protocol: ProtocolVersion::try_from(PROTOCOL_VERSION).unwrap(),
        });
        let hello_reply = |p| {
            Reply::Hello(HelloReply {
                protocol: ProtocolVersion::try_from(p).unwrap(),
                supervisor_version: SupervisorVersion::try_from("0.4.0").unwrap(),
            })
        };
        assert!(check_reply(&hello, &hello_reply(PROTOCOL_VERSION)).is_ok());
        assert!(check_reply(&hello, &hello_reply(PROTOCOL_VERSION - 1)).is_err());
        assert!(check_reply(&stop, &hello_reply(PROTOCOL_VERSION)).is_err());

        let mismatch = Reply::Error(ErrorReply {
            code: ErrorCode::VersionMismatch,
            protocol: Some(ProtocolVersion::try_from(2).unwrap()),
        });
        assert!(check_reply(&hello, &mismatch).is_ok());
        assert!(check_reply(&stop, &mismatch).is_err());
        let unknown = Reply::Error(ErrorReply {
            code: ErrorCode::UnknownClaim,
            protocol: None,
        });
        assert!(check_reply(&stop, &unknown).is_ok());
    }

    #[test]
    fn check_adopted_refuses_duplicates_itself() {
        let dup = Adopted {
            found: vec![record(0, VmState::Ready, None)],
            unknown: vec![claim("c0")],
        };
        assert!(check_adopted(&[claim("c0"), claim("b")], &dup).is_err());
        let ok = Adopted {
            found: vec![],
            unknown: vec![claim("c0")],
        };
        assert!(check_adopted(&[claim("c0"), claim("c0")], &ok).is_err());
        assert!(check_adopted(&[claim("c0")], &ok).is_ok());
    }

    #[test]
    fn array_forms_are_refused() {
        for f in [
            r#"{"id":1,"reply":{"done":[]}}"#,
            r#"{"id":1,"reply":{"hello":[1,"0.4.0"]}}"#,
            r#"{"id":1,"reply":{"vm":[0,"macos","a","stopped","requested",1]}}"#,
            r#"[1,{"done":{}}]"#,
            r#"{"id":1,"reply":{"vms":{"vms":[[]]}}}"#,
        ] {
            assert!(decode_response(format!("{f}\n").as_bytes()).is_err(), "{f}");
        }
        assert!(decode_request(b"{\"id\":2,\"request\":{\"stop\":[\"a\"]}}\n").is_err());
    }

    #[test]
    fn check_id_matches() {
        let f = ResponseFrame {
            id: id(3),
            reply: Reply::Done(Empty {}),
        };
        assert!(check_id(id(3), &f).is_ok());
        assert_eq!(check_id(id(4), &f), Err(ProtoError::IdMismatch));
    }

    #[test]
    fn errors_never_echo_the_peer() {
        let frames = [
            r#"{"id":1,"request":{"list":{}},"zzsecretzz":1}"#,
            r#"{"id":1,"request":{"list":{"zzsecretzz":1}}}"#,
            r#"{"id":1,"request":{"zzsecretzz":{}}}"#,
            r#"{"id":1,"request":{"stop":{"claim":"zzsecretzz!"}}}"#,
            r#"{"id":1,"request":{"stop":{"claim":"zzsecretzz","zzsecretzz":"zzsecretzz"}}}"#,
            r#"{"id":"zzsecretzz","request":{"list":{}}}"#,
            r#"{"id":1,"request":{"start":{"slot":0,"kind":"zzsecretzz","claim":"a","image":"b","cpus":1,"memory_mib":1024}}}"#,
        ];
        for f in frames {
            let line = format!("{f}\n");
            for e in [
                decode_request(line.as_bytes()).unwrap_err(),
                decode_response(line.as_bytes()).unwrap_err(),
            ] {
                let shown = format!("{e} {e:?}");
                assert!(!shown.contains("zzsecretzz"), "leaked: {shown}");
            }
        }
        let resp = r#"{"id":1,"reply":{"vm":{"slot":0,"kind":"macos","claim":"a","state":"zzsecretzz","started_unix":1}}}"#;
        let e = decode_response(format!("{resp}\n").as_bytes()).unwrap_err();
        assert!(!format!("{e} {e:?}").contains("zzsecretzz"));
    }

    #[test]
    fn jit_config_debug_is_redacted() {
        let j = JitConfig::try_from("c2VjcmV0c2VjcmV0").unwrap();
        assert_eq!(format!("{j:?}"), "JitConfig(<redacted, 16 bytes>)");
        let req = Request::Begin(Begin {
            claim: claim("c0"),
            jit_config: Some(j),
        });
        let shown = format!("{req:?} {:#?}", req);
        assert!(!shown.contains("c2VjcmV0"));
    }
}
