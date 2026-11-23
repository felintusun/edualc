//! The usage panel: each program's limits, and what the session used.

use crate::app::{LimitUse, ProviderUsageState, Status, UsagePanel};
use crate::external::wire::{self, ContextKind, LimitKind};
use crate::present::Frontend;
use crate::present::model::{StatRow, StatSection, UsageTab};
use crate::provider::Backend;

use super::tasks::short_count;
use super::{Presenter, index_for_slint, stat};

/// Who answers, for people: Claude Code names them by its own ids, and one this window doesn't
/// know stays as written.
pub(super) fn provider_name(provider: &str) -> &str {
    match provider {
        "firstParty" => "Anthropic",
        "bedrock" => "Amazon Bedrock",
        "vertex" => "Google Vertex AI",
        "foundry" => "Microsoft Foundry",
        other => other,
    }
}

pub(super) fn section(title: &str, note: String, rows: Vec<StatRow>) -> StatSection {
    StatSection {
        title: title.into(),
        note,
        rows,
    }
}

pub(super) fn limit_row(label: &str, limit: LimitUse) -> StatRow {
    let resets = limit
        .resets_at
        .map_or_else(String::new, |at| reset_text(at, true));
    stat(
        label,
        format!("{:.0}%", limit.used * 100.0),
        resets,
        limit.used,
    )
}

pub(super) fn usage_tab(backend: Backend, state: &ProviderUsageState, status: &Status) -> UsageTab {
    let mut sections = vec![limits_section(backend, state.report.as_ref(), status)];
    if backend == status.backend {
        if let Some(session) = state
            .report
            .as_ref()
            .and_then(|report| report.session.as_ref())
        {
            sections.push(session_section(session));
        }
        if let Some(context) = &state.context {
            sections.push(context_section(context));
        }
    }
    if let Some(report) = &state.report {
        let periods = [
            ("The last 24 hours", &report.behaviors.day),
            ("The last 7 days", &report.behaviors.week),
        ];
        sections.extend(
            periods
                .into_iter()
                .filter_map(|(title, period)| Some(period_section(title, period.as_ref()?))),
        );
    }
    // A section with nothing behind it says nothing.
    sections.retain(|section| !section.rows.is_empty() || !section.note.is_empty());
    UsageTab {
        id: <&'static str>::from(backend).to_owned(),
        label: backend.program().to_owned(),
        available: state.available,
        loading: state.waiting > 0,
        note: state.error.clone().unwrap_or_default(),
        sections,
    }
}

/// The plan's limits: every one the CLI's `/usage` lists, or the two its status events
/// carry until it answers.
pub(super) fn limits_section(
    backend: Backend,
    report: Option<&wire::UsageReport>,
    status: &Status,
) -> StatSection {
    let rows: Vec<StatRow> = match report {
        Some(report) if !report.rate_limits.limits.is_empty() => report
            .rate_limits
            .limits
            .iter()
            .map(|limit| {
                let model = limit
                    .scope
                    .as_ref()
                    .and_then(|scope| scope.model.as_ref())
                    .and_then(|model| model.display_name.as_deref());
                let label = limit
                    .label
                    .clone()
                    .unwrap_or_else(|| match (limit.kind, model) {
                        (LimitKind::Session, _) => "This session window (5 hours)".to_owned(),
                        (LimitKind::WeeklyAll, _) => "This week, all models".to_owned(),
                        (LimitKind::WeeklyScoped | LimitKind::Other, Some(model)) => {
                            format!("This week, {model}")
                        }
                        (LimitKind::WeeklyScoped, None) => "This week, one model".to_owned(),
                        (LimitKind::Other, None) => "Another limit".to_owned(),
                    });
                limit_row(&label, LimitUse::from(limit))
            })
            .collect(),
        Some(_) | None => {
            let limits = status.limits.for_backend(backend).unwrap_or_default();
            [
                ("This session window (5 hours)", limits.five_hour),
                ("This week", limits.seven_day),
            ]
            .into_iter()
            .filter_map(|(label, limit)| Some(limit_row(label, limit?)))
            .collect()
        }
    };
    // The plan as Claude Code names it ("max"), its first letter raised: "Max plan".
    let plan = if backend == Backend::Claude {
        status.claude_subscription.as_str()
    } else {
        ""
    };
    let note = plan.chars().next().map_or_else(String::new, |first| {
        let rest = plan.get(first.len_utf8()..).unwrap_or_default();
        format!("{}{rest} plan", first.to_uppercase())
    });
    section("Plan limits", note, rows)
}

pub(super) fn session_section(session: &wire::SessionUsage) -> StatSection {
    let mut rows = vec![
        stat(
            "Cost at API prices",
            format!("${:.2}", session.total_cost_usd),
            "What the same use would cost on the API; a plan covers it".to_owned(),
            -1.0,
        ),
        stat(
            "Time with the model",
            duration_text(session.total_api_duration_ms),
            format!("of {} open", duration_text(session.total_duration_ms)),
            -1.0,
        ),
        stat(
            "Lines changed",
            format!(
                "+{}  −{}",
                session.total_lines_added, session.total_lines_removed
            ),
            String::new(),
            -1.0,
        ),
    ];
    let mut models: Vec<(&String, &wire::ModelTokens)> = session.model_usage.iter().collect();
    models.sort_by(|left, right| left.0.cmp(right.0));
    rows.extend(models.into_iter().map(|(model, tokens)| {
        stat(
            model,
            format!("${:.2}", tokens.cost_usd),
            format!(
                "{} in · {} out · {} read from cache · {} written to it",
                short_count(tokens.input_tokens),
                short_count(tokens.output_tokens),
                short_count(tokens.cache_read_input_tokens),
                short_count(tokens.cache_creation_input_tokens),
            ),
            -1.0,
        )
    }));
    section("This session", String::new(), rows)
}

pub(super) fn context_section(context: &wire::ContextReport) -> StatSection {
    let rows = context
        .categories
        .iter()
        .map(|category| {
            let detail = match category.kind {
                ContextKind::Deferred => "Loaded only when needed; not in use now",
                ContextKind::Buffer => "Kept free so the conversation can be compacted",
                ContextKind::Used | ContextKind::Free | ContextKind::Other => "",
            };
            stat(
                &category.name,
                short_count(category.tokens),
                detail.to_owned(),
                fraction_of(category.tokens, context.max_tokens),
            )
        })
        .collect();
    let note = format!(
        "{} of {} tokens in use",
        short_count(context.total_tokens),
        short_count(context.max_tokens)
    );
    section("Context", note, rows)
}

pub(super) fn period_section(title: &str, period: &wire::Period) -> StatSection {
    let mut rows = vec![
        stat(
            "Requests",
            period.request_count.to_string(),
            String::new(),
            -1.0,
        ),
        stat(
            "Sessions",
            period.session_count.to_string(),
            String::new(),
            -1.0,
        ),
    ];
    let shares = |label: &str, shares: &[wire::Share]| {
        (!shares.is_empty()).then(|| {
            let listed: Vec<String> = shares
                .iter()
                .map(|share| format!("{} {:.0}%", share.name, share.pct))
                .collect();
            stat(label, String::new(), listed.join(", "), -1.0)
        })
    };
    rows.extend(shares("Agents", &period.agents));
    rows.extend(shares("Skills", &period.skills));
    // Claude Code's own advice, with the share of requests it is about.
    rows.extend(period.recommendations.iter().map(|advice| {
        stat(
            &advice.title,
            format!("{:.0}% of requests", advice.pct),
            advice.detail.clone(),
            -1.0,
        )
    }));
    section(title, String::new(), rows)
}

/// When a limit starts over: "resets 15:00", or with the time left, "resets in 4h 58m (15:00)".
pub(super) fn reset_text(at: jiff::Timestamp, with_time_left: bool) -> String {
    let seconds = at.as_second() - jiff::Timestamp::now().as_second();
    let clock = clock(at);
    if !with_time_left {
        return format!("resets {clock}");
    }
    let left = match seconds {
        ..=0 => return "resets now".to_owned(),
        1..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600),
    };
    format!("resets in {left} ({clock})")
}

/// A time to come, on the local clock: "15:00" within a day, "Mon 15:00" later.
pub(crate) fn clock(at: jiff::Timestamp) -> String {
    let seconds = at.as_second() - jiff::Timestamp::now().as_second();
    let local = at.to_zoned(jiff::tz::TimeZone::system());
    if seconds < 86_400 {
        local.strftime("%H:%M").to_string()
    } else {
        local.strftime("%a %H:%M").to_string()
    }
}

/// A time span as [`duration_text`] writes it.
pub fn short_duration(duration: std::time::Duration) -> String {
    duration_text(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

/// "45s", "3m 12s", "1h 4m".
pub(super) fn duration_text(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// `used` of `total` as a share from 0 to 1, for a bar.
pub(super) fn fraction_of(used: u64, total: u64) -> f32 {
    let percent = used.saturating_mul(100) / total.max(1);
    f32::from(u16::try_from(percent.min(100)).unwrap_or(100)) / 100.0
}

impl<F: Frontend> Presenter<F> {
    /// The usage panel: tabs for every coding agent (`Claude Code`, `agy`, `opencode`) with plan
    /// limits, active session cost/time, context breakdown, and recent activity.
    pub fn render_usage(&mut self, panel: &UsagePanel, status: &Status) {
        let usage = &mut self.usage;
        usage.open = panel.open;
        if !panel.open {
            return;
        }
        let mut tabs = Vec::with_capacity(Backend::ALL.len());
        let mut active_index = 0_usize;
        for (idx, backend) in Backend::ALL.iter().copied().enumerate() {
            if backend == panel.active_backend {
                active_index = idx;
            }
            let state = panel.for_backend(backend);
            tabs.push(usage_tab(backend, state, status));
        }
        usage.active_tab = index_for_slint(active_index);
        if let Some(active) = tabs.get(active_index) {
            usage.program.clone_from(&active.label);
            usage.loading = active.loading;
            usage.note.clone_from(&active.note);
            usage.sections.clone_from(&active.sections);
        }
        usage.tabs = tabs;
    }
}
