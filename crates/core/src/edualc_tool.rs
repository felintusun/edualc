//! edualc's own tools, which the programs call by these names.

/// A tool of edualc's, by the name a program calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::Display)]
pub enum EdualcTool {
    /// Sends a message to another session; an agy session calls it as an MCP tool.
    #[strum(serialize = "mcp__edualc__send_message")]
    SendMessage,
    /// Lists the sessions running; an agy session calls it as an MCP tool.
    #[strum(serialize = "mcp__edualc__list_sessions")]
    ListSessions,
    /// Not a tool: edualc asks under this name whether another session's message may come in
    /// (see [`crate::external::tool_input::IncomingMessageInput`]).
    #[strum(serialize = "edualc_incoming_message")]
    IncomingMessage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::wire::ToolName;

    #[test]
    fn edualcs_tools_keep_their_names_in_calls() {
        for (tool, name) in [
            (EdualcTool::SendMessage, "mcp__edualc__send_message"),
            (EdualcTool::ListSessions, "mcp__edualc__list_sessions"),
            (EdualcTool::IncomingMessage, "edualc_incoming_message"),
        ] {
            let read = ToolName::from(name.to_owned());
            assert_eq!(read, ToolName::Edualc(tool));
            assert_eq!(String::from(read), name);
        }
        for other in ["Edualc", "{0}", "SendMessage"] {
            assert_ne!(
                std::mem::discriminant(&ToolName::from(other.to_owned())),
                std::mem::discriminant(&ToolName::Edualc(EdualcTool::SendMessage)),
                "{other}"
            );
        }
    }
}
