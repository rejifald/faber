//! The `_faber` side of the socket to the VM supervisor (ADR-0026, slice 3):
//! `docs/vm-supervisor-protocol.md` over a Unix stream.
//!
//! [`super::proto`] decides what a frame may say; this module decides who may say
//! it and how much `_faber` reads before deciding. The supervisor runs as
//! `_faber_vm` and handles traffic from guests where job code is root, while
//! `_faber` holds the GitHub credential (ADR-0025). So:
//!
//! - **The peer is checked before a byte is sent.** The kernel's peer uid
//!   ([`crate::platform::peer_uid`]) must be `_faber_vm`'s, or the connection is
//!   closed unwritten: a socket another account managed to put at the path gets
//!   nothing, not even a hello.
//! - **Reads are capped where they happen.** One frame is read through a
//!   [`MAX_FRAME`]-byte window; a peer that sends no LF within it is refused before
//!   faber buffers more, never after reading a line whole.
//! - **Anything off-script poisons the connection.** One request is in flight per
//!   connection (lockstep). A reply with another id, a reply kind that does not fit
//!   the request, an I/O error or a timeout drops the connection, and the next call
//!   reconnects and handshakes again. Nothing is carried across.
//! - **Silent about the peer.** [`ClientError`] names categories, never the peer's
//!   bytes, and a [`JitConfig`] is never formatted (its `Debug` is redacted).
//!
//! The handshake offers exactly [`PROTOCOL_VERSION`]. A supervisor speaking another
//! one answers `version_mismatch`, reported as [`ClientError::VersionMismatch`];
//! such a connection is good for `exit` only (the frozen core), which is how a new
//! faber retires an old supervisor ([`super::restart`]). Separately, faber starts
//! no new VM while the supervisor's own version differs from faber's stamped
//! `FABER_VERSION` ([`ClientError::VersionSkew`]), though stop, list and adopt keep
//! working across the skew, so a half-applied release can still be cleaned up.

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::{timeout, Instant};

use super::job::{SupervisorError, VmSupervisor};
use super::proto::{
    check_hello, check_id, check_reply, decode_response, encode_request, Adopt, Adopted, Begin,
    ClaimId, Empty, ErrorCode, Expect, FrameId, Hello, JitConfig, Probe, ProtoError,
    ProtocolVersion, Reply, Request, RequestFrame, Start, Stop, SupervisorVersion, Verify,
    VmRecord, MAX_FRAME, PROTOCOL_VERSION,
};

/// Where the supervisor listens, and the only account allowed to answer there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub path: PathBuf,
    /// `_faber_vm`'s uid, from the trusted setup record.
    pub peer_uid: u32,
}

/// How long faber waits on the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Connecting, the peer check and the handshake together. A local socket
    /// answers in milliseconds; this only bounds a wedged supervisor.
    pub connect: Duration,
    /// One request and its reply, for everything but `start`.
    pub request: Duration,
    /// `start`. The supervisor clones the golden image and writes the claim
    /// marker before it answers; an APFS clone is quick, but on a host under disk
    /// pressure it is not, and timing out here is the expensive mistake: the job
    /// then tears down a VM that was starting fine.
    pub start: Duration,
    /// How long the connection may sit unused and still carry a request. The
    /// supervisor closes a connection idle for 30 s (`idleSeconds` in
    /// `launcher/Sources/Supervisor.swift`), and a request sent on one it has
    /// closed is lost: it reads as one that may have been acted on, and the
    /// caller cannot tell. One idle at least this long is replaced first.
    pub idle_reuse: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(30),
            start: Duration::from_secs(120),
            idle_reuse: Duration::from_secs(20),
        }
    }
}

/// Why a call failed. No variant carries or prints a byte from the peer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// No connection: the request was never sent.
    #[error("the VM supervisor is not reachable: {0}")]
    Unreachable(String),
    /// Something other than `_faber_vm` holds the socket. Nothing was sent.
    #[error("the VM supervisor socket is held by uid {uid}, not _faber_vm; nothing was sent")]
    WrongPeer { uid: u32 },
    /// The handshake failed: the supervisor's answer to `hello` broke the
    /// protocol. The request was never sent.
    #[error("the VM supervisor's handshake broke the protocol: {0}")]
    Handshake(ProtoError),
    /// The supervisor speaks another protocol. Only `exit` is sent to it.
    #[error("the VM supervisor speaks protocol {theirs}, faber speaks {PROTOCOL_VERSION}")]
    VersionMismatch { theirs: u16 },
    /// `start` is refused while the supervisor's version differs from faber's.
    /// Nothing was sent.
    #[error(
        "the VM supervisor is version {supervisor}, faber is {faber}: no new VM starts until they agree"
    )]
    VersionSkew { supervisor: String, faber: String },
    /// faber's own request does not fit the protocol. Nothing was sent.
    #[error("faber's request does not fit the protocol: {0}")]
    BadRequest(ProtoError),
    /// The supervisor answered with an error code: it did not act.
    #[error("the VM supervisor refused: {0:?}")]
    Refused(ErrorCode),
    /// The request was sent and no reply came in time: it may have been acted on.
    #[error("the VM supervisor did not answer in time")]
    NoReply,
    /// The connection broke after the request was sent: it may have been acted on.
    #[error("the connection to the VM supervisor was lost")]
    Lost,
    /// The reply broke the protocol; the connection is dropped.
    #[error("the VM supervisor broke the protocol: {0}")]
    Protocol(ProtoError),
}

impl From<ClientError> for SupervisorError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Unreachable(_) => SupervisorError::Unreachable,
            // Nothing was sent, but each of these means the host is misconfigured
            // or mid-release rather than merely down: say so where it is seen.
            ClientError::WrongPeer { .. }
            | ClientError::Handshake(_)
            | ClientError::VersionMismatch { .. }
            | ClientError::VersionSkew { .. } => {
                tracing::warn!("{e}");
                SupervisorError::Misconfigured
            }
            ClientError::Refused(code) => SupervisorError::Refused(code),
            ClientError::NoReply | ClientError::Lost => SupervisorError::NoReply,
            ClientError::BadRequest(p) | ClientError::Protocol(p) => SupervisorError::Protocol(p),
        }
    }
}

/// What the handshake left the connection able to do.
enum Peer {
    /// Speaks our protocol, at this supervisor version.
    Ready(SupervisorVersion),
    /// Speaks another protocol: `exit` only.
    Mismatched { theirs: u16 },
}

struct Conn {
    io: BufReader<UnixStream>,
    next_id: u32,
    peer: Peer,
    /// When the last request went out on it, which is when the supervisor
    /// last saw activity: it counts idle time from the requests it reads, not
    /// the replies it sends, so a slow `start` ages the connection too.
    used: Since,
}

/// A moment, on two clocks. `Instant` stops while the host sleeps and the
/// supervisor's clock (`Date()`) does not, so idle time is the larger of the
/// two; a wall clock stepped back counts as the limit, since replacing a
/// connection costs a handshake and sending on a dead one costs a request.
#[derive(Clone, Copy)]
struct Since {
    mono: Instant,
    wall: SystemTime,
}

impl Since {
    fn now() -> Self {
        Self {
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    fn idle_at_least(&self, limit: Duration) -> bool {
        self.mono.elapsed() >= limit
            || SystemTime::now()
                .duration_since(self.wall)
                .map_or(true, |d| d >= limit)
    }
}

/// The id after `n`: ids run 1 to `u32::MAX` and wrap to 1, never 0.
fn following(n: u32) -> u32 {
    if n == u32::MAX {
        1
    } else {
        n + 1
    }
}

/// Read one frame, never holding more than [`MAX_FRAME`] bytes of it.
async fn read_frame(io: &mut BufReader<UnixStream>) -> Result<Vec<u8>, ClientError> {
    let mut line = Vec::new();
    let n = (&mut *io)
        .take(MAX_FRAME as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|_| ClientError::Lost)?;
    if line.last() != Some(&b'\n') {
        return Err(if n >= MAX_FRAME {
            ClientError::Protocol(ProtoError::FrameTooLarge)
        } else {
            // EOF before the LF: closed, possibly mid-frame.
            ClientError::Lost
        });
    }
    // Lockstep: the supervisor never sends unasked, so nothing may follow the
    // frame. Bytes already buffered past the LF are a second frame.
    if !io.buffer().is_empty() {
        return Err(ClientError::Protocol(ProtoError::NotOneLine));
    }
    Ok(line)
}

impl Conn {
    /// Send one request and read its reply, checked against the request. An
    /// `error` reply is returned as a reply; the caller decides what it means.
    async fn exchange(&mut self, request: Request) -> Result<Reply, ClientError> {
        let id = FrameId::try_from(self.next_id).expect("ids are never 0");
        self.next_id = following(self.next_id);
        let frame = RequestFrame { id, request };
        let bytes = encode_request(&frame).map_err(ClientError::BadRequest)?;
        let stream = self.io.get_mut();
        stream
            .write_all(&bytes)
            .await
            .map_err(|_| ClientError::Lost)?;
        stream.flush().await.map_err(|_| ClientError::Lost)?;
        let line = read_frame(&mut self.io).await?;
        let response = decode_response(&line).map_err(ClientError::Protocol)?;
        check_id(id, &response).map_err(ClientError::Protocol)?;
        check_reply(&frame.request, &response.reply).map_err(ClientError::Protocol)?;
        Ok(response.reply)
    }
}

/// faber's client for the VM supervisor. One per process: the wire is lockstep,
/// so every job shares the one connection, one request at a time.
pub struct SupervisorClient {
    endpoint: Endpoint,
    timeouts: Timeouts,
    /// What the supervisor's version must equal before `start` is sent.
    faber_version: String,
    conn: Mutex<Option<Conn>>,
    /// The version the last successful handshake reported.
    seen: StdMutex<Option<SupervisorVersion>>,
}

impl SupervisorClient {
    pub fn new(endpoint: Endpoint) -> Self {
        Self::with(endpoint, Timeouts::default(), env!("FABER_VERSION"))
    }

    pub fn with(endpoint: Endpoint, timeouts: Timeouts, faber_version: &str) -> Self {
        Self {
            endpoint,
            timeouts,
            faber_version: faber_version.to_string(),
            conn: Mutex::new(None),
            seen: StdMutex::new(None),
        }
    }

    /// The supervisor version the last successful handshake reported.
    pub fn supervisor_version(&self) -> Option<SupervisorVersion> {
        self.seen.lock().unwrap().clone()
    }

    /// Connect, check the peer, handshake. Every failure here means the request
    /// it was opened for was never sent.
    async fn connect(&self) -> Result<Conn, ClientError> {
        let attempt = async {
            let stream = UnixStream::connect(&self.endpoint.path)
                .await
                .map_err(|e| ClientError::Unreachable(format!("connecting: {}", e.kind())))?;
            let uid = crate::platform::peer_uid(&stream)
                .map_err(|e| ClientError::Unreachable(format!("reading the peer: {}", e.kind())))?;
            if uid != self.endpoint.peer_uid {
                // Dropped unwritten.
                return Err(ClientError::WrongPeer { uid });
            }
            let mut conn = Conn {
                io: BufReader::new(stream),
                next_id: 1,
                // Until the handshake below says otherwise, `exit` only.
                peer: Peer::Mismatched { theirs: 0 },
                used: Since::now(),
            };
            let hello = Request::Hello(Hello {
                protocol: ProtocolVersion::try_from(PROTOCOL_VERSION).expect("a valid version"),
            });
            let reply = conn.exchange(hello).await.map_err(|e| match e {
                ClientError::Protocol(p) => ClientError::Handshake(p),
                ClientError::Lost => ClientError::Unreachable("closed during the handshake".into()),
                other => other,
            })?;
            conn.peer = match check_hello(&reply) {
                Ok(v) => Peer::Ready(v.clone()),
                Err(ProtoError::VersionMismatch { theirs }) => Peer::Mismatched { theirs },
                Err(p) => return Err(ClientError::Handshake(p)),
            };
            Ok(conn)
        };
        let conn = timeout(self.timeouts.connect, attempt)
            .await
            .unwrap_or_else(|_| Err(ClientError::Unreachable("the handshake timed out".into())))?;
        if let Peer::Ready(v) = &conn.peer {
            *self.seen.lock().unwrap() = Some(v.clone());
        }
        Ok(conn)
    }

    /// One request, in lockstep. `start` also requires the versions to agree.
    async fn call(&self, request: Request, limit: Duration) -> Result<Reply, ClientError> {
        request.validate().map_err(ClientError::BadRequest)?;
        let mut slot = self.conn.lock().await;
        if slot
            .as_ref()
            .is_some_and(|c| c.used.idle_at_least(self.timeouts.idle_reuse))
        {
            // The supervisor may have closed it: never send on it.
            *slot = None;
        }
        let conn = match &mut *slot {
            Some(c) => c,
            empty => empty.insert(self.connect().await?),
        };
        let is_exit = matches!(request, Request::Exit(_));
        match &conn.peer {
            // The frozen core: after a mismatch only `exit` may be sent, and the
            // supervisor closes on anything else. Reconnect next time; it may
            // have been replaced by then.
            Peer::Mismatched { theirs } if !is_exit => {
                let theirs = *theirs;
                *slot = None;
                return Err(ClientError::VersionMismatch { theirs });
            }
            Peer::Ready(v)
                if matches!(request, Request::Start(_)) && v.as_str() != self.faber_version =>
            {
                return Err(ClientError::VersionSkew {
                    supervisor: v.as_str().to_string(),
                    faber: self.faber_version.clone(),
                });
            }
            _ => {}
        }
        conn.used = Since::now();
        let result = match timeout(limit, conn.exchange(request)).await {
            Ok(Ok(Reply::Error(e))) => {
                // The supervisor closes after these two; anything else leaves the
                // connection usable.
                if matches!(e.code, ErrorCode::BadRequest | ErrorCode::HandshakeRequired) {
                    *slot = None;
                }
                return Err(ClientError::Refused(e.code));
            }
            Ok(r) => r,
            Err(_) => Err(ClientError::NoReply),
        };
        // A failure poisons the connection; `exit` ends it (the supervisor closes).
        if result.is_err() || is_exit {
            *slot = None;
        }
        result
    }

    /// Drop the connection, then connect and handshake afresh. The supervisor's
    /// version on success; [`ClientError::VersionMismatch`] when it speaks another
    /// protocol.
    pub async fn handshake(&self) -> Result<SupervisorVersion, ClientError> {
        let mut slot = self.conn.lock().await;
        *slot = None;
        let conn = self.connect().await?;
        match &conn.peer {
            Peer::Ready(v) => {
                let v = v.clone();
                *slot = Some(conn);
                Ok(v)
            }
            Peer::Mismatched { theirs } => Err(ClientError::VersionMismatch { theirs: *theirs }),
        }
    }

    /// Forget the current connection; the next call opens a new one.
    pub async fn disconnect(&self) {
        *self.conn.lock().await = None;
    }

    pub async fn start(&self, req: Start) -> Result<VmRecord, ClientError> {
        match self.call(Request::Start(req), self.timeouts.start).await? {
            Reply::Vm(v) => Ok(v),
            _ => unreachable!("check_reply admits only `vm` for start"),
        }
    }

    pub async fn probe(&self, claim: &ClaimId) -> Result<VmRecord, ClientError> {
        let req = Request::Probe(Probe {
            claim: claim.clone(),
        });
        match self.call(req, self.timeouts.request).await? {
            Reply::Vm(v) => Ok(v),
            _ => unreachable!("check_reply admits only `vm` for probe"),
        }
    }

    /// `jit` is `Some` for a macOS VM and `None` for the Linux VM.
    pub async fn begin(
        &self,
        claim: &ClaimId,
        jit: Option<JitConfig>,
    ) -> Result<VmRecord, ClientError> {
        let req = Request::Begin(Begin {
            claim: claim.clone(),
            jit_config: jit,
        });
        match self.call(req, self.timeouts.request).await? {
            Reply::Vm(v) => Ok(v),
            _ => unreachable!("check_reply admits only `vm` for begin"),
        }
    }

    /// ADR-0049 §7: run the image's checks in a proven macOS VM, held to `expect`.
    pub async fn verify(&self, claim: &ClaimId, expect: Expect) -> Result<VmRecord, ClientError> {
        let req = Request::Verify(Verify {
            claim: claim.clone(),
            expect,
        });
        match self.call(req, self.timeouts.request).await? {
            Reply::Vm(v) => Ok(v),
            _ => unreachable!("check_reply admits only `vm` for verify"),
        }
    }

    pub async fn stop(&self, claim: &ClaimId) -> Result<(), ClientError> {
        let req = Request::Stop(Stop {
            claim: claim.clone(),
        });
        match self.call(req, self.timeouts.request).await? {
            Reply::Done(_) => Ok(()),
            _ => unreachable!("check_reply admits only `done` for stop"),
        }
    }

    pub async fn list(&self) -> Result<Vec<VmRecord>, ClientError> {
        match self
            .call(Request::List(Empty {}), self.timeouts.request)
            .await?
        {
            Reply::Vms(l) => Ok(l.vms),
            _ => unreachable!("check_reply admits only `vms` for list"),
        }
    }

    pub async fn adopt(&self, claims: &[ClaimId]) -> Result<Adopted, ClientError> {
        let req = Request::Adopt(Adopt {
            claims: claims.to_vec(),
        });
        match self.call(req, self.timeouts.request).await? {
            Reply::Adopted(a) => Ok(a),
            _ => unreachable!("check_reply admits only `adopted` for adopt"),
        }
    }

    /// Ask the supervisor to exit; launchd starts it again. It stops no VM. Sent
    /// after a `version_mismatch` too: `exit` is in the frozen core.
    pub async fn exit(&self) -> Result<(), ClientError> {
        match self
            .call(Request::Exit(Empty {}), self.timeouts.request)
            .await?
        {
            Reply::Done(_) => Ok(()),
            _ => unreachable!("check_reply admits only `done` for exit"),
        }
    }
}

impl VmSupervisor for SupervisorClient {
    async fn start(&self, req: Start) -> Result<VmRecord, SupervisorError> {
        Ok(SupervisorClient::start(self, req).await?)
    }
    async fn probe(&self, claim: &ClaimId) -> Result<VmRecord, SupervisorError> {
        Ok(SupervisorClient::probe(self, claim).await?)
    }
    async fn begin(
        &self,
        claim: &ClaimId,
        jit: Option<JitConfig>,
    ) -> Result<VmRecord, SupervisorError> {
        Ok(SupervisorClient::begin(self, claim, jit).await?)
    }
    async fn verify(&self, claim: &ClaimId, expect: Expect) -> Result<VmRecord, SupervisorError> {
        Ok(SupervisorClient::verify(self, claim, expect).await?)
    }
    async fn stop(&self, claim: &ClaimId) -> Result<(), SupervisorError> {
        Ok(SupervisorClient::stop(self, claim).await?)
    }
    async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
        Ok(SupervisorClient::list(self).await?)
    }
    async fn adopt(&self, claims: &[ClaimId]) -> Result<Adopted, SupervisorError> {
        Ok(SupervisorClient::adopt(self, claims).await?)
    }
}

// ---------------------------------------------------------------------------
// A scripted fake supervisor, shared with `restart`'s tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

    use super::super::proto::{
        decode_request, decode_response, encode_response, Empty, ErrorCode, ErrorReply, HelloReply,
        ProtocolVersion, Reply, Request, ResponseFrame, SupervisorVersion,
    };

    /// A short directory for one socket: `sun_path` holds about 100 bytes, so
    /// `/tmp` rather than a possibly deep `TMPDIR` (as control.rs's tests do).
    pub struct SockDir(PathBuf);

    impl SockDir {
        pub fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from("/tmp").join(format!(
                "fbr-vmc-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn sock(&self) -> PathBuf {
            self.0.join("supervisor.sock")
        }

        /// This process's effective uid: the owner of the directory it made.
        pub fn euid(&self) -> u32 {
            std::fs::metadata(&self.0).unwrap().uid()
        }
    }

    impl Drop for SockDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What the fake does with one request.
    pub enum Act {
        Reply(Reply),
        /// Write these bytes verbatim and keep the connection.
        Raw(Vec<u8>),
        /// Write these bytes, then close.
        Partial(Vec<u8>),
        /// Wait, then write these bytes and close: a slow reply, after which
        /// the supervisor finds the connection idle since the request.
        SlowThenClose(Duration, Vec<u8>),
        /// Never answer.
        Hang,
        /// Reply, then close and remove the socket, like `exit`.
        ReplyThenExit(Reply),
    }

    /// Called with the connection's index (0 for the first) and the request.
    pub type Handler = Arc<dyn Fn(usize, &Request) -> Act + Send + Sync>;

    #[derive(Default)]
    pub struct Stats {
        /// Every byte received, on every connection.
        pub bytes: AtomicUsize,
        pub conns: AtomicUsize,
        /// Every request line received, verbatim, in order.
        pub lines: Mutex<Vec<Vec<u8>>>,
    }

    impl Stats {
        pub fn ops(&self) -> Vec<String> {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .map(|l| {
                    let f = decode_request(l).unwrap();
                    serde_json::to_value(&f.request)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .keys()
                        .next()
                        .unwrap()
                        .clone()
                })
                .collect()
        }
    }

    pub struct Fake {
        pub stats: Arc<Stats>,
        /// Notified once the fake has exited.
        pub exited: Arc<Notify>,
        task: JoinHandle<()>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    pub fn hello(version: &str) -> Reply {
        Reply::Hello(HelloReply {
            protocol: ProtocolVersion::try_from(super::super::proto::PROTOCOL_VERSION).unwrap(),
            supervisor_version: SupervisorVersion::try_from(version).unwrap(),
        })
    }

    pub fn mismatch(theirs: u16) -> Reply {
        Reply::Error(ErrorReply {
            code: ErrorCode::VersionMismatch,
            protocol: Some(ProtocolVersion::try_from(theirs).unwrap()),
        })
    }

    pub fn refuse(code: ErrorCode) -> Reply {
        Reply::Error(ErrorReply {
            code,
            protocol: None,
        })
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-proto")
    }

    /// A fixture's bytes.
    pub fn fixture(sub: &str) -> Vec<u8> {
        std::fs::read(fixtures().join(sub)).unwrap()
    }

    /// A response fixture's reply.
    pub fn reply_fixture(sub: &str) -> Reply {
        decode_response(&fixture(sub)).unwrap().reply
    }

    /// A supervisor that answers `hello` with `version`, `exit` with `done` and
    /// then exits, and everything else as `rest` says.
    pub fn plain(
        version: &'static str,
        rest: impl Fn(&Request) -> Act + Send + Sync + 'static,
    ) -> Handler {
        Arc::new(move |_, req| match req {
            Request::Hello(_) => Act::Reply(hello(version)),
            Request::Exit(_) => Act::ReplyThenExit(Reply::Done(Empty {})),
            other => rest(other),
        })
    }

    /// Bind `path` and serve `handler` until an [`Act::ReplyThenExit`].
    pub fn serve(path: &Path, handler: Handler) -> Fake {
        let listener = UnixListener::bind(path).unwrap();
        let stats = Arc::new(Stats::default());
        let exited = Arc::new(Notify::new());
        let path = path.to_path_buf();
        let task = {
            let (stats, exited) = (stats.clone(), exited.clone());
            tokio::spawn(async move {
                let stop = Arc::new(Notify::new());
                loop {
                    tokio::select! {
                        _ = stop.notified() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let n = stats.conns.fetch_add(1, Ordering::SeqCst);
                            tokio::spawn(connection(stream, n, handler.clone(), stats.clone(), stop.clone()));
                        }
                    }
                }
                drop(listener);
                let _ = std::fs::remove_file(&path);
                exited.notify_one();
            })
        };
        Fake {
            stats,
            exited,
            task,
        }
    }

    async fn connection(
        stream: UnixStream,
        n: usize,
        handler: Handler,
        stats: Arc<Stats>,
        stop: Arc<Notify>,
    ) {
        let mut io = BufReader::new(stream);
        loop {
            let mut line = Vec::new();
            match io.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => return,
                Ok(k) => stats.bytes.fetch_add(k, Ordering::SeqCst),
            };
            stats.lines.lock().unwrap().push(line.clone());
            let Ok(frame) = decode_request(&line) else {
                return;
            };
            let respond = |reply: Reply| {
                encode_response(&ResponseFrame {
                    id: frame.id,
                    reply,
                })
                .unwrap()
            };
            let w = io.get_mut();
            match handler(n, &frame.request) {
                Act::Reply(r) => {
                    let _ = w.write_all(&respond(r)).await;
                }
                Act::Raw(b) => {
                    let _ = w.write_all(&b).await;
                }
                Act::Partial(b) => {
                    let _ = w.write_all(&b).await;
                    return;
                }
                Act::SlowThenClose(wait, b) => {
                    tokio::time::sleep(wait).await;
                    let _ = w.write_all(&b).await;
                    return;
                }
                Act::Hang => std::future::pending::<()>().await,
                Act::ReplyThenExit(r) => {
                    let _ = w.write_all(&respond(r)).await;
                    stop.notify_one();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::fake::*;
    use super::*;
    use crate::vm::proto::{decode_request, encode_response, ResponseFrame, Slot, VmKind, VmState};

    fn client(dir: &SockDir, uid: u32, faber: &str) -> SupervisorClient {
        SupervisorClient::with(
            Endpoint {
                path: dir.sock(),
                peer_uid: uid,
            },
            Timeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_millis(300),
                start: Duration::from_secs(2),
                idle_reuse: Duration::from_secs(20),
            },
            faber,
        )
    }

    fn claim(s: &str) -> ClaimId {
        ClaimId::try_from(s).unwrap()
    }

    fn request_fixture(name: &str) -> Request {
        decode_request(&fixture(&format!("requests/valid/{name}.json")))
            .unwrap()
            .request
    }

    fn vm(claim_id: &str, state: VmState) -> Reply {
        Reply::Vm(VmRecord {
            slot: Slot::try_from(0).unwrap(),
            kind: VmKind::Macos,
            claim: claim(claim_id),
            state,
            stop_reason: None,
            failed: None,
            started_unix: 1_760_700_000u64.try_into().unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        })
    }

    #[tokio::test]
    async fn every_typed_request_matches_the_fixtures_byte_for_byte() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            plain("0.4.0", |req| match req {
                Request::Start(_) => Act::Reply(reply_fixture("responses/valid/vm-starting.json")),
                Request::Probe(p) => Act::Reply(vm(p.claim.as_str(), VmState::ProbeSent)),
                Request::Begin(b) => Act::Reply(vm(b.claim.as_str(), VmState::InService)),
                Request::Stop(_) => Act::Reply(reply_fixture("responses/valid/done.json")),
                Request::List(_) => Act::Reply(reply_fixture("responses/valid/vms-two.json")),
                Request::Adopt(_) => Act::Reply(reply_fixture("responses/valid/adopted.json")),
                _ => Act::Hang,
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        let Request::Start(start) = request_fixture("start-macos") else {
            unreachable!()
        };
        let Request::Begin(begin) = request_fixture("begin-macos") else {
            unreachable!()
        };
        let Request::Adopt(adopt) = request_fixture("adopt") else {
            unreachable!()
        };
        let id = claim("123456789-1760700000");

        assert_eq!(c.start(start).await.unwrap().state, VmState::Starting);
        assert_eq!(c.supervisor_version().unwrap().as_str(), "0.4.0");
        assert_eq!(c.probe(&id).await.unwrap().state, VmState::ProbeSent);
        let jit = begin.jit_config.unwrap();
        assert_eq!(
            c.begin(&id, Some(jit)).await.unwrap().state,
            VmState::InService
        );
        c.stop(&id).await.unwrap();
        assert_eq!(c.list().await.unwrap().len(), 2);
        let adopted = c.adopt(&adopt.claims).await.unwrap();
        assert_eq!((adopted.found.len(), adopted.unknown.len()), (1, 1));
        c.exit().await.unwrap();

        // One connection, ids 1..=8: exactly the fixtures, in their order.
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 1);
        let names = [
            "hello",
            "start-macos",
            "probe",
            "begin-macos",
            "stop",
            "list",
            "adopt",
            "exit",
        ];
        let lines = fake.stats.lines.lock().unwrap().clone();
        assert_eq!(lines.len(), names.len());
        for (line, name) in lines.iter().zip(names) {
            assert_eq!(
                *line,
                fixture(&format!("requests/valid/{name}.json")),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn the_supervisor_trait_maps_refusals_and_absence() {
        let dir = SockDir::new();
        let c = client(&dir, dir.euid(), "0.4.0");
        let err = VmSupervisor::list(&c).await.unwrap_err();
        assert!(matches!(err, SupervisorError::Unreachable), "{err:?}");

        let _fake = serve(
            &dir.sock(),
            plain("0.4.0", |_| Act::Reply(refuse(ErrorCode::UnknownClaim))),
        );
        let err = VmSupervisor::stop(&c, &claim("a")).await.unwrap_err();
        assert!(
            matches!(err, SupervisorError::Refused(ErrorCode::UnknownClaim)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_socket_held_by_another_uid_gets_zero_bytes() {
        let dir = SockDir::new();
        let fake = serve(&dir.sock(), plain("0.4.0", |_| Act::Hang));
        let c = client(&dir, dir.euid() + 1, "0.4.0");
        let err = c.list().await.unwrap_err();
        assert_eq!(err, ClientError::WrongPeer { uid: dir.euid() });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 1, "it connected");
        assert_eq!(
            fake.stats.bytes.load(Ordering::SeqCst),
            0,
            "and sent nothing"
        );
    }

    #[tokio::test]
    async fn a_version_mismatch_is_typed_and_still_takes_exit() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            Arc::new(|_, req| match req {
                Request::Hello(_) => Act::Reply(mismatch(1)),
                Request::Exit(_) => Act::Reply(Reply::Done(Empty {})),
                _ => Act::Hang,
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        assert_eq!(
            c.list().await.unwrap_err(),
            ClientError::VersionMismatch { theirs: 1 }
        );
        assert_eq!(
            c.handshake().await.unwrap_err(),
            ClientError::VersionMismatch { theirs: 1 }
        );
        c.exit().await.unwrap();
        assert_eq!(c.supervisor_version(), None);
        assert_eq!(fake.stats.ops(), ["hello", "hello", "hello", "exit"]);
    }

    #[tokio::test]
    async fn a_hello_reply_that_is_not_ours_is_refused() {
        for reply in [
            Reply::Done(Empty {}),
            reply_fixture("responses/refused-hello/hello-protocol-1.json"),
            refuse(ErrorCode::Internal),
        ] {
            let dir = SockDir::new();
            let fake = serve(
                &dir.sock(),
                Arc::new(move |_, req| match req {
                    Request::Hello(_) => Act::Reply(reply.clone()),
                    _ => Act::Reply(Reply::Done(Empty {})),
                }),
            );
            let c = client(&dir, dir.euid(), "0.4.0");
            let err = c.stop(&claim("a")).await.unwrap_err();
            assert!(matches!(err, ClientError::Handshake(_)), "{err:?}");
            assert!(matches!(
                SupervisorError::from(err),
                SupervisorError::Misconfigured
            ));
            assert_eq!(fake.stats.ops(), ["hello"], "the request was never sent");
        }
    }

    #[tokio::test]
    async fn an_oversized_frame_without_lf_is_refused() {
        let dir = SockDir::new();
        let _fake = serve(
            &dir.sock(),
            plain("0.4.0", |_| Act::Raw(vec![b'a'; 1 << 20])),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        assert_eq!(
            c.list().await.unwrap_err(),
            ClientError::Protocol(ProtoError::FrameTooLarge)
        );
    }

    #[tokio::test]
    async fn a_reply_with_another_id_or_kind_is_a_protocol_error() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            plain("0.4.0", |req| match req {
                // A valid `done`, but answering id 99.
                Request::Stop(_) => Act::Raw(b"{\"id\":99,\"reply\":{\"done\":{}}}\n".to_vec()),
                Request::List(_) => Act::Reply(Reply::Done(Empty {})),
                _ => Act::Hang,
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        assert_eq!(
            c.stop(&claim("a")).await.unwrap_err(),
            ClientError::Protocol(ProtoError::IdMismatch)
        );
        assert_eq!(
            c.list().await.unwrap_err(),
            ClientError::Protocol(ProtoError::WrongReply)
        );
        // Each break dropped the connection.
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_trailing_second_frame_is_a_protocol_error() {
        let dir = SockDir::new();
        let _fake = serve(
            &dir.sock(),
            plain("0.4.0", |_| {
                Act::Raw(
                    b"{\"id\":2,\"reply\":{\"done\":{}}}\n{\"id\":3,\"reply\":{\"done\":{}}}\n"
                        .to_vec(),
                )
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        let err = c.stop(&claim("a")).await.unwrap_err();
        assert_eq!(err, ClientError::Protocol(ProtoError::NotOneLine));
    }

    /// The supervisor closing an idle connection, as it does after 30 s:
    /// connection 0 answers `list` and then closes.
    fn closes_after_the_first_list() -> Handler {
        Arc::new(|n, req| match req {
            Request::Hello(_) => Act::Reply(hello("0.4.0")),
            Request::List(_) if n == 0 => Act::Partial(
                encode_response(&ResponseFrame {
                    id: FrameId::try_from(2).unwrap(),
                    reply: reply_fixture("responses/valid/vms-empty.json"),
                })
                .unwrap(),
            ),
            _ => Act::Reply(reply_fixture("responses/valid/vms-empty.json")),
        })
    }

    fn client_reusing_for(dir: &SockDir, idle_reuse: Duration) -> SupervisorClient {
        let mut c = client(dir, dir.euid(), "0.4.0");
        c.timeouts.idle_reuse = idle_reuse;
        c
    }

    #[tokio::test]
    async fn a_connection_idle_past_the_limit_is_replaced_before_a_request() {
        let dir = SockDir::new();
        let fake = serve(&dir.sock(), closes_after_the_first_list());
        let c = client_reusing_for(&dir, Duration::from_millis(50));
        assert!(c.list().await.unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Sent on a fresh connection, never on the one the supervisor closed.
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
        assert_eq!(fake.stats.ops(), ["hello", "list", "hello", "list"]);
    }

    #[tokio::test]
    async fn a_slow_reply_ages_the_connection_from_its_request() {
        // The supervisor counts idle time from the request it read: a reply
        // slower than the limit leaves a connection it may already have closed.
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            Arc::new(|n, req| match req {
                Request::Hello(_) => Act::Reply(hello("0.4.0")),
                Request::List(_) if n == 0 => Act::SlowThenClose(
                    Duration::from_millis(120),
                    encode_response(&ResponseFrame {
                        id: FrameId::try_from(2).unwrap(),
                        reply: reply_fixture("responses/valid/vms-empty.json"),
                    })
                    .unwrap(),
                ),
                _ => Act::Reply(reply_fixture("responses/valid/vms-empty.json")),
            }),
        );
        let c = client_reusing_for(&dir, Duration::from_millis(50));
        assert!(c.list().await.unwrap().is_empty());
        // At once, with no pause: still a fresh connection.
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_request_on_a_closed_connection_is_lost() {
        // What the limit prevents: the request goes out on the closed
        // connection and may have been acted on, as far as faber can tell.
        let dir = SockDir::new();
        let _fake = serve(&dir.sock(), closes_after_the_first_list());
        let c = client_reusing_for(&dir, Duration::from_secs(3600));
        assert!(c.list().await.unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(c.list().await.unwrap_err(), ClientError::Lost);
    }

    #[tokio::test]
    async fn a_timeout_poisons_the_connection_and_the_next_call_handshakes_again() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            Arc::new(|n, req| match req {
                Request::Hello(_) => Act::Reply(hello("0.4.0")),
                Request::List(_) if n == 0 => Act::Hang,
                _ => Act::Reply(reply_fixture("responses/valid/vms-empty.json")),
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        assert_eq!(c.list().await.unwrap_err(), ClientError::NoReply);
        assert!(matches!(
            SupervisorError::from(ClientError::NoReply),
            SupervisorError::NoReply
        ));
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
        assert_eq!(fake.stats.ops(), ["hello", "list", "hello", "list"]);
    }

    #[tokio::test]
    async fn start_is_refused_on_a_version_skew_while_list_works() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            plain("0.4.0", |req| match req {
                Request::List(_) => Act::Reply(reply_fixture("responses/valid/vms-empty.json")),
                _ => Act::Reply(reply_fixture("responses/valid/vm-starting.json")),
            }),
        );
        let c = client(&dir, dir.euid(), "0.5.0");
        let Request::Start(start) = request_fixture("start-macos") else {
            unreachable!()
        };
        let err = c.start(start).await.unwrap_err();
        assert_eq!(
            err,
            ClientError::VersionSkew {
                supervisor: "0.4.0".into(),
                faber: "0.5.0".into()
            }
        );
        assert!(c.list().await.unwrap().is_empty());
        assert_eq!(fake.stats.ops(), ["hello", "list"], "start was never sent");
    }

    #[tokio::test]
    async fn a_supervisor_closing_mid_frame_is_lost() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            Arc::new(|n, req| match req {
                Request::Hello(_) => Act::Reply(hello("0.4.0")),
                _ if n == 0 => Act::Partial(b"{\"id\":2,\"rep".to_vec()),
                _ => Act::Reply(Reply::Done(Empty {})),
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        assert_eq!(c.stop(&claim("a")).await.unwrap_err(), ClientError::Lost);
        c.stop(&claim("a")).await.unwrap();
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bad_request_drops_the_connection_and_other_refusals_keep_it() {
        let dir = SockDir::new();
        let fake = serve(
            &dir.sock(),
            plain("0.4.0", |req| match req {
                Request::Probe(_) => Act::Reply(refuse(ErrorCode::WrongState)),
                _ => Act::Reply(refuse(ErrorCode::BadRequest)),
            }),
        );
        let c = client(&dir, dir.euid(), "0.4.0");
        for _ in 0..2 {
            let err = c.probe(&claim("a")).await.unwrap_err();
            assert_eq!(err, ClientError::Refused(ErrorCode::WrongState));
        }
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 1);
        let err = c.stop(&claim("a")).await.unwrap_err();
        assert_eq!(err, ClientError::Refused(ErrorCode::BadRequest));
        c.probe(&claim("a")).await.unwrap_err();
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_invalid_request_is_never_sent() {
        let dir = SockDir::new();
        let fake = serve(&dir.sock(), plain("0.4.0", |_| Act::Hang));
        let c = client(&dir, dir.euid(), "0.4.0");
        let err = c.adopt(&[]).await.unwrap_err();
        assert!(matches!(err, ClientError::BadRequest(_)), "{err:?}");
        assert_eq!(fake.stats.conns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ids_wrap_to_one_and_never_zero() {
        assert_eq!(following(1), 2);
        assert_eq!(following(u32::MAX - 1), u32::MAX);
        assert_eq!(following(u32::MAX), 1);
    }

    #[test]
    fn errors_never_print_a_jit_config() {
        // The only place a JitConfig travels is inside a Begin request, which no
        // error carries; prove the Debug of what does travel stays redacted.
        let jit = JitConfig::try_from("c2VjcmV0c2VjcmV0").unwrap();
        let req = Request::Begin(Begin {
            claim: claim("a"),
            jit_config: Some(jit),
        });
        assert!(!format!("{req:?}").contains("c2VjcmV0"));
    }
}
