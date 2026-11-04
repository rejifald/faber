//! The release workflow pipes what `faber provenance-policy` prints, and what the
//! menu-bar app's `--provenance-policy` prints, into scripts/check-release-policy.sh.
//! The script's own cases (scripts/test-check-release-policy.sh) feed it policies
//! written by hand; this feeds it the text `faber-release` really prints, so a
//! renamed key or a reworded line fails here, and not in the cut's first release run.

use std::io::Write;
use std::process::{Command, Stdio};

use faber::provenance::{active_policy, describe_active_policy};

/// What the script makes of `policy` for a run in repository `repository_id` of
/// workflow `workflow_ref`: whether it accepted it, and everything it printed.
fn check(policy: &str, repository_id: &str, workflow_ref: &str) -> (bool, String) {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/check-release-policy.sh"
    );
    let mut child = Command::new("bash")
        .arg(script)
        .env("REPOSITORY_ID", repository_id)
        .env("WORKFLOW_REF", workflow_ref)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("starting bash");
    // A script that refuses before reading its input closes the pipe on us; the
    // verdict below is what matters then.
    let _ = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(policy.as_bytes());
    let out = child.wait_with_output().expect("waiting for the script");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[cfg(not(feature = "sandbox-policy"))]
#[test]
fn the_script_reads_the_policy_faber_release_prints() {
    // Until the cut the printed repository ID is the placeholder 0, which the
    // script refuses by design. Stand in for the real one: it is the only line
    // that changes at the cut.
    let policy = describe_active_policy()
        .lines()
        .map(|line| {
            if line.starts_with("repository_id: ") {
                "repository_id: 424242"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let workflow_ref = active_policy()
        .signer_workflow
        .strip_prefix("https://github.com/")
        .expect("the signer is a github.com workflow URL");

    let (accepted, out) = check(&policy, "424242", workflow_ref);
    assert!(
        accepted,
        "the script refused the policy faber-release prints:\n{out}"
    );

    let (accepted, _) = check(&policy, "424243", workflow_ref);
    assert!(!accepted, "a run in another repository was accepted");
    let (accepted, _) = check(
        &policy,
        "424242",
        "owner/demo/.github/workflows/release.yml@refs/heads/main",
    );
    assert!(!accepted, "a run of another workflow was accepted");
}
