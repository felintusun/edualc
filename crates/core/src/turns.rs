//! The turns under way in a session, as the window and the daemon count them (sans-io).

use serde::{Deserialize, Serialize};

use crate::external::wire::CommandState;
use crate::ids::PromptId;

/// The prompts sent and not yet finished, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turns {
    open: Vec<(PromptId, Taken)>,
    ends: Ends,
}

/// Whether a turn took the prompt in yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
enum Taken {
    Waiting,
    InTurn,
}

/// What tells that a prompt is finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
enum Ends {
    /// A turn's `result`, which finishes the oldest prompt: an ACP agent answers each prompt
    /// with one.
    #[default]
    ByResult,
    /// Claude Code's `command_lifecycle`, once it told one: a turn's `result` finishes every
    /// prompt the turn took in, which is more than one where a prompt sent during the turn was
    /// folded into it, and none where Claude Code started the turn itself (as for another
    /// session's message) and took none in.
    ByLifecycle,
}

impl Turns {
    pub fn sent(&mut self, prompt: PromptId) {
        self.open.push((prompt, Taken::Waiting));
    }

    /// Takes how far Claude Code got with `prompt`.
    pub fn told(&mut self, prompt: PromptId, state: CommandState) {
        self.ends = Ends::ByLifecycle;
        match state {
            CommandState::Started => {
                for (open, taken) in &mut self.open {
                    if *open == prompt {
                        *taken = Taken::InTurn;
                    }
                }
            }
            CommandState::Completed | CommandState::Cancelled => {
                self.open.retain(|(open, _)| *open != prompt);
            }
            CommandState::Queued | CommandState::Other => {}
        }
    }

    /// Takes a turn's `result`.
    pub fn result(&mut self) {
        match self.ends {
            Ends::ByResult if !self.open.is_empty() => {
                self.open.remove(0);
            }
            Ends::ByResult => {}
            Ends::ByLifecycle => self.open.retain(|(_, taken)| *taken == Taken::Waiting),
        }
    }

    /// Forgets every prompt, as the process ended.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    pub fn under_way(&self) -> bool {
        !self.open.is_empty()
    }

    pub fn count(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_folded_into_a_turn_ends_with_it() {
        let (first, folded) = (PromptId::new_random(), PromptId::new_random());
        let mut turns = Turns::default();
        turns.sent(first);
        // As Claude Code 2.1.329 tells it.
        turns.told(first, CommandState::Queued);
        turns.told(first, CommandState::Started);
        turns.sent(folded);
        turns.told(folded, CommandState::Queued);
        turns.told(folded, CommandState::Started);
        turns.result();
        assert!(!turns.under_way());
        turns.told(folded, CommandState::Completed);
        turns.told(first, CommandState::Completed);
        assert!(!turns.under_way());
    }

    #[test]
    fn a_prompt_waiting_for_a_turn_outlasts_the_one_under_way() {
        let (first, next) = (PromptId::new_random(), PromptId::new_random());
        let mut turns = Turns::default();
        turns.sent(first);
        turns.told(first, CommandState::Started);
        turns.sent(next);
        turns.told(next, CommandState::Queued);
        turns.result();
        assert_eq!(turns.count(), 1);
        turns.told(next, CommandState::Started);
        turns.result();
        assert!(!turns.under_way());
    }

    #[test]
    fn a_turn_claude_code_started_itself_ends_no_prompt() {
        let waiting = PromptId::new_random();
        let mut turns = Turns::default();
        turns.sent(waiting);
        turns.told(waiting, CommandState::Queued);
        turns.result();
        assert!(turns.under_way());
        turns.told(waiting, CommandState::Cancelled);
        assert!(!turns.under_way());
    }

    #[test]
    fn without_a_lifecycle_each_result_ends_a_prompt() {
        let mut turns = Turns::default();
        turns.sent(PromptId::new_random());
        turns.sent(PromptId::new_random());
        turns.result();
        assert_eq!(turns.count(), 1);
        turns.result();
        assert!(!turns.under_way());
    }
}
