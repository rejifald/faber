//! faber — a local just-in-time GitHub Actions runner provisioner.
//!
//! faber watches the queued-job state of your own repos and, for jobs that are
//! genuinely yours (your push / your branch / your same-repo PR), mints a
//! single-use ephemeral self-hosted runner on this machine. Untrusted code never
//! reaches the box: the primary control is `runs-on` trust-routing at the
//! workflow level (fork PRs and Dependabot are routed to GitHub-hosted runners);
//! the [`gate`] module is the independent defense-in-depth check.
//!
//! Charter: run as much of your *own* CI as possible on your own hardware —
//! speed, warm caches, control — not cost.

pub mod attention;
pub mod audit;
pub mod auth;
pub mod authorship;
pub mod awake;
pub mod backend;
pub mod cache;
pub mod config;
pub mod config_cli;
pub mod control;
pub mod cotenant;
pub mod cpu;
pub mod credential;
pub mod dashboard;
pub mod dind;
pub mod docker;
pub mod drain;
pub mod drift;
pub mod footprint;
pub mod freeze;
pub mod gate;
pub mod git_mirror;
pub mod github;
pub mod host;
pub mod image;
pub mod instance;
pub mod job_start;
pub mod jobs_view;
pub mod limiter;
pub mod lock;
pub mod logline;
pub mod machine;
pub mod mirror;
pub mod mirror_beside_dind;
pub mod needs;
pub mod netpool;
pub mod out;
pub mod parking;
pub mod platform;
pub mod privacy;
pub mod profile_image;
pub mod provenance;
pub mod reconcile;
pub mod reserve;
pub mod retry;
pub mod runner;
pub mod runner_retirement;
pub mod self_check;
pub mod services;
pub mod settings;
pub mod setup;
pub mod standby;
pub mod status;
pub mod sweep;
pub mod term;
pub mod ui_bundle;
pub mod untrusted;
pub mod update;
pub mod upgrade_cli;
pub mod vm;
pub mod wall;
pub mod waste;
pub mod web;
