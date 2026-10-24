//! Chat seating over the socket — the bulk sibling of the TUI's
//! "Move to branch/module..." menu (TP-CHAT-MOVE-13/14).
//!
//! The menu path parks a request and lets the App loop call
//! `apply_chat_move` once per chat; that is right for a person moving one
//! chat and wrong for a plan moving hundreds — each call would sync and
//! write the ledger to disk. This handler runs on the same `&mut App` the
//! API runtime already holds, folds every entry into the ledger first, and
//! then syncs ONCE and saves ONCE.

use super::responses::{encode_error, encode_success};
use crate::api::schema::response::ResponseResult;
use crate::api::schema::{ChatOpenParams, ChatSeatParams, ChatUnseatParams};
use crate::persist::workspace_chats::{SeatOutcome, USER_MOVE_SOURCE};

impl super::App {
    /// `chat.open`: the conversation's identity is the whole request
    /// (TP-AGPANEL-57..62). Refusals answer with a stable code and a reason a
    /// person can read, exactly as the revival road does (TP-AGPANEL-24).
    pub(super) fn handle_chat_open(&mut self, id: String, params: ChatOpenParams) -> String {
        let projects_dir = crate::claude_sessions::default_claude_projects_dir();
        match self.open_chat_by_session(&params.session_id, projects_dir.as_deref()) {
            Ok(opened) => encode_success(
                id,
                ResponseResult::ChatOpened {
                    road: opened.road,
                    pane_id: opened.pane_id,
                },
            ),
            Err(refusal) => encode_error(id, refusal.code(), refusal.message()),
        }
    }

    pub(super) fn handle_chat_seat(&mut self, id: String, params: ChatSeatParams) -> String {
        let source = params
            .source
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(USER_MOVE_SOURCE);
        let (mut applied, mut unchanged, mut refused) = (0usize, 0usize, 0usize);
        for entry in &params.seats {
            let move_outcome = self.workspace_chat_ledger.set_move_from(
                &entry.session_id,
                &entry.target_key,
                source,
            );
            // Extras ride the move's own ownership rules; either half
            // changing counts the entry as applied (TP-CHAT-MOVE-15).
            let extras_outcome = if entry.extra_keys.is_empty() {
                SeatOutcome::Unchanged
            } else {
                self.workspace_chat_ledger.set_extra_seats(
                    &entry.session_id,
                    &entry.extra_keys,
                    source,
                )
            };
            match (move_outcome, extras_outcome) {
                (SeatOutcome::Applied, _) | (_, SeatOutcome::Applied) => applied += 1,
                (SeatOutcome::Refused, SeatOutcome::Refused)
                | (SeatOutcome::Refused, SeatOutcome::Unchanged) => refused += 1,
                _ => unchanged += 1,
            }
        }
        if applied > 0 {
            self.finish_chat_ledger_mutation();
        }
        encode_success(
            id,
            ResponseResult::ChatSeatReport {
                applied,
                unchanged,
                refused,
            },
        )
    }

    pub(super) fn handle_chat_unseat(&mut self, id: String, params: ChatUnseatParams) -> String {
        let cleared = self
            .workspace_chat_ledger
            .clear_moves_by_source(&params.source);
        if cleared > 0 {
            self.finish_chat_ledger_mutation();
        }
        encode_success(id, ResponseResult::ChatUnseatReport { cleared })
    }

    /// TP-CHAT-SYNC-01: re-read the rows and re-title the bound tabs on
    /// request. Nothing is written: the ledger did not change, only what the
    /// transcripts say a chat is called — the one thing the rows never noticed.
    pub(super) fn handle_chat_sync(&mut self, id: String) -> String {
        self.sync_workspace_chat_rows();
        encode_success(id, ResponseResult::Ok {})
    }

    /// The single sync + single save every ledger mutation road ends in —
    /// the same shape `apply_chat_move` uses, shared so a bulk road cannot
    /// forget the `no_session` guard and write a fixture's moves into the
    /// machine's real ledger.
    fn finish_chat_ledger_mutation(&mut self) {
        self.sync_workspace_chat_rows();
        if self.no_session() {
            return;
        }
        let path = crate::persist::workspace_chats::default_ledger_path();
        if let Err(err) =
            crate::persist::workspace_chats::save_to_path(&path, &self.workspace_chat_ledger)
        {
            tracing::warn!(path = %path.display(), %err, "failed to save workspace chat ledger");
        }
    }
}
