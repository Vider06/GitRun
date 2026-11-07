#[cfg(all(not(kani), not(verus)))]
use serde::{Deserialize, Serialize};
#[cfg(verus)]
use vstd::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(all(not(kani), not(verus)), derive(Serialize, Deserialize))]
pub enum RunnerState {
    Offline,
    Idle,
    Busy,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(all(not(kani), not(verus)), derive(Serialize, Deserialize))]
pub struct Runner {
    pub name: String,
    pub repository: String,
    pub state: RunnerState,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerPool {
    runners: Vec<Runner>,
}

impl Runner {
    #[cfg_attr(verus, verus_verify(dual_spec, open))]
    #[cfg_attr(
        verus,
        verus_spec(returns Runner::desired_count(minimum, maximum, busy, queued))
    )]
    pub fn desired_count(minimum: u32, maximum: u32, busy: u32, queued: u32) -> u32 {
        minimum.max(busy.saturating_add(queued)).min(maximum)
    }
}

impl RunnerPool {
    pub fn new(runners: Vec<Runner>) -> Self {
        Self { runners }
    }
    pub fn runners(&self) -> &[Runner] {
        &self.runners
    }
    pub fn online_count(&self) -> u32 {
        self.runners
            .iter()
            .filter(|r| !matches!(r.state, RunnerState::Offline | RunnerState::Failed))
            .count() as u32
    }
    pub fn busy_count(&self) -> u32 {
        self.runners
            .iter()
            .filter(|r| r.state == RunnerState::Busy)
            .count() as u32
    }
    pub fn failed_count(&self) -> u32 {
        self.runners
            .iter()
            .filter(|r| r.state == RunnerState::Failed)
            .count() as u32
    }
    pub fn reconcile_target(&self, minimum: u32, maximum: u32, queued: u32) -> u32 {
        Runner::desired_count(minimum, maximum, self.busy_count(), queued)
    }
    pub fn add(&mut self, runner: Runner) {
        if !self
            .runners
            .iter()
            .any(|existing| existing.name == runner.name)
        {
            self.runners.push(runner);
        }
    }
    pub fn mark_state(&mut self, name: &str, state: RunnerState) -> bool {
        if let Some(runner) = self.runners.iter_mut().find(|runner| runner.name == name) {
            runner.state = state;
            true
        } else {
            false
        }
    }
    pub fn remove(&mut self, name: &str) -> Option<Runner> {
        let index = self.runners.iter().position(|runner| runner.name == name)?;
        Some(self.runners.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runner(name: &str, state: RunnerState) -> Runner {
        Runner {
            name: name.into(),
            repository: "owner/repo".into(),
            state,
            labels: vec!["self-hosted".into()],
        }
    }
    #[test]
    fn desired_count_respects_bounds() {
        assert_eq!(Runner::desired_count(3, 8, 0, 0), 3);
        assert_eq!(Runner::desired_count(3, 8, 4, 2), 6);
        assert_eq!(Runner::desired_count(3, 8, 8, 9), 8);
    }
    #[test]
    fn pool_tracks_runner_states() {
        let mut pool = RunnerPool::new(vec![
            runner("a", RunnerState::Idle),
            runner("b", RunnerState::Busy),
        ]);
        pool.add(runner("b", RunnerState::Failed));
        pool.add(runner("c", RunnerState::Failed));
        assert_eq!(pool.online_count(), 2);
        assert_eq!(pool.busy_count(), 1);
        assert_eq!(pool.failed_count(), 1);
        assert_eq!(pool.reconcile_target(2, 5, 2), 3);
        assert!(pool.mark_state("c", RunnerState::Idle));
        assert!(pool.remove("c").is_some());
        assert!(!pool.mark_state("missing", RunnerState::Busy));
    }
}

#[cfg(kani)]
mod kani_verification {
    use super::Runner;

    /// Proves the desired runner count is clamped between the bounds.
    #[kani::proof]
    fn desired_count_respects_bounds_for_all_u32_inputs() {
        let minimum: u32 = kani::any();
        let maximum: u32 = kani::any();
        let busy: u32 = kani::any();
        let queued: u32 = kani::any();

        let desired = Runner::desired_count(minimum, maximum, busy, queued);
        assert!(desired <= maximum);
        assert!(desired >= minimum.min(maximum));
    }
}

#[cfg(verus)]
mod verus_verification {
    use super::Runner;
    use vstd::prelude::*;

    verus! {
        /// Verifies both clamping guarantees over the full u32 input domain.
        proof fn desired_count_respects_bounds(
            minimum: u32,
            maximum: u32,
            busy: u32,
            queued: u32,
        )
            ensures
                Runner::desired_count(minimum, maximum, busy, queued) <= maximum,
                Runner::desired_count(minimum, maximum, busy, queued) >= minimum.min(maximum),
        {
        }
    }
}

#[cfg(kani)]
fn main() {}

#[cfg(verus)]
fn main() {}
