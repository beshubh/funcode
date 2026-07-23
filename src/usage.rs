#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) reasoning_tokens: u64,
}

impl TokenUsage {
    pub(crate) const fn is_reported(self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.total_tokens != 0
            || self.reasoning_tokens != 0
    }

    pub(crate) const fn normalized_total(self) -> u64 {
        if self.total_tokens != 0 {
            self.total_tokens
        } else {
            self.input_tokens.saturating_add(self.output_tokens)
        }
    }
}

/// Cumulative provider usage and retained session context.
///
/// Provider calls are accumulated for eventual cost reporting. Retained
/// context is measured separately over a complete user request: the first
/// call supplies the request input and the final successful call supplies the
/// assistant output retained for the next request. Intermediate tool calls and
/// failed retry outputs must not replace the session context measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    reported_calls: u64,
    context_tokens: Option<u64>,
    request_context: RequestContextMeasurement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RequestContextMeasurement {
    first_input_tokens: Option<u64>,
    latest_retained_output_tokens: Option<u64>,
}

impl SessionUsage {
    pub(crate) fn begin_request(&mut self) {
        self.request_context = RequestContextMeasurement::default();
    }

    pub(crate) fn record(&mut self, usage: TokenUsage) {
        if !usage.is_reported() {
            return;
        }

        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.normalized_total());
        self.reported_calls = self.reported_calls.saturating_add(1);

        if self.request_context.first_input_tokens.is_none() && usage.input_tokens != 0 {
            self.request_context.first_input_tokens = Some(usage.input_tokens);
        }
        self.request_context.latest_retained_output_tokens =
            Some(usage.output_tokens.saturating_sub(usage.reasoning_tokens));
    }

    pub(crate) fn retry_request(&mut self) {
        self.request_context.latest_retained_output_tokens = None;
    }

    pub(crate) fn complete_request(&mut self) {
        if let Some(input_tokens) = self.request_context.first_input_tokens {
            let measured = input_tokens.saturating_add(
                self.request_context
                    .latest_retained_output_tokens
                    .unwrap_or_default(),
            );
            self.context_tokens = Some(measured);
        }
        self.request_context = RequestContextMeasurement::default();
    }

    pub(crate) fn abandon_request(&mut self) {
        self.request_context = RequestContextMeasurement::default();
    }

    pub(crate) fn fail_request(&mut self) {
        if let Some(input_tokens) = self.request_context.first_input_tokens {
            self.context_tokens = Some(input_tokens);
        }
        self.request_context = RequestContextMeasurement::default();
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
    fn session_usage_accumulates_calls_but_commits_context_once_per_request() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 120,
            output_tokens: 30,
            total_tokens: 150,
            reasoning_tokens: 0,
        });
        usage.record(TokenUsage {
            input_tokens: 240,
            output_tokens: 20,
            total_tokens: 260,
            reasoning_tokens: 0,
        });

        assert_eq!(usage.total_tokens(), Some(410));
        assert_eq!(usage.context_tokens(), None);

        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(140));
        assert_eq!(usage.context_utilization_percent(Some(1_000)), Some(14));
    }

    #[test]
    fn missing_provider_metrics_leave_usage_and_context_unknown() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage::default());
        usage.complete_request();

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
            reasoning_tokens: 0,
        });

        assert_eq!(usage.total_tokens(), Some(15));
    }

    #[test]
    fn partial_usage_without_input_keeps_the_previous_context_measurement() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
            reasoning_tokens: 0,
        });
        usage.complete_request();

        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 0,
            output_tokens: 20,
            total_tokens: 20,
            reasoning_tokens: 0,
        });
        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(110));
        assert_eq!(usage.context_utilization_percent(Some(1_000)), Some(11));
    }

    #[test]
    fn retry_discards_the_failed_attempt_context_sample() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 4_800,
            output_tokens: 200,
            total_tokens: 5_000,
            reasoning_tokens: 0,
        });

        usage.retry_request();
        usage.record(TokenUsage {
            input_tokens: 4_800,
            output_tokens: 20,
            total_tokens: 4_820,
            reasoning_tokens: 0,
        });
        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(4_820));
        assert_eq!(usage.total_tokens(), Some(9_820));
    }

    #[test]
    fn failed_request_retains_its_input_but_not_partial_output() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            reasoning_tokens: 0,
        });

        usage.fail_request();

        assert_eq!(usage.context_tokens(), Some(100));
        assert_eq!(usage.total_tokens(), Some(120));
    }

    #[test]
    fn a_completed_request_replaces_the_previous_context_measurement() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 4_800,
            output_tokens: 200,
            total_tokens: 5_000,
            reasoning_tokens: 0,
        });
        usage.complete_request();

        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 400,
            output_tokens: 100,
            total_tokens: 500,
            reasoning_tokens: 0,
        });
        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(500));
    }

    #[test]
    fn hidden_reasoning_is_not_counted_as_retained_session_context() {
        let mut usage = SessionUsage::default();
        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 8_000,
            output_tokens: 2_000,
            total_tokens: 10_000,
            reasoning_tokens: 1_000,
        });
        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(9_000));

        usage.begin_request();
        usage.record(TokenUsage {
            input_tokens: 9_000,
            output_tokens: 100,
            total_tokens: 9_100,
            reasoning_tokens: 70,
        });
        usage.complete_request();

        assert_eq!(usage.context_tokens(), Some(9_030));
    }
}
