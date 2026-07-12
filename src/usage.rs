#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl TokenUsage {
    pub(crate) const fn is_reported(self) -> bool {
        self.input_tokens != 0 || self.output_tokens != 0 || self.total_tokens != 0
    }

    pub(crate) const fn normalized_total(self) -> u64 {
        if self.total_tokens != 0 {
            self.total_tokens
        } else {
            self.input_tokens.saturating_add(self.output_tokens)
        }
    }
}

/// Usage accumulated only from provider-reported completion snapshots.
///
/// `context_tokens` is the input plus output size of the most recent provider
/// completion. It approximates the context available to the next call, unlike
/// the session totals, which include repeated conversation history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    reported_calls: u64,
    context_tokens: Option<u64>,
}

impl SessionUsage {
    pub(crate) fn record(&mut self, usage: TokenUsage) {
        if !usage.is_reported() {
            return;
        }

        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.normalized_total());
        self.reported_calls = self.reported_calls.saturating_add(1);
        self.context_tokens = (usage.input_tokens != 0).then_some(usage.normalized_total());
    }

    #[cfg(test)]
    pub(crate) const fn total_tokens(self) -> Option<u64> {
        if self.reported_calls != 0 {
            Some(self.total_tokens)
        } else {
            None
        }
    }

    pub(crate) const fn context_tokens(self) -> Option<u64> {
        self.context_tokens
    }

    pub(crate) fn context_utilization_percent(self, context_window: Option<u64>) -> Option<u8> {
        let context_tokens = self.context_tokens?;
        let context_window = context_window.filter(|window| *window != 0)?;
        let percent = context_tokens.saturating_mul(100) / context_window;
        Some(percent.min(100) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionUsage, TokenUsage};

    #[test]
    fn session_usage_accumulates_calls_and_uses_the_latest_total_for_context() {
        let mut usage = SessionUsage::default();
        usage.record(TokenUsage {
            input_tokens: 120,
            output_tokens: 30,
            total_tokens: 150,
        });
        usage.record(TokenUsage {
            input_tokens: 240,
            output_tokens: 20,
            total_tokens: 260,
        });

        assert_eq!(usage.total_tokens(), Some(410));
        assert_eq!(usage.context_tokens(), Some(260));
        assert_eq!(usage.context_utilization_percent(Some(1_000)), Some(26));
    }

    #[test]
    fn missing_provider_metrics_leave_usage_and_context_unknown() {
        let mut usage = SessionUsage::default();
        usage.record(TokenUsage::default());

        assert_eq!(usage.total_tokens(), None);
        assert_eq!(usage.context_tokens(), None);
        assert_eq!(usage.context_utilization_percent(Some(1_000)), None);
    }

    #[test]
    fn a_provider_without_total_uses_the_reported_input_and_output_sum() {
        let mut usage = SessionUsage::default();
        usage.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 0,
        });

        assert_eq!(usage.total_tokens(), Some(15));
    }

    #[test]
    fn partial_usage_without_input_clears_the_previous_context_measurement() {
        let mut usage = SessionUsage::default();
        usage.record(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
        });
        usage.record(TokenUsage {
            input_tokens: 0,
            output_tokens: 20,
            total_tokens: 20,
        });

        assert_eq!(usage.context_tokens(), None);
        assert_eq!(usage.context_utilization_percent(Some(1_000)), None);
    }
}
