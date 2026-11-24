//! The request bar: what a program asks, with its choices in its own words.

use std::collections::VecDeque;
use std::path::Path;

use crate::app::{Ask, AskKind, Mode};
use crate::call::{AcpCall, AcpOptionKind};
use crate::external::tool_input::{BashInput, IncomingMessageInput, WebFetchInput};
use crate::external::wire::{self, ToolKind, ToolName};
use crate::present::Frontend;
use crate::present::model::{PlanSection, PromptKind, Question, QuestionOption};
use crate::tools;

use super::transcript::{answered, md_block};
use super::{Presenter, index_for_slint};

/// Returns Claude's questions, with the options picked and what was written for each.
pub(super) fn questions(
    questions: &[wire::Question],
    picked: &[Vec<bool>],
    other: &[String],
) -> Vec<Question> {
    questions
        .iter()
        .zip(picked)
        .zip(other)
        .map(|((question, picks), other)| Question {
            question: question.question.clone(),
            header: question.header.clone(),
            answered: answered(picks, other),
            multi: question.multi_select,
            previews: question
                .options
                .iter()
                .any(|option| !option.preview.is_empty()),
            options: question
                .options
                .iter()
                .zip(picks)
                .map(|(option, &selected)| QuestionOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                    preview: option.preview.clone(),
                    selected,
                })
                .collect(),
        })
        .collect()
}

/// What the bar asks about a tool call, and what it shows of it.
pub(super) struct ToolPrompt {
    pub(super) title: String,
    pub(super) summary: String,
    /// Words (a message to another session, an agent's task) or code, as on the tool's card.
    pub(super) summary_reads_as: tools::Writing,
}

/// Returns what the bar asks about an ACP agent's call, by its kind, showing what the agent
/// says it does.
pub(super) fn acp_prompt(
    call: &AcpCall,
    cwd: &Path,
    backend: crate::provider::Backend,
) -> ToolPrompt {
    let view = tools::describe_acp(call, cwd, tools::Depth::Headline);
    let agent = backend.agent();
    let (title, summary_reads_as) = match (call.kind(), call.file()) {
        (ToolKind::Edit | ToolKind::Write, Some(file)) => (
            format!("Allow {agent} to edit {}?", tools::short_path(file, cwd)),
            tools::Writing::Prose,
        ),
        (ToolKind::Command, _) => (
            format!("Allow {agent} to run this command?"),
            tools::Writing::Code,
        ),
        (ToolKind::Web, _) => (
            format!("Allow {agent} to fetch this page?"),
            tools::Writing::Code,
        ),
        (
            ToolKind::Read
            | ToolKind::Edit
            | ToolKind::Write
            | ToolKind::Search
            | ToolKind::Agent
            | ToolKind::Other,
            _,
        ) => (format!("Allow {agent} to go ahead?"), tools::Writing::Prose),
    };
    let summary = [view.title, view.summary]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("  ");
    ToolPrompt {
        title,
        summary,
        summary_reads_as,
    }
}

pub(super) fn tool_prompt(
    tool: &ToolName,
    input: &serde_json::Value,
    cwd: &Path,
    backend: crate::provider::Backend,
) -> ToolPrompt {
    let view = tools::describe(tool, input, cwd, tools::Depth::Headline);
    let agent = backend.agent();
    let (title, summary) = match (tool, tool.kind()) {
        (ToolName::EdualcIncomingMessage, _) => {
            let incoming = tools::read::<IncomingMessageInput>(tool, input);
            let from = incoming
                .as_ref()
                .map_or("Another session", |incoming| incoming.from.as_str());
            (
                format!("Let this message from {from} in?"),
                incoming.map_or_default(|incoming| incoming.message),
            )
        }
        (ToolName::Bash, _) => {
            let bash = tools::read::<BashInput>(tool, input);
            // Claude can ask for a command to run outside Claude Code's own sandbox.
            let outside = bash
                .as_ref()
                .is_some_and(|bash| bash.outside_sandbox == Some(true));
            let title = if outside {
                format!("Allow {agent} to run this command outside Claude Code's sandbox?")
            } else {
                format!("Allow {agent} to run this command?")
            };
            (title, bash.map_or_default(|bash| bash.command))
        }
        (ToolName::WebFetch, _) => (
            format!("Allow {agent} to fetch this page?"),
            tools::read::<WebFetchInput>(tool, input).map_or_default(|fetch| fetch.url),
        ),
        (_, ToolKind::Edit | ToolKind::Write) => (
            format!("Allow {agent} to edit {}?", view.summary),
            String::new(),
        ),
        (
            _,
            ToolKind::Read
            | ToolKind::Command
            | ToolKind::Search
            | ToolKind::Web
            | ToolKind::Agent
            | ToolKind::Other,
        ) => (
            format!("Allow {agent} to use {}?", view.title),
            view.summary,
        ),
    };
    ToolPrompt {
        title,
        summary,
        summary_reads_as: view.summary_reads_as,
    }
}

/// Labels the button that remembers a rule Claude Code suggests, in the words of Claude Code
/// 2.1.329's permission dialog: "Yes, and don't ask again for npm test commands in ~/code/app".
pub(super) fn always_label(rule: &wire::Suggestion, cwd: &Path) -> String {
    let incoming = ToolName::EdualcIncomingMessage.to_string();
    let bash = ToolName::Bash.to_string();
    let web_fetch = ToolName::WebFetch.to_string();
    match rule {
        // edualc's own offer, for a message waiting to come in: its sender, for the session.
        wire::Suggestion::AddRules { rules, .. }
            if let [rule] = rules.as_slice()
                && rule.tool_name == incoming =>
        {
            format!(
                "Let messages from {} in",
                rule.rule_content.as_deref().unwrap_or("this sender")
            )
        }
        // Commands, by the start they share: `npm test:*` is "npm test".
        wire::Suggestion::AddRules { rules, .. }
            if !rules.is_empty() && rules.iter().all(|rule| rule.tool_name == bash) =>
        {
            let starts: Option<Vec<&str>> = rules
                .iter()
                .map(|rule| {
                    let content = rule.rule_content.as_deref()?;
                    Some(
                        content
                            .strip_suffix(":*")
                            .or_else(|| content.strip_suffix(" *"))
                            .unwrap_or(content),
                    )
                })
                .collect();
            match starts {
                Some(starts) => format!(
                    "Yes, and don't ask again for {} commands in {}",
                    listed(&starts),
                    tools::home_relative(cwd)
                ),
                None => format!("Yes, and don't ask again for {}", rules_named(rules)),
            }
        }
        // A site, by its domain.
        wire::Suggestion::AddRules { rules, destination }
            if let [rule] = rules.as_slice()
                && rule.tool_name == web_fetch
                && let Some(domain) = rule
                    .rule_content
                    .as_deref()
                    .and_then(|content| content.strip_prefix("domain:")) =>
        {
            let everywhere = match destination {
                wire::RuleDestination::UserSettings => " in any project (user settings)",
                wire::RuleDestination::Session
                | wire::RuleDestination::LocalSettings
                | wire::RuleDestination::ProjectSettings
                | wire::RuleDestination::Other => "",
            };
            format!("Yes, and don't ask again for {domain}{everywhere}")
        }
        wire::Suggestion::AddRules { rules, .. } => {
            format!("Yes, and don't ask again for {}", rules_named(rules))
        }
        wire::Suggestion::SetMode {
            mode: Mode::Known(mode),
        } => format!(
            "Yes, and switch to {} for this session",
            mode.claude_code_words()
        ),
        wire::Suggestion::SetMode {
            mode: Mode::Unknown(mode),
        } => format!("Yes, and switch to {mode} for this session"),
        wire::Suggestion::AddDirectories { directories, .. } => {
            let folders: Vec<&str> = directories.iter().map(String::as_str).collect();
            format!(
                "Yes, and always allow access to {} from this project",
                listed(&folders)
            )
        }
        wire::Suggestion::Other => String::new(),
    }
}

/// Lists rules as Claude Code writes them, `Tool(content)`.
pub(super) fn rules_named(rules: &[wire::Rule]) -> String {
    let named: Vec<String> = rules
        .iter()
        .map(|rule| match &rule.rule_content {
            Some(content) => format!("{}({content})", rule.tool_name),
            None => rule.tool_name.clone(),
        })
        .collect();
    let named: Vec<&str> = named.iter().map(String::as_str).collect();
    listed(&named)
}

/// Lists items in a sentence as Claude Code does: "a", "a and b", "a, b, and c".
pub(super) fn listed(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

impl<F: Frontend> Presenter<F> {
    /// Shows the bar above the composer for the first request waiting.
    pub fn render_prompt(
        &mut self,
        asks: &VecDeque<Ask>,
        cwd: &Path,
        backend: crate::provider::Backend,
    ) {
        let Some(ask) = asks.front() else {
            self.prompt.kind = PromptKind::None;
            self.reviewing = Some(false);
            self.prompt_shown = None;
            return;
        };
        let han = self.han;
        let fresh = self.prompt_shown.as_ref() != Some(&ask.request);
        let prompt = &mut self.prompt;
        prompt.request = ask.request.to_string();
        prompt.queued = index_for_slint(asks.len() - 1);
        if fresh {
            prompt.serial = prompt.serial.wrapping_add(1);
        }
        match &ask.kind {
            AskKind::Questions {
                asked,
                picked,
                other,
                ..
            } => {
                prompt.kind = PromptKind::Question;
                let agent = backend.agent();
                prompt.title = if asked.questions.len() > 1 {
                    format!("{agent} has questions")
                } else {
                    format!("{agent} has a question")
                };
                prompt.questions = questions(&asked.questions, picked, other);
            }
            AskKind::Plan {
                input,
                parts,
                editing,
            } => {
                prompt.kind = PromptKind::Plan;
                prompt.title = format!(
                    "{}'s plan is ready. Comment on its parts, then approve it or keep planning.",
                    backend.agent()
                );
                let count = parts.iter().filter(|part| !part.comment.is_empty()).count();
                prompt.comment_count = index_for_slint(count);
                prompt.plan = parts
                    .iter()
                    .enumerate()
                    .map(|(index, part)| PlanSection {
                        block: md_block(&part.block, han),
                        excerpt: crate::plan::excerpt(&input.plan, part),
                        comment: part.comment.clone(),
                        editing: *editing == Some(index),
                    })
                    .collect();
                if fresh {
                    self.reviewing = Some(true);
                }
            }
            AskKind::Tool {
                tool,
                input,
                suggestions,
                description,
            } => {
                let ToolPrompt {
                    title,
                    summary,
                    summary_reads_as,
                } = tool_prompt(tool, input, cwd, backend);
                // The description often only repeats what the title or summary say.
                let repeats =
                    title.contains(description.as_str()) || summary.contains(description.as_str());
                let detail = if repeats { "" } else { description.as_str() };
                prompt.kind = PromptKind::Tool;
                prompt.title = title;
                prompt.summary = summary;
                prompt.summary_prose = summary_reads_as == tools::Writing::Prose;
                detail.clone_into(&mut prompt.detail);
                // Claude Code's dialog words for them.
                "Yes".clone_into(&mut prompt.allow_label);
                "No".clone_into(&mut prompt.deny_label);
                prompt.takes_feedback = true;
                // Only a rule the window can say in full is offered.
                prompt.always_label = suggestions
                    .first()
                    .and_then(|offered| offered.rule.as_ref())
                    .map_or_default(|rule| always_label(rule, cwd));
            }
            // The agent's choices, in its words.
            AskKind::Acp { call, options } => {
                let ToolPrompt {
                    title,
                    summary,
                    summary_reads_as,
                } = acp_prompt(call, cwd, backend);
                let named = |kinds: &[AcpOptionKind], otherwise: &str| {
                    crate::call::first_of(options, kinds)
                        .map_or_else(|| otherwise.to_owned(), |option| option.name.clone())
                };
                prompt.kind = PromptKind::Tool;
                prompt.title = title;
                prompt.summary = summary;
                prompt.summary_prose = summary_reads_as == tools::Writing::Prose;
                prompt.detail = String::new();
                prompt.allow_label = named(&[AcpOptionKind::AllowOnce], "Allow");
                prompt.always_label = named(&[AcpOptionKind::AllowAlways], "");
                prompt.deny_label = named(
                    &[AcpOptionKind::RejectOnce, AcpOptionKind::RejectAlways],
                    "Deny",
                );
                prompt.takes_feedback = false;
            }
        }
        self.prompt_shown = Some(ask.request.clone());
    }
}
