//! What a job says it needs from its runner (ADR-0010).
//!
//! Everything before this module tried to make *every runner acceptable to every
//! job* — a superset image (ADR-0008), one image generation per repo, a hold when
//! two are incomparable (ADR-0009). That is the only move available while the job
//! says nothing: GitHub matches a job to a runner by labels, and faber does not
//! author `runs-on`.
//!
//! A job that states its needs inverts it. `runs-on` carries `faber.<tool>` or
//! `faber.<tool>-<version>`, faber registers the runner with those same labels,
//! and **GitHub's own matcher performs the binding faber cannot** — including
//! across instances, since which host minted a runner stops mattering once the
//! label decides who may have it.
//!
//! Two rules keep this from becoming a way for a pull request to order builds on
//! the operator's host, and they close different holes:
//!
//! 1. **A runner may only carry a need label its image can honour.** See
//!    [`evaluate`], which also records why this replaced ADR-0010 §3's
//!    validate-against-the-lockfile rule: `runs-on` is PR-reachable, but image
//!    resolution never reads the label, so a label cannot commission a build. What
//!    it can do is make the pool lie about itself.
//! 2. **The operator's allowlist bounds what may be built.** A pull request may
//!    legitimately edit its own lockfile, so the ceiling on what CI can cause to be
//!    built on the host lives in faber's own config — see `[needs.<tool>]` in
//!    [`crate::config`], applied on the resolution path.
//!
//! Failure here is **closed**, unlike ADR-0009's pool rule where every unknown
//! fails open. The asymmetry is deliberate: an under-minted pool starves a job,
//! which is bad, while an unvalidated label causes an unreviewed build on the
//! host, which is worse. A refused job does not run — but it could not have run
//! anyway, since faber declines to mint the label it was asking for.

use std::collections::BTreeSet;

use crate::services::{ServicePlan, ServiceSpec, ServicesPolicy, POSTGRES};

/// The prefix marking a label as a faber need rather than one of the operator's
/// own routing labels. Namespaced so the two can never collide, and stable: once
/// a repo's `runs-on` names it, renaming it breaks that repo's CI.
pub const PREFIX: &str = "faber.";

/// One `faber.<tool>` / `faber.<tool>-<version>` label, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// The tool, e.g. `playwright`.
    pub tool: String,
    /// The version the job pinned, if it named one. `None` is the capability form
    /// (`faber.playwright`) — "this tool, at whatever my lockfile pins" — which
    /// partitions the pool by tool but not by version.
    pub version: Option<String>,
    /// The label verbatim, so the runner can be registered with exactly what the
    /// job asked for rather than a re-rendering of it.
    pub label: String,
}

/// How a job's declared needs came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No `faber.*` label: ADR-0010 does not apply, and the provisioning path is
    /// byte-for-byte what it was. Most jobs, and every repo that adopts nothing.
    Unlabelled,
    /// Validated. `labels` are the need labels the minted runner must carry *in
    /// addition to* its profile's, so that `job ⊆ runner` holds and GitHub can
    /// hand this job to it.
    Satisfied { labels: Vec<String> },
    /// Refused, with a reason fit for an operator to read in the audit log. The
    /// job is not provisioned for; faber does not mint an unlabelled runner as a
    /// fallback, because the job asked for a label and could not land on one.
    Refused { reason: String },
}

/// Every `faber.*` need a job's `runs-on` declares, in the order given.
///
/// A malformed label (`faber.`, `faber.playwright-`) is **not** silently dropped:
/// it is returned with an empty tool so [`evaluate`] can refuse it. Dropping it
/// would let a typo read as "no needs at all" and quietly provision a runner the
/// job can never land on.
pub fn parse(job_labels: &[String]) -> Vec<Need> {
    job_labels
        .iter()
        .filter_map(|l| l.strip_prefix(PREFIX).map(|rest| (l, rest)))
        .map(|(label, rest)| {
            // Split at the first `-` that begins a version, so a hyphenated tool
            // name (`some-tool-1.2.3`) keeps its hyphens.
            let split = rest.char_indices().find(|(i, c)| {
                *c == '-' && rest[i + 1..].starts_with(|n: char| n.is_ascii_digit())
            });
            match split {
                Some((i, _)) => Need {
                    tool: rest[..i].to_string(),
                    version: Some(rest[i + 1..].to_string()),
                    label: label.clone(),
                },
                None => Need {
                    tool: rest.to_string(),
                    version: None,
                    label: label.clone(),
                },
            }
        })
        .collect()
}

/// Whether a parsed tool name is plausible: non-empty, ASCII alphanumeric with
/// internal hyphens only.
///
/// The trailing-hyphen case is the one that matters and is why this is not just an
/// `is_empty` check — `faber.playwright-` parses as a *capability* need for a tool
/// called `playwright-`, which would provision a runner labelled for a tool that
/// does not exist and leave the job unable to land on anything. A typo has to be
/// refused out loud, not resolved into something plausible.
fn is_tool_name(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with('-')
        && !t.ends_with('-')
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Validate a job's declared needs against what its resolved image actually
/// serves.
///
/// `serves` is [`crate::reconcile`]'s `PoolImage::serves` **after** the pool
/// decision has had its say, so it describes the image the container will really
/// start from — including a substitution onto a dominating image (ADR-0009).
///
/// Judging against the served set rather than against the raw lockfile pin is a
/// correction to ADR-0010 §3 found while implementing it. §3 was written to stop a
/// pull request naming `faber.playwright-99.0.0` and causing
/// `npx playwright@99.0.0 install` to run on the host daemon. That attack does not
/// exist: image resolution never reads the label, only the lockfile, so a label
/// cannot commission a build. What a label *can* do is register a runner under a
/// claim that is false about its own image — a pool where the label lies, which is
/// exactly the incoherence ADR-0009 fixed. So the rule that matters is the one
/// enforced here: **a runner may only carry a need label its image can honour.**
/// Bounding what may be built stays with the operator's `[needs.<tool>]`
/// allowlist, on the resolution path where the lockfile is read.
///
/// Empty `serves` means *unknown*, and unknown fails **closed** here — the
/// opposite of ADR-0009's pool rule, deliberately. An under-constrained pool
/// starves a job; a runner labelled with a promise nothing verified is handed work
/// it cannot do.
///
/// Service labels (`faber.postgres-16`, `faber.s3`, ADR-0046) are skipped
/// unconditionally: [`services`] decides them against `[services.*]`, and judged
/// here against `serves` they would all be refused. A label is a service label
/// when its tool is `postgres`, or `redis`, `mailpit` or a name in `policy` in
/// the bare form ([`ServicesPolicy::is_service_label`]); any other, a versioned
/// `faber.<service>-<v>` of a per-job name included, is a tool, as before.
pub fn evaluate(
    job_labels: &[String],
    serves: &BTreeSet<String>,
    policy: &ServicesPolicy,
) -> Outcome {
    let needs: Vec<Need> = parse(job_labels)
        .into_iter()
        .filter(|n| !policy.is_service_label(&n.tool.to_ascii_lowercase(), n.version.as_deref()))
        .collect();
    if needs.is_empty() {
        return Outcome::Unlabelled;
    }
    let mut labels = Vec::with_capacity(needs.len());
    for need in &needs {
        if !is_tool_name(&need.tool) || need.version.as_deref() == Some("") {
            return Outcome::Refused {
                reason: format!("malformed need label `{}`", need.label),
            };
        }
        if serves.is_empty() {
            return Outcome::Refused {
                reason: format!(
                    "`{}` cannot be honoured: faber knows nothing about what this \
                     job's image serves (no opt-in label, unreadable lockfile, or \
                     [runner_image] off)",
                    need.label
                ),
            };
        }
        if let Some(v) = &need.version {
            if !serves.contains(v) {
                return Outcome::Refused {
                    reason: format!(
                        "`{}` asks for {v}, which its image does not serve (serves {})",
                        need.label,
                        serves.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                };
            }
        }
        labels.push(need.label.clone());
    }
    Outcome::Satisfied { labels }
}

/// How a job's declared services came out (ADR-0046 §2).
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceOutcome {
    /// No service label: ADR-0046 does not apply.
    Unlabelled,
    /// Every service label is one the operator's `[services.*]` provides.
    Provide(crate::services::ServicePlan),
    /// A service label the operator does not provide, or a version outside its
    /// `versions`. The job gets no runner, as with [`Outcome::Refused`].
    Refuse { label: String, reason: String },
}

/// Resolve the service labels among `job_labels` (`faber.postgres-16`,
/// `faber.redis`) against the operator's policy. Labels that name no service are
/// left to [`evaluate`].
///
/// The rules (ADR-0046 §2, §10), each failing closed:
///
/// - **No table, no service.** A service label whose `[services.<name>]` is
///   absent is refused.
/// - **The label form follows the table.** A table with `versions` accepts only
///   `faber.<name>-<major>` with a listed major; a table without accepts only the
///   bare `faber.<name>`. Any other form is refused with the form expected.
/// - **One of each.** Two Postgres majors would both answer as `postgres` on the
///   job's network, so a job naming two is refused. The same label twice is one.
///
/// Labels are compared case-insensitively, as GitHub matches `runs-on`, and the
/// spec keeps the job's spelling verbatim, which the runner is registered with.
/// `Refuse` names the first label refused.
pub fn services(job_labels: &[String], policy: &ServicesPolicy) -> ServiceOutcome {
    let mut specs: Vec<ServiceSpec> = Vec::new();
    for label in job_labels {
        let lower = label.to_ascii_lowercase();
        let Some(need) = parse(std::slice::from_ref(&lower)).pop() else {
            continue;
        };
        if !policy.is_service_label(&need.tool, need.version.as_deref()) {
            continue;
        }
        let name = need.tool.as_str();
        let refuse = |reason: String| ServiceOutcome::Refuse {
            label: label.clone(),
            reason,
        };
        let spec = if name == POSTGRES {
            let Some(p) = &policy.postgres else {
                return refuse(not_provided(name));
            };
            let listed = p.versions.join(", ");
            let Some(v) = need.version.as_deref() else {
                return refuse(format!(
                    "postgres is declared with its major version: expected \
                     faber.postgres-<major>, with <major> one of {listed}"
                ));
            };
            if !p.versions.iter().any(|x| x == v) {
                return refuse(format!(
                    "postgres {v} is not provided here: expected faber.postgres-<major>, \
                     with <major> one of {listed}"
                ));
            }
            ServiceSpec {
                service: POSTGRES.to_string(),
                version: Some(v.to_string()),
                image: p
                    .image
                    .replace(crate::config::SERVICE_VERSION_PLACEHOLDER, v),
                memory_mib: p.memory_mib,
                label: label.clone(),
                per_job: None,
            }
        } else {
            let Some(p) = policy.per_job.get(name) else {
                return refuse(not_provided(name));
            };
            ServiceSpec {
                service: name.to_string(),
                version: None,
                image: p.image.clone(),
                memory_mib: p.memory_mib,
                label: label.clone(),
                per_job: Some(p.clone()),
            }
        };
        match specs.iter().find(|s| s.service == spec.service) {
            None => specs.push(spec),
            Some(s) if s.version == spec.version => {}
            Some(s) => {
                return refuse(format!(
                    "a job gets one {name}, and this one also declares `{}`",
                    s.label
                ))
            }
        }
    }
    if specs.is_empty() {
        ServiceOutcome::Unlabelled
    } else {
        ServiceOutcome::Provide(ServicePlan { specs })
    }
}

/// The reason a service with no `[services.<name>]` table is refused.
fn not_provided(name: &str) -> String {
    format!("this host does not provide {name}: its config has no [services.{name}] table")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::evaluate`] against a host with no `[services.*]` table, which is
    /// what the tool-need tests are about. The built-in service names are
    /// skipped whatever the policy; the generic ones have their own tests.
    fn evaluate(job_labels: &[String], serves: &BTreeSet<String>) -> Outcome {
        super::evaluate(job_labels, serves, &ServicesPolicy::default())
    }

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn serves(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_job_with_no_faber_label_is_untouched() {
        assert_eq!(
            evaluate(&l(&["faber", "docker"]), &serves(&["1.61.1"])),
            Outcome::Unlabelled,
            "the adopt-nothing path must stay exactly what it was"
        );
    }

    #[test]
    fn parse_splits_the_version_off_a_hyphenated_tool() {
        let got = parse(&l(&["faber", "faber.some-tool-1.2.3", "faber.playwright"]));
        assert_eq!(got.len(), 2, "non-faber labels are not needs");
        assert_eq!(got[0].tool, "some-tool");
        assert_eq!(got[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(got[1].tool, "playwright");
        assert_eq!(got[1].version, None, "the capability form names no version");
    }

    #[test]
    fn a_label_its_image_cannot_honour_is_refused() {
        // The property that matters: a runner must never be registered under a
        // claim that is false about its own image — a pool where the label lies is
        // the incoherence ADR-0009 exists to prevent.
        let out = evaluate(
            &l(&["faber", "docker", "faber.playwright-99.0.0"]),
            &serves(&["1.61.1", "1.62.1"]),
        );
        match out {
            Outcome::Refused { reason } => assert!(
                reason.contains("99.0.0") && reason.contains("1.61.1"),
                "{reason}"
            ),
            other => panic!("an unservable label must not be registered: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_served_set_fails_closed() {
        // The opposite of ADR-0009's pool rule, deliberately: an under-constrained
        // pool starves a job, while a runner labelled with an unverified promise is
        // handed work it cannot do.
        let out = evaluate(&l(&["faber", "faber.playwright-1.61.1"]), &BTreeSet::new());
        assert!(matches!(out, Outcome::Refused { .. }));
    }

    #[test]
    fn a_variant_serving_both_versions_honours_either_label() {
        // The superset property ADR-0008 bought with PLAYWRIGHT_SKIP_BROWSER_GC=1,
        // load-bearing for a third reason now: one variant can back two pools.
        for want in ["faber.playwright-1.61.1", "faber.playwright-1.62.1"] {
            assert_eq!(
                evaluate(&l(&["faber", want]), &serves(&["1.61.1", "1.62.1"])),
                Outcome::Satisfied {
                    labels: vec![want.to_string()]
                }
            );
        }
    }

    #[test]
    fn the_capability_form_needs_only_a_known_image() {
        assert_eq!(
            evaluate(&l(&["faber", "faber.playwright"]), &serves(&["1.61.1"])),
            Outcome::Satisfied {
                labels: vec!["faber.playwright".to_string()]
            }
        );
    }

    #[test]
    fn a_malformed_need_is_refused_not_ignored() {
        for bad in [vec!["faber", "faber."], vec!["faber", "faber.playwright-"]] {
            let out = evaluate(&l(&bad), &serves(&["1.61.1"]));
            assert!(
                matches!(out, Outcome::Refused { .. }),
                "a typo must not read as `no needs`: {bad:?}"
            );
        }
    }

    #[test]
    fn satisfied_returns_only_the_need_labels() {
        // The caller adds these to the profile's own labels; a non-faber extra the
        // job asked for is deliberately not echoed back, keeping the label
        // namespace faber registers under faber's control.
        let out = evaluate(
            &l(&["faber", "docker", "faber.playwright-1.61.1", "custom"]),
            &serves(&["1.61.1"]),
        );
        assert_eq!(
            out,
            Outcome::Satisfied {
                labels: vec!["faber.playwright-1.61.1".to_string()]
            }
        );
    }

    // --- declared services (ADR-0046) ---------------------------------------

    use crate::services::{PerJobPolicy, PostgresPolicy};
    use std::time::Duration;

    /// Every table, as ADR-0046 §2's example writes them.
    fn full() -> ServicesPolicy {
        ServicesPolicy {
            postgres: Some(PostgresPolicy {
                versions: vec!["16".into(), "17".into()],
                image: "postgres:{version}-alpine".into(),
                extensions: vec!["postgis".into()],
                memory_mib: 2048,
                idle_stop: Duration::from_secs(900),
            }),
            per_job: [
                (
                    "redis",
                    per_job(
                        "redis:7-alpine",
                        256,
                        &[("REDIS_URL", "redis://redis:6379/0")],
                    ),
                ),
                (
                    "mailpit",
                    per_job(
                        "axllent/mailpit:v1.21",
                        128,
                        &[
                            ("MAIL_SMTP_URL", "smtp://mailpit:1025"),
                            ("MAIL_API_URL", "http://mailpit:8025"),
                        ],
                    ),
                ),
                (
                    "s3",
                    per_job(
                        "versity/versitygw:v1.8.0",
                        64,
                        &[("S3_ENDPOINT", "http://s3:7070")],
                    ),
                ),
            ]
            .into_iter()
            .map(|(n, p)| (n.to_string(), p))
            .collect(),
        }
    }

    fn per_job(image: &str, memory_mib: u64, job_env: &[(&str, &str)]) -> PerJobPolicy {
        PerJobPolicy {
            image: image.into(),
            memory_mib,
            args: vec![],
            env: Default::default(),
            job_env: job_env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            health_cmd: None,
        }
    }

    /// The outcome's shape: `Some(names)` for `Provide`, `None` for `Refuse`, and
    /// `Some([])` for `Unlabelled`.
    fn shape(out: &ServiceOutcome) -> Option<Vec<String>> {
        match out {
            ServiceOutcome::Unlabelled => Some(Vec::new()),
            ServiceOutcome::Provide(plan) => {
                assert!(!plan.is_empty(), "Provide carries at least one service");
                Some(plan.names())
            }
            ServiceOutcome::Refuse { .. } => None,
        }
    }

    #[test]
    fn service_decision_table() {
        let none = ServicesPolicy::default();
        let no_pg = ServicesPolicy {
            postgres: None,
            ..full()
        };
        let no_redis = {
            let mut p = full();
            p.per_job.remove("redis");
            p
        };
        let all = full();
        // (labels, policy, Some(service names) or None for refused)
        type Case<'a> = (&'a [&'a str], &'a ServicesPolicy, Option<&'a [&'a str]>);
        let cases: &[Case] = &[
            // No service label: untouched, whatever the policy.
            (&["faber", "docker"], &all, Some(&[])),
            (&["faber", "docker"], &none, Some(&[])),
            (&[], &none, Some(&[])),
            // Other needs are `evaluate`'s, never judged here.
            (&["faber", "faber.playwright-1.61.1"], &all, Some(&[])),
            (&["faber", "faber.node"], &none, Some(&[])),
            // A label that merely contains a service name is not one.
            (&["faber", "postgres", "redis"], &none, Some(&[])),
            (&["faber", "faber.postgresql-16"], &all, Some(&[])),
            // Versioned: only a listed major.
            (
                &["faber", "faber.postgres-16"],
                &all,
                Some(&["postgres-16"]),
            ),
            (
                &["faber", "faber.postgres-17"],
                &all,
                Some(&["postgres-17"]),
            ),
            (&["faber", "faber.postgres-15"], &all, None),
            (&["faber", "faber.postgres-16.2"], &all, None),
            (&["faber", "faber.postgres-016"], &all, None),
            // The bare form against a versioned table.
            (&["faber", "faber.postgres"], &all, None),
            // Unversioned: only the bare label.
            (&["faber", "faber.redis"], &all, Some(&["redis"])),
            (&["faber", "faber.mailpit"], &all, Some(&["mailpit"])),
            // A per-job service has only its bare label: a versioned one is a
            // `[needs]` label, left to `evaluate`.
            (&["faber", "faber.redis-7"], &all, Some(&[])),
            (&["faber", "faber.mailpit-1"], &all, Some(&[])),
            // A configured name is a service whatever it is called (A1); one
            // that is not configured is a tool, which `evaluate` judges.
            (&["faber", "faber.s3"], &all, Some(&["s3"])),
            (&["faber", "faber.S3"], &all, Some(&["s3"])),
            (&["faber", "faber.s3-1"], &all, Some(&[])),
            (&["faber", "faber.s3", "faber.s3-1"], &all, Some(&["s3"])),
            (
                &["faber", "faber.s3", "faber.redis"],
                &all,
                Some(&["s3", "redis"]),
            ),
            (&["faber", "faber.s3"], &none, Some(&[])),
            (&["faber", "faber.s3"], &no_redis, Some(&["s3"])),
            // No table, no service.
            (&["faber", "faber.postgres-16"], &none, None),
            (&["faber", "faber.postgres-16"], &no_pg, None),
            (&["faber", "faber.redis"], &no_redis, None),
            (&["faber", "faber.redis"], &none, None),
            (&["faber", "faber.mailpit"], &none, None),
            // GitHub matches labels case-insensitively.
            (
                &["faber", "faber.Postgres-16"],
                &all,
                Some(&["postgres-16"]),
            ),
            (&["faber", "FABER.REDIS"], &all, Some(&["redis"])),
            // Several, in the order the job gave them.
            (
                &[
                    "faber",
                    "docker",
                    "faber.redis",
                    "faber.postgres-16",
                    "faber.mailpit",
                ],
                &all,
                Some(&["redis", "postgres-16", "mailpit"]),
            ),
            // One refused label refuses the job, whatever else it asks for.
            (
                &["faber", "faber.postgres-16", "faber.redis"],
                &no_redis,
                None,
            ),
            // The same service twice is one; two Postgres majors are refused.
            (
                &["faber", "faber.redis", "faber.REDIS"],
                &all,
                Some(&["redis"]),
            ),
            (
                &["faber", "faber.postgres-16", "faber.postgres-16"],
                &all,
                Some(&["postgres-16"]),
            ),
            (
                &["faber", "faber.postgres-16", "faber.postgres-17"],
                &all,
                None,
            ),
        ];
        for (labels, policy, want) in cases {
            let out = services(&l(labels), policy);
            let want = want.map(|w| w.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            assert_eq!(shape(&out), want, "{labels:?} → {out:?}");
        }
    }

    #[test]
    fn a_spec_carries_the_operators_image_and_the_jobs_label() {
        let out = services(
            &l(&["faber", "faber.Postgres-17", "faber.redis", "faber.mailpit"]),
            &full(),
        );
        let ServiceOutcome::Provide(plan) = out else {
            panic!("{out:?}");
        };
        let pg = &plan.specs[0];
        assert_eq!(pg.service, "postgres");
        assert!(pg.per_job.is_none());
        assert_eq!(pg.version.as_deref(), Some("17"));
        assert_eq!(pg.image, "postgres:17-alpine", "{{version}} substituted");
        assert_eq!(pg.memory_mib, 2048);
        assert_eq!(
            pg.label, "faber.Postgres-17",
            "the job's spelling, verbatim"
        );
        assert_eq!(plan.specs[1].image, "redis:7-alpine");
        assert_eq!(plan.specs[1].service, "redis");
        let run = plan.specs[1].per_job.as_ref().unwrap();
        assert_eq!(run.job_env["REDIS_URL"], "redis://redis:6379/0");
        assert_eq!(plan.specs[1].version, None);
        assert_eq!(plan.specs[2].memory_mib, 128);
        assert_eq!(
            plan.labels(),
            ["faber.Postgres-17", "faber.redis", "faber.mailpit"]
        );
        // The shared server's memory is the server's, not the job's.
        assert_eq!(plan.per_job_mib(), 256 + 128);
    }

    #[test]
    fn a_refusal_names_the_label_and_the_form_expected() {
        let refusal = |labels: &[&str], policy: &ServicesPolicy| match services(&l(labels), policy)
        {
            ServiceOutcome::Refuse { label, reason } => (label, reason),
            other => panic!("{labels:?}: {other:?}"),
        };
        let (label, reason) = refusal(&["faber", "faber.postgres-15"], &full());
        assert_eq!(label, "faber.postgres-15");
        assert!(
            reason.contains("faber.postgres-<major>") && reason.contains("16, 17"),
            "{reason}"
        );
        let (_, reason) = refusal(&["faber", "faber.postgres"], &full());
        assert!(reason.contains("faber.postgres-<major>"), "{reason}");
        let (_, reason) = refusal(&["faber", "faber.mailpit"], &ServicesPolicy::default());
        assert!(reason.contains("[services.mailpit]"), "{reason}");
        let (label, reason) = refusal(
            &["faber", "faber.postgres-16", "faber.postgres-17"],
            &full(),
        );
        assert_eq!(label, "faber.postgres-17");
        assert!(reason.contains("faber.postgres-16"), "{reason}");
    }

    /// `evaluate` judges a label against the image's `serves`, which would
    /// refuse every service label: it skips them, whatever their case.
    #[test]
    fn evaluate_skips_service_labels() {
        let s = serves(&["1.61.1"]);
        for only in [
            vec!["faber", "faber.postgres-16"],
            vec!["faber", "faber.redis", "faber.mailpit"],
            vec!["faber", "faber.Postgres-99"],
        ] {
            assert_eq!(evaluate(&l(&only), &s), Outcome::Unlabelled, "{only:?}");
            assert_eq!(
                evaluate(&l(&only), &BTreeSet::new()),
                Outcome::Unlabelled,
                "an unknown image does not refuse a service: {only:?}"
            );
        }
        assert_eq!(
            evaluate(
                &l(&["faber", "faber.postgres-16", "faber.playwright-1.61.1"]),
                &s
            ),
            Outcome::Satisfied {
                labels: vec!["faber.playwright-1.61.1".to_string()]
            },
            "the service label is not echoed back as a need"
        );
        // A name the operator configured is a service label too (A1); the same
        // label with no table is a tool, judged against what the image serves.
        let with_s3 = full();
        let job = l(&["faber", "faber.s3"]);
        assert_eq!(
            super::evaluate(&job, &s, &with_s3),
            Outcome::Unlabelled,
            "configured: skipped"
        );
        assert_eq!(
            super::evaluate(&job, &s, &ServicesPolicy::default()),
            Outcome::Satisfied {
                labels: vec!["faber.s3".to_string()]
            },
            "unconfigured: a tool need, echoed back as one"
        );
        assert!(matches!(
            super::evaluate(&job, &BTreeSet::new(), &ServicesPolicy::default()),
            Outcome::Refused { .. }
        ));
        // A versioned label of a per-job name is a tool need, however the table
        // is named (A1): `[services.playwright]` captures only `faber.playwright`.
        let mut pw = ServicesPolicy::default();
        pw.per_job.insert(
            "playwright".into(),
            PerJobPolicy::builtin("playwright", "x:1", 1),
        );
        let versioned = l(&["faber", "faber.playwright-1.61.1"]);
        assert_eq!(
            super::evaluate(&versioned, &s, &pw),
            Outcome::Satisfied {
                labels: vec!["faber.playwright-1.61.1".to_string()]
            }
        );
        assert_eq!(services(&versioned, &pw), ServiceOutcome::Unlabelled);
        let bare = l(&["faber", "faber.playwright"]);
        assert_eq!(super::evaluate(&bare, &s, &pw), Outcome::Unlabelled);
        assert!(matches!(services(&bare, &pw), ServiceOutcome::Provide(_)));
        // A malformed service-ish label is still a malformed need.
        assert!(matches!(
            evaluate(&l(&["faber", "faber.postgres-"]), &s),
            Outcome::Refused { .. }
        ));
    }
}
