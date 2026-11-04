//! Declared service containers (ADR-0046): a job asks for a Postgres, or for a
//! service the operator declared, with a need label, and faber runs it next to
//! the runner, without dind.
//!
//! - **A job declares a service with a label**, in ADR-0010's namespace:
//!   `faber.postgres-16`, `faber.redis`, `faber.s3`. No `services:` block and no
//!   `dind`. [`crate::needs::services`] turns the labels into a
//!   [`ServicePlan`], or refuses the job.
//! - **The operator decides what may be asked for**, in `[services.<name>]` of
//!   faber's own config ([`ServicesPolicy`]). No table, no service; a version
//!   outside `versions` is refused. The image is always the operator's.
//! - **Postgres is shared, everything else is per job.** One server per major
//!   version (`faber-svc-postgres-<v>`, labelled [`KIND_SERVICE_SERVER`]),
//!   started on first demand and stopped after `idle_stop` unused, with one role
//!   and one database per runner ([`role_name`]). Any other name is a per-job
//!   service the operator's table describes ([`PerJobPolicy`]: image, `args`,
//!   `env`, the runner's `job_env`, a `health_cmd`, with `{secret}` filled per
//!   job by [`PerJobPolicy::env_for_job`], ADR-0062): one container per job
//!   (`faber-svc-<name>-<id>`, [`KIND_SERVICE`]), torn down with the runner and
//!   reaped by `sweep` on its `org.faber.id`. A table with a `health_cmd` (Docker
//!   Engine 25+) is waited for: faber starts every per-job service, then waits for
//!   those to report healthy ([`wait_healthy`]). One without gets no health flag
//!   and no wait, whatever its image declares.
//! - **Services reach the job by name, on the job's own network.** The shared
//!   server is connected to each job's network with `--alias postgres` and
//!   disconnected at teardown ([`release_network`] before a network is
//!   removed). There is no shared network between runners and no `localhost`.
//! - **faber names the database and the credentials**, and hands them to the
//!   runner as environment ([`Attached`]); a profile's own `env` wins.
//! - **Teardown is generated inside the server**: a script fed to `psql` over
//!   stdin, `\gexec`, and faber reads only the exit status, because what the
//!   shared server prints is input a job could shape (ADR-0025).
//! - **The shared server's memory is reserved only while it runs**
//!   ([`crate::limiter::Limiter::hold_service`]); a per-job service's
//!   `memory_mib` is added to the job's reservation ([`ServicePlan::per_job_mib`]).
//!
//! A host without `[services.*]` provisions exactly as before (§9).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tracing::{debug, info, warn};

use crate::config::SERVICE_SECRET_PLACEHOLDER;
use crate::docker::{self, CLI_TIMEOUT, LABEL_KIND, LABEL_OWNED, LAUNCH_TIMEOUT, TEARDOWN_TIMEOUT};
use crate::limiter::ServiceHold;
use crate::untrusted;

/// `org.faber.kind` of a per-job service container: reapable, carries
/// `org.faber.id` like everything else a job owns.
pub(crate) const KIND_SERVICE: &str = "service";
/// `org.faber.kind` of a shared server, its data volume and its home network.
/// Never swept: `adopt` keeps it across a restart, and idle stop retires it.
pub(crate) const KIND_SERVICE_SERVER: &str = "service-server";
/// Which service a container is: `postgres-16`, `redis`, `mailpit`
/// ([`ServiceSpec::name`]).
pub(crate) const LABEL_SERVICE: &str = "org.faber.service";
/// The shared servers' internal home network, which holds no runner.
pub(crate) const SERVER_NETWORK: &str = "faber-svc";

/// The one shared service, and the one name a per-job table may not take.
pub const POSTGRES: &str = "postgres";

/// The per-job services faber has built-in defaults for (ADR-0046 A1): the
/// variables a job reads when the table leaves out `job_env`.
pub const BUILTIN_PER_JOB: [&str; 2] = ["redis", "mailpit"];

/// Whether `tool` (the `<tool>` of `faber.<tool>`) is `postgres`, `redis` or
/// `mailpit`: names that are services whether or not a table exists, so a job
/// asking for an unconfigured one is refused as a service and not judged as a
/// tool.
pub fn is_builtin_service(tool: &str) -> bool {
    tool == POSTGRES || BUILTIN_PER_JOB.contains(&tool)
}

/// A built-in per-job service's `job_env` when its table leaves it out
/// (ADR-0046 §4). Empty for any other name.
pub fn builtin_job_env(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "redis" => &[("REDIS_URL", "redis://redis:6379/0")],
        "mailpit" => &[
            ("MAIL_SMTP_URL", "smtp://mailpit:1025"),
            ("MAIL_API_URL", "http://mailpit:8025"),
        ],
        _ => &[],
    }
}

/// Whether `name` is a host name faber can put on a job's network as a
/// container alias (ADR-0046 A1): `^[a-z][a-z0-9]{0,31}$`. It says nothing of
/// the names a table may not take (`postgres`, `dind`, a `[needs]` key).
pub fn service_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The operator's `[services.*]` tables, resolved. A `None` service cannot be
/// asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServicesPolicy {
    pub postgres: Option<PostgresPolicy>,
    /// Every other `[services.<name>]`, by name: one container per job.
    pub per_job: BTreeMap<String, PerJobPolicy>,
}

/// `[services.postgres]`: the shared server.
#[derive(Debug, Clone, PartialEq)]
pub struct PostgresPolicy {
    /// The major versions a job may ask for.
    pub versions: Vec<String>,
    /// The image, with `{version}` standing for the major version.
    pub image: String,
    /// Installed into `faber_tpl`, the template every job's database is created
    /// from.
    pub extensions: Vec<String>,
    /// Reserved while the server runs, and its `--memory` cap.
    pub memory_mib: u64,
    /// How long the server may sit unused before faber stops it.
    pub idle_stop: Duration,
}

/// Environment variables as name/value pairs, in the order they are set.
pub type EnvVars = Vec<(String, String)>;

/// `[services.<name>]` for any name but `postgres`: one container per job
/// (ADR-0046 A1).
#[derive(Clone, PartialEq, Eq)]
pub struct PerJobPolicy {
    pub image: String,
    /// The container's cap, added to the job's reservation.
    pub memory_mib: u64,
    /// Appended after the image: the container's command. Never a secret.
    pub args: Vec<String>,
    /// The container's own environment. Travels name-only on the argv.
    pub env: BTreeMap<String, String>,
    /// What the runner gets, resolved: the table's own, or the built-in default
    /// of `redis` and `mailpit` when it states none.
    pub job_env: BTreeMap<String, String>,
    /// Passed as `--health-cmd`; absent, the image's own `HEALTHCHECK` counts.
    pub health_cmd: Option<String>,
}

/// Values of `env` may be credentials, so a `{:?}` shows the names alone.
impl std::fmt::Debug for PerJobPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerJobPolicy")
            .field("image", &self.image)
            .field("memory_mib", &self.memory_mib)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("job_env", &self.job_env)
            .field("health_cmd", &self.health_cmd)
            .finish()
    }
}

impl PerJobPolicy {
    /// A table with only an image and a cap, and the built-in variables when
    /// `name` is `redis` or `mailpit`: what a bare `[services.<name>]` resolves
    /// to.
    pub fn builtin(name: &str, image: &str, memory_mib: u64) -> Self {
        Self {
            image: image.to_string(),
            memory_mib,
            args: Vec::new(),
            env: BTreeMap::new(),
            job_env: builtin_job_env(name)
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            health_cmd: None,
        }
    }

    /// Whether a value of `env` or `job_env` asks for a generated secret
    /// (ADR-0062).
    pub fn wants_secret(&self) -> bool {
        self.env
            .values()
            .chain(self.job_env.values())
            .any(|v| v.contains(SERVICE_SECRET_PLACEHOLDER))
    }

    /// The container's `env` and the runner's `job_env` for one job, with every
    /// [`SERVICE_SECRET_PLACEHOLDER`] replaced by `secret`: the two sides of one
    /// credential (ADR-0062).
    pub fn env_for_job(&self, secret: &str) -> (EnvVars, EnvVars) {
        let fill = |vars: &BTreeMap<String, String>| -> EnvVars {
            vars.iter()
                .map(|(k, v)| (k.clone(), v.replace(SERVICE_SECRET_PLACEHOLDER, secret)))
                .collect()
        };
        (fill(&self.env), fill(&self.job_env))
    }
}

impl ServicesPolicy {
    /// Whether no service is configured: the host provisions exactly as before.
    pub fn is_empty(&self) -> bool {
        self.postgres.is_none() && self.per_job.is_empty()
    }

    /// Whether a `faber.<tool>` label's `tool` is a service (ADR-0046 A1):
    /// `postgres`, `redis`, `mailpit`, or a configured `[services]` name. Case
    /// is the caller's: labels are compared lowercase.
    pub fn is_service_tool(&self, tool: &str) -> bool {
        is_builtin_service(tool) || self.per_job.contains_key(tool)
    }

    /// Whether `tool` with this `version` (`None` = the bare label) is a service
    /// label (ADR-0046 A1). A per-job service is asked for only by its bare label,
    /// so `faber.<tool>-<v>` of a per-job table is a `[needs]` label; Postgres is
    /// the versioned one, and takes both forms here so a malformed one is refused
    /// as a service, not judged as a tool. Labels are compared lowercase.
    pub fn is_service_label(&self, tool: &str, version: Option<&str>) -> bool {
        if tool == POSTGRES {
            return true;
        }
        self.is_service_tool(tool) && version.is_none()
    }
}

/// One service a job will get, resolved against the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The service: `postgres`, or a per-job table's name. Also its host name on
    /// the job's network.
    pub service: String,
    /// The major version, for a service that has one.
    pub version: Option<String>,
    /// The image, `{version}` already substituted.
    pub image: String,
    pub memory_mib: u64,
    /// The need label verbatim, which the runner is registered with.
    pub label: String,
    /// How to run it, for a per-job service. `None` for the shared Postgres.
    pub per_job: Option<PerJobPolicy>,
}

impl ServiceSpec {
    /// `postgres-16`, `redis`: what [`LABEL_SERVICE`] and the audit line say.
    pub fn name(&self) -> String {
        match &self.version {
            Some(v) => format!("{}-{v}", self.service),
            None => self.service.clone(),
        }
    }

    /// Whether one server serves every job (Postgres), rather than one container
    /// per job (§3).
    pub fn shared(&self) -> bool {
        self.service == POSTGRES
    }
}

/// Every service one job will get. Empty for a job that declared none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServicePlan {
    pub specs: Vec<ServiceSpec>,
}

impl ServicePlan {
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The need labels the runner must carry for these services.
    pub fn labels(&self) -> Vec<String> {
        self.specs.iter().map(|s| s.label.clone()).collect()
    }

    /// What the per-job containers add to the job's reservation. A shared
    /// server's memory is held by the server, not the job (§7).
    pub fn per_job_mib(&self) -> u64 {
        self.specs
            .iter()
            .filter(|s| !s.shared())
            .map(|s| s.memory_mib)
            .sum()
    }

    /// [`ServiceSpec::name`] of each service, in order.
    pub fn names(&self) -> Vec<String> {
        self.specs.iter().map(ServiceSpec::name).collect()
    }

    /// The names, comma-joined: one label value a container can carry.
    pub fn mark(&self) -> String {
        self.names().join(",")
    }
}

/// The job a service is attached to or detached from.
pub struct JobNet<'a> {
    /// The claim id, `org.faber.id`.
    pub id: &'a str,
    pub owner_repo: &'a str,
    /// The job's private network (ADR-0020).
    pub net: &'a str,
}

/// What [`Services::attach`] gives the runner: its environment.
///
/// No `Debug` and no `Display`: it carries the role password.
pub struct Attached {
    env: Vec<(String, String)>,
}

impl Attached {
    /// Nothing attached: a job with no services.
    pub fn none() -> Self {
        Self { env: Vec::new() }
    }

    /// The variable names alone, safe to log.
    pub fn env_names(&self) -> Vec<&str> {
        self.env.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// The environment to set on the runner container.
    pub fn env(&self) -> Vec<(&str, &str)> {
        self.env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// What the services need from the rest of faber, set once at boot.
pub struct Ctx {
    pub policy: ServicesPolicy,
    /// Where a shared server's reservation is held.
    pub limiter: Arc<crate::limiter::Limiter>,
    /// `[host] pull_unqualified_images`, for
    /// [`crate::profile_image::pull_allowed`].
    pub pull_unqualified: Option<bool>,
}

/// Every service faber runs. One per process ([`global`]), because the docker
/// backend's launch path and the reconcile loop both reach it.
#[derive(Default)]
pub struct Services {
    /// Shared with the idle ticker [`Self::configure`] spawns, which outlives the
    /// borrow `configure` is called with.
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    ctx: OnceLock<Ctx>,
    /// One slot per Postgres major version, made on first demand or by `adopt`.
    /// The map lock is held only to find or insert a slot, never across a call.
    slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Slot>>>>,
    /// Two versions' cold starts both make sure `faber-svc` exists; this keeps
    /// them from racing each other into a spurious "already exists".
    net_once: tokio::sync::Mutex<()>,
}

/// One shared server's state. Its async lock is the single flight: an attach
/// that finds the server down starts it while holding the lock, and every
/// concurrent attach for the same version waits on that one start.
struct Slot {
    version: String,
    /// `Some` while the server runs: its memory, held out of the Docker budget
    /// (§7). Dropping it credits the memory back.
    running: Option<ServiceHold>,
    /// Claim ids of the runners attached to it. A server with users is never
    /// stopped for idleness.
    users: HashSet<String>,
    /// Since when `users` has been empty.
    idle_since: Instant,
}

impl Slot {
    fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
            running: None,
            users: HashSet::new(),
            idle_since: Instant::now(),
        }
    }
}

/// The process's services.
pub fn global() -> &'static Services {
    static SERVICES: OnceLock<Services> = OnceLock::new();
    SERVICES.get_or_init(Services::default)
}

impl Services {
    /// Turn services on for this process. Called once at boot, and only when a
    /// `[services.*]` table exists.
    pub fn configure(&self, ctx: Ctx) {
        let period = tick_period(ctx.policy.postgres.as_ref().map(|p| p.idle_stop));
        if self.inner.ctx.set(ctx).is_err() {
            warn!("services: configured twice; keeping the first");
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let inner = self.inner.clone();
                rt.spawn(async move {
                    let mut tick = tokio::time::interval(period);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tick.tick().await;
                        inner.idle_pass().await;
                    }
                });
            }
            Err(_) => warn!(
                "services: configured outside a runtime; shared servers will not stop when idle"
            ),
        }
    }

    /// Whether any service is configured.
    pub fn enabled(&self) -> bool {
        self.inner.ctx.get().is_some()
    }

    /// Bring up `plan`'s services for `job`: start each per-job container on
    /// the job's network, start the shared server if it is not running, connect
    /// it to the network, and create the job's role and database. Returns the
    /// runner's environment.
    pub async fn attach(&self, plan: &ServicePlan, job: &JobNet<'_>) -> anyhow::Result<Attached> {
        if plan.is_empty() {
            return Ok(Attached::none());
        }
        let ctx = self
            .inner
            .ctx
            .get()
            .context("services: a job declared services, but none are configured")?;
        match self.inner.attach_all(ctx, plan, job).await {
            Ok(attached) => Ok(attached),
            Err(e) => {
                // Undo whatever got this far. Detach is idempotent, so undoing a
                // service this attach never reached is a no-op.
                self.detach(&plan.names(), job).await;
                Err(e)
            }
        }
    }

    /// Undo [`Self::attach`] for `job`: drop its role and every database it
    /// owns, disconnect the shared server from its network, and remove its
    /// per-job containers. Best effort: `sweep` is the backstop.
    pub async fn detach(&self, plan_names: &[String], job: &JobNet<'_>) {
        for name in plan_names {
            match parse_service_name(name) {
                Some((POSTGRES, Some(version))) => {
                    self.inner.leave_postgres(version, job).await;
                }
                Some((service, None)) => {
                    let Some(container) = per_job_container(service, job.id) else {
                        warn!("services: {} is not a claim id; skipping", job.id);
                        continue;
                    };
                    if let Err(e) = docker::docker_rm(&["rm", "-f", "-v", &container]).await {
                        warn!("services: removing {container}: {e:#}");
                    }
                }
                _ => warn!(
                    "services: {:?} is not a service name faber gives; skipping",
                    untrusted::for_log(name, 64)
                ),
            }
        }
    }

    /// After a restart: keep the running servers and the roles of
    /// `live_claim_ids`, and drop the roles of every other runner.
    pub async fn adopt(&self, live_claim_ids: &[String]) {
        self.inner.adopt(live_claim_ids).await;
    }
}

impl Inner {
    fn slot(&self, version: &str) -> Arc<tokio::sync::Mutex<Slot>> {
        self.slots
            .lock()
            .unwrap()
            .entry(version.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Slot::new(version))))
            .clone()
    }

    fn existing_slot(&self, version: &str) -> Option<Arc<tokio::sync::Mutex<Slot>>> {
        self.slots.lock().unwrap().get(version).cloned()
    }

    async fn attach_all(
        &self,
        ctx: &Ctx,
        plan: &ServicePlan,
        job: &JobNet<'_>,
    ) -> anyhow::Result<Attached> {
        anyhow::ensure!(
            untrusted::object_name(job.net),
            "services: the job's network is not a name faber gives"
        );
        let mut env = Vec::new();
        // Every per-job container is started before any is waited for, so their
        // start-ups overlap (ADR-0046 A3).
        let mut started_containers: Vec<(String, String)> = Vec::new();
        for spec in &plan.specs {
            let started = Instant::now();
            match &spec.per_job {
                None => env.extend(self.attach_postgres(ctx, spec, job).await?),
                Some(run) => {
                    let args = per_job_run_args(spec, job)
                        .context("services: the job is not a claim faber minted")?;
                    ensure_image(&spec.image, ctx.pull_unqualified).await?;
                    // One secret per job and service, never stored (ADR-0062).
                    let secret = if run.wants_secret() {
                        random_password()?
                    } else {
                        String::new()
                    };
                    let (service_env, job_env) = run.env_for_job(&secret);
                    let child_env: Vec<(&str, &str)> = service_env
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    docker::docker_ok_with_env(&args, LAUNCH_TIMEOUT, &child_env)
                        .await
                        .with_context(|| format!("services: starting {}", spec.name()))?;
                    if run.health_cmd.is_some() {
                        if let Some(container) = per_job_container(&spec.service, job.id) {
                            started_containers.push((spec.name(), container));
                        }
                    }
                    env.extend(job_env);
                }
            }
            debug!(
                "services: {} attached to {} in {:?}",
                spec.name(),
                job.id,
                started.elapsed()
            );
        }
        wait_healthy(&started_containers).await?;
        Ok(Attached { env })
    }

    async fn attach_postgres(
        &self,
        ctx: &Ctx,
        spec: &ServiceSpec,
        job: &JobNet<'_>,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let version = spec
            .version
            .as_deref()
            .filter(|v| valid_version(v))
            .context("services: a postgres service without a major version")?;
        let role =
            Role::for_claim(job.id).context("services: the job is not a claim faber minted")?;
        let password = random_password()?;
        let server = server_name(version);
        // Twice at most: a server that died since its last use is restarted, once.
        for attempt in 0..2 {
            let slot = self.slot(version);
            self.ensure_server(ctx, &slot, spec, job.id).await?;
            match join_server(&server, job.net, &role, &password).await {
                Ok(()) => return Ok(postgres_env(&role, &password)),
                Err(e) if attempt == 0 && !server_running(&server).await => {
                    warn!("services: {server} is not running ({e:#}); restarting it");
                    let mut s = slot.lock().await;
                    if s.running.take().is_some() {
                        remove_server(version).await;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        anyhow::bail!("services: {server} died twice while a job attached")
    }

    /// Start `spec`'s server if it is down, and count `id` as a user of it.
    async fn ensure_server(
        &self,
        ctx: &Ctx,
        slot: &tokio::sync::Mutex<Slot>,
        spec: &ServiceSpec,
        id: &str,
    ) -> anyhow::Result<()> {
        let mut s = slot.lock().await;
        if s.running.is_none() {
            let started = Instant::now();
            let hold = self.start_server(ctx, &s.version, spec).await?;
            info!(
                "services: {} started in {:?}",
                server_name(&s.version),
                started.elapsed()
            );
            s.running = Some(hold);
        }
        s.users.insert(id.to_string());
        Ok(())
    }

    /// Start a fresh server for `version`, with its template database, and hold
    /// its memory. On failure nothing it made is left behind.
    async fn start_server(
        &self,
        ctx: &Ctx,
        version: &str,
        spec: &ServiceSpec,
    ) -> anyhow::Result<ServiceHold> {
        let name = server_name(version);
        let extensions = ctx
            .policy
            .postgres
            .as_ref()
            .map(|p| p.extensions.clone())
            .unwrap_or_default();
        let template = template_sql(&extensions)?;
        ensure_image(&spec.image, ctx.pull_unqualified).await?;
        self.ensure_server_net().await?;
        // A server this process did not adopt is a leftover: throwaway by design
        // (§7), and starting over is how a dead server comes back.
        remove_server(version).await;
        let hold = ctx.limiter.hold_service(spec.memory_mib);
        let started = async {
            docker::docker_ok(&server_volume_create_args(version), CLI_TIMEOUT).await?;
            // The superuser's password is never used: faber reaches the server
            // over its local socket. It exists so the image does not fall back to
            // `trust`, which would make every job a superuser over TCP.
            let superuser = random_password()?;
            docker::docker_ok_with_env(
                &server_run_args(version, &spec.image, spec.memory_mib),
                LAUNCH_TIMEOUT,
                &[("POSTGRES_PASSWORD", &superuser)],
            )
            .await
            .with_context(|| format!("services: starting {name}"))?;
            wait_ready(&name).await?;
            let t = Instant::now();
            psql_ok(&name, template, LAUNCH_TIMEOUT, "creating faber_tpl").await?;
            info!("services: {name}: faber_tpl created in {:?}", t.elapsed());
            anyhow::Ok(())
        }
        .await;
        match started {
            Ok(()) => Ok(hold),
            Err(e) => {
                remove_server(version).await;
                drop(hold);
                Err(e)
            }
        }
    }

    /// Make sure `faber-svc` exists: `--internal`, and no runner joins it.
    async fn ensure_server_net(&self) -> anyhow::Result<()> {
        let _once = self.net_once.lock().await;
        if docker::docker_ok(&server_net_inspect_args(), CLI_TIMEOUT)
            .await
            .is_ok()
        {
            return Ok(());
        }
        let created = docker::docker_ok(&server_net_create_args(), LAUNCH_TIMEOUT).await;
        if created.is_err()
            && docker::docker_ok(&server_net_inspect_args(), CLI_TIMEOUT)
                .await
                .is_ok()
        {
            return Ok(());
        }
        created.with_context(|| format!("services: creating {SERVER_NETWORK}"))
    }

    async fn leave_postgres(&self, version: &str, job: &JobNet<'_>) {
        let server = server_name(version);
        let slot = self.existing_slot(version);
        // Unconfigured, a slot is only made by `adopt`, for a server it left
        // running for its remaining users, and holds no memory.
        let unconfigured = self.ctx.get().is_none();
        // A server known to be down has nothing to drop: it comes back empty.
        let running = match &slot {
            Some(s) => unconfigured || s.lock().await.running.is_some(),
            None => true,
        };
        if running {
            match Role::for_claim(job.id) {
                Some(role) => {
                    if let Err(e) =
                        psql_ok(&server, teardown_sql(&role), TEARDOWN_TIMEOUT, "teardown").await
                    {
                        warn!("services: {server}: dropping {}: {e:#}", role.0);
                    }
                }
                None => warn!("services: {} is not a claim id; no role to drop", job.id),
            }
        }
        if untrusted::object_name(job.net) {
            disconnect(job.net, &server).await;
        }
        let Some(slot) = slot else { return };
        let mut s = slot.lock().await;
        if s.users.remove(job.id) && s.users.is_empty() {
            s.idle_since = Instant::now();
            if (unconfigured || s.running.is_some()) && !self.version_wanted(version) {
                info!("services: {server} is no longer in [services.postgres] and unused; stopping it");
                remove_server(version).await;
                s.running = None;
            }
        }
    }

    /// Whether the policy still offers `version`.
    fn version_wanted(&self, version: &str) -> bool {
        self.ctx
            .get()
            .and_then(|c| c.policy.postgres.as_ref())
            .is_some_and(|p| p.versions.iter().any(|v| v.trim() == version))
    }

    /// One idle-ticker pass: stop every server unused for `idle_stop`, and every
    /// unused one whose version the policy no longer offers.
    async fn idle_pass(&self) {
        let idle_stop = self
            .ctx
            .get()
            .and_then(|c| c.policy.postgres.as_ref())
            .map(|p| p.idle_stop);
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            // A locked slot is starting or being used: not idle.
            let Ok(mut s) = slot.try_lock() else { continue };
            if s.running.is_none() || !s.users.is_empty() {
                continue;
            }
            let wanted = self.version_wanted(&s.version);
            let idle = idle_stop.is_some_and(|limit| s.idle_since.elapsed() >= limit);
            if !wanted || idle {
                let version = s.version.clone();
                info!(
                    "services: stopping {} (idle {:?}, offered: {wanted})",
                    server_name(&version),
                    s.idle_since.elapsed()
                );
                remove_server(&version).await;
                s.running = None;
            }
        }
    }

    async fn adopt(&self, live_claim_ids: &[String]) {
        let listing = match docker::docker_lines(&server_ls_args(), CLI_TIMEOUT).await {
            Ok(lines) => lines,
            Err(e) => {
                warn!("services: listing shared servers: {e:#}");
                return;
            }
        };
        let versions: Vec<String> =
            untrusted::accept(
                listing,
                "services: server listing",
                |n| match version_of_server(&n) {
                    Some(v) => Ok(Some(v.to_string())),
                    None => Err("a container name is not one faber gives a shared server"),
                },
            );
        let live_roles: Vec<Role> = live_claim_ids
            .iter()
            .filter_map(|id| Role::for_claim(id))
            .collect();
        let ctx = self.ctx.get();
        let mut kept: HashSet<String> = HashSet::new();
        for version in &versions {
            let server = server_name(version);
            let state = inspect_server(&server).await;
            let users: HashSet<String> = match &state {
                Some(st) => live_claim_ids
                    .iter()
                    .filter(|id| st.networks.contains(&job_network(id)))
                    .cloned()
                    .collect(),
                None => HashSet::new(),
            };
            match (state, ctx) {
                (Some(st), Some(ctx)) if st.running => {
                    let mib = match st.memory_mib {
                        0 => ctx.policy.postgres.as_ref().map_or(0, |p| p.memory_mib),
                        m => m,
                    };
                    let hold = ctx.limiter.hold_service(mib);
                    if let Err(e) = psql_ok(
                        &server,
                        orphan_sql(&live_roles),
                        TEARDOWN_TIMEOUT,
                        "orphans",
                    )
                    .await
                    {
                        warn!("services: {server}: dropping orphaned roles: {e:#}");
                    }
                    let slot = self.slot(version);
                    let mut s = slot.lock().await;
                    s.running = Some(hold);
                    s.idle_since = Instant::now();
                    s.users = users;
                    info!(
                        "services: adopted {server} with {} runner(s) on it",
                        s.users.len()
                    );
                    if s.users.is_empty() && !self.version_wanted(version) {
                        remove_server(version).await;
                        s.running = None;
                    } else {
                        kept.insert(version.clone());
                    }
                }
                (Some(st), None) if st.running && !users.is_empty() => {
                    warn!(
                        "services: {server} still serves {} runner(s), but no service is \
                         configured; stopping it once they finish",
                        users.len()
                    );
                    if let Err(e) = psql_ok(
                        &server,
                        orphan_sql(&live_roles),
                        TEARDOWN_TIMEOUT,
                        "orphans",
                    )
                    .await
                    {
                        warn!("services: {server}: dropping orphaned roles: {e:#}");
                    }
                    // No memory is held: with no limiter there is nothing to
                    // hold it in. `leave_postgres` stops the server when the
                    // last of these users leaves.
                    let slot = self.slot(version);
                    let mut s = slot.lock().await;
                    s.idle_since = Instant::now();
                    s.users = users;
                    kept.insert(version.clone());
                }
                _ => {
                    info!("services: removing {server}: stopped, unreadable, or unused and unconfigured");
                    remove_server(version).await;
                }
            }
        }
        // A data volume whose server is gone is throwaway (§7).
        if let Ok(vols) = docker::docker_lines(&server_volume_ls_args(), CLI_TIMEOUT).await {
            for vol in vols {
                let Some(version) = version_of_volume(&vol) else {
                    continue;
                };
                if !kept.contains(version) {
                    let _ = docker::docker_rm(&["volume", "rm", &vol]).await;
                }
            }
        }
    }
}

/// Whether any shared server container exists, running or not. `true` when the
/// daemon cannot be listed, so a caller releases networks rather than skip it.
pub(crate) async fn any_server() -> bool {
    match docker::docker_lines(&server_ls_args(), CLI_TIMEOUT).await {
        Ok(lines) => lines.iter().any(|n| version_of_server(n).is_some()),
        Err(_) => true,
    }
}

/// Disconnect every shared server from `net` (`network disconnect -f`), so a
/// leaked attachment does not block its `network rm`.
pub(crate) async fn release_network(net: &str) {
    if net == SERVER_NETWORK || !untrusted::object_name(net) {
        return;
    }
    let listing = match docker::docker_lines(&server_ls_args(), CLI_TIMEOUT).await {
        Ok(lines) => lines,
        Err(e) => {
            warn!("services: listing shared servers to release {net}: {e:#}");
            return;
        }
    };
    for name in listing {
        if version_of_server(&name).is_some() {
            disconnect(net, &name).await;
        }
    }
}

/// The role and database name of a runner: `faber_r_<job>_<ts>`, from its claim
/// id. `None` when `claim_id` is not one ([`crate::untrusted::claim_id`]), so a
/// name faber did not mint never reaches SQL.
pub(crate) fn role_name(claim_id: &str) -> Option<String> {
    let (job, ts) = crate::untrusted::claim_id(claim_id)?;
    Some(format!("faber_r_{job}_{ts}"))
}

// --- docker calls ------------------------------------------------------------

/// Connect the server to the job's network and make the job's role and database.
async fn join_server(server: &str, net: &str, role: &Role, password: &str) -> anyhow::Result<()> {
    docker::docker_ok(&connect_args(net, server), LAUNCH_TIMEOUT)
        .await
        .with_context(|| format!("services: connecting {server} to {net}"))?;
    let t = Instant::now();
    psql_ok(server, create_sql(role, password), LAUNCH_TIMEOUT, "create").await?;
    debug!(
        "services: {server}: {} created in {:?}",
        role.0,
        t.elapsed()
    );
    Ok(())
}

/// Feed `script` to the server's `psql` over stdin, and read only its exit
/// status. What it printed is the shared server's, which a job can shape
/// (ADR-0025), and its stderr may quote a statement that carries a password, so
/// none of it reaches an error or a log.
async fn psql_ok(server: &str, script: Vec<u8>, limit: Duration, what: &str) -> anyhow::Result<()> {
    let out = docker::docker_fed(&psql_args(server), script, limit).await?;
    anyhow::ensure!(
        out.status.success(),
        "services: {server}: psql ({what}) exited {}",
        out.status
    );
    Ok(())
}

/// `network disconnect -f`, where "not connected" and "gone" are both done.
async fn disconnect(net: &str, server: &str) {
    if let Err(e) = docker::docker_ok(&disconnect_args(net, server), TEARDOWN_TIMEOUT).await {
        let msg = e.to_string().to_lowercase();
        if !(msg.contains("not connected") || msg.contains("no such") || msg.contains("not found"))
        {
            warn!("services: disconnecting {server} from {net}: {e:#}");
        }
    }
}

/// Remove a server and its data volume. Both are faber's, rebuilt from config
/// alone (§7). Never its image, which faber did not build (ADR-0007).
async fn remove_server(version: &str) {
    let name = server_name(version);
    if let Err(e) = docker::docker_rm(&["rm", "-f", "-v", &name]).await {
        warn!("services: removing {name}: {e:#}");
    }
    let vol = server_volume(version);
    if let Err(e) = docker::docker_rm(&["volume", "rm", &vol]).await {
        warn!("services: removing {vol}: {e:#}");
    }
}

/// Wait until the server answers on TCP. Not on its socket: the image's
/// entrypoint first runs a temporary server that listens on the socket only, and
/// stops it again once initialisation is done.
async fn wait_ready(server: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if docker::docker_ok(&ready_args(server), READY_PROBE_TIMEOUT)
            .await
            .is_ok()
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "services: {server} did not become ready within {READY_TIMEOUT:?}"
        );
        tokio::time::sleep(READY_POLL).await;
    }
}

async fn server_running(server: &str) -> bool {
    inspect_server(server).await.is_some_and(|s| s.running)
}

/// What faber reads of a server from the daemon (never from the server).
struct ServerState {
    running: bool,
    memory_mib: u64,
    networks: HashSet<String>,
}

async fn inspect_server(server: &str) -> Option<ServerState> {
    let out = docker::docker_stdout(&server_inspect_args(server), CLI_TIMEOUT, 64 * 1024)
        .await
        .ok()?;
    parse_server_state(&out)
}

fn parse_server_state(out: &str) -> Option<ServerState> {
    let mut words = out.split_whitespace();
    let running = match words.next()? {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    let bytes: u64 = words.next()?.parse().ok()?;
    let networks = words
        .filter(|n| untrusted::object_name(n))
        .map(String::from)
        .collect();
    Some(ServerState {
        running,
        memory_mib: bytes / (1024 * 1024),
        networks,
    })
}

/// The image is present, or pulled now under the pull rule (#156).
async fn ensure_image(image: &str, pull_unqualified: Option<bool>) -> anyhow::Result<()> {
    anyhow::ensure!(
        untrusted::image_ref(image),
        "services: the image is not a reference faber passes to docker"
    );
    if docker::docker_ok(&image_inspect_args(image), CLI_TIMEOUT)
        .await
        .is_ok()
    {
        return Ok(());
    }
    anyhow::ensure!(
        crate::profile_image::pull_allowed(image, pull_unqualified),
        "services: {image} is not present, and pull_unqualified_images forbids pulling it"
    );
    let t = Instant::now();
    docker::pull_image(image)
        .await
        .map_err(|e| anyhow::anyhow!("services: pulling {image}: {e}"))?;
    info!("services: pulled {image} in {:?}", t.elapsed());
    Ok(())
}

/// 32 random hex characters from `/dev/urandom`: a password that needs no
/// quoting in a URL or an SQL literal.
pub(crate) fn random_password() -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("services: reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// --- names, argv and SQL builders (pure, unit-tested) ------------------------

const POSTGRES_PORT: u16 = 5432;
/// Sized for concurrency x pool size across every job on one server, not for one
/// job (Consequences). Each idle connection costs a few MiB at most.
const MAX_CONNECTIONS: u32 = 500;
/// `/dev/shm` for parallel query workers: Docker's 64 MiB default fails them.
const SHM_MIB: u64 = 256;
const TEMPLATE_DB: &str = "faber_tpl";
/// A cold start is an initdb plus a restart: seconds, not a minute.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL: Duration = Duration::from_millis(200);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The idle ticker's longest period. A shorter `idle_stop` ticks faster.
const TICK_MAX: Duration = Duration::from_secs(30);

fn tick_period(idle_stop: Option<Duration>) -> Duration {
    idle_stop.map_or(TICK_MAX, |d| {
        (d / 4).clamp(Duration::from_secs(1), TICK_MAX)
    })
}

/// A Postgres major version as faber puts it in a name: digits only.
fn valid_version(v: &str) -> bool {
    (1..=4).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit())
}

/// `faber-svc-postgres-<v>`.
pub(crate) fn server_name(version: &str) -> String {
    format!("faber-svc-postgres-{version}")
}

fn server_volume(version: &str) -> String {
    format!("{}-data", server_name(version))
}

fn version_of_server(name: &str) -> Option<&str> {
    name.strip_prefix("faber-svc-postgres-")
        .filter(|v| valid_version(v))
}

fn version_of_volume(name: &str) -> Option<&str> {
    name.strip_suffix("-data").and_then(version_of_server)
}

/// The job network the docker backend names for a claim (`docker.rs`'s
/// `Names::for_runner`). Only `adopt` needs it, to tell which live runners a
/// server it found is still connected to.
fn job_network(id: &str) -> String {
    format!("faber-net-{id}")
}

/// A per-job service's container: `faber-svc-<name>-<id>` (ADR-0046 A4). The
/// `svc` keeps an operator's name from colliding with a container faber names
/// itself. `None` when `id` is not a claim id or `name` is not a service name.
pub(crate) fn per_job_container(name: &str, id: &str) -> Option<String> {
    crate::untrusted::claim_id(id)?;
    (service_name_ok(name) && name != POSTGRES).then(|| format!("faber-svc-{name}-{id}"))
}

/// `postgres-16` → ("postgres", Some("16")); `s3` → ("s3", None). Anything else,
/// a bare `postgres` included, is `None`: the forms faber writes into
/// [`LABEL_SERVICE`] and the `org.faber.services` label, and no others.
fn parse_service_name(name: &str) -> Option<(&str, Option<&str>)> {
    match name.split_once('-') {
        Some((POSTGRES, v)) => valid_version(v).then_some((POSTGRES, Some(v))),
        Some(_) => None,
        None => (name != POSTGRES && service_name_ok(name)).then_some((name, None)),
    }
}

/// The form of a service name in the `org.faber.services` label, for adoption
/// (ADR-0046 A4): `postgres-<digits>` or a bare name.
pub(crate) fn service_name_in_faber_form(name: &str) -> bool {
    parse_service_name(name).is_some()
}

fn label_args(pairs: &[(&str, &str)]) -> Vec<String> {
    pairs
        .iter()
        .flat_map(|(k, v)| ["--label".to_string(), format!("{k}={v}")])
        .collect()
}

/// What a server, its volume and `faber-svc` carry: no repo and no id, so
/// `sweep` never takes them for a job's leak.
fn server_marks(service: &str) -> Vec<String> {
    label_args(&[
        (LABEL_OWNED, "true"),
        (LABEL_KIND, KIND_SERVICE_SERVER),
        (LABEL_SERVICE, service),
    ])
}

fn server_net_inspect_args() -> Vec<String> {
    vec![
        "network".into(),
        "inspect".into(),
        "--format".into(),
        "{{.Name}}".into(),
        SERVER_NETWORK.into(),
    ]
}

fn server_net_create_args() -> Vec<String> {
    let mut a: Vec<String> = vec!["network".into(), "create".into(), "--internal".into()];
    a.extend(label_args(&[
        (LABEL_OWNED, "true"),
        (LABEL_KIND, KIND_SERVICE_SERVER),
    ]));
    a.push(SERVER_NETWORK.into());
    a
}

fn server_volume_create_args(version: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["volume".into(), "create".into()];
    a.extend(server_marks(&format!("postgres-{version}")));
    a.push(server_volume(version));
    a
}

/// Where the data volume mounts: the image's own `VOLUME`, so it leaves no
/// anonymous volume beside it. Postgres 18's image moved it up one level.
fn data_mount(version: &str) -> &'static str {
    match version.parse::<u32>() {
        Ok(v) if v >= 18 => "/var/lib/postgresql",
        _ => "/var/lib/postgresql/data",
    }
}

/// `docker run` for a shared server. The superuser password travels name-only
/// (`-e POSTGRES_PASSWORD`), its value in the CLI child's environment.
fn server_run_args(version: &str, image: &str, memory_mib: u64) -> Vec<String> {
    let mem = format!("{memory_mib}m");
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--pull=never".into(),
        "--name".into(),
        server_name(version),
        "--network".into(),
        SERVER_NETWORK.into(),
        "--memory".into(),
        mem.clone(),
        "--memory-swap".into(),
        mem,
        "--shm-size".into(),
        format!("{SHM_MIB}m"),
    ];
    a.extend(server_marks(&format!("postgres-{version}")));
    a.extend([
        "-v".into(),
        format!("{}:{}", server_volume(version), data_mount(version)),
        "-e".into(),
        "POSTGRES_PASSWORD".into(),
        image.into(),
        "postgres".into(),
    ]);
    for setting in [
        "fsync=off".to_string(),
        "synchronous_commit=off".to_string(),
        "full_page_writes=off".to_string(),
        format!("max_connections={MAX_CONNECTIONS}"),
    ] {
        a.push("-c".into());
        a.push(setting);
    }
    a
}

fn ready_args(server: &str) -> Vec<String> {
    [
        "exec",
        server,
        "pg_isready",
        "-q",
        "-h",
        "127.0.0.1",
        "-p",
        "5432",
        "-U",
        "postgres",
    ]
    .map(String::from)
    .to_vec()
}

/// `psql` as the superuser over the server's local socket, reading its script
/// from stdin: `-X` skips any `.psqlrc`, `-q` and `ON_ERROR_STOP` make the exit
/// status the whole answer.
fn psql_args(server: &str) -> Vec<String> {
    [
        "exec",
        "-i",
        server,
        "psql",
        "-X",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "-U",
        "postgres",
        "-d",
        "postgres",
    ]
    .map(String::from)
    .to_vec()
}

fn connect_args(net: &str, server: &str) -> Vec<String> {
    ["network", "connect", "--alias", POSTGRES, net, server]
        .map(String::from)
        .to_vec()
}

fn disconnect_args(net: &str, server: &str) -> Vec<String> {
    ["network", "disconnect", "-f", net, server]
        .map(String::from)
        .to_vec()
}

fn image_inspect_args(image: &str) -> Vec<String> {
    ["image", "inspect", "--format", "{{.Id}}", image]
        .map(String::from)
        .to_vec()
}

fn server_filter_args() -> Vec<String> {
    vec![
        "--filter".into(),
        format!("label={LABEL_OWNED}=true"),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_SERVICE_SERVER}"),
    ]
}

fn server_ls_args() -> Vec<String> {
    let mut a: Vec<String> = vec!["ps".into(), "-a".into()];
    a.extend(server_filter_args());
    a.extend(["--format".into(), "{{.Names}}".into()]);
    a
}

fn server_volume_ls_args() -> Vec<String> {
    let mut a: Vec<String> = vec!["volume".into(), "ls".into()];
    a.extend(server_filter_args());
    a.extend(["--format".into(), "{{.Name}}".into()]);
    a
}

fn server_inspect_args(server: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "--format".into(),
        "{{.State.Running}} {{.HostConfig.Memory}}{{range $k, $v := .NetworkSettings.Networks}} {{$k}}{{end}}"
            .into(),
        server.into(),
    ]
}

/// How long the per-job services with a `health_cmd` have to report healthy, all
/// of a job's together (ADR-0046 A3).
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_POLL: Duration = Duration::from_millis(250);
/// Passed with a table's `health_cmd` only, so the check runs at once and then
/// every second until it first passes, instead of after Docker's thirty-second
/// default interval. Needs Docker Engine 25 or later.
const HEALTH_START_INTERVAL: &str = "--health-start-interval=1s";
/// How long failures of that early check do not yet count against the service.
const HEALTH_START_PERIOD: &str = "--health-start-period=60s";

/// `docker run` for a per-job service, on the job's network under its name as
/// alias. `env` is name-only (`-e NAME`): the values travel in the CLI child's
/// environment, so none reaches the argv. `None` when the job's id is not a claim
/// id, or the spec is not a per-job one.
fn per_job_run_args(spec: &ServiceSpec, job: &JobNet<'_>) -> Option<Vec<String>> {
    let run = spec.per_job.as_ref()?;
    let name = per_job_container(&spec.service, job.id)?;
    let mem = format!("{}m", spec.memory_mib);
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--pull=never".into(),
        "--name".into(),
        name,
        "--network".into(),
        job.net.into(),
        "--network-alias".into(),
        spec.service.clone(),
        "--memory".into(),
        mem.clone(),
        "--memory-swap".into(),
        mem,
    ];
    // No `health_cmd`, no health flag: the argv is what it was before A3.
    if let Some(cmd) = &run.health_cmd {
        a.push("--health-cmd".into());
        a.push(cmd.clone());
        a.push(HEALTH_START_INTERVAL.into());
        a.push(HEALTH_START_PERIOD.into());
    }
    a.extend(docker::marks(KIND_SERVICE, job.owner_repo, job.id));
    a.extend(label_args(&[(LABEL_SERVICE, &spec.name())]));
    for name in run.env.keys() {
        a.push("-e".into());
        a.push(name.clone());
    }
    a.push(spec.image.clone());
    a.extend(run.args.iter().cloned());
    Some(a)
}

/// What Docker reports of a container's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// No healthcheck at all: nothing to wait for.
    Unchecked,
    Starting,
    Healthy,
    Unhealthy,
}

fn health_inspect_args(container: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "-f".into(),
        "{{if .State.Health}}{{.State.Health.Status}}{{end}}".into(),
        container.into(),
    ]
}

/// Read the status the daemon wrote, against its fixed set. Anything else is
/// `None`: faber never reads the check's own output, which a service could shape
/// (ADR-0025).
fn parse_health(out: &str) -> Option<Health> {
    match out.trim() {
        "" => Some(Health::Unchecked),
        "starting" => Some(Health::Starting),
        "healthy" => Some(Health::Healthy),
        "unhealthy" => Some(Health::Unhealthy),
        _ => None,
    }
}

/// What one reading of a container's health means for the wait.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Ready,
    /// Not yet: look again.
    Continue,
    Fail(String),
}

/// The decision for one reading of `name`: ready when healthy or unchecked,
/// failed when unhealthy or when it is still starting at the deadline (which
/// says what it last reported), otherwise keep waiting.
fn health_verdict(name: &str, health: Health, deadline_passed: bool) -> Verdict {
    match health {
        Health::Unchecked | Health::Healthy => Verdict::Ready,
        Health::Unhealthy => Verdict::Fail(format!("services: {name} reported unhealthy")),
        Health::Starting if deadline_passed => Verdict::Fail(format!(
            "services: {name} was not healthy within {HEALTH_TIMEOUT:?} (last status: starting)"
        )),
        Health::Starting => Verdict::Continue,
    }
}

/// Wait until every `(name, container)` is healthy, up to [`HEALTH_TIMEOUT`] in
/// all (ADR-0046 A3). Only containers started with a `health_cmd` are listed. An
/// `unhealthy` one, or one still starting at the deadline, fails the provision,
/// naming the service.
async fn wait_healthy(containers: &[(String, String)]) -> anyhow::Result<()> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    for (name, container) in containers {
        loop {
            let out = docker::docker_stdout(&health_inspect_args(container), CLI_TIMEOUT, 1024)
                .await
                .with_context(|| format!("services: inspecting {name}"))?;
            let health = parse_health(&out).with_context(|| {
                format!("services: {name}: Docker reported a health status faber does not know")
            })?;
            match health_verdict(name, health, Instant::now() >= deadline) {
                Verdict::Ready => break,
                Verdict::Continue => tokio::time::sleep(HEALTH_POLL).await,
                Verdict::Fail(msg) => anyhow::bail!(msg),
            }
        }
    }
    Ok(())
}

/// A role and database name faber minted: only [`role_name`] makes one, so it
/// is `faber_r_` and digits, and needs no quoting anywhere.
struct Role(String);

impl Role {
    fn for_claim(claim_id: &str) -> Option<Self> {
        role_name(claim_id).map(Self)
    }
}

/// `"name"`, for an operator's extension name. `None` for one that would need
/// more than doubling a quote: a control character or a backslash could reach
/// `psql`'s own parser.
fn quote_ident(s: &str) -> Option<String> {
    if s.is_empty() || s.len() > 63 || s.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    Some(format!("\"{}\"", s.replace('"', "\"\"")))
}

/// The first line of every superuser script, and of every connection a `\c`
/// opens in one. On Postgres 13/14 PUBLIC may create in `postgres`'s `public`
/// schema, so a job could plant e.g. `public.format(text, name)`, which beats
/// `pg_catalog.format(text, VARIADIC "any")` in overload resolution and would
/// run as the superuser. With only `pg_catalog` (and `pg_temp`, last, which
/// functions never resolve from) on the path, nothing a job made is reachable.
const SAFE_PATH: &str = "SET search_path = pg_catalog, pg_temp;\n";

/// Close the server to job-planted objects before any job joins it, then create
/// `faber_tpl` with the operator's extensions, mark it a template, and let no
/// job connect to it.
///
/// `postgres` loses PUBLIC's `CREATE` on `public` (Postgres 15's default; 13/14
/// still grant it). `template1` loses PUBLIC's `CONNECT`, so nothing a job plants
/// there is copied into another job's self-made database; `CREATE DATABASE`
/// from it still works, as it never connects to the template. Its `public`
/// schema keeps its ACL, so on 13/14 a job can still create tables in a test
/// database it makes.
///
/// Inside `faber_tpl` the path is `public` alone, so extensions land there as
/// they would by default; `pg_catalog` is then searched implicitly first. No job
/// has ever connected to `faber_tpl`, which this makes on a fresh server.
fn template_sql(extensions: &[String]) -> anyhow::Result<Vec<u8>> {
    let mut s = format!(
        "\\o /dev/null\n{SAFE_PATH}\
         REVOKE CREATE ON SCHEMA public FROM PUBLIC;\n\
         REVOKE CONNECT ON DATABASE template1 FROM PUBLIC;\n\
         CREATE DATABASE {TEMPLATE_DB};\n\
         \\c {TEMPLATE_DB}\n\
         SET search_path = public;\n"
    );
    for ext in extensions {
        let q = quote_ident(ext)
            .with_context(|| format!("services: {ext:?} is not an extension name faber passes"))?;
        s.push_str(&format!("CREATE EXTENSION IF NOT EXISTS {q};\n"));
    }
    s.push_str(&format!(
        "\\c postgres\n{SAFE_PATH}\
         ALTER DATABASE {TEMPLATE_DB} IS_TEMPLATE true;\n\
         REVOKE CONNECT ON DATABASE {TEMPLATE_DB} FROM PUBLIC;\n"
    ));
    Ok(s.into_bytes())
}

/// §4: the runner's role and database. Fed on stdin, the only place the
/// password goes besides the runner's environment.
fn create_sql(role: &Role, password: &str) -> Vec<u8> {
    debug_assert!(password.bytes().all(|b| b.is_ascii_hexdigit()));
    let r = &role.0;
    format!(
        "\\o /dev/null\n{SAFE_PATH}\
         CREATE ROLE {r} LOGIN CREATEDB PASSWORD '{password}';\n\
         CREATE DATABASE {r} TEMPLATE {TEMPLATE_DB} OWNER {r};\n\
         REVOKE CONNECT ON DATABASE {r} FROM PUBLIC;\n"
    )
    .into_bytes()
}

/// §6: drop every database `role` owns, what it owns in `postgres`, and the
/// role. Generated inside the server with `\gexec`, so no name a job chose ever
/// passes through faber. Idempotent: a role already gone selects nothing.
fn teardown_sql(role: &Role) -> Vec<u8> {
    let r = &role.0;
    format!(
        "\\o /dev/null\n{SAFE_PATH}\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '{r}';\n\
         SELECT format('DROP DATABASE %I WITH (FORCE)', datname) FROM pg_database \
         WHERE datdba = (SELECT oid FROM pg_roles WHERE rolname = '{r}') \\gexec\n\
         SELECT format('DROP OWNED BY %I', rolname) FROM pg_roles WHERE rolname = '{r}' \\gexec\n\
         DROP ROLE IF EXISTS {r};\n"
    )
    .into_bytes()
}

/// §6 for a restart: the same teardown for every `faber_r_*` role not in `live`.
/// Jobs hold `CREATEDB` but not `CREATEROLE`, so every such role is faber's.
fn orphan_sql(live: &[Role]) -> Vec<u8> {
    let list = live
        .iter()
        .map(|r| format!("'{}'", r.0))
        .collect::<Vec<_>>()
        .join(", ");
    let orphan =
        |col: &str| format!("{col} LIKE 'faber\\_r\\_%' AND {col} <> ALL (ARRAY[{list}]::name[])");
    format!(
        "\\o /dev/null\n{SAFE_PATH}\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE {a};\n\
         SELECT format('DROP DATABASE %I WITH (FORCE)', d.datname) FROM pg_database d \
         JOIN pg_roles r ON r.oid = d.datdba WHERE {b} \\gexec\n\
         SELECT format('DROP OWNED BY %I', rolname) FROM pg_roles WHERE {c} \\gexec\n\
         SELECT format('DROP ROLE %I', rolname) FROM pg_roles WHERE {c} \\gexec\n",
        a = orphan("usename"),
        b = orphan("r.rolname"),
        c = orphan("rolname"),
    )
    .into_bytes()
}

/// §4's Postgres variables.
fn postgres_env(role: &Role, password: &str) -> Vec<(String, String)> {
    let r = &role.0;
    let host = POSTGRES;
    [
        (
            "DATABASE_URL",
            format!("postgres://{r}:{password}@{host}:{POSTGRES_PORT}/{r}"),
        ),
        ("PGHOST", host.to_string()),
        ("PGPORT", POSTGRES_PORT.to_string()),
        ("PGUSER", r.clone()),
        ("PGPASSWORD", password.to_string()),
        ("PGDATABASE", r.clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "1001-1756000000000";

    fn role() -> Role {
        Role::for_claim(ID).unwrap()
    }

    /// A spec of `service`: `postgres` (16), or a per-job one with the built-in
    /// variables where it has them.
    fn spec(service: &str) -> ServiceSpec {
        let image = match service {
            "postgres" => "postgres:16-alpine",
            "redis" => "redis:7-alpine",
            "mailpit" => "axllent/mailpit:v1.21",
            _ => "versity/versitygw:v1.8.0",
        };
        ServiceSpec {
            service: service.to_string(),
            version: (service == POSTGRES).then(|| "16".to_string()),
            image: image.to_string(),
            memory_mib: 256,
            label: format!("faber.{service}"),
            per_job: (service != POSTGRES).then(|| PerJobPolicy::builtin(service, image, 256)),
        }
    }

    /// The S3 gateway of ADR-0046 A1, secrets and all.
    fn s3() -> ServiceSpec {
        let mut s = spec("s3");
        let run = s.per_job.as_mut().unwrap();
        run.args = ["--port", ":7070", "posix", "/data"]
            .map(String::from)
            .to_vec();
        run.env = [
            ("ROOT_ACCESS_KEY", "ci-access-key"),
            ("ROOT_SECRET_KEY", "ci-secret-value"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        run.job_env = [("S3_ENDPOINT", "http://s3:7070")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        run.health_cmd = Some("wget -qO- http://127.0.0.1:7070/health".to_string());
        s
    }

    fn job() -> JobNet<'static> {
        JobNet {
            id: ID,
            owner_repo: "owner/demo",
            net: "faber-net-1001-1756000000000",
        }
    }

    fn pairs(a: &[String]) -> Vec<(String, String)> {
        a.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect()
    }

    #[test]
    fn a_role_name_comes_only_from_a_claim_id() {
        assert_eq!(
            role_name("1001-1756000000000").as_deref(),
            Some("faber_r_1001_1756000000000")
        );
        assert_eq!(role_name("x; DROP ROLE postgres"), None);
    }

    #[test]
    fn a_role_name_fits_postgres_identifier_limit() {
        let r = role_name(&format!("{}-{}", u64::MAX, u64::MAX)).unwrap();
        assert!(r.len() <= 63, "{} chars", r.len());
    }

    #[test]
    fn no_secret_reaches_an_argv() {
        let pw = random_password().unwrap();
        assert_eq!(pw.len(), 32);
        assert!(pw.bytes().all(|b| b.is_ascii_hexdigit()));
        let j = job();
        let argvs = [
            server_run_args("16", "postgres:16-alpine", 2048),
            psql_args("faber-svc-postgres-16"),
            ready_args("faber-svc-postgres-16"),
            connect_args(j.net, "faber-svc-postgres-16"),
            disconnect_args(j.net, "faber-svc-postgres-16"),
            per_job_run_args(&spec("redis"), &j).unwrap(),
            per_job_run_args(&spec("mailpit"), &j).unwrap(),
            per_job_run_args(&s3(), &j).unwrap(),
            server_volume_create_args("16"),
            server_net_create_args(),
        ];
        for argv in &argvs {
            assert!(!argv.iter().any(|a| a.contains(&pw)), "{argv:?}");
            // Nor a per-job service's own `env` values (ADR-0046 A4).
            for secret in ["ci-access-key", "ci-secret-value"] {
                assert!(!argv.iter().any(|a| a.contains(secret)), "{argv:?}");
            }
            // No `-e NAME=value` anywhere: values travel in the child's env.
            for (flag, val) in pairs(argv) {
                if flag == "-e" {
                    assert!(!val.contains('='), "{argv:?}");
                }
            }
        }
        // The password does reach the server: on psql's stdin.
        let sql = String::from_utf8(create_sql(&role(), &pw)).unwrap();
        assert!(sql.contains(&format!("PASSWORD '{pw}'")));
    }

    #[test]
    fn a_server_runs_tuned_capped_and_labelled_on_faber_svc() {
        let a = server_run_args("16", "postgres:16-alpine", 2048);
        let p = pairs(&a);
        assert_eq!(&a[..3], ["run", "-d", "--pull=never"]);
        for want in [
            ("--name", "faber-svc-postgres-16"),
            ("--network", SERVER_NETWORK),
            ("--memory", "2048m"),
            ("--memory-swap", "2048m"),
            ("--label", "org.faber.owned=true"),
            ("--label", "org.faber.kind=service-server"),
            ("--label", "org.faber.service=postgres-16"),
            ("-v", "faber-svc-postgres-16-data:/var/lib/postgresql/data"),
            ("-e", "POSTGRES_PASSWORD"),
            ("-c", "fsync=off"),
            ("-c", "synchronous_commit=off"),
            ("-c", "full_page_writes=off"),
            ("-c", "max_connections=500"),
        ] {
            assert!(
                p.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} missing from {a:?}"
            );
        }
        // No repo, no id: sweep must never take it for a job's leak.
        assert!(!a.iter().any(|x| x.starts_with("org.faber.id=")));
        let img = a.iter().position(|x| x == "postgres:16-alpine").unwrap();
        assert_eq!(a[img + 1], "postgres");
        assert!(a[img + 2..].iter().step_by(2).all(|f| f == "-c"));
        let a18 = server_run_args("18", "postgres:18-alpine", 2048);
        assert!(a18.contains(&"faber-svc-postgres-18-data:/var/lib/postgresql".to_string()));
    }

    #[test]
    fn the_server_network_is_internal_and_faber_s() {
        let a = server_net_create_args();
        assert_eq!(&a[..3], ["network", "create", "--internal"]);
        assert!(a.contains(&"org.faber.kind=service-server".to_string()));
        assert_eq!(a.last().unwrap(), SERVER_NETWORK);
    }

    #[test]
    fn a_per_job_service_is_a_job_resource_on_the_job_network() {
        let j = job();
        let a = per_job_run_args(&spec("redis"), &j).unwrap();
        let p = pairs(&a);
        assert_eq!(&a[..3], ["run", "-d", "--pull=never"]);
        for want in [
            ("--name", "faber-svc-redis-1001-1756000000000"),
            ("--network", j.net),
            ("--network-alias", "redis"),
            ("--memory", "256m"),
            ("--memory-swap", "256m"),
            ("--label", "org.faber.kind=service"),
            ("--label", "org.faber.id=1001-1756000000000"),
            ("--label", "org.faber.repo=owner/demo"),
            ("--label", "org.faber.service=redis"),
        ] {
            assert!(
                p.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} missing from {a:?}"
            );
        }
        assert_eq!(a.last().unwrap(), "redis:7-alpine");
        let bad = JobNet {
            id: "not-a-claim",
            ..job()
        };
        assert!(per_job_run_args(&spec("redis"), &bad).is_none());
        assert!(
            per_job_run_args(&spec("postgres"), &j).is_none(),
            "the shared server is not a per-job container"
        );
    }

    /// ADR-0046 A1, A3, A4: a generic table's container is named, aliased,
    /// commanded, checked and given its environment as the table says.
    #[test]
    fn a_generic_per_job_service_runs_as_its_table_says() {
        let j = job();
        let a = per_job_run_args(&s3(), &j).unwrap();
        let p = pairs(&a);
        for want in [
            ("--name", "faber-svc-s3-1001-1756000000000"),
            ("--network-alias", "s3"),
            ("--label", "org.faber.service=s3"),
            ("--label", "org.faber.kind=service"),
            ("--health-cmd", "wget -qO- http://127.0.0.1:7070/health"),
            ("-e", "ROOT_ACCESS_KEY"),
            ("-e", "ROOT_SECRET_KEY"),
        ] {
            assert!(
                p.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} missing from {a:?}"
            );
        }
        // A table with a `health_cmd` checks at once and early (A3).
        assert!(a.iter().any(|x| x == "--health-start-interval=1s"));
        assert!(a.iter().any(|x| x == "--health-start-period=60s"));
        // A bare table has no health flag at all: the pre-amendment argv.
        let plain = per_job_run_args(&spec("redis"), &j).unwrap();
        assert!(
            !plain.iter().any(|x| x.starts_with("--health")),
            "{plain:?}"
        );
        // The image, then the arguments as its command, in order, last.
        let img = a
            .iter()
            .position(|x| x == "versity/versitygw:v1.8.0")
            .unwrap();
        assert_eq!(a[img + 1..], ["--port", ":7070", "posix", "/data"]);
        // Flags all precede the image.
        assert!(a[..img].iter().all(|x| x != "--port"));
        // The values are for the CLI child's environment, never the argv.
        assert!(!a.iter().any(|x| x.contains('=') && x.starts_with("ROOT_")));
    }

    /// ADR-0062: `{secret}` becomes one value per job, the same in the
    /// container's `env` and the runner's `job_env`, and never reaches the argv.
    #[test]
    fn a_generated_secret_fills_both_sides_and_stays_off_the_argv() {
        assert!(!s3().per_job.unwrap().wants_secret());
        let mut s = s3();
        let run = s.per_job.as_mut().unwrap();
        run.env
            .insert("ROOT_SECRET_KEY".into(), SERVICE_SECRET_PLACEHOLDER.into());
        run.job_env.insert(
            "AWS_SECRET_ACCESS_KEY".into(),
            SERVICE_SECRET_PLACEHOLDER.into(),
        );
        run.job_env
            .insert("S3_URL".into(), "http://faber:{secret}@s3:7070".into());
        assert!(run.wants_secret());

        let secret = random_password().unwrap();
        assert_ne!(secret, random_password().unwrap());
        let (service_env, job_env) = run.env_for_job(&secret);
        let get = |vars: &[(String, String)], k: &str| {
            vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        assert_eq!(get(&service_env, "ROOT_SECRET_KEY"), Some(secret.clone()));
        assert_eq!(
            get(&service_env, "ROOT_ACCESS_KEY").as_deref(),
            Some("ci-access-key")
        );
        assert_eq!(get(&job_env, "AWS_SECRET_ACCESS_KEY"), Some(secret.clone()));
        assert_eq!(
            get(&job_env, "S3_URL"),
            Some(format!("http://faber:{secret}@s3:7070"))
        );
        assert_eq!(
            get(&job_env, "S3_ENDPOINT").as_deref(),
            Some("http://s3:7070")
        );

        let a = per_job_run_args(&s, &job()).unwrap();
        assert!(pairs(&a).contains(&("-e".into(), "ROOT_SECRET_KEY".into())));
        assert!(
            !a.iter()
                .any(|x| x.contains(SERVICE_SECRET_PLACEHOLDER) || x.contains(&secret)),
            "{a:?}"
        );
    }

    #[test]
    fn a_container_name_is_faber_svc_name_id_for_a_service_name_only() {
        assert_eq!(
            per_job_container("s3", ID).as_deref(),
            Some("faber-svc-s3-1001-1756000000000")
        );
        for bad in ["postgres", "S3", "s-3", "", "3s", "a;b"] {
            assert_eq!(per_job_container(bad, ID), None, "{bad:?}");
        }
        assert_eq!(per_job_container("s3", "not-a-claim"), None);
    }

    #[test]
    fn a_service_name_is_a_dns_label() {
        for ok in ["s3", "redis", "a", "mailpit2", &"a".repeat(32)] {
            assert!(service_name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "S3",
            "3s",
            "s-3",
            "s_3",
            "s.3",
            "é",
            &"a".repeat(33),
            " s3",
        ] {
            assert!(!service_name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn health_is_read_from_the_daemon_against_a_fixed_set() {
        assert_eq!(parse_health(""), Some(Health::Unchecked));
        assert_eq!(parse_health("\n"), Some(Health::Unchecked));
        assert_eq!(parse_health("starting\n"), Some(Health::Starting));
        assert_eq!(parse_health("healthy\n"), Some(Health::Healthy));
        assert_eq!(parse_health("unhealthy"), Some(Health::Unhealthy));
        for odd in [
            "Healthy",
            "ok",
            "healthy\nforged",
            "unhealthy: boom",
            "none",
        ] {
            assert_eq!(parse_health(odd), None, "{odd:?}");
        }
        let a = health_inspect_args("faber-svc-s3-1-2");
        assert_eq!(a[1], "-f");
        assert!(a[2].contains(".State.Health.Status") && !a[2].contains("Log"));
        assert_eq!(a.last().unwrap(), "faber-svc-s3-1-2");
    }

    #[test]
    fn the_wait_decides_each_reading() {
        let v = |h, late| health_verdict("s3", h, late);
        assert_eq!(v(Health::Healthy, false), Verdict::Ready);
        assert_eq!(v(Health::Unchecked, true), Verdict::Ready);
        assert_eq!(v(Health::Starting, false), Verdict::Continue);
        let Verdict::Fail(m) = v(Health::Unhealthy, false) else {
            panic!()
        };
        assert!(m.contains("s3") && m.contains("unhealthy"), "{m}");
        let Verdict::Fail(m) = v(Health::Starting, true) else {
            panic!()
        };
        assert!(
            m.contains("s3") && m.contains("last status: starting"),
            "{m}"
        );
        // Healthy at the deadline is still healthy.
        assert_eq!(v(Health::Healthy, true), Verdict::Ready);
    }

    #[tokio::test]
    async fn nothing_to_wait_for_is_ready_at_once() {
        wait_healthy(&[]).await.unwrap();
    }

    #[test]
    fn a_policy_knows_which_tools_are_services() {
        let mut p = ServicesPolicy::default();
        for t in ["postgres", "redis", "mailpit"] {
            assert!(p.is_service_tool(t), "{t}");
        }
        assert!(!p.is_service_tool("s3"));
        p.per_job
            .insert("s3".into(), PerJobPolicy::builtin("s3", "x:1", 1));
        assert!(p.is_service_tool("s3"));
        assert!(!p.is_service_tool("playwright"));
    }

    #[test]
    fn the_debug_of_a_policy_shows_env_names_not_values() {
        let shown = format!("{:?}", s3().per_job.unwrap());
        assert!(shown.contains("ROOT_SECRET_KEY"), "{shown}");
        assert!(!shown.contains("ci-secret-value"), "{shown}");
    }

    #[test]
    fn the_server_joins_a_job_under_the_postgres_alias() {
        assert_eq!(
            connect_args("faber-net-1-2", "faber-svc-postgres-16"),
            [
                "network",
                "connect",
                "--alias",
                "postgres",
                "faber-net-1-2",
                "faber-svc-postgres-16"
            ]
        );
        assert_eq!(
            disconnect_args("faber-net-1-2", "faber-svc-postgres-16"),
            [
                "network",
                "disconnect",
                "-f",
                "faber-net-1-2",
                "faber-svc-postgres-16"
            ]
        );
    }

    #[test]
    fn psql_reads_its_script_from_stdin_and_stops_on_error() {
        let a = psql_args("faber-svc-postgres-16");
        assert_eq!(&a[..3], ["exec", "-i", "faber-svc-postgres-16"]);
        assert!(a.contains(&"ON_ERROR_STOP=1".to_string()));
        assert!(a.contains(&"-X".to_string()));
        // Readiness is asked over TCP, past the entrypoint's socket-only server.
        assert!(ready_args("s").windows(2).any(|w| w == ["-h", "127.0.0.1"]));
    }

    #[test]
    fn the_create_script_is_section_4() {
        let sql = String::from_utf8(create_sql(&role(), "00ff")).unwrap();
        let r = "faber_r_1001_1756000000000";
        assert!(sql.contains(&format!("CREATE ROLE {r} LOGIN CREATEDB PASSWORD '00ff';")));
        assert!(sql.contains(&format!(
            "CREATE DATABASE {r} TEMPLATE faber_tpl OWNER {r};"
        )));
        assert!(sql.contains(&format!("REVOKE CONNECT ON DATABASE {r} FROM PUBLIC;")));
        assert!(sql.starts_with("\\o /dev/null\n"));
    }

    #[test]
    fn teardown_is_generated_server_side_and_idempotent() {
        let sql = String::from_utf8(teardown_sql(&role())).unwrap();
        let r = "faber_r_1001_1756000000000";
        assert!(sql.contains("format('DROP DATABASE %I WITH (FORCE)', datname)"));
        assert!(sql.contains(&format!("rolname = '{r}') \\gexec\n")));
        assert!(sql.contains(&format!("DROP ROLE IF EXISTS {r};")));
        // Every statement but the last is generated or a no-op for a missing role.
        assert!(!sql.contains("DROP OWNED BY faber"));
    }

    #[test]
    fn orphan_cleanup_spares_exactly_the_live_roles() {
        let live = [role(), Role::for_claim("1-2").unwrap()];
        let sql = String::from_utf8(orphan_sql(&live)).unwrap();
        assert!(sql.contains(
            "rolname <> ALL (ARRAY['faber_r_1001_1756000000000', 'faber_r_1_2']::name[])"
        ));
        assert!(sql.contains("LIKE 'faber\\_r\\_%'"));
        assert_eq!(sql.matches("\\gexec").count(), 3);
        let none = String::from_utf8(orphan_sql(&[])).unwrap();
        assert!(none.contains("ARRAY[]::name[]"));
    }

    #[test]
    fn the_template_carries_the_extensions_and_admits_no_one() {
        let sql = String::from_utf8(
            template_sql(&["postgis".to_string(), "pg_trgm".to_string()]).unwrap(),
        )
        .unwrap();
        assert!(
            sql.contains("CREATE DATABASE faber_tpl;\n\\c faber_tpl\nSET search_path = public;\n")
        );
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS \"postgis\";"));
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS \"pg_trgm\";"));
        assert!(sql.contains("REVOKE CONNECT ON DATABASE faber_tpl FROM PUBLIC;"));
        assert!(sql.contains("ALTER DATABASE faber_tpl IS_TEMPLATE true;"));
        assert_eq!(quote_ident("we\"ird").as_deref(), Some("\"we\"\"ird\""));
        assert!(template_sql(&["x\n\\! sh".to_string()]).is_err());
        assert!(template_sql(&["a\\b".to_string()]).is_err());
        assert!(template_sql(&[String::new()]).is_err());
    }

    #[test]
    fn every_superuser_script_resolves_only_from_pg_catalog() {
        let live = [role()];
        let scripts = [
            template_sql(&["pg_trgm".to_string()]).unwrap(),
            create_sql(&role(), "00ff"),
            teardown_sql(&role()),
            orphan_sql(&live),
        ];
        for script in scripts {
            let sql = String::from_utf8(script).unwrap();
            assert!(
                sql.starts_with("\\o /dev/null\nSET search_path = pg_catalog, pg_temp;\n"),
                "{sql}"
            );
            // Every connection a `\c` opens sets its path before anything else.
            for (i, line) in sql.lines().enumerate() {
                if let Some(db) = line.strip_prefix("\\c ") {
                    let next = sql.lines().nth(i + 1).unwrap_or_default();
                    match db {
                        TEMPLATE_DB => assert_eq!(next, "SET search_path = public;"),
                        _ => assert_eq!(next, "SET search_path = pg_catalog, pg_temp;"),
                    }
                }
            }
        }
        let tpl = String::from_utf8(template_sql(&[]).unwrap()).unwrap();
        let revoke = "REVOKE CREATE ON SCHEMA public FROM PUBLIC;\n\
                      REVOKE CONNECT ON DATABASE template1 FROM PUBLIC;\n\
                      CREATE DATABASE faber_tpl;";
        assert!(tpl.contains(revoke), "{tpl}");
    }

    #[test]
    fn the_env_is_section_4() {
        let env = postgres_env(&role(), "abc");
        let r = "faber_r_1001_1756000000000";
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(
            get("DATABASE_URL"),
            Some(format!("postgres://{r}:abc@postgres:5432/{r}").as_str())
        );
        assert_eq!(get("PGHOST"), Some("postgres"));
        assert_eq!(get("PGPORT"), Some("5432"));
        assert_eq!(get("PGUSER"), Some(r));
        assert_eq!(get("PGPASSWORD"), Some("abc"));
        assert_eq!(get("PGDATABASE"), Some(r));
        assert_eq!(
            builtin_job_env("redis"),
            [("REDIS_URL", "redis://redis:6379/0")]
        );
        let mail = builtin_job_env("mailpit");
        assert_eq!(mail[0].1, "smtp://mailpit:1025");
        assert_eq!(mail[1].1, "http://mailpit:8025");
        assert!(builtin_job_env("s3").is_empty());
    }

    #[test]
    fn service_and_server_names_parse_only_in_faber_s_forms() {
        assert_eq!(
            parse_service_name("postgres-16"),
            Some((POSTGRES, Some("16")))
        );
        assert_eq!(parse_service_name("redis"), Some(("redis", None)));
        assert_eq!(parse_service_name("mailpit"), Some(("mailpit", None)));
        // A generic name is accepted whether or not the config declares it (A4).
        assert_eq!(parse_service_name("s3"), Some(("s3", None)));
        assert_eq!(parse_service_name("postgres"), None);
        assert_eq!(parse_service_name("postgres-16;x"), None);
        assert_eq!(parse_service_name("redis-7"), None);
        assert_eq!(parse_service_name("mysql-8"), None);
        assert_eq!(parse_service_name("S3"), None);
        assert_eq!(parse_service_name("s3;x"), None);
        assert!(service_name_in_faber_form("s3"));
        assert!(!service_name_in_faber_form("s3-1"));
        assert_eq!(version_of_server("faber-svc-postgres-17"), Some("17"));
        assert_eq!(version_of_server("faber-svc-postgres-17x"), None);
        assert_eq!(version_of_server("other"), None);
        assert_eq!(version_of_volume("faber-svc-postgres-16-data"), Some("16"));
        assert_eq!(version_of_volume("faber-svc-postgres-16"), None);
    }

    #[test]
    fn a_server_state_is_read_from_the_daemon_strictly() {
        let st = parse_server_state("true 2147483648 faber-net-1-2 faber-svc\n").unwrap();
        assert!(st.running);
        assert_eq!(st.memory_mib, 2048);
        assert!(st.networks.contains("faber-net-1-2"));
        assert!(st.networks.contains(SERVER_NETWORK));
        assert!(parse_server_state("maybe 1").is_none());
        assert!(parse_server_state("true x").is_none());
        assert!(!parse_server_state("false 0").unwrap().running);
    }

    #[test]
    fn the_idle_ticker_scales_with_idle_stop() {
        assert_eq!(tick_period(None), TICK_MAX);
        assert_eq!(tick_period(Some(Duration::from_secs(900))), TICK_MAX);
        assert_eq!(
            tick_period(Some(Duration::from_secs(8))),
            Duration::from_secs(2)
        );
        assert_eq!(
            tick_period(Some(Duration::from_millis(10))),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn an_empty_plan_attaches_nothing_even_unconfigured() {
        let s = Services::default();
        assert!(!s.enabled());
        let a = s.attach(&ServicePlan::default(), &job()).await.unwrap();
        assert!(a.env().is_empty());
        let plan = ServicePlan {
            specs: vec![spec("redis")],
        };
        assert!(s.attach(&plan, &job()).await.is_err());
    }
}
