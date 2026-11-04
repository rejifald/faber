//! The real upstream client's download policy against a server on this
//! machine: how many redirects it follows, that each hop is re-checked, and
//! that an allowed name resolving to a loopback address is refused. Never the
//! real GitHub.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use faber_mirror::egress::{Egress, MAX_REDIRECTS};
use faber_mirror::lfs::{Action, GithubUpstream, Upstream, UpstreamError};
use futures_util::StreamExt;

#[derive(Clone)]
struct Server {
    port: u16,
    hits: Arc<AtomicUsize>,
}

async fn hop(State(s): State<Server>, Path(n): Path<usize>) -> Response {
    s.hits.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        return (StatusCode::OK, "the object").into_response();
    }
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/hop/{}", n - 1))],
    )
        .into_response()
}

async fn off(State(s): State<Server>) -> Response {
    s.hits.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::FOUND,
        [(
            header::LOCATION,
            format!("http://127.0.0.1:{}/hop/0", s.port),
        )],
    )
        .into_response()
}

async fn serve() -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s = Server {
        port: listener.local_addr().unwrap().port(),
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/hop/{n}", get(hop))
        .route("/off", get(off))
        .with_state(s.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    s
}

fn upstream(egress: Egress) -> GithubUpstream {
    GithubUpstream::new("http://localhost:1", "/nonexistent".into(), egress).unwrap()
}

async fn download(u: &GithubUpstream, href: String) -> Result<String, String> {
    let action = Action {
        href,
        header: Default::default(),
    };
    match u.download(&action).await {
        Ok(mut body) => {
            let mut out = Vec::new();
            while let Some(c) = body.next().await {
                out.extend_from_slice(&c?);
            }
            Ok(String::from_utf8(out).unwrap())
        }
        Err(UpstreamError::Upstream { message, .. } | UpstreamError::Local(message)) => {
            Err(message)
        }
    }
}

#[tokio::test]
async fn redirects_are_followed_up_to_the_limit_and_each_is_rechecked() {
    let s = serve().await;
    let u = upstream(Egress::for_test(&["localhost"], true));
    let at = |path: &str| format!("http://localhost:{}{path}", s.port);

    assert_eq!(download(&u, at("/hop/0")).await.unwrap(), "the object");
    assert_eq!(
        download(&u, at(&format!("/hop/{MAX_REDIRECTS}")))
            .await
            .unwrap(),
        "the object"
    );
    let err = download(&u, at(&format!("/hop/{}", MAX_REDIRECTS + 1)))
        .await
        .unwrap_err();
    assert!(err.contains("more than"), "{err}");

    // A redirect to a host off the list is refused before it is requested.
    let before = s.hits.load(Ordering::Relaxed);
    let err = download(&u, at("/off")).await.unwrap_err();
    assert!(err.contains("is not one of the test's hosts"), "{err}");
    assert_eq!(
        s.hits.load(Ordering::Relaxed),
        before + 1,
        "only /off was hit"
    );
}

#[tokio::test]
async fn an_allowed_name_that_resolves_to_loopback_is_refused() {
    let s = serve().await;
    // `localhost` is on this policy's list, but it resolves to 127.0.0.1 and
    // ::1, which the resolver drops: nothing is connected to.
    let u = upstream(Egress::for_test(&["localhost"], false));
    let err = download(&u, format!("http://localhost:{}/hop/0", s.port))
        .await
        .unwrap_err();
    assert!(err.contains("downloading from GitHub"), "{err}");
    assert_eq!(s.hits.load(Ordering::Relaxed), 0, "the server was reached");

    // A loopback IP literal never gets as far as the resolver.
    let u = upstream(Egress::for_test(&["127.0.0.1"], false));
    let err = download(&u, format!("http://127.0.0.1:{}/hop/0", s.port))
        .await
        .unwrap_err();
    assert!(err.contains("not a public address"), "{err}");
    assert_eq!(s.hits.load(Ordering::Relaxed), 0);
}

async fn npm_get(
    u: &faber_mirror::npm::Registry,
    path: &str,
    follow: bool,
) -> Result<(u16, String), String> {
    let ask = faber_mirror::npm::Ask {
        method: axum::http::Method::GET,
        path: path.to_string(),
        headers: Default::default(),
        body: axum::body::Body::empty(),
        follow,
    };
    match faber_mirror::npm::Upstream::send(u, ask).await {
        Ok(mut reply) => {
            let mut out = Vec::new();
            while let Some(c) = reply.body.next().await {
                out.extend_from_slice(&c?);
            }
            Ok((reply.status.as_u16(), String::from_utf8(out).unwrap()))
        }
        Err(UpstreamError::Upstream { message, .. } | UpstreamError::Local(message)) => {
            Err(message)
        }
    }
}

#[tokio::test]
async fn the_npm_client_follows_checked_redirects_for_its_own_fetches_only() {
    let s = serve().await;
    let base = format!("http://localhost:{}", s.port);
    let u =
        faber_mirror::npm::Registry::new(&base, Egress::for_test(&["localhost"], true)).unwrap();

    let ok = npm_get(&u, &format!("/hop/{MAX_REDIRECTS}"), true).await;
    assert_eq!(ok.unwrap(), (200, "the object".to_string()));
    let err = npm_get(&u, &format!("/hop/{}", MAX_REDIRECTS + 1), true)
        .await
        .unwrap_err();
    assert!(err.contains("more than"), "{err}");
    // A redirect off the allowed hosts is refused before it is requested.
    let before = s.hits.load(Ordering::Relaxed);
    let err = npm_get(&u, "/off", true).await.unwrap_err();
    assert!(err.contains("is not one of the test's hosts"), "{err}");
    assert_eq!(s.hits.load(Ordering::Relaxed), before + 1);
    // A forwarded request's redirect goes back to the job as it came.
    let (status, _) = npm_get(&u, "/hop/1", false).await.unwrap();
    assert_eq!(status, 302);

    // The registry's own client allows only the registry.
    let e = Egress::npmjs();
    assert!(e.check(&format!("{}/demo", faber_mirror::NPMJS)).is_ok());
    assert!(e.check(&format!("{base}/demo")).is_err());
}
