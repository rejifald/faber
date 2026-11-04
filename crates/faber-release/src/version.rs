//! Version parsing, release selection and the version binding (ADR-0031).

use tracing::debug;

use crate::release::Release;

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

/// Parse a version string — `"0.2.0"`, `"v0.2.0"`, or `"0.2.0+g1a2b3c4"` —
/// into its `(major, minor, patch)` triple. Everything from the first `+` or
/// `-` onward is build metadata or a pre-release tag and is discarded:
/// faber's own stamped version (`FABER_VERSION`, set in `build.rs`) carries
/// `+g<sha>` on every build made from a git checkout, and comparing that
/// suffix would make a locally-built binary compare unequal to the release it
/// was cut from. `None` for anything that isn't exactly three dot-separated
/// unsigned integers once metadata is stripped.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let core = s.split(['+', '-']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        // A fourth dotted component — not a version faber's three-part
        // scheme can represent, so treat it as unparseable rather than
        // silently truncating it.
        return None;
    }
    Some((major, minor, patch))
}

/// The release a check chose, and the reason shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Version without the `v` prefix, as the rest of the updater carries it.
    pub version: String,
    /// e.g. `"v0.4.2: highest published above v0.4.1"`.
    pub reason: String,
}

/// Choose what to install from the published releases (ADR-0031, issue #120).
///
/// Never `/releases/latest`: that is a mutable pointer, and a token able to
/// delete a published release can move it to an older genuine build. Instead:
/// the highest semver among non-draft, non-pre-release releases whose tag is
/// exactly `vX.Y.Z`, and only if strictly above `current` -- so a deleted
/// newest release leaves a host where it is rather than downgrading it, and a
/// stale "latest" cannot hide a newer release. With `pin` set, exactly the
/// pinned version is the only candidate (still never one at or below
/// `current`). Unparsable tags are skipped with a debug log.
pub fn select_candidate(
    releases: &[Release],
    current: &str,
    pin: Option<&str>,
) -> Option<Candidate> {
    let cur = parse_version(current)?;
    let cur_label = format!("v{}.{}.{}", cur.0, cur.1, cur.2);
    let pinned = pin.map(parse_version);
    let mut best: Option<(u64, u64, u64)> = None;
    for r in releases {
        let parsed = r
            .tag_name
            .strip_prefix('v')
            .filter(|t| !t.contains(['-', '+']))
            .and_then(parse_version);
        let Some(v) = parsed else {
            debug!(tag = %r.tag_name, "skipping release whose tag is not vX.Y.Z");
            continue;
        };
        match pinned {
            // An explicit pin is the operator's choice: it selects exactly one
            // tag, whatever GitHub marks it as.
            Some(p) => {
                if p != Some(v) {
                    continue;
                }
            }
            None => {
                if r.draft || r.prerelease {
                    continue;
                }
            }
        }
        if v <= cur {
            continue;
        }
        if best.is_none_or(|b| v > b) {
            best = Some(v);
        }
    }
    let v = best?;
    let version = format!("{}.{}.{}", v.0, v.1, v.2);
    let reason = if pin.is_some() {
        format!("v{version}: pinned, above {cur_label}")
    } else {
        format!("v{version}: highest published above {cur_label}")
    };
    Some(Candidate { version, reason })
}

/// Why `select_candidate` returned nothing for a pin that is set, as a message
/// for `last_error` (ADR-0031 refuses suffixed versions by design; an absent
/// result must be loud, not silent). `None` when the pin is simply the version
/// already running, which is not a fault.
pub fn pin_problem(releases: &[Release], current: &str, pin: &str) -> Option<String> {
    let bare = pin.strip_prefix('v').unwrap_or(pin);
    let plain = !bare.contains(['-', '+']) && parse_version(bare).is_some();
    if !plain {
        return Some(format!(
            "update.pin v{bare} is not a plain vX.Y.Z release; pins must name one"
        ));
    }
    let v = parse_version(bare)?;
    if !releases.iter().any(|r| r.tag_name == format!("v{bare}")) {
        return Some(format!("update.pin v{bare} matches no published release"));
    }
    let cur = parse_version(current)?;
    if v < cur {
        return Some(format!(
            "update.pin v{bare} is not above the running v{}.{}.{}",
            cur.0, cur.1, cur.2
        ));
    }
    None
}

/// Whether `candidate` is a strictly newer version than `current`. Compares
/// the parsed `(major, minor, patch)` triples numerically — never as
/// strings, which get `"0.10.0" < "0.9.0"` backwards (`'1' < '9'` lexically)
/// — and never downgrades: an unparseable `candidate` or `current`, or two
/// equal versions, both read as "not newer". This is the one gate standing
/// between `update.pin` and installing an older release than the one already
/// running, so it fails closed on anything it cannot confidently compare.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Pure half of the binding. `output` is `faber <x.y.z>+g<sha>` (the shape
/// `release.yml` asserts); the version must equal the tag's exactly and the sha
/// must be a prefix of `commit`, the commit the tag points to.
pub fn check_reported_version(output: &str, tag: &str, commit: &str) -> Result<(), String> {
    check_reported_version_for("faber", output, tag, commit)
}

/// The guest agent's form (ADR-0026): `faber-agent <x.y.z>+g<sha> protocol=<n>`.
/// The protocol suffix is the agent's own handshake number, not part of the
/// build binding, so it is split off (and must be digits) before the rest is
/// bound to the tag and commit exactly like the launcher's line.
pub fn check_reported_agent_version(output: &str, tag: &str, commit: &str) -> Result<(), String> {
    let line = output.trim();
    let Some((rest, protocol)) = line.rsplit_once(' ') else {
        return Err(format!(
            "version binding: unparsable --version output {line:?}, expected 'faber-agent <x.y.z>+g<sha> protocol=<n>'"
        ));
    };
    let digits = protocol.strip_prefix("protocol=").unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "version binding: the agent's trailing {protocol:?} is not protocol=<n>"
        ));
    }
    check_reported_version_for("faber-agent", rest, tag, commit)
}

/// [`check_reported_version`] for any release binary: `output` must be
/// `<program> <x.y.z>+g<sha>` and nothing else. The program name is part of
/// the binding, so `faber-vm <v>` never passes the `faber` check, nor the
/// reverse (the launcher is a separate asset, ADR-0026).
pub fn check_reported_version_for(
    program: &str,
    output: &str,
    tag: &str,
    commit: &str,
) -> Result<(), String> {
    let want = tag.strip_prefix('v').unwrap_or(tag);
    if !is_strict_version(want) {
        return Err(format!(
            "version binding: tag {tag} is not vX.Y.Z, so there is no exact version to bind to"
        ));
    }
    let line = output.trim();
    let Some(reported) = line
        .strip_prefix(program)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Err(format!(
            "version binding: unparsable --version output {line:?}, expected '{program} {want}+g<sha>'"
        ));
    };
    let Some((version, build)) = reported.split_once('+') else {
        return Err(format!(
            "version binding: the binary reports {reported:?} with no +g<sha> build suffix, so its commit cannot be checked"
        ));
    };
    if version != want {
        return Err(format!(
            "version binding: the binary reports version {version}, but tag {tag} is {want}"
        ));
    }
    let sha = build
        .strip_prefix('g')
        .filter(|s| {
            (7..=40).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| {
            format!("version binding: build suffix +{build} is not +g<7-40 lowercase hex digits>")
        })?;
    if !commit.to_ascii_lowercase().starts_with(sha) {
        return Err(format!(
            "version binding: the binary was built from commit {sha}, but tag {tag} points to {commit}"
        ));
    }
    Ok(())
}

/// The app-bundle form of the version binding (ADR-0031, #80). The bundle's
/// `CFBundleShortVersionString` must equal the tag without its `v`, exactly:
/// unlike the daemon (`vX.Y.Z` reported as `X.Y.Z+g<sha>`), a bundle carries no
/// build suffix, so a `-` or `+` part is refused, never ignored.
pub fn check_bundle_version(short_version: &str, tag: &str) -> Result<(), String> {
    let want = tag.strip_prefix('v').unwrap_or(tag);
    if !is_strict_version(want) {
        return Err(format!(
            "version binding: tag {tag} is not vX.Y.Z, so there is no exact version to bind to"
        ));
    }
    if short_version != want {
        return Err(format!(
            "version binding: the app bundle reports version {short_version}, but tag {tag} is {want}"
        ));
    }
    Ok(())
}

/// Exactly three dot-separated all-digit components: no pre-release, no build
/// metadata, nothing `parse_version` would discard.
fn is_strict_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_plain_and_v_prefixed() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
    }

    #[test]
    fn parse_version_ignores_build_metadata() {
        assert_eq!(parse_version("0.2.0+g1a2b3c4"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.4.10+g1a2b3c4"), Some((1, 4, 10)));
    }

    #[test]
    fn parse_version_rejects_junk() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.x"), None);
    }

    #[test]
    fn is_newer_compares_numerically_not_lexically() {
        // A naive string compare gets this backwards: "0.10.0" < "0.9.0"
        // lexicographically because '1' < '9'.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn is_newer_equal_versions_are_not_newer() {
        assert!(!is_newer("0.2.0", "0.2.0"));
    }

    #[test]
    fn is_newer_build_metadata_does_not_count_as_newer() {
        assert!(!is_newer("0.2.0+g1a2b3c4", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0+gabc1234"));
    }

    // -- decide() --------------------------------------------------------

    // -- select_candidate (ADR-0031, #120) --------------------------------

    fn rel(tag: &str) -> Release {
        Release {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            published_at: None,
            assets: Vec::new(),
        }
    }

    fn pick(tags: &[&str], current: &str, pin: Option<&str>) -> Option<String> {
        let rs: Vec<Release> = tags.iter().map(|t| rel(t)).collect();
        select_candidate(&rs, current, pin).map(|c| c.version)
    }

    #[test]
    fn selection_ignores_which_release_latest_points_at() {
        // GitHub's "latest" pointing at v1.0.0 is invisible here: only the
        // list is read, and the list order is irrelevant.
        let tags = ["v1.0.0", "v3.0.0", "v2.0.0"];
        assert_eq!(pick(&tags, "2.0.0", None).as_deref(), Some("3.0.0"));
    }

    #[test]
    fn a_deleted_newest_release_never_downgrades() {
        let tags = ["v1.0.0", "v2.0.0"]; // v3.0.0 deleted
        assert_eq!(pick(&tags, "3.0.0", None), None);
        assert_eq!(pick(&tags, "1.0.0", None).as_deref(), Some("2.0.0"));
    }

    #[test]
    fn drafts_and_prereleases_are_never_candidates() {
        let mut draft = rel("v9.0.0");
        draft.draft = true;
        let mut pre = rel("v8.0.0");
        pre.prerelease = true;
        let rs = vec![draft, pre, rel("v1.1.0")];
        let c = select_candidate(&rs, "1.0.0", None).unwrap();
        assert_eq!(c.version, "1.1.0");
    }

    #[test]
    fn multi_digit_components_compare_numerically() {
        assert_eq!(
            pick(&["v0.9.9", "v0.10.0", "v0.2.0"], "0.9.0", None).as_deref(),
            Some("0.10.0")
        );
        assert_eq!(
            pick(&["v1.2.10", "v1.2.9"], "1.2.0", None).as_deref(),
            Some("1.2.10")
        );
    }

    #[test]
    fn equal_version_is_nothing_and_build_metadata_is_ignored() {
        assert_eq!(pick(&["v0.4.1"], "0.4.1", None), None);
        assert_eq!(pick(&["v0.4.1"], "0.4.1+gabc1234", None), None);
    }

    #[test]
    fn unparsable_and_suffixed_tags_are_skipped() {
        let tags = ["nightly", "v1.0", "v1.0.0-rc1", "1.5.0", "v1.0.1"];
        // "1.5.0" lacks the v prefix: not a release tag of this channel.
        assert_eq!(pick(&tags, "1.0.0", None).as_deref(), Some("1.0.1"));
    }

    #[test]
    fn the_reason_names_the_candidate_and_the_running_version() {
        let rs = vec![rel("v0.4.2"), rel("v0.4.0")];
        let c = select_candidate(&rs, "0.4.1+g1234567", None).unwrap();
        assert_eq!(c.reason, "v0.4.2: highest published above v0.4.1");
    }

    #[test]
    fn a_pin_selects_exactly_that_version_and_never_a_lower_one() {
        let tags = ["v0.3.0", "v0.4.0", "v0.5.0"];
        assert_eq!(
            pick(&tags, "0.3.0", Some("0.4.0")).as_deref(),
            Some("0.4.0")
        );
        // Pinned below, or at, the running version: nothing, never a downgrade.
        assert_eq!(pick(&tags, "0.5.0", Some("0.4.0")), None);
        assert_eq!(pick(&tags, "0.4.0", Some("0.4.0")), None);
        // Pinned to something that is not published.
        assert_eq!(pick(&tags, "0.3.0", Some("0.9.0")), None);
        // The pin is the operator's explicit choice, even for a pre-release.
        let mut pre = rel("v0.6.0");
        pre.prerelease = true;
        let c = select_candidate(&[pre], "0.5.0", Some("0.6.0")).unwrap();
        assert_eq!(c.reason, "v0.6.0: pinned, above v0.5.0");
    }

    #[test]
    fn a_pin_that_selects_nothing_says_why() {
        let rs = vec![rel("v0.3.0"), rel("v0.4.0")];
        assert_eq!(
            pin_problem(&rs, "0.3.0", "0.6.0-rc1").unwrap(),
            "update.pin v0.6.0-rc1 is not a plain vX.Y.Z release; pins must name one"
        );
        assert_eq!(
            pin_problem(&rs, "0.3.0", "v0.9.0").unwrap(),
            "update.pin v0.9.0 matches no published release"
        );
        assert_eq!(
            pin_problem(&rs, "0.4.0", "0.3.0").unwrap(),
            "update.pin v0.3.0 is not above the running v0.4.0"
        );
        // A selectable pin is not a problem.
        assert_eq!(pin_problem(&rs, "0.3.0", "0.4.0"), None);
    }

    const COMMIT: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";

    #[test]
    fn a_matching_version_and_commit_prefix_passes() {
        assert_eq!(
            check_reported_version("faber 1.2.3+g1a2b3c4\n", "v1.2.3", COMMIT),
            Ok(())
        );
    }

    #[test]
    fn an_older_genuine_build_under_a_newer_tag_is_refused() {
        let err = check_reported_version("faber 1.2.2+g1a2b3c4", "v1.2.3", COMMIT).unwrap_err();
        assert!(err.contains("1.2.2") && err.contains("1.2.3"), "{err}");
    }

    #[test]
    fn a_matching_version_from_a_different_commit_is_refused() {
        let err = check_reported_version("faber 1.2.3+gdeadbee", "v1.2.3", COMMIT).unwrap_err();
        assert!(err.contains("deadbee") && err.contains(COMMIT), "{err}");
    }

    #[test]
    fn a_suffixless_or_malformed_version_is_refused() {
        for out in [
            "faber 1.2.3",
            "faber 1.2.3+1a2b3c4",
            "faber 1.2.3+g1a2b",
            "faber 1.2.3+g1A2B3C4",
            "faber 1.2.3-rc1+g1a2b3c4",
            "1.2.3+g1a2b3c4",
            "",
        ] {
            assert!(
                check_reported_version(out, "v1.2.3", COMMIT).is_err(),
                "{out:?} should be refused"
            );
        }
        assert!(check_reported_version("faber 1.2.3+g1a2b3c4", "v1.2.3-rc1", COMMIT).is_err());
    }

    #[test]
    fn a_bundle_version_equal_to_the_tag_passes() {
        assert_eq!(check_bundle_version("1.2.3", "v1.2.3"), Ok(()));
    }

    #[test]
    fn a_bundle_version_that_differs_from_the_tag_is_refused() {
        let err = check_bundle_version("1.2.2", "v1.2.3").unwrap_err();
        assert!(err.starts_with("version binding:"), "{err}");
        assert!(err.contains("1.2.2") && err.contains("1.2.3"), "{err}");
    }

    #[test]
    fn a_bundle_version_with_a_build_or_prerelease_suffix_is_refused() {
        for v in ["1.2.3+g1234567", "1.2.3-rc1", "1.2.3-rc1+g1234567"] {
            assert!(
                check_bundle_version(v, "v1.2.3").is_err(),
                "{v:?} should be refused"
            );
        }
    }

    #[test]
    fn a_non_strict_tag_is_refused_for_a_bundle() {
        for tag in [
            "v1.2.3-rc1",
            "v1.2.3+g1234567",
            "v1.2",
            "v1.2.3.4",
            "vx.y.z",
            "",
        ] {
            let err = check_bundle_version("1.2.3", tag).unwrap_err();
            assert!(err.starts_with("version binding:"), "{tag}: {err}");
        }
    }

    #[test]
    fn a_tag_without_the_v_prefix_is_read_the_way_the_daemon_form_reads_it() {
        assert_eq!(check_bundle_version("1.2.3", "1.2.3"), Ok(()));
        assert!(check_reported_version("faber 1.2.3+g1a2b3c4", "1.2.3", COMMIT).is_ok());
        assert!(check_bundle_version("1.2.4", "1.2.3").is_err());
    }

    #[test]
    fn the_launcher_form_binds_its_own_program_name() {
        assert_eq!(
            check_reported_version_for("faber-vm", "faber-vm 1.2.3+g1a2b3c4\n", "v1.2.3", COMMIT),
            Ok(())
        );
        // Wrong version and wrong commit are refused as for the daemon.
        let err =
            check_reported_version_for("faber-vm", "faber-vm 1.2.2+g1a2b3c4", "v1.2.3", COMMIT)
                .unwrap_err();
        assert!(err.contains("reports version 1.2.2"), "{err}");
        let err =
            check_reported_version_for("faber-vm", "faber-vm 1.2.3+gdeadbee", "v1.2.3", COMMIT)
                .unwrap_err();
        assert!(err.contains("built from commit"), "{err}");
    }

    #[test]
    fn the_daemon_and_launcher_forms_never_pass_for_each_other() {
        let vm = "faber-vm 1.2.3+g1a2b3c4";
        let daemon = "faber 1.2.3+g1a2b3c4";
        assert!(check_reported_version(vm, "v1.2.3", COMMIT).is_err());
        assert!(check_reported_version_for("faber-vm", daemon, "v1.2.3", COMMIT).is_err());
        // A prefix of the program name is not the program.
        assert!(check_reported_version_for("faber", vm, "v1.2.3", COMMIT).is_err());
        assert!(
            check_reported_version_for("faber-vm", "other 1.2.3+g1a2b3c4", "v1.2.3", COMMIT)
                .is_err()
        );
    }

    #[test]
    fn the_agent_form_binds_its_name_build_and_a_numeric_protocol() {
        let ok = "faber-agent 1.2.3+g1a2b3c4 protocol=2\n";
        assert_eq!(check_reported_agent_version(ok, "v1.2.3", COMMIT), Ok(()));
        for bad in [
            "faber-agent 1.2.3+g1a2b3c4",                  // no protocol suffix
            "faber-agent 1.2.3+g1a2b3c4 protocol=",        // empty protocol
            "faber-agent 1.2.3+g1a2b3c4 protocol=x",       // non-numeric protocol
            "faber-agent 1.2.3+g1a2b3c4 extra protocol=2", // smuggled field
            "faber-agent 1.2.2+g1a2b3c4 protocol=2",       // wrong version
            "faber-agent 1.2.3+gdeadbee protocol=2",       // wrong commit
            "faber-vm 1.2.3+g1a2b3c4 protocol=2",          // another program
            "faber 1.2.3+g1a2b3c4 protocol=2",             // the daemon
        ] {
            assert!(
                check_reported_agent_version(bad, "v1.2.3", COMMIT).is_err(),
                "{bad}"
            );
        }
        // And the agent's line is not a launcher's or a daemon's.
        assert!(check_reported_version_for("faber-vm", ok, "v1.2.3", COMMIT).is_err());
        assert!(check_reported_version(ok, "v1.2.3", COMMIT).is_err());
    }
}
