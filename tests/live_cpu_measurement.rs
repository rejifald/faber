//! A live exercise of ADR-0019's CPU measurement against the real Docker daemon.
//!
//! `#[ignore]`d, because it needs a Docker daemon and starts containers. The unit
//! tests in `src/cpu.rs` pin down the arithmetic on captured fixtures; what only a
//! live run can answer is whether the VM actually behaves the way that arithmetic
//! assumes: that `docker info` reports a core count, that the probe container can
//! read PSI next to `MemAvailable`, and that a container's own `cpu.stat` read
//! through `docker exec` — with `/proc/uptime` as the clock — measures a quota'd
//! busy loop at no more than its quota.
//!
//! ```sh
//! cargo test --test live_cpu_measurement -- --ignored --nocapture
//! ```
//!
//! Everything it starts it names under its own prefix and removes on every exit
//! path.

use faber::cpu::{parse_cpu_stat, parse_uptime_usec, CpuTally};

const NAME: &str = "faber-live-adr19-busy";

fn docker(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("running docker");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Removes the busy container however the test ends.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", NAME]);
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; starts containers"]
async fn the_vm_probes_answer() {
    let ncpu = faber::docker::probe_ncpu().await.expect("docker info NCPU");
    assert!(ncpu >= 1, "a VM reports at least one core, got {ncpu}");

    let p = faber::docker::probe_vm_pressure("busybox")
        .await
        .expect("the combined pressure probe runs");
    assert!(p.mem_available_mib > 0, "{p:?}");
    // A kernel without PSI reports no reading and the gate fails open; one that
    // has it must report a percentage.
    if let Some(avg60) = p.cpu_some_avg60 {
        assert!((0.0..=100.0).contains(&avg60), "{p:?}");
    }
    println!("ncpu={ncpu} {p:?}");
}

#[test]
#[ignore = "needs a Docker daemon; starts containers"]
fn a_quotad_busy_loop_measures_at_most_its_quota() {
    let _cleanup = Cleanup;
    let _ = docker(&["rm", "-f", NAME]);
    let (ok, _) = docker(&[
        "run",
        "-d",
        "--pull=never",
        "--name",
        NAME,
        "--cpus=1",
        "busybox",
        "sh",
        "-c",
        "while :; do :; done",
    ]);
    assert!(
        ok,
        "starting the busy container (needs a local busybox image)"
    );

    let mut tally = CpuTally::default();
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (ok, text) = docker(&[
            "exec",
            NAME,
            "cat",
            "/sys/fs/cgroup/cpu.stat",
            "/proc/uptime",
        ]);
        assert!(ok, "reading cpu.stat through docker exec");
        let stat = parse_cpu_stat(&text).expect("cpu.stat parses");
        let at_us = parse_uptime_usec(&text).expect("/proc/uptime parses");
        tally.observe(at_us, stat);
    }
    let usage = tally.finish().expect("four readings give a span");
    println!("{usage:?}");
    // The quota is one core. Contention on a busy VM can hold the loop below it,
    // but nothing can push the measurement meaningfully above it — which is the
    // property the learner's censoring rule relies on.
    assert!(usage.avg_mcpu > 0, "{usage:?}");
    assert!(usage.peak_mcpu <= 1_150, "{usage:?}");
    assert!((0.0..=1.0).contains(&usage.throttled_ratio), "{usage:?}");
}
