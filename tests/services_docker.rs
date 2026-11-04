//! A live exercise of ADR-0046's declared services against a real Docker daemon.
//!
//! `#[ignore]`d, because it needs Docker and the service images. The unit tests
//! in `src/services.rs` pin every argv and SQL script; what only a live run
//! answers is whether they work together: that the shared Postgres starts and
//! answers on the job's network under the `postgres` alias, that the injected
//! credentials open the job's own database and nothing else, that teardown
//! leaves no role and no database behind, that a restart's adoption drops the
//! roles of runners that are gone, and that an idle server stops.
//!
//! ```sh
//! cargo test --test services_docker -- --ignored --nocapture
//! ```
//!
//! **Which Docker it reaches.** faber's `docker_command()` pins its own Linux
//! VM's socket only on a host set up in the SYSTEM layout (`/etc/faber/setup.json`
//! present and trusted). Anywhere else it passes `DOCKER_HOST` and
//! `DOCKER_CONTEXT` through, so the test uses whatever the `docker` CLI uses:
//! the current context, or `DOCKER_HOST=unix:///path/to/docker.sock` to choose
//! another. On a set-up host, run it as an account that can reach faber's
//! socket, or not at all.
//!
//! The images must be present or pullable: `postgres:16-alpine`,
//! `redis:7-alpine`, `axllent/mailpit:v1.21` (override the Postgres one with
//! `FABER_LIVE_PG_IMAGE`). The test removes the shared server, its volume and its
//! job networks on the way out. It leaves the `faber-svc` network, which a host
//! keeps anyway.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use faber::services::{
    Ctx, JobNet, PerJobPolicy, PostgresPolicy, ServicePlan, ServiceSpec, Services, ServicesPolicy,
};

const SERVER: &str = "faber-svc-postgres-16";
const VOLUME: &str = "faber-svc-postgres-16-data";
const SERVER_14: &str = "faber-svc-postgres-14";
const VOLUME_14: &str = "faber-svc-postgres-14-data";

fn docker(args: &[&str]) -> (bool, String) {
    docker_env(args, &[])
}

fn docker_env(args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let out = std::process::Command::new("docker")
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("running docker");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Run `script` in a throwaway client container on `net`, with `env` passed
/// name-only like faber passes it to a runner.
fn client(net: &str, image: &str, env: &[(&str, &str)], script: &str) -> (bool, String) {
    let mut args = vec!["run", "--rm", "--network", net];
    for (k, _) in env {
        args.extend(["-e", k]);
    }
    args.extend([image, "sh", "-c", script]);
    docker_env(&args, env)
}

/// A superuser query on the shared server, for the test's own checks.
fn sql(query: &str) -> String {
    sql_on(SERVER, "postgres", query)
}

fn sql_on(server: &str, db: &str, query: &str) -> String {
    let (ok, out) = docker(&[
        "exec", server, "psql", "-X", "-U", "postgres", "-d", db, "-tAc", query,
    ]);
    assert!(ok, "{query}: {out}");
    out.trim().to_string()
}

fn container_exists(name: &str) -> bool {
    let (_, out) = docker(&["ps", "-a", "--filter", &format!("name=^{name}$"), "-q"]);
    !out.trim().is_empty()
}

fn role_of(id: &str) -> String {
    format!("faber_r_{}", id.replace('-', "_"))
}

fn exists(role: &str) -> (bool, bool) {
    let r = sql(&format!(
        "select count(*) from pg_roles where rolname = '{role}'"
    ));
    let d = sql(&format!(
        "select count(*) from pg_database where datname = '{role}'"
    ));
    (r == "1", d == "1")
}

struct Job {
    id: String,
    net: String,
}

impl Job {
    fn new(job: u64, ts: u64) -> Self {
        let id = format!("{job}-{ts}");
        let net = format!("faber-net-{id}");
        let (ok, out) = docker(&["network", "create", &net]);
        assert!(ok, "{out}");
        Self { id, net }
    }

    fn net(&self) -> JobNet<'_> {
        JobNet {
            id: &self.id,
            owner_repo: "owner/demo",
            net: &self.net,
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        for kind in ["redis", "mailpit"] {
            docker(&["rm", "-f", &format!("faber-svc-{kind}-{}", self.id)]);
        }
        docker(&["network", "disconnect", "-f", &self.net, SERVER]);
        docker(&["network", "disconnect", "-f", &self.net, SERVER_14]);
        docker(&["network", "rm", &self.net]);
    }
}

fn cleanup_server() {
    docker(&["rm", "-f", "-v", SERVER]);
    docker(&["volume", "rm", VOLUME]);
}

fn ctx(idle_stop: Duration, pg_image: &str) -> Ctx {
    ctx_for("16", idle_stop, pg_image)
}

fn ctx_for(version: &str, idle_stop: Duration, pg_image: &str) -> Ctx {
    let cfg = faber::config::AutoLimitFile::default().resolve(&Default::default());
    Ctx {
        policy: ServicesPolicy {
            postgres: Some(PostgresPolicy {
                versions: vec![version.into()],
                image: pg_image.replace(version, "{version}"),
                extensions: vec!["pg_trgm".into()],
                memory_mib: 1024,
                idle_stop,
            }),
            per_job: [
                ("redis", "redis:7-alpine"),
                ("mailpit", "axllent/mailpit:v1.21"),
            ]
            .into_iter()
            .map(|(n, image)| (n.to_string(), PerJobPolicy::builtin(n, image, 128)))
            .collect(),
        },
        limiter: faber::limiter::Limiter::new(cfg, &HashMap::new()),
        pull_unqualified: Some(true),
    }
}

fn plan(pg_image: &str, with_per_job: bool) -> ServicePlan {
    let mut specs = vec![pg_spec("16", pg_image)];
    if with_per_job {
        for (name, image) in [
            ("redis", "redis:7-alpine"),
            ("mailpit", "axllent/mailpit:v1.21"),
        ] {
            specs.push(ServiceSpec {
                service: name.into(),
                version: None,
                image: image.into(),
                memory_mib: 128,
                label: format!("faber.{name}"),
                per_job: Some(PerJobPolicy::builtin(name, image, 128)),
            });
        }
    }
    ServicePlan { specs }
}

fn pg_spec(version: &str, pg_image: &str) -> ServiceSpec {
    ServiceSpec {
        service: "postgres".into(),
        version: Some(version.into()),
        image: pg_image.into(),
        memory_mib: 1024,
        label: format!("faber.postgres-{version}"),
        per_job: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon and the service images"]
async fn services_run_isolated_and_leave_nothing_behind() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let pg_image =
        std::env::var("FABER_LIVE_PG_IMAGE").unwrap_or_else(|_| "postgres:16-alpine".into());
    cleanup_server();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let a = Job::new(900_001, ts);
    let b = Job::new(900_002, ts);
    let c = Job::new(900_003, ts);

    let first = Services::default();
    let ctx1 = ctx(Duration::from_secs(900), &pg_image);
    let limiter = ctx1.limiter.clone();
    first.configure(ctx1);
    assert!(first.enabled());

    // Cold start: A and B attach concurrently, and wait on one server start.
    let full = plan(&pg_image, true);
    let pg_only = plan(&pg_image, false);
    let t = Instant::now();
    let (net_a, net_b) = (a.net(), b.net());
    let (att_a, att_b) = tokio::join!(first.attach(&full, &net_a), first.attach(&pg_only, &net_b));
    let cold = t.elapsed();
    let att_a = att_a.expect("attach A");
    let att_b = att_b.expect("attach B");
    assert_eq!(
        limiter.snapshot().reserved_mib,
        1024,
        "the server's memory is held once while it runs"
    );

    // Warm attach: the server runs, so this is a role and a template copy.
    let t = Instant::now();
    let att_c = first.attach(&pg_only, &c.net()).await.expect("attach C");
    let warm_pg = t.elapsed();

    let names = att_a.env_names();
    for want in [
        "DATABASE_URL",
        "PGHOST",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "REDIS_URL",
        "MAIL_SMTP_URL",
        "MAIL_API_URL",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }

    // A's credentials open A's database, and pg_trgm came from the template.
    let (ok, out) = client(
        &a.net,
        &pg_image,
        &att_a.env(),
        r#"psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -c 'create table t(x int)' -c "select similarity('a','a')" && psql -v ON_ERROR_STOP=1 -c 'insert into t values (1)'"#,
    );
    assert!(ok, "A uses its database: {out}");

    // A per-job Redis and Mailpit answer on A's network by name.
    let (ok, out) = client(
        &a.net,
        "redis:7-alpine",
        &att_a.env(),
        r#"redis-cli -u "$REDIS_URL" ping"#,
    );
    assert!(ok && out.contains("PONG"), "redis: {out}");
    let (ok, out) = client(
        &a.net,
        &pg_image,
        &att_a.env(),
        r#"wget -qO- "$MAIL_API_URL/api/v1/info""#,
    );
    assert!(ok, "mailpit: {out}");

    // B cannot open A's database with its own credentials.
    let env_b = att_b.env();
    let mut cross = env_b.clone();
    let a_db = role_of(&a.id);
    cross.push(("A_DB", a_db.as_str()));
    let (ok, out) = client(
        &b.net,
        &pg_image,
        &cross,
        r#"psql "postgres://$PGUSER:$PGPASSWORD@postgres:5432/$A_DB" -c 'select 1'"#,
    );
    assert!(!ok, "B must not connect to A's database: {out}");
    assert!(out.contains("permission denied"), "{out}");
    // Nor to the template.
    let (ok, out) = client(
        &b.net,
        &pg_image,
        &env_b,
        r#"psql "postgres://$PGUSER:$PGPASSWORD@postgres:5432/faber_tpl" -c 'select 1'"#,
    );
    assert!(!ok, "B must not connect to faber_tpl: {out}");
    // B may create a database of its own, which teardown later finds by owner.
    let (ok, out) = client(
        &b.net,
        &pg_image,
        &env_b,
        r#"psql -v ON_ERROR_STOP=1 -d postgres -c "create database ${PGUSER}_test""#,
    );
    assert!(ok, "B creates its own test database: {out}");

    // Detach A: role, database, per-job containers and attachment are gone.
    let t = Instant::now();
    first.detach(&full.names(), &a.net()).await;
    let detach_time = t.elapsed();
    assert_eq!(exists(&role_of(&a.id)), (false, false));
    let (_, out) = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=faber-svc-redis-{}", a.id),
        "-q",
    ]);
    assert!(out.trim().is_empty(), "A's redis is gone");
    let (_, out) = docker(&[
        "inspect",
        "--format",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
        SERVER,
    ]);
    assert!(!out.contains(&a.net), "the server left A's network: {out}");
    // Idempotent.
    first.detach(&full.names(), &a.net()).await;

    // A restart: a new process adopts the server with only B alive. C's role and
    // database are dropped; B's survive, its self-made database included.
    let second = Services::default();
    let ctx2 = ctx(Duration::from_secs(2), &pg_image);
    let limiter2 = ctx2.limiter.clone();
    second.configure(ctx2);
    second.adopt(std::slice::from_ref(&b.id)).await;
    assert_eq!(exists(&role_of(&c.id)), (false, false), "C was orphaned");
    assert_eq!(exists(&role_of(&b.id)), (true, true), "B is live");
    assert_eq!(limiter2.snapshot().reserved_mib, 1024, "adoption re-holds");
    drop(att_c);

    // B detaches: its role and both its databases go.
    second.detach(&pg_only.names(), &b.net()).await;
    assert_eq!(exists(&role_of(&b.id)), (false, false));
    assert_eq!(
        sql(&format!(
            "select count(*) from pg_database where datname = '{}_test'",
            role_of(&b.id)
        )),
        "0"
    );

    // Unused for idle_stop (2 s), the server stops and its memory goes back.
    let t = Instant::now();
    loop {
        let (_, out) = docker(&["ps", "-a", "--filter", &format!("name=^{SERVER}$"), "-q"]);
        if out.trim().is_empty() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the server never idled out"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(limiter2.snapshot().reserved_mib, 0);
    let (ok, _) = docker(&["volume", "inspect", VOLUME]);
    assert!(!ok, "the data volume went with the server");

    // A per-job Redis on a warm image, alone.
    let d = Job::new(900_004, ts);
    let redis_only = ServicePlan {
        specs: vec![full.specs[1].clone()],
    };
    let t = Instant::now();
    second
        .attach(&redis_only, &d.net())
        .await
        .expect("attach D");
    let redis_start = t.elapsed();
    second.detach(&redis_only.names(), &d.net()).await;

    println!("timings: cold attach (server start + template + 2 jobs) {cold:?}");
    println!("timings: warm postgres attach (connect + role + template copy) {warm_pg:?}");
    println!("timings: per-job redis start {redis_start:?}");
    println!("timings: detach of postgres + redis + mailpit {detach_time:?}");

    drop((a, b, c, d));
    cleanup_server();
    drop(first);
}

fn cleanup_server_14() {
    docker(&["rm", "-f", "-v", SERVER_14]);
    docker(&["volume", "rm", VOLUME_14]);
}

/// Postgres 13/14 still let PUBLIC create in `postgres`'s `public` schema. A job
/// that plants `public.format(text, name)` there must not get it run by faber's
/// superuser scripts; and a server left over from a run with `[services]` must
/// go once nothing uses it, even after `[services]` is removed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon and the service images"]
async fn superuser_scripts_ignore_job_functions_and_unconfigured_servers_go() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let pg_image =
        std::env::var("FABER_LIVE_PG14_IMAGE").unwrap_or_else(|_| "postgres:14-alpine".into());
    cleanup_server_14();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let a = Job::new(900_011, ts);
    let b = Job::new(900_012, ts);
    let pg = ServicePlan {
        specs: vec![pg_spec("14", &pg_image)],
    };

    let first = Services::default();
    first.configure(ctx_for("14", Duration::from_secs(900), &pg_image));
    let att_a = first.attach(&pg, &a.net()).await.expect("attach A");
    first.attach(&pg, &b.net()).await.expect("attach B");

    // The server closed `postgres`'s `public` and `template1` to jobs.
    let plant = r#"psql -v ON_ERROR_STOP=1 -d postgres -c 'create function public.format(text, name) returns text language plpgsql as $f$ begin if current_user = $q$postgres$q$ then execute $q$create table public.pwned_as_superuser()$q$; end if; return $1; end $f$'"#;
    let (ok, out) = client(&a.net, &pg_image, &att_a.env(), plant);
    assert!(!ok, "A must not create in postgres's public: {out}");
    let (ok, out) = client(
        &a.net,
        &pg_image,
        &att_a.env(),
        "psql -d template1 -c 'select 1'",
    );
    assert!(!ok, "A must not connect to template1: {out}");
    let (ok, out) = client(
        &a.net,
        &pg_image,
        &att_a.env(),
        r#"psql -v ON_ERROR_STOP=1 -d postgres -c "create database ${PGUSER}_test""#,
    );
    assert!(ok, "A still creates its own test database: {out}");

    // Even with the grant back, faber's scripts never resolve to A's function.
    sql_on(
        SERVER_14,
        "postgres",
        "grant create on schema public to public",
    );
    let (ok, out) = client(&a.net, &pg_image, &att_a.env(), plant);
    assert!(ok, "A plants its function once allowed: {out}");
    first.detach(&pg.names(), &b.net()).await;
    assert_eq!(
        sql_on(
            SERVER_14,
            "postgres",
            "select count(*) from pg_class where relname = 'pwned_as_superuser'"
        ),
        "0",
        "teardown ran A's public.format as the superuser"
    );
    assert_eq!(
        sql_on(
            SERVER_14,
            "postgres",
            &format!(
                "select count(*) from pg_roles where rolname = '{}'",
                role_of(&b.id)
            )
        ),
        "0",
        "B's teardown still finished"
    );

    // A restart with `[services]` removed keeps the server while A uses it ...
    let second = Services::default();
    second.adopt(std::slice::from_ref(&a.id)).await;
    assert!(container_exists(SERVER_14), "A still uses the server");
    // ... and stops it, volume and all, when A leaves.
    second.detach(&pg.names(), &a.net()).await;
    assert!(
        !container_exists(SERVER_14),
        "the unconfigured server stopped"
    );
    let (ok, _) = docker(&["volume", "inspect", VOLUME_14]);
    assert!(!ok, "its volume went with it");

    // A server nothing uses goes at once on an unconfigured restart.
    let c = Job::new(900_013, ts);
    let third = Services::default();
    third.configure(ctx_for("14", Duration::from_secs(900), &pg_image));
    third.attach(&pg, &c.net()).await.expect("attach C");
    third.detach(&pg.names(), &c.net()).await;
    assert!(container_exists(SERVER_14), "idle, not yet stopped");
    Services::default().adopt(&[]).await;
    assert!(!container_exists(SERVER_14), "an unused server is removed");
    let (ok, _) = docker(&["volume", "inspect", VOLUME_14]);
    assert!(!ok, "and its volume");

    drop((a, b, c));
    cleanup_server_14();
}
