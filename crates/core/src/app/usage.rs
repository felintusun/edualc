//! The usage panel (the CLI's `/usage` and `/context`).

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{App, LimitUse, Reading, Request};
use crate::external::wire::{ContextReport, ControlRequest, LimitKind, UsageReport};
use crate::ipc::{ProviderUsage, Read};
use crate::present::Frontend;
use crate::provider::Backend;

impl<F: Frontend> App<F> {
    pub(super) fn show_usage(&mut self) {
        if !self.usage.open {
            self.usage.active_backend = self.status.backend;
        }
        self.usage.open = true;
        self.redraw.usage = true;
        self.ask_usage();
    }

    pub(super) fn refresh_usage(&mut self) {
        self.usage.open = true;
        self.redraw.usage = true;
        self.ask_usage();
    }

    pub(super) fn select_usage_tab(&mut self, index: usize) {
        if let Some(&backend) = Backend::ALL.get(index) {
            self.usage.active_backend = backend;
            self.redraw.usage = true;
        }
    }

    /// Queries usage for all coding agent providers: the active session's own process when one runs
    /// here, and on-demand daemon probes for every available provider otherwise.
    pub(super) fn ask_usage(&mut self) {
        let cwd = self.location.cwd().map(Path::to_path_buf);
        for backend in Backend::ALL {
            let live_here =
                self.watch.is_none() && self.process.is_some() && self.status.backend == backend;
            let available = self.available_backends.contains(&backend)
                || (self.status.backend == backend
                    && (self.process.is_some() || self.watch.is_some()));
            {
                let state = self.usage.for_backend_mut(backend);
                state.available = available;
                state.error = None;
                state.waiting = 0;
            }
            let reports_plan_limits = backend
                .capabilities(crate::provider::SignInWith::Account)
                .plan_limits;
            if live_here {
                let mut sent = 0_usize;
                if reports_plan_limits
                    && self.request(ControlRequest::GetUsage, Request::Usage(backend))
                {
                    sent += 1;
                }
                if self.request(
                    ControlRequest::GetContextUsage,
                    Request::ContextUsage(backend),
                ) {
                    sent += 1;
                }
                let state = self.usage.for_backend_mut(backend);
                state.waiting = sent;
                if !reports_plan_limits {
                    state.error = Some(format!(
                        "{} doesn't report usage or plan limits.",
                        backend.program()
                    ));
                } else if sent == 0 {
                    state.error = Some(format!("{} isn't running to ask.", backend.program()));
                }
            } else if !available {
                self.usage.for_backend_mut(backend).error =
                    Some(format!("{} isn't installed on PATH.", backend.program()));
            } else if !reports_plan_limits {
                self.usage.for_backend_mut(backend).error = Some(format!(
                    "{} doesn't report usage or plan limits.",
                    backend.program()
                ));
            } else {
                self.usage.for_backend_mut(backend).waiting = 1;
                self.read(
                    Read::ProviderUsage {
                        backend,
                        cwd: cwd.clone(),
                    },
                    Reading::ProviderUsage { backend },
                );
            }
        }
    }

    pub(super) fn usage_answered(&mut self, backend: Backend, outcome: Result<Value, String>) {
        let report = self.usage_answer::<UsageReport>(backend, outcome);
        if let Some(report) = &report {
            self.take_plan_limits_for(backend, report);
        }
        if backend == self.status.backend {
            self.limits_reported(report.as_ref());
        }
        if let Some(report) = report {
            self.usage.for_backend_mut(backend).report = Some(report);
        }
    }

    pub(super) fn context_answered(&mut self, backend: Backend, outcome: Result<Value, String>) {
        if let Some(report) = self.usage_answer::<ContextReport>(backend, outcome) {
            self.usage.for_backend_mut(backend).context = Some(report);
        }
    }

    pub(super) fn provider_usage_read(&mut self, backend: Backend, usage: ProviderUsage) {
        let state = self.usage.for_backend_mut(backend);
        state.waiting = state.waiting.saturating_sub(1);
        match usage {
            ProviderUsage::Unavailable => {
                state.available = false;
                state.error = Some(format!("{} isn't installed on PATH.", backend.program()));
            }
            ProviderUsage::Available { report, note } => {
                state.available = true;
                state.error = note;
                if let Some(report) = report.into_iter().next() {
                    self.take_plan_limits_for(backend, &report);
                    if backend == self.status.backend {
                        self.limits_reported(Some(&report));
                    }
                    self.usage.for_backend_mut(backend).report = Some(report);
                }
            }
        }
        self.redraw.usage = true;
    }

    /// One of the panel's answers, or why there is none.
    fn usage_answer<T: DeserializeOwned>(
        &mut self,
        backend: Backend,
        outcome: Result<Value, String>,
    ) -> Option<T> {
        let state = self.usage.for_backend_mut(backend);
        state.waiting = state.waiting.saturating_sub(1);
        self.redraw.usage = true;
        match super::answer_as(outcome) {
            Ok(answer) => Some(answer),
            Err(error) => {
                tracing::warn!(%error, backend = backend.program(), "no usage numbers");
                self.usage.for_backend_mut(backend).error = Some(error.to_string());
                None
            }
        }
    }

    /// The session's quota bars from the fuller answer: five-hour and weekly windows for the
    /// active model/plan. Clears stale windows when a full report omits them (e.g., weekly-only
    /// models like Argon).
    fn take_plan_limits_for(&mut self, backend: Backend, report: &UsageReport) {
        let Some(kept) = self.status.limits.mut_for_backend(backend) else {
            return;
        };
        if report.rate_limits.limits.is_empty() {
            return;
        }
        let mut five_hour = None;
        let mut seven_day = None;
        for limit in &report.rate_limits.limits {
            let used = LimitUse::from(limit);
            match limit.kind {
                LimitKind::Session => five_hour = Some(used),
                LimitKind::WeeklyAll => seven_day = Some(used),
                LimitKind::WeeklyScoped | LimitKind::Other => {}
            }
        }
        kept.five_hour = five_hour;
        kept.seven_day = seven_day;
    }
}
