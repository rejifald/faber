//! The `faber-mirror` binary: the service itself, and the commands faber runs
//! inside its container.
//!
//! - `faber-mirror` (or `faber-mirror serve`) serves jobs on
//!   [`faber_mirror::PORT`] and the daemon's control routes on
//!   `127.0.0.1:`[`faber_mirror::ADMIN_PORT`].
//! - `faber-mirror health` asks the service on the container's own loopback for
//!   `/healthz`, prints the body, and exits 0 only on a 200.
//! - `faber-mirror sync` reads a sync request on stdin, hands it to the control
//!   port, and prints the counters it answers with.
//!
//! faber runs the last two with `docker exec`, so the daemon needs no port
//! published from the container, and the tokens a sync carries travel on a
//! pipe, never an argument.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

/// The faber version the image was built from (`deploy/build-mirror-image.sh`
/// passes it), or this crate's own when built by hand.
const VERSION: &str = match option_env!("FABER_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// How long the probe and a sync wait for the whole answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);

fn main() -> ExitCode {
    let mode = std::env::args().nth(1);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("faber-mirror: starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match mode.as_deref() {
        None | Some("serve") => rt.block_on(serve()),
        Some("health") => rt.block_on(probe()),
        Some("sync") => rt.block_on(sync()),
        Some("version" | "--version") => {
            println!("{VERSION}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("faber-mirror: unknown command {other:?} (serve | health | sync | version)");
            ExitCode::from(2)
        }
    }
}

async fn serve() -> ExitCode {
    tracing_subscriber::fmt().with_target(false).init();
    let config = match faber_mirror::Config::from_vars(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("faber-mirror: {e}");
            return ExitCode::from(2);
        }
    };
    let max_bytes = u64::from(config.max_size_gib) << 30;
    let control = Arc::new(faber_mirror::control::Control::new());
    // One store under one cap for every ecosystem (ADR-0047 §8).
    let store = Arc::new(faber_mirror::store::Store::open(
        &config.cache_dir.join("store"),
        max_bytes,
    ));
    if let Some(why) = store.broken() {
        warn!("faber-mirror: {why} — passing every request through");
    }
    let credential = Path::new(faber_mirror::CREDENTIAL_DIR).join(faber_mirror::CREDENTIAL_FILE);
    let upstream = match faber_mirror::lfs::GithubUpstream::new(
        faber_mirror::GITHUB,
        credential,
        faber_mirror::egress::Egress::github(),
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("faber-mirror: {e}");
            return ExitCode::FAILURE;
        }
    };
    let lfs = faber_mirror::lfs::Lfs::new(control.clone(), store.clone(), upstream);
    let npmjs = match faber_mirror::npm::Registry::new(
        faber_mirror::NPMJS,
        faber_mirror::egress::Egress::npmjs(),
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("faber-mirror: {e}");
            return ExitCode::FAILURE;
        }
    };
    let npm = faber_mirror::npm::Npm::new(control.clone(), store.clone(), &config.cache_dir, npmjs);
    let registry = faber_mirror::builtin(lfs, npm);
    let service = faber_mirror::Service::new(VERSION, &config.cache_dir, control, store);
    let app = faber_mirror::router(&registry, service.clone());
    let admin = faber_mirror::admin_router(service);

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", faber_mirror::PORT)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("faber-mirror: binding port {}: {e}", faber_mirror::PORT);
            return ExitCode::FAILURE;
        }
    };
    // Loopback only: a job reaches the container's network interfaces, never
    // its loopback, so the control routes are the daemon's alone.
    let admin_listener =
        match tokio::net::TcpListener::bind(("127.0.0.1", faber_mirror::ADMIN_PORT)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "faber-mirror: binding the control port {}: {e}",
                    faber_mirror::ADMIN_PORT
                );
                return ExitCode::FAILURE;
            }
        };
    tokio::spawn(async move {
        if let Err(e) = axum::serve(admin_listener, admin).await {
            warn!("faber-mirror: control server stopped: {e}");
        }
    });
    info!(
        "faber-mirror {VERSION}: serving on :{} — cache {} (limit {} GiB), adapters: {:?}",
        faber_mirror::PORT,
        config.cache_dir.display(),
        config.max_size_gib,
        registry.names()
    );
    // With the address each connection arrived on: which job network, so which
    // container, an npm request came from (`faber_mirror::Peer`).
    let app = app.into_make_service_with_connect_info::<faber_mirror::Peer>();
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        warn!("faber-mirror: server stopped: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Resolves on SIGTERM (`docker stop`) or Ctrl-C.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn probe() -> ExitCode {
    let ask = exchange(
        faber_mirror::PORT,
        faber_mirror::PROBE_REQUEST.to_vec(),
        faber_mirror::PROBE_CAP,
    );
    finish("health", PROBE_TIMEOUT, ask).await
}

async fn sync() -> ExitCode {
    let mut body = Vec::new();
    if let Err(e) = tokio::io::stdin()
        .take(faber_mirror::SYNC_CAP as u64 + 1)
        .read_to_end(&mut body)
        .await
    {
        eprintln!("faber-mirror sync: reading stdin: {e}");
        return ExitCode::FAILURE;
    }
    if body.len() > faber_mirror::SYNC_CAP {
        eprintln!("faber-mirror sync: the request is larger than the cap");
        return ExitCode::FAILURE;
    }
    let ask = exchange(
        faber_mirror::ADMIN_PORT,
        faber_mirror::sync_request(&body),
        faber_mirror::SYNC_CAP,
    );
    finish("sync", SYNC_TIMEOUT, ask).await
}

async fn finish(
    what: &str,
    limit: Duration,
    ask: impl std::future::Future<Output = Result<String, String>>,
) -> ExitCode {
    match tokio::time::timeout(limit, ask).await {
        Ok(Ok(body)) => {
            println!("{body}");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            eprintln!("faber-mirror {what}: {e}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("faber-mirror {what}: no answer within {limit:?}");
            ExitCode::FAILURE
        }
    }
}

/// Send `request` to the service on the container's loopback `port` and return
/// the body of a 200 answer.
async fn exchange(port: u16, request: Vec<u8>, cap: usize) -> Result<String, String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("connecting: {e}"))?;
    stream
        .write_all(&request)
        .await
        .map_err(|e| format!("asking: {e}"))?;
    let mut raw = Vec::new();
    stream
        .take(cap as u64)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| format!("reading: {e}"))?;
    faber_mirror::parse_probe_response(&raw).map(str::to_string)
}
