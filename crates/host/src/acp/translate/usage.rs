//! What the session uses: the plan's quotas, its limits, the context.

use crate::agent::Event;
use crate::external::agy::{self, QuotaWindow};
use crate::external::wire::{self, LimitKind};
use crate::ids::RequestId;

use super::{Effect, Translator, failure, success_as};

/// How much of the model's context window the session uses, and the window's size, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContextUsed {
    pub(super) used: u64,
    pub(super) size: u64,
}

impl Translator {
    pub(super) fn active_model_name(&self) -> Option<&str> {
        self.model.as_deref().and_then(|id| {
            self.models
                .iter()
                .find(|model| model.model_id == id)
                .map(|model| model.name.as_str())
        })
    }

    /// Takes the plan's quotas, to answer the window's `get_usage` or to update the session's
    /// bars from the quotas the model draws on.
    pub fn quotas_told(
        &mut self,
        request: Option<RequestId>,
        told: Result<agy::Quotas, crate::acp::QuotaError>,
    ) -> Vec<Effect> {
        let quotas = match told {
            Ok(quotas) => quotas,
            Err(error) => {
                tracing::warn!(%error, agent = %self.agent, "the plan's quotas couldn't be read");
                return request
                    .map(|request| {
                        failure(
                            request,
                            &format!("{} couldn't say its plan's quotas: {error}", self.agent),
                        )
                    })
                    .into_iter()
                    .collect();
            }
        };
        let limits = plan_limits(&quotas, self.active_model_name());
        self.last_quotas = Some(quotas);
        match request {
            Some(request) => vec![success_as(
                request,
                &wire::UsageReport {
                    rate_limits: wire::RateLimitReport { limits },
                    ..wire::UsageReport::default()
                },
            )],
            None => vec![Effect::ToWindow(Event::RateLimits(wire::RateLimits {
                windows: limit_windows(&limits),
            }))],
        }
    }
}

pub(super) fn is_quota_exhausted(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("quota reached") || lower.contains("quota exceeded")
}

/// Returns the context's use as `get_context_usage` answers it: the conversation, and what is
/// free.
pub(super) fn context_report(context: ContextUsed) -> wire::ContextReport {
    let free = context.size.saturating_sub(context.used);
    wire::ContextReport {
        categories: [
            ("Conversation", context.used, wire::ContextKind::Used),
            ("Free space", free, wire::ContextKind::Free),
        ]
        .into_iter()
        .filter(|(_, tokens, _)| *tokens > 0)
        .map(|(name, tokens, kind)| wire::ContextCategory {
            name: name.to_owned(),
            tokens,
            kind,
        })
        .collect(),
        total_tokens: context.used,
        max_tokens: context.size,
    }
}

/// Returns the share of a quota used, from 0 to 1.
fn quota_used(quota: &agy::Quota) -> f32 {
    (1.0 - quota.remaining_fraction).clamp(0.0, 1.0)
}

/// Returns agy's quotas as the plan's limits, each named by its group. The group `model` draws
/// on gives the session's five hours and week, which the sidebar shows.
pub(crate) fn plan_limits(quotas: &agy::Quotas, model: Option<&str>) -> Vec<wire::PlanLimit> {
    quotas
        .groups
        .iter()
        .flat_map(|group| {
            let drawn_on = model.is_some_and(|model| group.covers(model));
            group.quotas.iter().map(move |quota| {
                let (kind, span) = match (quota.window, drawn_on) {
                    (QuotaWindow::FiveHours, true) => (LimitKind::Session, "5 hours"),
                    (QuotaWindow::Week, true) => (LimitKind::WeeklyAll, "this week"),
                    (QuotaWindow::FiveHours, false) => (LimitKind::Other, "5 hours"),
                    (QuotaWindow::Week, false) => (LimitKind::Other, "this week"),
                    (QuotaWindow::Other, _) => (LimitKind::Other, "another span"),
                };
                wire::PlanLimit {
                    kind,
                    percent: quota_used(quota) * 100.0,
                    resets_at: Some(quota.reset_time),
                    scope: None,
                    label: Some(format!("{}, {span}", group.name)),
                }
            })
        })
        .collect()
}

/// Returns the sidebar's bars, the session's five hours and its week, from the plan's limits.
pub(super) fn limit_windows(limits: &[wire::PlanLimit]) -> wire::LimitWindows {
    let window = |kind| {
        limits
            .iter()
            .find(|limit| limit.kind == kind)
            .map(|limit| wire::LimitWindow {
                utilization: Some(limit.percent / 100.0),
                resets_at: limit.resets_at,
            })
    };
    wire::LimitWindows {
        five_hour: window(LimitKind::Session),
        seven_day: window(LimitKind::WeeklyAll),
    }
}
