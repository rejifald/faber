//! Whether one job gets a Docker-in-Docker daemon.
//!
//! **The rule** (ADR-0026 § Consequences: only jobs that ask for
//! `[faber, dind]` get a daemon). A job gets a daemon only when its `runs-on`
//! labels include `dind` *and* its profile grants one — `dind = true`, or no
//! `dind` key at all ([`DindGrant::Unstated`], which still grants, ADR-0013). A
//! job without the label gets no daemon on any profile, and a job that asks on a
//! profile with `dind = false` is refused rather than handed a runner without
//! one.
//!
//! **The rule is a host setting until the cut.** It withdraws a daemon from
//! every job that does not ask, which a host may already rely on, so it is behind
//! `[host] dind_requires_label` ([`crate::config::DindLabelRule`], AGENTS.md):
//! absent or `false` keeps the behaviour from before the rule — every job on a
//! granting profile gets a daemon whatever its labels, a job on a denying
//! profile gets none and is never refused — and `true` is the rule above. The
//! cut's host sitting sets it `true`; the key is removed after the cut (#213).
//!
//! **The label narrows; it never widens.** The grant stays the operator's, in
//! `config.toml`. ADR-0013's "Alternatives considered" rejected a label as the
//! grant itself — a workflow author would then be the one deciding to hand their
//! job host root — but left room for a label that narrows an operator's grant to
//! the jobs that use it. That is all this is: the label is a *precondition* on
//! top of the grant, so no label, on any profile, can produce a daemon the
//! profile did not already allow.
//!
//! **Only `dind` is checked here.** The `faber` half of `[faber, dind]` is
//! settled before this runs: a job reaches a profile only through
//! [`crate::backend::match_profile`], whose labels it must cover. What is left
//! to decide per job is whether it asked for the daemon.
//!
//! **The residual.** A JIT runner is not job-pinned. GitHub hands a runner any
//! queued job of the same repo whose labels are a subset of the runner's, so a
//! runner minted with a daemon for a dind job can still be given a sibling job
//! that never asked for one: labels can require, they cannot exclude. The
//! reverse is closed — a daemonless runner never carries `dind`, so a dind job
//! can only land on a runner that has one. The sibling case is closed by #108 (a
//! VM per dind job).

use crate::docker::DindGrant;

/// The `runs-on` label a job carries to ask for a daemon.
pub const LABEL: &str = "dind";

/// What one job gets, given its labels and its profile's grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The job asked (or the rule is off), and the profile grants: start a
    /// daemon beside its runner — registered with [`LABEL`] only while the rule
    /// is on.
    Provide,
    /// No daemon: with the rule on, the job did not ask, whatever the profile
    /// grants; with it off, the profile denies one.
    Withhold,
    /// The job asked on a profile that denies a daemon. Nothing it could land on
    /// here would serve it, so no runner is minted.
    Refuse { reason: String },
}

impl Decision {
    /// Whether a daemon is started for this job.
    pub fn provides(&self) -> bool {
        matches!(self, Decision::Provide)
    }
}

/// Decide whether a job with `job_labels`, routed to the profile `profile_name`
/// whose grant is `grant`, gets a daemon.
///
/// `requires_label` is `[host] dind_requires_label` resolved
/// ([`crate::config::DindLabelRule::requires_label`]). Off, the labels are not
/// read: the grant alone decides, [`Decision::Provide`] or
/// [`Decision::Withhold`], never a refusal — the behaviour before the rule.
///
/// The label match is ASCII case-insensitive and exact: GitHub matches `runs-on`
/// labels without regard to case, so `DinD` asks; `dind2` or `no-dind` do not.
pub fn decide(
    job_labels: &[String],
    grant: DindGrant,
    requires_label: bool,
    profile_name: &str,
) -> Decision {
    if !requires_label {
        return if grant.granted() {
            Decision::Provide
        } else {
            Decision::Withhold
        };
    }
    let asks = job_labels.iter().any(|l| l.eq_ignore_ascii_case(LABEL));
    if !asks {
        return Decision::Withhold;
    }
    if grant.granted() {
        Decision::Provide
    } else {
        Decision::Refuse {
            reason: format!(
                "the job's runs-on includes `{LABEL}`, but profile '{profile_name}' does not \
                 grant a Docker daemon (`dind = false`)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn decision_table_with_the_rule_on() {
        use DindGrant::*;
        // (labels, grant, provides, refused)
        let cases: &[(&[&str], DindGrant, bool, bool)] = &[
            // No label: no daemon, whatever the grant.
            (&["faber"], Granted, false, false),
            (&["faber"], Denied, false, false),
            (&["faber"], Unstated, false, false),
            (&[], Unstated, false, false),
            // The label on a granting (or unstated) profile: a daemon.
            (&["faber", "dind"], Granted, true, false),
            (&["faber", "dind"], Unstated, true, false),
            // The label on a denying profile: refused, never silently withheld.
            (&["faber", "dind"], Denied, false, true),
            // GitHub matches labels case-insensitively.
            (&["faber", "DinD"], Granted, true, false),
            (&["faber", "DIND"], Denied, false, true),
            // Anywhere among other labels.
            (
                &["faber", "docker", "faber.node-22", "dind"],
                Unstated,
                true,
                false,
            ),
            // A label that merely contains the word does not ask.
            (&["faber", "dind2"], Granted, false, false),
            (&["faber", "faber.dind"], Granted, false, false),
            (&["faber", "no-dind"], Unstated, false, false),
            (&["faber", " dind"], Granted, false, false),
        ];
        for (l, grant, provides, refused) in cases {
            let d = decide(&labels(l), *grant, true, "p");
            assert_eq!(d.provides(), *provides, "{l:?} × {grant:?} → {d:?}");
            assert_eq!(
                matches!(d, Decision::Refuse { .. }),
                *refused,
                "{l:?} × {grant:?} → {d:?}"
            );
            if !provides && !refused {
                assert_eq!(d, Decision::Withhold, "{l:?} × {grant:?}");
            }
        }
    }

    #[test]
    fn with_the_rule_off_the_grant_alone_decides() {
        // Unstated or `false` `[host] dind_requires_label`: the behaviour before
        // the rule — labels are not read, and nothing is ever refused.
        use DindGrant::*;
        let label_sets: &[&[&str]] = &[
            &[],
            &["faber"],
            &["faber", "dind"],
            &["faber", "DIND"],
            &["faber", "dind2"],
            &["faber", "no-dind"],
        ];
        for l in label_sets {
            for (grant, want) in [
                (Granted, Decision::Provide),
                (Unstated, Decision::Provide),
                (Denied, Decision::Withhold),
            ] {
                assert_eq!(
                    decide(&labels(l), grant, false, "p"),
                    want,
                    "{l:?} × {grant:?}"
                );
            }
        }
    }

    #[test]
    fn a_refusal_names_the_profile_and_the_setting() {
        let Decision::Refuse { reason } =
            decide(&labels(&["dind"]), DindGrant::Denied, true, "linux")
        else {
            panic!("expected a refusal");
        };
        assert!(reason.contains("'linux'"), "{reason}");
        assert!(reason.contains("dind = false"), "{reason}");
    }
}
