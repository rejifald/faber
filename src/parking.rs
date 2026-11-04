//! Which gate parked a claim, and for how long — issue #85.
//!
//! ADR-0022 made a parked claim say why: the gate holding it and a line saying
//! why, refreshed at every retry ([`WaitView`]). That answers "why is this job
//! waiting now?". It does not answer the question an operator asks afterwards:
//! which limit actually bound this host over the last hour, and whether
//! raising it would have bought anything. Before this, that took reading
//! `limiter.rs`.
//!
//! So the cell a claim publishes its reason into also keeps time. Every change of
//! gate closes the interval spent at the one before, so a claim leaves admission
//! carrying how long it waited at each gate, and its **binding gate**: the one it
//! waited at longest. The outcome's audit line records both, and a
//! process-wide [`GateTally`] keeps the same per gate for `/api/state`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::limiter::WaitView;

/// One gate's counters since this process started, as `/api/state`'s
/// `budget.gates.<gate>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GateCount {
    /// Claims parked at this gate right now.
    pub parked_now: u64,
    /// How many times a claim has arrived at this gate.
    pub parks: u64,
    /// The time claims have spent parked here, in milliseconds, intervals still
    /// open excluded.
    pub parked_ms: u64,
    /// Claims admitted whose longest wait was at this gate.
    pub binding: u64,
}

/// Per-gate counters for every claim this process has parked. Shared by every
/// [`Parking`] cell made from it.
#[derive(Debug, Default)]
pub struct GateTally(Mutex<BTreeMap<String, GateCount>>);

impl GateTally {
    /// The counters, by gate.
    pub fn snapshot(&self) -> BTreeMap<String, GateCount> {
        self.0.lock().unwrap().clone()
    }

    fn with(&self, gate: &str, f: impl FnOnce(&mut GateCount)) {
        f(self.0.lock().unwrap().entry(gate.to_string()).or_default());
    }

    fn arrive(&self, gate: &str) {
        self.with(gate, |c| {
            c.parks += 1;
            c.parked_now += 1;
        });
    }

    fn leave(&self, gate: &str, ms: u64) {
        self.with(gate, |c| {
            c.parked_now = c.parked_now.saturating_sub(1);
            c.parked_ms = c.parked_ms.saturating_add(ms);
        });
    }

    fn bound(&self, gate: &str) {
        self.with(gate, |c| c.binding += 1);
    }
}

/// How long a claim waited before it was admitted, gate by gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parked {
    /// Milliseconds spent at each gate.
    pub by_gate: BTreeMap<String, u64>,
}

/// The wait of a claim that never parked: an adopted one, whose container was
/// already running when this process met it.
pub static NOT_PARKED: Parked = Parked {
    by_gate: BTreeMap::new(),
};

impl Parked {
    /// The gate the claim waited at longest. On a tie, the gate first in name
    /// order, so the answer does not depend on anything but the times.
    pub fn binding(&self) -> Option<&str> {
        let mut best: Option<(&str, u64)> = None;
        for (gate, &ms) in &self.by_gate {
            if best.is_none_or(|(_, top)| ms > top) {
                best = Some((gate, ms));
            }
        }
        best.map(|(gate, _)| gate)
    }
}

/// A claim's parking cell: why it is parked now (the [`WaitView`] the dashboard
/// shows), and how long it has spent at each gate so far.
#[derive(Debug, Default)]
pub struct Parking {
    tally: Arc<GateTally>,
    /// The reason in force and when its gate was reached.
    current: Option<(WaitView, u64)>,
    spent: BTreeMap<String, u64>,
}

impl Parking {
    /// An empty cell counting into `tally`.
    pub fn new(tally: Arc<GateTally>) -> Self {
        Self {
            tally,
            current: None,
            spent: BTreeMap::new(),
        }
    }

    /// Count into `tally` from here on. A claim's cell is made before anything
    /// that holds the limiter sees it, so its provision task points it at the
    /// process-wide tally before its first wait.
    pub fn count_into(&mut self, tally: Arc<GateTally>) {
        self.tally = tally;
    }

    /// Why the claim is parked, if it is.
    pub fn waiting(&self) -> Option<WaitView> {
        self.current.as_ref().map(|(w, _)| w.clone())
    }

    /// Say why the claim is parked as of `at_ms`. The same gate again only
    /// refreshes the line, since the detail changes on every retry while the
    /// gate does not. A new gate closes the interval at the old one.
    pub fn publish(&mut self, wait: WaitView, at_ms: u64) {
        if let Some((now, _)) = &mut self.current {
            if now.gate == wait.gate {
                *now = wait;
                return;
            }
        }
        self.close(at_ms);
        self.tally.arrive(&wait.gate);
        self.current = Some((wait, at_ms));
    }

    /// The claim is admitted at `at_ms`: close the open interval, count the
    /// binding gate, and hand back the whole wait.
    pub fn settle(&mut self, at_ms: u64) -> Parked {
        self.close(at_ms);
        let parked = Parked {
            by_gate: std::mem::take(&mut self.spent),
        };
        if let Some(gate) = parked.binding() {
            self.tally.bound(gate);
        }
        parked
    }

    fn close(&mut self, at_ms: u64) {
        if let Some((wait, since)) = self.current.take() {
            let ms = at_ms.saturating_sub(since);
            self.tally.leave(&wait.gate, ms);
            *self.spent.entry(wait.gate).or_default() += ms;
        }
    }
}

impl Drop for Parking {
    /// A claim released while parked still spent that time at its gate, and is no
    /// longer parked there.
    fn drop(&mut self) {
        self.close(crate::audit::now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(gate: &str, detail: &str) -> WaitView {
        WaitView::new(gate, detail)
    }

    #[test]
    fn a_claim_carries_its_time_at_each_gate_and_its_binding_one() {
        let tally = Arc::new(GateTally::default());
        let mut p = Parking::new(tally.clone());
        p.publish(wait("image", "resolving"), 1_000);
        p.publish(wait("memory", "needs 6000 MiB, 2000 free"), 3_000);
        // A retry refreshes the line without starting a new park.
        p.publish(wait("memory", "needs 6000 MiB, 4000 free"), 60_000);
        assert_eq!(p.waiting().unwrap().detail, "needs 6000 MiB, 4000 free");
        p.publish(wait("vm-memory", "1 GiB available"), 90_000);
        let snap = tally.snapshot();
        assert_eq!(snap["memory"].parked_now, 0);
        assert_eq!(snap["vm-memory"].parked_now, 1);
        assert_eq!(snap["memory"].parks, 1);
        let parked = p.settle(100_000);
        assert_eq!(p.waiting(), None);
        assert_eq!(
            parked.by_gate,
            [
                ("image".to_string(), 2_000),
                ("memory".to_string(), 87_000),
                ("vm-memory".to_string(), 10_000),
            ]
            .into()
        );
        assert_eq!(parked.binding(), Some("memory"));
        let snap = tally.snapshot();
        assert_eq!(snap["memory"].binding, 1);
        assert_eq!(snap["memory"].parked_ms, 87_000);
        assert_eq!(snap["vm-memory"].parked_now, 0);
        assert_eq!(snap["image"].binding, 0);
    }

    #[test]
    fn coming_back_to_a_gate_adds_to_it_and_counts_a_new_park() {
        let tally = Arc::new(GateTally::default());
        let mut p = Parking::new(tally.clone());
        p.publish(wait("memory", "a"), 0);
        p.publish(wait("paused", "b"), 10);
        p.publish(wait("memory", "c"), 30);
        let parked = p.settle(100);
        assert_eq!(parked.by_gate["memory"], 80);
        assert_eq!(tally.snapshot()["memory"].parks, 2);
    }

    #[test]
    fn a_claim_never_parked_has_no_binding_gate() {
        let tally = Arc::new(GateTally::default());
        let parked = Parking::new(tally.clone()).settle(5);
        assert_eq!(parked, Parked::default());
        assert_eq!(parked.binding(), None);
        assert!(tally.snapshot().is_empty());
    }

    #[test]
    fn a_tie_is_named_by_the_gate_first_in_order() {
        let parked = Parked {
            by_gate: [("memory".to_string(), 5), ("cpu".to_string(), 5)].into(),
        };
        assert_eq!(parked.binding(), Some("cpu"));
    }

    #[test]
    fn a_claim_released_while_parked_leaves_its_gate() {
        let tally = Arc::new(GateTally::default());
        {
            let mut p = Parking::new(tally.clone());
            p.publish(wait("disk", "full"), 0);
            assert_eq!(tally.snapshot()["disk"].parked_now, 1);
        }
        let snap = tally.snapshot();
        assert_eq!(snap["disk"].parked_now, 0);
        assert_eq!(snap["disk"].parks, 1);
        assert_eq!(snap["disk"].binding, 0, "released, not admitted");
    }

    #[test]
    fn the_counter_contract_field_names() {
        let v = serde_json::to_value(GateCount::default()).unwrap();
        for key in ["parked_now", "parks", "parked_ms", "binding"] {
            assert!(v.get(key).is_some(), "{key}");
        }
    }
}
