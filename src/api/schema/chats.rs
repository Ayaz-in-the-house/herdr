use serde::{Deserialize, Serialize};

/// One chat re-home in a bulk seat request: the ledger key is either a
/// checkout directory or a `module:<node-key>` seat — the same vocabulary
/// the TUI's "Move to branch/module..." menu writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChatSeatEntry {
    pub session_id: String,
    pub target_key: String,
    /// Extra drawers the chat ALSO appears in (M12: one conversation that
    /// did several jobs shows a row under each module it worked in; every
    /// row is the same conversation). Additive: old clients omit it.
    #[serde(default)]
    pub extra_keys: Vec<String>,
}

/// Bulk chat seating (`chat.seat`). `source` stamps who decided: a plan
/// applier names itself (e.g. "seat-plan") so its moves can be told apart
/// from — and never override — a person's own menu moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChatSeatParams {
    pub seats: Vec<ChatSeatEntry>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Open a conversation by its identity (`chat.open`).
///
/// The id is all a client sends: where the conversation lives, which recipe
/// resumes it and whether it is already running are the server's to answer
/// (a headstone, the ledger, the transcript — in that order). A client that
/// named a directory too could name a mismatched pair, and there is only one
/// right answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChatOpenParams {
    pub session_id: String,
}

/// Which road `chat.open` took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatOpenRoad {
    /// The conversation was already running; its tab was focused.
    Focused,
    /// A headstone remembered it; the revival road brought it back.
    Revived,
    /// Nothing remembered it; it was resumed from the ledger's or the
    /// transcript's directory.
    Resumed,
}

/// Withdraw every move a given source wrote (`chat.unseat`) — the undo gate
/// for automated seating. A person's moves are source "user" and can only be
/// withdrawn by naming that source explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChatUnseatParams {
    pub source: String,
}
