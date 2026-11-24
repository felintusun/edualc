//! The agent's permission requests, and the window's answers to them.

use serde_json::Value;

use crate::agent::Event;
use crate::call::{AcpOption, Call, OptionId};
use crate::edualc_tool::EdualcTool;
use crate::external::acp::{self, OptionKind, PermissionAsked};
use crate::external::wire::{self, IncomingRequest, Rule, RuleDestination, Suggestion, ToolName};
use crate::ids::{RequestId, ToolUseId};

use super::calls::{Entry, drawn, merged};
use super::{Effect, Translator};

/// A permission request of the agent's that the window was asked.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Permission {
    id: Value,
    options: Vec<acp::PermissionOption>,
    tool: String,
}

impl Translator {
    /// Answers the agent with the option you chose. An id the agent didn't offer cancels the
    /// request rather than pick another.
    pub(super) fn window_chose(&mut self, request: &RequestId, option: &OptionId) -> Vec<Effect> {
        let Some(permission) = self.permissions.remove(request) else {
            return Vec::new();
        };
        let chosen = permission
            .options
            .iter()
            .find(|offered| offered.option_id == option.as_str());
        let outcome = match chosen {
            Some(chosen) => {
                let rejects = matches!(
                    chosen.kind,
                    OptionKind::RejectOnce | OptionKind::RejectAlways
                );
                if rejects && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool)
                {
                    shown.denied = true;
                }
                acp::Outcome::Selected {
                    option_id: chosen.option_id.clone(),
                }
            }
            None => {
                tracing::warn!(
                    option = option.as_str(),
                    "a choice the agent didn't offer: cancelled"
                );
                acp::Outcome::Cancelled
            }
        };
        vec![Self::answer(
            permission.id,
            acp::Reply::Result(acp::PermissionAnswer { outcome }),
        )]
    }

    /// Takes the window's answer to a permission request, the agent's or the messaging tool's.
    pub(super) fn window_answered(&mut self, reply: wire::Reply) -> Vec<Effect> {
        let request = match &reply {
            wire::Reply::Success { request_id, .. } | wire::Reply::Error { request_id, .. } => {
                request_id
            }
        };
        if let Some(form) = self.forms.remove(request) {
            return vec![Self::form_answered(form, reply)];
        }
        if let Some(content) = self.waiting.remove(request) {
            return match reply {
                wire::Reply::Success {
                    response:
                        wire::PermissionResult::Allow {
                            updated_permissions,
                            ..
                        },
                    ..
                } => self.let_in(content, &updated_permissions),
                // Turned down: the agent never sees it, and its turn goes on.
                wire::Reply::Success {
                    response: wire::PermissionResult::Deny { .. },
                    ..
                } => Vec::new(),
                wire::Reply::Error { error, .. } => {
                    tracing::warn!(%error, "the window couldn't ask about a message: turned down");
                    Vec::new()
                }
            };
        }
        let (request_id, verdict) = match reply {
            wire::Reply::Success {
                request_id,
                response:
                    wire::PermissionResult::Allow {
                        updated_permissions,
                        ..
                    },
            } => (
                request_id,
                Verdict::Allow {
                    always: !updated_permissions.is_empty(),
                },
            ),
            wire::Reply::Success {
                request_id,
                response: wire::PermissionResult::Deny { interrupt, .. },
            } => (request_id, Verdict::Deny { interrupt }),
            wire::Reply::Error { request_id, error } => {
                tracing::warn!(%error, "the window couldn't answer a permission request: cancelled");
                (request_id, Verdict::Unanswered)
            }
        };
        let Some(permission) = self.permissions.remove(&request_id) else {
            return Vec::new();
        };
        let outcome = match verdict {
            Verdict::Allow { always: true } => outcome_of_any(
                &permission.options,
                &[OptionKind::AllowAlways, OptionKind::AllowOnce],
            ),
            Verdict::Allow { always: false } => {
                outcome_of(&permission.options, OptionKind::AllowOnce)
            }
            Verdict::Deny { .. } => outcome_of(&permission.options, OptionKind::RejectOnce),
            Verdict::Unanswered => acp::Outcome::Cancelled,
        };
        if let Verdict::Deny { .. } = verdict
            && let Some(Entry::Shown(shown)) = self.entries.get_mut(&permission.tool)
        {
            shown.denied = true;
        }
        let mut effects = vec![Self::answer(
            permission.id,
            acp::Reply::Result(acp::PermissionAnswer { outcome }),
        )];
        if let Verdict::Deny { interrupt: true } = verdict
            && let Some(session) = self.acp_session()
        {
            effects.push(Self::cancel(session.to_owned()));
        }
        effects
    }

    /// Asks you in the window whether the agent's tool call may go ahead.
    pub(super) fn asked_permission(&mut self, id: Value, asked: PermissionAsked) -> Vec<Effect> {
        let tool = asked.tool_call.tool_call_id.clone();
        let mut effects = Vec::new();
        match self.entries.get(&tool) {
            Some(Entry::Kept(kept)) => {
                let call = merged(kept, &asked.tool_call);
                effects.extend(self.show_call(&call));
            }
            Some(Entry::Shown(_)) => {}
            None => effects.extend(self.show_call(&asked.tool_call)),
        }
        let drawn = match self.entries.get(&tool) {
            Some(Entry::Shown(shown)) => shown.drawn.clone(),
            Some(Entry::Kept(_)) | None => drawn(self.agent, &asked.tool_call),
        };
        let (name, input) = match drawn {
            Call::Named { name, input } => (name, input),
            // Asked about as the agent asks: the call, and the agent's choices.
            Call::Acp(call) => {
                let request = RequestId::from(self.local_id("permission"));
                let options = asked.options.iter().cloned().map(AcpOption::from).collect();
                self.permissions.insert(
                    request.clone(),
                    Permission {
                        id,
                        options: asked.options,
                        tool: tool.clone(),
                    },
                );
                effects.push(Effect::ToWindow(Event::PermissionAsked {
                    request_id: request,
                    tool_use_id: ToolUseId::from(tool),
                    call,
                    options,
                }));
                return effects;
            }
        };
        // The agent asks before every MCP tool, in every mode. edualc's own go without asking,
        // as a Claude Code session's messages do.
        if matches!(
            name,
            ToolName::Edualc(EdualcTool::SendMessage | EdualcTool::ListSessions)
        ) {
            let outcome = outcome_of(&asked.options, OptionKind::AllowOnce);
            effects.push(Self::answer(
                id,
                acp::Reply::Result(acp::PermissionAnswer { outcome }),
            ));
            return effects;
        }
        let permission_suggestions = asked
            .options
            .iter()
            .any(|option| option.kind == OptionKind::AllowAlways)
            .then(|| {
                serde_json::to_value(Suggestion::AddRules {
                    rules: vec![Rule {
                        tool_name: name.to_string(),
                        rule_content: None,
                    }],
                    destination: RuleDestination::Session,
                })
                .ok()
            })
            .flatten()
            .into_iter()
            .collect();
        let request = RequestId::from(self.local_id("permission"));
        self.permissions.insert(
            request.clone(),
            Permission {
                id,
                options: asked.options,
                tool: tool.clone(),
            },
        );
        effects.push(Effect::ToWindow(Event::ControlRequest {
            request_id: request,
            request: IncomingRequest::CanUseTool {
                tool_name: name,
                input,
                permission_suggestions,
                description: asked.tool_call.title.unwrap_or_default(),
                tool_use_id: Some(ToolUseId::from(tool)),
                suppress_always_allow_rule: false,
            },
        }));
        effects
    }
}

/// The window's answer to a permission request, as edualc acts on it.
enum Verdict {
    Allow {
        always: bool,
    },
    Deny {
        interrupt: bool,
    },
    /// The window couldn't answer, so the request is cancelled.
    Unanswered,
}

/// Returns the first option of `wanted`'s kind, if the agent offered one.
fn outcome_of(options: &[acp::PermissionOption], wanted: OptionKind) -> acp::Outcome {
    outcome_of_any(options, &[wanted])
}

/// Returns the first option of any kind in `wanted`, in that order, else `Cancelled`.
fn outcome_of_any(options: &[acp::PermissionOption], wanted: &[OptionKind]) -> acp::Outcome {
    wanted
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .map_or(acp::Outcome::Cancelled, |option| acp::Outcome::Selected {
            option_id: option.option_id.clone(),
        })
}
