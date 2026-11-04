//! Whether each Docker profile's own image is on this host (#63).
//!
//! A profile names the operator's base runner image. When that image is not in
//! the local Docker, every `docker run` for the profile fails — and faber used to
//! keep going regardless: register a JIT runner, fail to start it, try again,
//! every few minutes, for as long as a job was queued. On one host that was over
//! a thousand provisions and no completed jobs in about a day, under a dashboard
//! reading healthy, while a peer quietly took the jobs.
//!
//! **Detect, never rebuild.** The image is the operator's, often a local-only tag
//! that no registry holds, and faber may not infer that it can recreate it
//! (AGENTS.md, ADR-0016). So this only notices, says so, and stops minting into
//! it; the operator rebuilds it, and faber picks back up on its own the first
//! cycle after it reappears.
//!
//! **Absent is not missing.** A profile image may live in a registry and never
//! have been pulled to this host, which `docker run` has always handled by
//! pulling it. Refusing to mint on "not local" alone would withdraw that from
//! every host that relies on it. So an absent image is *pulled* — what the run
//! would have done anyway, ahead of registering a runner rather than after — and
//! only an image still absent after a failed pull counts as missing. A pull that
//! times out, or a daemon that answers nothing usable, decides nothing.
//!
//! **Local, or pulled on purpose (#156).** A runner container starts with
//! `--pull=never`, so the only pull of a profile image is the one this module
//! decides on. Before that, a deleted local-only tag `owner/x` made `docker run`
//! fetch `docker.io/owner/x` by itself, and if somebody else held that Docker Hub
//! namespace, jobs ran in their image. So a claim also waits while its profile
//! image has not been checked yet or is absent and being pulled, where `docker
//! run` used to pull it on the spot; and a reference whose pull could land in a
//! namespace nobody vouched for is classified by [`origin`] and pulled only as
//! `[host] pull_unqualified_images` says.
//!
//! This module is the pure state; the probes live in [`crate::docker`] and the
//! per-cycle I/O in [`crate::reconcile`].

use std::collections::BTreeMap;
use std::time::Duration;

/// How long after one pull faber may try the next for the same profile. A
/// local-only tag fails its pull in a second; this keeps a missing image from
/// turning every cycle into a registry round trip, while a registry outage
/// that ends is still noticed within minutes.
pub const PULL_RETRY: Duration = Duration::from_secs(10 * 60);

/// Where a pull of an image reference would fetch it from, as far as it matters
/// to who controls what arrives (#156).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The first path component names a registry host (`ghcr.io/…`,
    /// `registry.local:5000/…`, `localhost/…`, `docker.io/…`): the operator said
    /// where it lives.
    Registry,
    /// Pinned by digest. A pull can only ever return those exact bytes, whoever
    /// owns the namespace.
    Digest,
    /// No host and no `/` (`ubuntu:24.04`): Docker Hub's `library/` namespace,
    /// whose images Docker publishes itself. Nobody can register a name in it.
    Official,
    /// No host, but a `/` (`someuser/runner:1`): a Docker Hub user or
    /// organisation namespace. Anyone can register one that is free, which is the
    /// squatting risk #156 is about.
    HubUser,
}

/// Classify `image` the way Docker resolves it: the first `/`-separated component
/// is a registry host when it contains `.` or `:`, is `localhost`, or has an
/// uppercase letter (which no repository name may) — Docker's own rule.
/// Otherwise the reference is Docker Hub's, in `library/` when there is no `/`.
pub fn origin(image: &str) -> Origin {
    let image = image.trim();
    if image.contains('@') {
        return Origin::Digest;
    }
    match image.split_once('/') {
        Some((first, _))
            if first.contains(['.', ':'])
                || first == "localhost"
                || first.bytes().any(|b| b.is_ascii_uppercase()) =>
        {
            Origin::Registry
        }
        Some(_) => Origin::HubUser,
        None => Origin::Official,
    }
}

/// Whether faber may pull `image` itself when it is absent, under
/// `[host] pull_unqualified_images` (`None` when unstated).
///
/// A registry-qualified or digest-pinned reference is always pulled: the operator
/// named its source, or its content. A reference with no registry host — official
/// or user namespace alike — is pulled unless the key says `false`. Absent keeps
/// that pull, which hosts relying on `docker run`'s implicit one depend on
/// (AGENTS.md); [`unstated_risk`] is what makes that loud.
pub fn pull_allowed(image: &str, setting: Option<bool>) -> bool {
    match origin(image) {
        Origin::Registry | Origin::Digest => true,
        Origin::Official | Origin::HubUser => setting.unwrap_or(true),
    }
}

/// Whether `image` is pulled from a Docker Hub user namespace only because
/// `pull_unqualified_images` is unstated — the case boot warns about and the
/// dashboard shows. An official-library reference is left out: its namespace
/// cannot be squatted, so a pull of it returns Docker's own image or nothing.
pub fn unstated_risk(image: &str, setting: Option<bool>) -> bool {
    setting.is_none() && origin(image) == Origin::HubUser
}

/// The line boot logs and the dashboard shows for a profile whose image is
/// pulled from a Docker Hub user namespace only because the key is unstated.
pub fn unstated_warning(profile: &str, image: &str) -> String {
    format!(
        "profile {profile}: image {image} names no registry host, so a pull fetches it from \
         Docker Hub's {ns:?} namespace — if this is a local-only tag and it goes missing, faber \
         pulls whatever that namespace holds, which may be someone else's image. Prefix \
         it with its registry (ghcr.io/…, or localhost/… for a local-only tag), or set \
         [host] pull_unqualified_images = false to never pull it, or = true to accept this",
        ns = image.trim().split('/').next().unwrap_or_default()
    )
}

/// A profile whose image is confirmed missing: absent locally, and a pull could
/// not bring it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// The image the profile names.
    pub image: String,
    /// When faber confirmed it missing, epoch ms.
    pub since_ms: u64,
    /// Why the pull failed, as the daemon said it (already bounded and escaped).
    pub cause: String,
}

#[derive(Debug, Default)]
struct Watch {
    image: String,
    missing: Option<Missing>,
    pulling: bool,
    last_pull_ms: Option<u64>,
    /// The last inspect found the image absent. Since `--pull=never` (#156) a
    /// runner cannot start from it until a pull brings it back.
    absent: bool,
}

/// Why a claim for a profile must wait on that profile's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait<'a> {
    /// Not inspected yet under this image: the first cycle after boot, or after
    /// the profile changed image. Lifts within a cycle.
    Unchecked,
    /// Absent, and not (yet) confirmed missing: a pull is running or due.
    Absent,
    /// Confirmed missing.
    Missing(&'a Missing),
}

/// The per-profile verdicts, keyed by profile name.
#[derive(Debug, Default)]
pub struct ProfileImages {
    watch: BTreeMap<String, Watch>,
}

impl ProfileImages {
    /// The entry for `profile`, reset when the profile now names a different image.
    fn entry(&mut self, profile: &str, image: &str) -> &mut Watch {
        let w = self.watch.entry(profile.to_string()).or_default();
        if w.image != image {
            *w = Watch {
                image: image.to_string(),
                ..Watch::default()
            };
        }
        w
    }

    /// The image is present locally. Returns what was missing, when this is the
    /// recovery, so the caller can close the fault exactly once.
    pub fn present(&mut self, profile: &str, image: &str) -> Option<Missing> {
        let w = self.entry(profile, image);
        w.last_pull_ms = None;
        w.absent = false;
        w.missing.take()
    }

    /// The inspect answered nothing usable. Counts as checked — a daemon that
    /// cannot say must not park every claim — so an earlier `Absent` no longer
    /// holds claims either; a confirmed `Missing` verdict is kept.
    pub fn unknown(&mut self, profile: &str, image: &str) {
        self.entry(profile, image).absent = false;
    }

    /// The image is absent and faber may not pull it (#156): confirm it missing at
    /// once. `true` only when this is new, like [`ProfileImages::pulled`].
    pub fn unpullable(&mut self, profile: &str, image: &str, cause: String, now_ms: u64) -> bool {
        let w = self.entry(profile, image);
        w.absent = true;
        if w.missing.is_some() {
            return false;
        }
        w.missing = Some(Missing {
            image: image.to_string(),
            since_ms: now_ms,
            cause,
        });
        true
    }

    /// The image is absent locally. `true` when the caller should start a pull
    /// now: none is already running and the last one was at least
    /// [`PULL_RETRY`] ago.
    pub fn absent(&mut self, profile: &str, image: &str, now_ms: u64) -> bool {
        let w = self.entry(profile, image);
        w.absent = true;
        let due = w
            .last_pull_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= PULL_RETRY.as_millis() as u64);
        if w.pulling || !due {
            return false;
        }
        w.pulling = true;
        w.last_pull_ms = Some(now_ms);
        true
    }

    /// A pull finished. `still_missing` is the pull's failure when the image is
    /// still absent after it, and `None` when it is back or nothing was learnt.
    /// Returns `true` only when this newly confirms the image missing, which is
    /// the one moment worth a fault.
    pub fn pulled(
        &mut self,
        profile: &str,
        image: &str,
        still_missing: Option<String>,
        now_ms: u64,
    ) -> bool {
        let w = self.entry(profile, image);
        w.pulling = false;
        match still_missing {
            Some(cause) if w.missing.is_none() => {
                w.missing = Some(Missing {
                    image: image.to_string(),
                    since_ms: now_ms,
                    cause,
                });
                true
            }
            // Still missing keeps the first sighting's time and cause; nothing
            // learnt changes nothing.
            _ => false,
        }
    }

    /// Why a runner for `profile` that starts from `image` must wait, if it must.
    /// `profile_image` is the image the profile names: only that one is watched.
    /// A faber-built variant (ADR-0008) is another tag, which faber built here and
    /// resolved for this very claim, so it never waits on this.
    pub fn wait(&self, profile: &str, profile_image: &str, image: &str) -> Option<Wait<'_>> {
        if image != profile_image {
            return None;
        }
        let Some(w) = self.watch.get(profile).filter(|w| w.image == image) else {
            return Some(Wait::Unchecked);
        };
        match &w.missing {
            Some(m) => Some(Wait::Missing(m)),
            None if w.absent => Some(Wait::Absent),
            None => None,
        }
    }

    /// Every profile whose image is confirmed missing, by profile name.
    pub fn missing(&self) -> Vec<(String, Missing)> {
        self.watch
            .iter()
            .filter_map(|(p, w)| w.missing.clone().map(|m| (p.clone(), m)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: &str = "ghcr.io/owner/my-runner:2.335.1";
    const RETRY: u64 = PULL_RETRY.as_millis() as u64;

    /// The confirmed-missing verdict a claim for the profile's own image would wait on.
    fn blocks<'a>(s: &'a ProfileImages, profile: &str, image: &str) -> Option<&'a Missing> {
        match s.wait(profile, image, image) {
            Some(Wait::Missing(m)) => Some(m),
            _ => None,
        }
    }

    #[test]
    fn an_unknown_inspect_after_an_absent_one_stops_holding_claims() {
        let mut s = ProfileImages::default();
        s.absent("docker", IMG, 1_000);
        assert_eq!(s.wait("docker", IMG, IMG), Some(Wait::Absent));
        s.unknown("docker", IMG);
        assert_eq!(s.wait("docker", IMG, IMG), None, "a daemon that cannot say");
    }

    #[test]
    fn an_absent_image_is_pulled_before_it_is_called_missing() {
        let mut s = ProfileImages::default();
        assert!(s.absent("docker", IMG, 1_000), "first sighting pulls");
        assert!(
            blocks(&s, "docker", IMG).is_none(),
            "not missing while pulling"
        );
        assert!(!s.absent("docker", IMG, 2_000), "one pull at a time");
    }

    #[test]
    fn a_failed_pull_confirms_it_missing_once() {
        let mut s = ProfileImages::default();
        assert!(s.absent("docker", IMG, 1_000));
        assert!(s.pulled("docker", IMG, Some("not found".into()), 2_000));
        let m = blocks(&s, "docker", IMG).expect("missing after a failed pull");
        assert_eq!(m.since_ms, 2_000);
        // The next attempt, and its failure, do not restate it: one fault per
        // outage, not one per pull.
        assert!(!s.absent("docker", IMG, 1_000 + RETRY - 1), "backed off");
        assert!(s.absent("docker", IMG, 1_000 + RETRY));
        assert!(!s.pulled("docker", IMG, Some("denied".into()), 3_000 + RETRY));
        assert_eq!(blocks(&s, "docker", IMG).unwrap().since_ms, 2_000);
        assert_eq!(s.missing().len(), 1);
    }

    #[test]
    fn a_pull_that_learnt_nothing_decides_nothing() {
        let mut s = ProfileImages::default();
        assert!(s.absent("docker", IMG, 1_000));
        assert!(!s.pulled("docker", IMG, None, 2_000));
        assert!(blocks(&s, "docker", IMG).is_none());
    }

    #[test]
    fn the_image_reappearing_recovers_it() {
        let mut s = ProfileImages::default();
        s.absent("docker", IMG, 1_000);
        s.pulled("docker", IMG, Some("not found".into()), 2_000);
        let was = s.present("docker", IMG).expect("the recovery edge");
        assert_eq!(was.image, IMG);
        assert!(blocks(&s, "docker", IMG).is_none());
        assert!(s.present("docker", IMG).is_none(), "recovered once");
        // And a later loss pulls again at once rather than waiting out a backoff
        // left over from the previous outage.
        assert!(s.absent("docker", IMG, 3_000));
    }

    #[test]
    fn only_the_profiles_own_image_is_blocked() {
        let mut s = ProfileImages::default();
        s.absent("docker", IMG, 1_000);
        s.pulled("docker", IMG, Some("not found".into()), 2_000);
        // A variant built from it is another tag (ADR-0008): not this check's.
        assert_eq!(s.wait("docker", IMG, "faber-variant:abc-1.58.0"), None);
        assert!(blocks(&s, "other", IMG).is_none());
    }

    #[test]
    fn a_profile_that_now_names_another_image_starts_over() {
        let mut s = ProfileImages::default();
        s.absent("docker", IMG, 1_000);
        s.pulled("docker", IMG, Some("not found".into()), 2_000);
        assert!(s
            .present("docker", "ghcr.io/owner/my-runner:2.336.0")
            .is_none());
        assert!(blocks(&s, "docker", IMG).is_none());
    }

    #[test]
    fn a_reference_is_classified_by_its_first_component() {
        for (image, want) in [
            ("ghcr.io/owner/my-runner:2.335.1", Origin::Registry),
            ("registry.local:5000/my-runner", Origin::Registry),
            ("localhost:5000/x:y", Origin::Registry),
            ("localhost/my-runner:1", Origin::Registry),
            ("docker.io/owner/my-runner:1", Origin::Registry),
            ("Registry/x", Origin::Registry),
            ("owner/my-runner:1", Origin::HubUser),
            ("owner/team/my-runner", Origin::HubUser),
            ("owner/my-runner@sha256:00ff", Origin::Digest),
            ("ubuntu:24.04", Origin::Official),
            ("my-runner", Origin::Official),
        ] {
            assert_eq!(origin(image), want, "{image}");
        }
    }

    #[test]
    fn only_an_unqualified_reference_heeds_the_key_and_only_a_hub_user_one_warns() {
        for image in ["ghcr.io/owner/my-runner:1", "owner/my-runner@sha256:00ff"] {
            for setting in [None, Some(true), Some(false)] {
                assert!(pull_allowed(image, setting), "{image} {setting:?}");
                assert!(!unstated_risk(image, setting), "{image} {setting:?}");
            }
        }
        for image in ["owner/my-runner:1", "ubuntu:24.04"] {
            // Absent keeps today's implicit pull (AGENTS.md); false refuses it.
            assert!(pull_allowed(image, None), "{image}");
            assert!(pull_allowed(image, Some(true)), "{image}");
            assert!(!pull_allowed(image, Some(false)), "{image}");
        }
        assert!(unstated_risk("owner/my-runner:1", None));
        assert!(!unstated_risk("owner/my-runner:1", Some(true)));
        assert!(!unstated_risk("owner/my-runner:1", Some(false)));
        assert!(
            !unstated_risk("ubuntu:24.04", None),
            "library/ cannot be squatted"
        );
        let w = unstated_warning("docker", "owner/my-runner:1");
        assert!(w.contains("\"owner\" namespace"), "{w}");
        assert!(w.contains("pull_unqualified_images"), "{w}");
    }

    #[test]
    fn a_claim_waits_until_the_image_is_checked_and_local() {
        const NEXT: &str = "ghcr.io/owner/my-runner:2.336.0";
        let mut s = ProfileImages::default();
        assert_eq!(s.wait("docker", IMG, IMG), Some(Wait::Unchecked));
        assert_eq!(
            s.wait("docker", IMG, "faber-runner:abc-pw-1.58.0"),
            None,
            "a variant"
        );
        s.unknown("docker", IMG);
        assert_eq!(s.wait("docker", IMG, IMG), None, "an unanswered inspect");
        assert!(s.absent("docker", IMG, 1_000));
        assert_eq!(s.wait("docker", IMG, IMG), Some(Wait::Absent));
        // A pull that learnt nothing leaves it absent, and `--pull=never` would
        // not start it.
        s.pulled("docker", IMG, None, 2_000);
        assert_eq!(s.wait("docker", IMG, IMG), Some(Wait::Absent));
        s.present("docker", IMG);
        assert_eq!(s.wait("docker", IMG, IMG), None);
        assert_eq!(s.wait("docker", NEXT, NEXT), Some(Wait::Unchecked));
    }

    #[test]
    fn an_unpullable_image_is_missing_at_once_and_said_once() {
        const HUB: &str = "owner/my-runner:1";
        let mut s = ProfileImages::default();
        assert!(s.unpullable("docker", HUB, "never pulled".into(), 1_000));
        assert!(!s.unpullable("docker", HUB, "never pulled".into(), 2_000));
        assert!(matches!(
            s.wait("docker", HUB, HUB),
            Some(Wait::Missing(m)) if m.since_ms == 1_000
        ));
        assert!(s.present("docker", HUB).is_some(), "rebuilt: recovers");
        assert_eq!(s.wait("docker", HUB, HUB), None);
    }
}
