use std::time::{Duration, Instant};

pub struct ToolCallBudget {
    pub max: u32,
    pub used: u32,
}

impl ToolCallBudget {
    pub fn new(max: u32) -> Self {
        Self { max, used: 0 }
    }

    pub fn try_consume(&mut self) -> Result<(), String> {
        if self.used >= self.max {
            return Err(format!(
                "max_tool_calls exceeded ({}/{})",
                self.used, self.max
            ));
        }
        self.used += 1;
        Ok(())
    }

    pub fn used(&self) -> u32 {
        self.used
    }
}

pub struct RunDeadline {
    pub started: Instant,
    pub limit: Duration,
}

impl RunDeadline {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            started: Instant::now(),
            limit: Duration::from_secs(timeout_secs.max(1)),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.started.elapsed() > self.limit {
            Err(format!("agent run timeout after {}s", self.limit.as_secs()))
        } else {
            Ok(())
        }
    }

    pub fn remaining(&self) -> Duration {
        self.limit.saturating_sub(self.started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_up_to_max() {
        let mut b = ToolCallBudget::new(2);
        assert!(b.try_consume().is_ok());
        assert!(b.try_consume().is_ok());
        assert!(b.try_consume().is_err());
        assert_eq!(b.used(), 2);
    }

    #[test]
    fn budget_zero_blocks_immediately() {
        let mut b = ToolCallBudget::new(0);
        assert!(b.try_consume().is_err());
    }

    #[test]
    fn deadline_check_before_timeout() {
        let d = RunDeadline::new(60);
        assert!(d.check().is_ok());
    }

    #[test]
    fn deadline_remaining_is_positive() {
        let d = RunDeadline::new(60);
        assert!(d.remaining().as_secs() > 0);
    }
}
