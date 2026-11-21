//! Rolling crash budget shared by every member of one placement.

use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

/// Failures older than this no longer count against the placement.
pub const WINDOW: Duration = Duration::from_secs(300);
/// The failure that reaches this count inside [`WINDOW`] disables the placement instead of restarting it.
pub const LIMIT: usize = 3;

#[derive(Debug, Default)]
pub struct Budget {
    failures: VecDeque<Instant>,
}

impl Budget {
    /// Records one placement failure at `now` and reports whether it exhausted the budget.
    pub fn exhausted(&mut self, now: Instant) -> bool {
        while self
            .failures
            .front()
            .is_some_and(|&at| now.duration_since(at) >= WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        self.failures.len() >= LIMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every instant is injected, so no runtime is needed.
    #[test]
    fn rolling_window() {
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);

        // Failures 1 and 2 restart; the third inside the window disables.
        let mut budget = Budget::default();
        assert!(!budget.exhausted(at(0)));
        assert!(!budget.exhausted(at(1)));
        assert!(budget.exhausted(at(299)));

        // An older failure ages out, so the third failure overall still restarts.
        let mut budget = Budget::default();
        assert!(!budget.exhausted(at(0)));
        assert!(!budget.exhausted(at(200)));
        assert!(!budget.exhausted(at(301)));
        // ...until three again fall inside one window.
        assert!(budget.exhausted(at(350)));

        // A failure exactly one window old no longer counts.
        let mut budget = Budget::default();
        assert!(!budget.exhausted(at(0)));
        assert!(!budget.exhausted(at(10)));
        assert!(!budget.exhausted(at(300)));
        assert!(budget.exhausted(at(309)));
    }
}
