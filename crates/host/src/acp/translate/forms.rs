//! An agent's forms, asked in the window as Claude Code's questions are.

use serde_json::{Map, Value};

use crate::agent::Event;
use crate::external::acp;
use crate::external::wire::{self, IncomingRequest, ToolName};
use crate::ids::{RequestId, ToolUseId};

use super::{Effect, Translator};

/// A form of the agent's, asked in the window as Claude Code's questions are.
#[derive(Debug)]
pub(super) struct Form {
    /// The agent's request, answered with the form filled in.
    id: Value,
    questions: Vec<FormQuestion>,
}

/// A question of a form, and the fields its answer goes to.
#[derive(Debug, PartialEq, Eq)]
struct FormQuestion {
    question: String,
    field: String,
    takes: Takes,
    options: Vec<acp::FormOption>,
    /// The field for an answer in words, where the options don't hold it.
    other: Option<String>,
}

/// What a question's field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    OneOption,
    Options,
    Text,
}

impl Translator {
    /// Answers a form of the agent's under the id it came with.
    pub(super) fn form_answer(id: Value, answer: acp::ElicitationAnswer) -> Effect {
        Effect::ToAgent(acp::ToAgent::Elicited(acp::Answer {
            jsonrpc: acp::JsonRpc::TwoPointZero,
            id,
            reply: acp::Reply::Result(answer),
        }))
    }

    /// Asks a form's questions in the window, as Claude Code's AskUserQuestion does, on the
    /// tool call the form is for.
    pub(super) fn asked_form(&mut self, id: Value, form: acp::Elicitation) -> Vec<Effect> {
        let questions = form_questions(&form.requested_schema);
        if form.mode != acp::ElicitationMode::Form || questions.is_empty() {
            tracing::warn!(mode = ?form.mode, message = %form.message, "a form edualc can't ask: declined");
            return vec![Self::form_answer(id, acp::ElicitationAnswer::Decline)];
        }
        let asked = wire::Questions {
            questions: questions
                .iter()
                .map(|question| wire::Question {
                    question: question.question.clone(),
                    header: String::new(),
                    options: question
                        .options
                        .iter()
                        .map(|option| wire::QuestionOption {
                            label: option.title.clone(),
                            description: String::new(),
                            preview: String::new(),
                            rest: Map::new(),
                        })
                        .collect(),
                    multi_select: question.takes == Takes::Options,
                    rest: Map::new(),
                })
                .collect(),
            answers: std::collections::BTreeMap::new(),
            annotations: std::collections::BTreeMap::new(),
            rest: Map::new(),
        };
        let input = match serde_json::to_value(&asked) {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "a form's questions can't be written: cancelled");
                return vec![Self::form_answer(id, acp::ElicitationAnswer::Cancel)];
            }
        };
        let request = RequestId::from(self.local_id("form"));
        self.forms.insert(request.clone(), Form { id, questions });
        vec![Effect::ToWindow(Event::ControlRequest {
            request_id: request,
            request: IncomingRequest::CanUseTool {
                tool_name: ToolName::AskUserQuestion,
                input,
                permission_suggestions: Vec::new(),
                description: form.message,
                tool_use_id: form.tool_call_id.map(ToolUseId::from),
                suppress_always_allow_rule: false,
            },
        })]
    }

    /// Turns the window's answers to a form's questions into the form filled in, or declined.
    pub(super) fn form_answered(form: Form, reply: wire::Reply) -> Effect {
        let answer = match reply {
            wire::Reply::Success {
                response: wire::PermissionResult::Allow { updated_input, .. },
                ..
            } => {
                let given = match updated_input {
                    wire::ToolInput::Answered(questions) => Some(questions.answers),
                    wire::ToolInput::AsAsked(input) => {
                        serde_json::from_value::<wire::GivenAnswers>(input)
                            .inspect_err(|error| {
                                tracing::warn!(%error, "a form's answers edualc can't read");
                            })
                            .ok()
                            .map(|given| given.answers)
                    }
                    wire::ToolInput::Plan(_) => None,
                };
                match given {
                    Some(answers) => acp::ElicitationAnswer::Accept {
                        content: filled_in(&form.questions, &answers),
                    },
                    None => acp::ElicitationAnswer::Cancel,
                }
            }
            wire::Reply::Success {
                response: wire::PermissionResult::Deny { .. },
                ..
            } => acp::ElicitationAnswer::Decline,
            wire::Reply::Error { error, .. } => {
                tracing::warn!(%error, "the window couldn't ask a form's questions: cancelled");
                acp::ElicitationAnswer::Cancel
            }
        };
        Self::form_answer(form.id, answer)
    }
}

/// Returns the questions a form asks: one for each field with options, or for text alone.
/// agy 1.5.9 puts a field for an answer in words beside each question, `<field>_other`, which
/// belongs to that question rather than being one of its own.
fn form_questions(schema: &acp::FormSchema) -> Vec<FormQuestion> {
    let fields = &schema.properties;
    let other_of = |name: &str| {
        let other = format!("{name}_other");
        fields.contains_key(&other).then_some(other)
    };
    let beside = |name: &str| {
        name.strip_suffix("_other").is_some_and(|base| {
            fields
                .get(base)
                .is_some_and(|field| !options_of(field).1.is_empty())
        })
    };
    fields
        .iter()
        .filter(|(name, _)| !beside(name))
        .filter_map(|(name, field)| {
            let (takes, options) = options_of(field);
            if takes == Takes::Text && field.kind != acp::FieldKind::String {
                tracing::warn!(field = %name, "a form's field edualc doesn't ask: left out");
                return None;
            }
            Some(FormQuestion {
                question: field.title.clone().unwrap_or_else(|| name.clone()),
                field: name.clone(),
                other: (takes != Takes::Text).then(|| other_of(name)).flatten(),
                takes,
                options,
            })
        })
        .collect()
}

/// Returns what a form's field takes, and its options.
fn options_of(field: &acp::FormField) -> (Takes, Vec<acp::FormOption>) {
    if !field.one_of.is_empty() {
        return (Takes::OneOption, field.one_of.clone());
    }
    match &field.items {
        Some(items) if !items.any_of.is_empty() => (Takes::Options, items.any_of.clone()),
        Some(_) | None => (Takes::Text, Vec::new()),
    }
}

/// Fills in the form from the window's answers, by question. An answer that is an option's
/// label (or labels, where several are taken) becomes the option's value; anything else was
/// typed, and goes in the question's field for words, or its own.
fn filled_in(
    questions: &[FormQuestion],
    answers: &std::collections::BTreeMap<String, String>,
) -> acp::FormContent {
    let mut content = acp::FormContent::new();
    for question in questions {
        let Some(answer) = answers.get(&question.question) else {
            continue;
        };
        let value_of = |label: &str| {
            question
                .options
                .iter()
                .find(|option| option.title == label)
                .map(|option| option.value.clone())
        };
        let picked = match question.takes {
            Takes::OneOption => value_of(answer).map(acp::FieldValue::Text),
            Takes::Options => answer
                .split(", ")
                .map(value_of)
                .collect::<Option<Vec<String>>>()
                .map(acp::FieldValue::Picked),
            Takes::Text => None,
        };
        match (picked, &question.other) {
            (Some(picked), _) => {
                content.insert(question.field.clone(), picked);
            }
            (None, Some(other)) => {
                content.insert(other.clone(), acp::FieldValue::Text(answer.clone()));
            }
            (None, None) => {
                content.insert(
                    question.field.clone(),
                    acp::FieldValue::Text(answer.clone()),
                );
            }
        }
    }
    content
}
