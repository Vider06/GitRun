use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunnerState { Offline, Idle, Busy, Failed }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Runner {
    pub name: String,
    pub repository: String,
    pub state: RunnerState,
    pub labels: Vec<String>,
}

impl Runner {
    pub fn desired_count(minimum: u32, maximum: u32, busy: u32, queued: u32) -> u32 {
        minimum.max(busy.saturating_add(queued)).min(maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desired_count_respects_bounds() {
        assert_eq!(Runner::desired_count(3, 8, 0, 0), 3);
        assert_eq!(Runner::desired_count(3, 8, 4, 2), 6);
        assert_eq!(Runner::desired_count(3, 8, 8, 9), 8);
    }
}
