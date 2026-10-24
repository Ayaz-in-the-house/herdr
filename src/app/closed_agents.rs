//! The ledger of recently closed agents — dead data rows, not processes.
//!
//! A closed agent is a *record* carrying an identity and a revival recipe:
//! no PTY, no runtime, no timer, no subscription. Nothing ticks for it, which
//! is what lets the panel show a graveyard of any size for free. The record
//! is written by a trigger (the agent closed), and removed by a trigger (it
//! was revived, or it aged out of the ring). Anything periodic here would be
//! a design regression, not an optimisation problem.

use std::collections::VecDeque;
use std::path::PathBuf;

/// How many closed agents the ledger remembers.
///
/// This used to be eight, and its stated reason was the sidebar's height — a
/// separator plus eight grey rows fitting under the active cards. That reason
/// belonged to the drawing layer, which enforces its own bound anyway:
/// `closed_agent_row_slots` hands out only the rows the body actually has, so
/// a longer ledger cannot push anything off screen.
///
/// Leaving it at eight once the store existed would have been silent data
/// loss: a run loads up to `MAX_RECORDS` from disk, then the very next close
/// truncates the list back to eight and the month of history the user asked
/// for disappears on the first agent they shut down. The bound still matters —
/// an unbounded list is unbounded memory wearing a feature's name — so it is
/// the store's bound, kept in one place rather than two that can drift.
pub(crate) const CLOSED_AGENT_CAPACITY: usize = crate::persist::closed_agents::MAX_RECORDS;

/// Where a ghost is in its journey back to life.
///
/// Revival is a one-way transition, not a debounce: a second click during
/// `Reviving` is inert because the state already says a spawn is in flight.
/// Timing plays no part, so the guarantee is testable on the slowest machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevivalState {
    /// A dead row waiting to be clicked.
    Dormant,
    /// A spawn is in flight; further clicks mean nothing until it lands.
    Reviving,
}

/// Everything a revival needs, frozen at the moment the agent closed.
///
/// Each field earns its place by what breaks without it: the identity keeps
/// one agent from haunting the list twice, the cwd is the whole difference
/// between reopening where the user worked and reopening in `$HOME`, and the
/// session key is what lets the chat drawer reattach to the same conversation
/// instead of presenting the revival as a stranger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedAgentRecord {
    /// Stable identity; the dedup key and what a revival resolves back to.
    pub agent_id: String,
    /// The name the grey row wears; live state is gone, so it travels here.
    pub label: String,
    /// Where the agent last worked. Derived-at-revival was #46's root cause.
    /// `None` when the close never knew one — the ghost still shows, and the
    /// revival is the layer that refuses to guess (never `$HOME`).
    pub cwd: Option<PathBuf>,
    /// Which workspace/branch the revival returns under.
    pub workspace_key: Option<String>,
    /// The resume recipe, frozen at close time: the agent's own reported
    /// session (source, agent, session ref) — everything the existing resume
    /// roads need to bring the conversation back. `None` when the close never
    /// knew one; such a ghost is visible history whose revival is refused
    /// with a reason rather than launching a stranger.
    pub session: Option<crate::agent_resume::PersistedAgentSession>,
    /// When it closed, in milliseconds; newest-first ordering and eviction.
    pub closed_at: u64,
    /// The revival state machine (see [`RevivalState`]).
    pub revival: RevivalState,
    /// Whether `label` is a name somebody wrote on purpose (the tab was renamed
    /// before it closed). Recorded at close instead of guessed from the
    /// label's shape later: a label that is NOT explicit follows the
    /// conversation's current title, an explicit one is never touched
    /// (TP-AGPANEL-64).
    pub label_explicit: bool,
    /// The conversation was opened and never spoken to — no transcript
    /// anywhere, or one with no message in it. A fact about the transcript
    /// store as of the last weighing (seed, close, or a revival gesture), so it
    /// is never persisted; such a headstone is remembered but not drawn, and
    /// reviving it is refused with a reason (TP-AGPANEL-67).
    pub never_spoke: bool,
}

impl ClosedAgentRecord {
    /// The session value a revival resumes and wires its new tab to.
    pub fn session_value(&self) -> Option<&str> {
        self.session
            .as_ref()
            .map(|session| session.session_ref.value.as_str())
    }
}

/// Why a revival was refused. Every variant is a visible reason: the
/// alternative to refusing is guessing, and #46 measured exactly where a
/// guessed directory lands ($HOME). A refusal never flips the row's state —
/// the ghost stays clickable for the moment the answer changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevivalRefusal {
    /// No record carries this identity — a stale click, or the row aged out.
    UnknownGhost,
    /// A spawn is already in flight; the click is inert, not queued.
    RevivalInFlight,
    /// The record closed without a directory; reviving would mean guessing.
    NoCwd,
    /// The recorded directory no longer exists (worktree removed).
    CwdGone,
    /// The record closed without a session recipe; there is nothing to resume.
    NoSessionRecipe,
    /// The spawn road produced no wired tab; the ghost returns to rest.
    SpawnFailed,
    /// The conversation was never spoken to: there is nothing to resume.
    NothingToResume,
}

impl RevivalRefusal {
    /// Stable machine-readable code for the API surface.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::UnknownGhost => "unknown_closed_agent",
            Self::RevivalInFlight => "revival_in_flight",
            Self::NoCwd => "closed_agent_cwd_unknown",
            Self::CwdGone => "closed_agent_cwd_gone",
            Self::NoSessionRecipe => "closed_agent_session_unknown",
            Self::SpawnFailed => "revival_spawn_failed",
            Self::NothingToResume => "nothing_to_resume",
        }
    }

    /// The visible reason behind the code.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::UnknownGhost => "no recently closed agent carries this id",
            Self::RevivalInFlight => "a revival for this agent is already in flight",
            Self::NoCwd => "the closed agent never recorded a working directory",
            Self::CwdGone => "the recorded working directory no longer exists",
            Self::NoSessionRecipe => "the closed agent has no session to resume",
            Self::SpawnFailed => "the revival spawn produced no tab",
            Self::NothingToResume => {
                "this conversation was never spoken to; there is nothing to resume"
            }
        }
    }
}

/// What the transcript store knows about one headstone's conversation: its
/// current title by the store's own rule, and whether anyone ever spoke.
///
/// `None` from a lookup means the store holds no transcript for it at all —
/// which is itself an answer (nobody ever spoke), not an unknown.
pub(crate) type ConversationFacts = crate::claude_sessions::HeadstoneFacts;

/// Ask the Claude transcript store about one conversation. The headstone's
/// directory rides along as a hint; the store probes by id when it misses.
pub(crate) fn conversation_facts_in(
    projects_dir: &std::path::Path,
    session: &str,
    cwd: Option<&std::path::Path>,
) -> Option<ConversationFacts> {
    let hint = cwd
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::claude_sessions::read_headstone_facts_for_session(projects_dir, &hint, session)
}

/// A spawnable launch: the argv and the extra environment it carries.
pub(crate) type LaunchRecipe = (Vec<String>, Vec<(String, String)>);

/// The launch a revival uses: claude rides the chat drawer's own launch
/// (identical flags, identical env), every other agent gets the plan the
/// session-restore road builds for it. A revival argv invented here would be
/// a third road for the next UI change to forget.
pub(crate) fn revival_launch(
    session: &crate::agent_resume::PersistedAgentSession,
) -> Option<LaunchRecipe> {
    if session.agent == "claude" {
        return Some(super::projects::project_chat_launch(
            "claude",
            Some(session.session_ref.value.as_str()),
        ));
    }
    crate::agent_resume::plan(&session.source, &session.agent, &session.session_ref)
        .map(|plan| (plan.argv, Vec::new()))
}

/// Where a headstone opens (TP-AGPANEL-73): where the conversation is
/// LABELLED — the directory it was filed in, a move or a module seat — then
/// where it closed, then a directory whose drawer still lists it. Never the
/// transcript's guess (TP-AGPANEL-61) and never `$HOME` (#46): with nothing
/// left the answer is `None` and the revival refuses.
pub(crate) fn revival_home(
    session: &str,
    recorded_cwd: Option<&std::path::Path>,
    ledger: &crate::persist::workspace_chats::WorkspaceChatLedger,
    module_dir: &dyn Fn(&str) -> Option<PathBuf>,
    is_dir: &dyn Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    crate::app::chat_open::filed_chat_directory(session, ledger, module_dir, is_dir)
        .or_else(|| {
            recorded_cwd
                .filter(|dir| is_dir(dir))
                .map(std::path::Path::to_path_buf)
        })
        .or_else(|| crate::app::chat_open::listed_chat_directory(session, ledger, is_dir))
}

impl super::App {
    /// Revive a ghost: spawn its agent back in the recorded directory, wired
    /// to the recorded conversation, riding the same road a chat-drawer
    /// resume takes — including its focus-the-wired-tab spam guard.
    ///
    /// Validation happens before the claim so a refusal leaves the row
    /// dormant and clickable; the claim happens before the spawn so two fast
    /// clicks cannot both win (TP-AGPANEL-10).
    pub(crate) fn revive_closed_agent(&mut self, agent_id: &str) -> Result<(), RevivalRefusal> {
        let dir = self.headstone_projects_dir();
        self.revive_closed_agent_in(agent_id, dir.as_deref())
    }

    /// Where headstones are weighed against the transcript store — nowhere on
    /// a `--no-session` run, for the reason TP-AGPANEL-36 gives: a fixture must
    /// not inherit whatever the machine's real store happens to hold.
    pub(super) fn headstone_projects_dir(&self) -> Option<PathBuf> {
        if self.no_session() {
            return None;
        }
        crate::claude_sessions::default_claude_projects_dir()
    }

    /// Record a closing agent while its state is still alive
    /// ([`crate::app::state::AppState::note_agent_closed`]) and weigh the new
    /// headstone against the transcript store.
    pub(crate) fn note_agent_closed(&mut self, ws_idx: usize, pane_id: crate::layout::PaneId) {
        let dir = self.headstone_projects_dir();
        self.note_agent_closed_in(ws_idx, pane_id, dir.as_deref());
    }

    /// [`Self::note_agent_closed`] with the transcript store named by the
    /// caller.
    ///
    /// TP-AGPANEL-67: the close is the moment to look — one transcript, read
    /// at its two ends, once, on the trigger. With no store to ask the
    /// headstone is simply drawn: not knowing is not knowing it is empty.
    pub(crate) fn note_agent_closed_in(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        projects_dir: Option<&std::path::Path>,
    ) {
        let Some(agent_id) = self.state.note_agent_closed(ws_idx, pane_id) else {
            return;
        };
        let Some(dir) = projects_dir else {
            return;
        };
        self.state.closed_agents.weigh(&agent_id, |session, cwd| {
            conversation_facts_in(dir, session, cwd)
        });
    }

    /// [`Self::revive_closed_agent`] with the transcript store named by the
    /// caller.
    pub(crate) fn revive_closed_agent_in(
        &mut self,
        agent_id: &str,
        projects_dir: Option<&std::path::Path>,
    ) -> Result<(), RevivalRefusal> {
        let Some((revival, never_spoke)) = self
            .state
            .closed_agents
            .entries()
            .find(|record| record.agent_id == agent_id)
            .map(|record| (record.revival, record.never_spoke))
        else {
            return Err(RevivalRefusal::UnknownGhost);
        };
        if revival == RevivalState::Reviving {
            return Err(RevivalRefusal::RevivalInFlight);
        }
        // TP-AGPANEL-67: `never_spoke` is a fact as of the last weighing, and
        // refusing on a stale fact would be a bug of its own — so the gesture
        // weighs again, and a conversation someone has spoken in since then
        // revives like any other. With no store to ask, the flag stands.
        if never_spoke {
            let still_silent = projects_dir.is_none_or(|dir| {
                self.state
                    .closed_agents
                    .weigh(agent_id, |session, cwd| {
                        conversation_facts_in(dir, session, cwd)
                    })
                    .unwrap_or(true)
            });
            if still_silent {
                return Err(RevivalRefusal::NothingToResume);
            }
        }
        let Some(record) = self
            .state
            .closed_agents
            .entries()
            .find(|record| record.agent_id == agent_id)
        else {
            return Err(RevivalRefusal::UnknownGhost);
        };
        let recorded_cwd = record.cwd.clone();
        let Some(session) = record.session.clone() else {
            return Err(if recorded_cwd.is_none() {
                RevivalRefusal::NoCwd
            } else {
                RevivalRefusal::NoSessionRecipe
            });
        };
        let home = revival_home(
            &session.session_ref.value,
            recorded_cwd.as_deref(),
            &self.workspace_chat_ledger,
            &|node| self.state.module_directory_for_key(node),
            &|dir| dir.is_dir(),
        );
        let Some(cwd) = home else {
            return Err(if recorded_cwd.is_none() {
                RevivalRefusal::NoCwd
            } else {
                RevivalRefusal::CwdGone
            });
        };
        let Some((argv, extra_env)) = revival_launch(&session) else {
            return Err(RevivalRefusal::NoSessionRecipe);
        };
        let session_value = session.session_ref.value;

        if !self.state.closed_agents.try_begin_revival(agent_id) {
            return Err(RevivalRefusal::RevivalInFlight);
        }
        // TP-AGPANEL-74: in that directory's own workspace — a fresh one
        // rooted there when none is open — never whichever workspace happens
        // to be active.
        self.open_project_chat_tab_homed(
            crate::app::state::ProjectChatTabRequest {
                project_path: cwd,
                session_id: Some(session_value.clone()),
            },
            &argv,
            extra_env,
            crate::app::projects::ChatTabHome::OwnDirectory,
        );
        // The road wires whichever tab it spawned (or focused) to the session;
        // that wiring is the road's own receipt that the revival landed.
        if self.state.find_resumed_chat_tab(&session_value).is_some() {
            self.state.closed_agents.resolve_revival(agent_id);
            Ok(())
        } else {
            self.state.closed_agents.abort_revival(agent_id);
            Err(RevivalRefusal::SpawnFailed)
        }
    }
}

/// The session a Claude headstone's transcript is filed under, owned so the
/// record stays free to be changed while it is in hand.
fn claude_session_of(record: &ClosedAgentRecord) -> Option<String> {
    record
        .session
        .as_ref()
        .filter(|session| session.agent == "claude")
        .map(|session| session.session_ref.value.clone())
}

/// Take `title` as the record's label when it is one: trimmed, not empty, not
/// what the record already wears, and not a placeholder.
fn adopt_title(record: &mut ClosedAgentRecord, title: &str) -> bool {
    let title = title.trim();
    let agent = record
        .session
        .as_ref()
        .map(|session| session.agent.as_str());
    if title.is_empty()
        || title == record.label
        || crate::persist::closed_agents::is_placeholder_label(title, &record.agent_id, agent)
    {
        return false;
    }
    record.label = title.to_string();
    true
}

/// Write what the transcript store said onto one record; `None` is "no
/// transcript anywhere". Returns whether anything changed.
fn apply_facts(record: &mut ClosedAgentRecord, facts: Option<ConversationFacts>) -> bool {
    let (title, spoke) = match facts {
        Some(facts) => (facts.title, facts.spoke),
        None => (None, false),
    };
    let mut changed = record.never_spoke == spoke;
    record.never_spoke = !spoke;
    if !record.label_explicit {
        if let Some(title) = title {
            changed |= adopt_title(record, &title);
        }
    }
    changed
}

/// The fixed-capacity, newest-first ring of closed agents.
#[derive(Debug, Default)]
pub(crate) struct ClosedAgentLedger {
    records: VecDeque<ClosedAgentRecord>,
}

impl ClosedAgentLedger {
    /// Remember a closed agent, newest first.
    ///
    /// Closing an agent that is already remembered refreshes its row rather
    /// than adding a second one: two ghosts with one identity would leave the
    /// user guessing which of them revives, and the answer would be neither
    /// reliably. The refreshed row starts `Dormant` again — whatever revival
    /// was in flight belonged to a life that has since ended. Beyond capacity
    /// the oldest row is evicted.
    pub fn record_closed(&mut self, mut record: ClosedAgentRecord) {
        // A fresh death always starts dormant, whatever the writer handed in:
        // any revival that was in flight belonged to a life that just ended.
        record.revival = RevivalState::Dormant;
        self.records.retain(|r| r.agent_id != record.agent_id);
        self.records.push_front(record);
        self.records.truncate(CLOSED_AGENT_CAPACITY);
    }

    /// The remembered ghosts, newest first.
    pub fn entries(&self) -> impl Iterator<Item = &ClosedAgentRecord> {
        self.records.iter()
    }

    /// Drop one ghost from the graveyard.
    ///
    /// TP-AGPANEL-45: until now the list only ever grew — records arrive when
    /// an agent closes and leave only by ageing past capacity or by being
    /// revived, and reviving is the opposite of what someone wants when they
    /// look at a row they are done with. Measured on the reporting machine: 62
    /// records standing, none of them removable.
    ///
    /// Returns whether anything went, so the caller can skip a save and can
    /// tell "removed" from "was not there" rather than reporting success for a
    /// row a refresh had already taken away.
    pub fn forget(&mut self, agent_id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|record| record.agent_id != agent_id);
        before != self.records.len()
    }

    /// Give headstones the names their conversations wear today.
    ///
    /// TP-AGPANEL-53: a record is frozen at close time, and at close time a
    /// chat may not have a title yet — the cron namer writes one hours later,
    /// a person renames it the next day. The label is the one field a
    /// headstone wears for people, so it follows the conversation the way a
    /// tab's does (TP-TAB-CHAT-01).
    ///
    /// TP-AGPANEL-63: WHICH labels follow is decided by what was recorded, not
    /// by what a label looks like. Until 2026-09-19 only labels shaped like a
    /// placeholder were repaired, and 39 of 115 headstones measured that day
    /// wore a name their conversation no longer had — a directory name, an
    /// outgrown AI title, a terminal title — none of them placeholder-shaped.
    /// A name written on purpose (`label_explicit`) is never overwritten and
    /// `title_for` is never even asked about it, which is what bounds a caller
    /// that reads a file per answer; every other label takes the title, unless
    /// the lookup knows nothing or only a placeholder
    /// ([`crate::persist::closed_agents::is_placeholder_label`]) — a
    /// placeholder is not an answer. Returns how many rows changed.
    pub fn adopt_titles(&mut self, mut title_for: impl FnMut(&str) -> Option<String>) -> usize {
        let mut changed = 0;
        for record in &mut self.records {
            if record.label_explicit {
                continue;
            }
            let Some(title) = title_for(&record.agent_id) else {
                continue;
            };
            changed += usize::from(adopt_title(record, &title));
        }
        changed
    }

    /// Pull each headstone's clock forward to the last time anything saw its
    /// conversation alive.
    ///
    /// TP-AGPANEL-65: `closed_at` is the ring's order AND the age a row wears
    /// (TP-AGPANEL-55) — one clock, so the two can never disagree. It used to
    /// know only the deaths the server witnessed: a chat resumed through
    /// another door and then lost with the server kept the hour of its first
    /// death, first in the PWA's history and seventeenth in the panel
    /// (measured 2026-09-19). `seen_at` answers by SESSION, the identity a
    /// sighting is recorded under; a later sighting moves the headstone and
    /// the ring is re-ordered (stably — equal clocks keep their places), an
    /// earlier one takes nothing back, and a headstone with no conversation is
    /// never asked about. Returns how many rows moved.
    pub fn adopt_sightings(&mut self, mut seen_at: impl FnMut(&str) -> Option<u64>) -> usize {
        let mut changed = 0;
        for record in &mut self.records {
            let Some(seen) = record
                .session
                .as_ref()
                .and_then(|session| seen_at(session.session_ref.value.as_str()))
            else {
                continue;
            };
            if seen > record.closed_at {
                record.closed_at = seen;
                changed += 1;
            }
        }
        if changed > 0 {
            self.records
                .make_contiguous()
                .sort_by_key(|record| std::cmp::Reverse(record.closed_at));
        }
        changed
    }

    /// Weigh each Claude headstone against the transcript store: the title
    /// its conversation wears today, and whether anyone ever spoke in it.
    ///
    /// TP-AGPANEL-63 / TP-AGPANEL-67: asked by SESSION (a transcript is filed
    /// under the conversation's id), with the record's directory as the hint.
    /// Only Claude conversations are asked about — another agent keeps its
    /// sessions elsewhere and that store is not ours to judge — and a lookup
    /// that finds no transcript at all is an answer, not an unknown: nobody
    /// spoke. A name written on purpose is weighed like any other and keeps
    /// its name. Returns how many rows changed.
    pub fn adopt_conversation_facts(
        &mut self,
        mut facts_for: impl FnMut(&str, Option<&std::path::Path>) -> Option<ConversationFacts>,
    ) -> usize {
        let mut changed = 0;
        for record in &mut self.records {
            let Some(facts) = claude_session_of(record)
                .map(|session| facts_for(session.as_str(), record.cwd.as_deref()))
            else {
                continue;
            };
            changed += usize::from(apply_facts(record, facts));
        }
        changed
    }

    /// [`Self::adopt_conversation_facts`] for one headstone — the close that
    /// just recorded it, or the gesture about to revive it. Answers whether
    /// the conversation (still) has nobody in it; `None` when the record is
    /// unknown or is not a Claude conversation.
    pub fn weigh(
        &mut self,
        agent_id: &str,
        facts_for: impl FnOnce(&str, Option<&std::path::Path>) -> Option<ConversationFacts>,
    ) -> Option<bool> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.agent_id == agent_id)?;
        let session = claude_session_of(record)?;
        let facts = facts_for(session.as_str(), record.cwd.as_deref());
        apply_facts(record, facts);
        Some(record.never_spoke)
    }

    /// Rebuild from what the store kept, newest first.
    ///
    /// TP-AGPANEL-34: a loaded ghost is always `Dormant`. Revival state
    /// describes *this* process — a spawn that was in flight when the server
    /// was replaced did not survive the replacement, and a row that came back
    /// claiming `Reviving` would be inert forever, because the claim it is
    /// waiting on belongs to a process that no longer exists.
    ///
    /// Records that cannot be understood are skipped rather than fatal: this
    /// runs at startup, and one malformed row must not cost the whole
    /// graveyard.
    pub fn load_stored(&mut self, stored: Vec<crate::persist::closed_agents::StoredClosedAgent>) {
        self.records = stored
            .into_iter()
            .map(|row| ClosedAgentRecord {
                agent_id: row.agent_id,
                label: row.label,
                cwd: row.cwd.map(PathBuf::from),
                workspace_key: row.workspace_key,
                session: row
                    .session
                    .map(|session| crate::agent_resume::PersistedAgentSession {
                        source: session.source,
                        agent: session.agent,
                        session_ref: crate::agent_resume::AgentSessionRef {
                            kind: session.ref_kind,
                            value: session.ref_value,
                        },
                    }),
                closed_at: row.closed_at,
                revival: RevivalState::Dormant,
                label_explicit: row.label_explicit,
                // Weighed afresh by whoever loads the store: a fact about the
                // transcripts as they stand now, not as they stood at the save.
                never_spoke: false,
            })
            .collect();
    }

    /// Project to the disk shape, newest first.
    pub fn to_stored(&self) -> Vec<crate::persist::closed_agents::StoredClosedAgent> {
        self.records
            .iter()
            .map(|row| crate::persist::closed_agents::StoredClosedAgent {
                agent_id: row.agent_id.clone(),
                label: row.label.clone(),
                cwd: row
                    .cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                workspace_key: row.workspace_key.clone(),
                session: row.session.as_ref().map(|session| {
                    crate::persist::closed_agents::StoredSession {
                        source: session.source.clone(),
                        agent: session.agent.clone(),
                        ref_kind: session.session_ref.kind,
                        ref_value: session.session_ref.value.clone(),
                    }
                }),
                closed_at: row.closed_at,
                label_explicit: row.label_explicit,
            })
            .collect()
    }

    /// Claim the right to revive a ghost — the atomic half of spam safety.
    ///
    /// Returns `true` exactly once per dormancy: the transition to `Reviving`
    /// happens here, before any spawn is attempted, so two fast clicks cannot
    /// both come back `true`. A ghost that is unknown or already reviving
    /// yields `false` and the caller does nothing — a stale target is inert,
    /// the same principle TP-AGPANEL-06 pinned for the chat road.
    pub fn try_begin_revival(&mut self, agent_id: &str) -> bool {
        match self.records.iter_mut().find(|r| r.agent_id == agent_id) {
            Some(row) if row.revival == RevivalState::Dormant => {
                row.revival = RevivalState::Reviving;
                true
            }
            _ => false,
        }
    }

    /// The spawn landed: the row leaves the graveyard.
    ///
    /// From here on the agent is an ordinary live one, and clicking it means
    /// focus, not revival — the ordinary panel behaviour, no special case.
    pub fn resolve_revival(&mut self, agent_id: &str) {
        self.records.retain(|r| r.agent_id != agent_id);
    }

    /// The spawn failed: the ghost returns to rest, clickable again.
    ///
    /// Without this, one failed revival would brick the row forever in
    /// `Reviving` — inert to every later click for no reason the user can see.
    pub fn abort_revival(&mut self, agent_id: &str) {
        if let Some(row) = self.records.iter_mut().find(|r| r.agent_id == agent_id) {
            if row.revival == RevivalState::Reviving {
                row.revival = RevivalState::Dormant;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, closed_at: u64) -> ClosedAgentRecord {
        ClosedAgentRecord {
            agent_id: id.to_string(),
            label: format!("agent {id}"),
            cwd: Some(PathBuf::from("/tmp")),
            workspace_key: None,
            session: crate::agent_resume::AgentSessionRef::id(format!("session-{id}")).map(
                |session_ref| crate::agent_resume::PersistedAgentSession {
                    source: "herdr:claude".into(),
                    agent: "claude".into(),
                    session_ref,
                },
            ),
            closed_at,
            revival: RevivalState::Dormant,
            label_explicit: false,
            never_spoke: false,
        }
    }

    // TP-AGPANEL-53: a headstone that only says which KIND of agent died —
    // `Claude`, `claude`, `(untitled)`, `<short id> · claude` — is a
    // placeholder, and a placeholder adopts the conversation's title the
    // moment something knows one. A real name is never overwritten, a lookup
    // that knows nothing changes nothing, and another placeholder is not an
    // answer.
    #[test]
    fn a_placeholder_headstone_adopts_the_conversations_title() {
        let mut ledger = ClosedAgentLedger::default();
        for (id, label) in [
            ("kind", "Claude"),
            ("canon", "claude"),
            ("untitled", "(untitled)"),
            ("shortid", "shortid · claude"),
            ("named", "Adres listesi diff analizi"),
            ("unknown", "claude"),
            ("still", "claude"),
        ] {
            let mut record = record(id, 1);
            record.label = label.to_string();
            // TP-AGPANEL-63: a name is kept because it was written on purpose.
            record.label_explicit = id == "named";
            ledger.record_closed(record);
        }
        let titles: std::collections::HashMap<&str, &str> = [
            ("kind", "Sync sırasında adresler eksiliyor"),
            ("canon", " MEGA CONTEXT HOOK "),
            ("untitled", "Windows laptop OTDR routine"),
            ("shortid", "Kitty terminalini kapattık"),
            ("named", "some later title"),
            ("still", "(untitled)"),
        ]
        .into_iter()
        .collect();

        let changed = ledger.adopt_titles(|id| titles.get(id).map(|t| t.to_string()));

        assert_eq!(changed, 4);
        let label = |id: &str| {
            ledger
                .entries()
                .find(|r| r.agent_id == id)
                .map(|r| r.label.clone())
                .expect("record")
        };
        assert_eq!(label("kind"), "Sync sırasında adresler eksiliyor");
        assert_eq!(label("canon"), "MEGA CONTEXT HOOK", "titles arrive trimmed");
        assert_eq!(label("untitled"), "Windows laptop OTDR routine");
        assert_eq!(label("shortid"), "Kitty terminalini kapattık");
        assert_eq!(
            label("named"),
            "Adres listesi diff analizi",
            "a name written on purpose is never overwritten"
        );
        assert_eq!(
            label("unknown"),
            "claude",
            "nothing known: the placeholder stays, honestly"
        );
        assert_eq!(
            label("still"),
            "claude",
            "another placeholder is not an answer"
        );
    }

    // TP-AGPANEL-63: a headstone wears its conversation's CURRENT title. The
    // label froze at close time and only labels that LOOKED like placeholders
    // were ever repaired — measured 2026-09-19 on the reporting machine, 39 of
    // 115 headstones wore something the conversation is no longer called: the
    // directory name a fresh chat tab is seeded with (the home directory's own name, ten of them), an
    // AI title the chat has since outgrown, a terminal title
    // (`Not logged in · Please run /login`). None of those has a placeholder's
    // shape, so none could heal. What decides now is what was RECORDED: a name
    // written on purpose is never touched, every other label follows the
    // title the moment something knows one.
    #[test]
    fn a_headstone_follows_its_conversations_title_unless_it_was_named_on_purpose() {
        let mut ledger = ClosedAgentLedger::default();
        for (id, label, explicit) in [
            ("dir", "tester", false),
            ("stale", "An AI title from the first minute", false),
            ("terminal", "Not logged in · Please run /login", false),
            ("purpose", "benim adım", true),
            ("unknown", "tester", false),
            ("still", "tester", false),
            ("same", "Zaten güncel", false),
        ] {
            let mut record = record(id, 1);
            record.label = label.to_string();
            record.label_explicit = explicit;
            ledger.record_closed(record);
        }
        let titles: std::collections::HashMap<&str, &str> = [
            ("dir", "Mezar taşı adları neden bayat"),
            ("stale", "PRD-147 sohbet her yerden açılır"),
            ("terminal", " Giriş sorunu çözümü "),
            ("purpose", "konuşmanın kendi başlığı"),
            ("still", "(untitled)"),
            ("same", "Zaten güncel"),
        ]
        .into_iter()
        .collect();
        let mut asked: Vec<String> = Vec::new();

        let changed = ledger.adopt_titles(|id| {
            asked.push(id.to_string());
            titles.get(id).map(|t| t.to_string())
        });

        let label = |id: &str| {
            ledger
                .entries()
                .find(|r| r.agent_id == id)
                .map(|r| r.label.clone())
                .expect("record")
        };
        assert_eq!(label("dir"), "Mezar taşı adları neden bayat");
        assert_eq!(label("stale"), "PRD-147 sohbet her yerden açılır");
        assert_eq!(
            label("terminal"),
            "Giriş sorunu çözümü",
            "titles arrive trimmed"
        );
        assert_eq!(
            label("purpose"),
            "benim adım",
            "a name written on purpose is never overwritten"
        );
        assert_eq!(label("unknown"), "tester", "nothing known: the label stays");
        assert_eq!(
            label("still"),
            "tester",
            "a placeholder title is not an answer"
        );
        assert_eq!(changed, 3, "an unchanged label is not a change");
        assert!(
            !asked.iter().any(|id| id == "purpose"),
            "a caller that opens a file per answer is never asked about a name written on purpose"
        );
    }

    // TP-AGPANEL-64: whether a name was written on purpose is RECORDED, and
    // the record survives the store. A file written before the field existed
    // reads as `false` — nothing recorded the intent, so the label follows the
    // conversation — and a headstone that is not explicit does not pay for the
    // field on disk.
    #[test]
    fn a_name_written_on_purpose_survives_the_store() {
        let mut ledger = ClosedAgentLedger::default();
        let mut named = record("named", 2);
        named.label_explicit = true;
        // The ring is newest-RECORDED first: `named` closes last, stands first.
        ledger.record_closed(record("plain", 1));
        ledger.record_closed(named);

        let stored = ledger.to_stored();
        let json = serde_json::to_value(&stored).expect("json");
        assert_eq!(json[0]["label_explicit"], serde_json::json!(true));
        assert!(
            json[1].get("label_explicit").is_none(),
            "absent when false: {json}"
        );

        let mut restored = ClosedAgentLedger::default();
        restored.load_stored(stored);
        let explicit: Vec<(String, bool)> = restored
            .entries()
            .map(|r| (r.agent_id.clone(), r.label_explicit))
            .collect();
        assert_eq!(
            explicit,
            vec![("named".to_string(), true), ("plain".to_string(), false)]
        );

        let old: Vec<crate::persist::closed_agents::StoredClosedAgent> = serde_json::from_str(
            r#"[{"agent_id":"old","label":"written before the field","closed_at":5}]"#,
        )
        .expect("an older file still reads");
        assert!(!old[0].label_explicit);
    }

    // TP-AGPANEL-65: order and age come from ONE clock — the last time
    // anything saw the conversation alive. `closed_at` only knew the deaths
    // the server witnessed: a chat resumed through another door and then lost
    // with the server (no close to witness) kept the hour of its FIRST death —
    // measured 2026-09-19, first in the PWA's history and seventeenth in the
    // panel. A later sighting pulls the headstone forward and the ring is
    // re-ordered; an earlier one takes nothing back; a headstone with no
    // conversation is never asked about.
    #[test]
    fn a_headstones_clock_follows_the_last_sighting_of_its_conversation() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 100));
        ledger.record_closed(record("b", 200));
        let mut sessionless = record("pane-7", 250);
        sessionless.session = None;
        ledger.record_closed(sessionless);
        ledger.record_closed(record("c", 300));
        assert_eq!(ids(&ledger), ["c", "pane-7", "b", "a"], "precondition");

        let mut asked: Vec<String> = Vec::new();
        let changed = ledger.adopt_sightings(|session| {
            asked.push(session.to_string());
            match session {
                "session-a" => Some(500),
                "session-b" => Some(150),
                _ => None,
            }
        });

        assert_eq!(changed, 1, "only the later sighting is a change");
        assert_eq!(ids(&ledger), ["a", "c", "pane-7", "b"]);
        let clock = |id: &str| {
            ledger
                .entries()
                .find(|r| r.agent_id == id)
                .map(|r| r.closed_at)
        };
        assert_eq!(clock("a"), Some(500), "the age reads the same clock");
        assert_eq!(
            clock("b"),
            Some(200),
            "an earlier sighting takes nothing back"
        );
        asked.sort();
        assert_eq!(
            asked,
            ["session-a", "session-b", "session-c"],
            "asked by SESSION, and never for a headstone that has none"
        );
        assert_eq!(
            ledger.adopt_sightings(|_| Some(500)),
            2,
            "b and c move; a is already there"
        );
        assert_eq!(ledger.adopt_sightings(|_| Some(500)), 0, "and then nothing");
    }

    // TP-AGPANEL-67 (the weighing): a Claude headstone is weighed against the
    // transcript store — no transcript anywhere, or one nobody spoke in, means
    // there is no conversation behind it. Other agents keep their sessions
    // elsewhere and are never asked; a headstone named on purpose is weighed
    // like any other but keeps its name.
    #[test]
    fn a_headstone_is_weighed_against_its_transcript() {
        let mut ledger = ClosedAgentLedger::default();
        for id in ["spoke", "silent", "missing", "purpose"] {
            let mut r = record(id, 1);
            r.label = "tester".to_string();
            r.label_explicit = id == "purpose";
            ledger.record_closed(r);
        }
        let mut codex = record("codex", 1);
        if let Some(session) = codex.session.as_mut() {
            session.agent = "codex".to_string();
            session.source = "herdr:codex".to_string();
        }
        ledger.record_closed(codex);
        let mut sessionless = record("pane-3", 1);
        sessionless.session = None;
        ledger.record_closed(sessionless);

        // Asked by SESSION — the transcript is filed under the conversation's
        // id, which is only usually the headstone's.
        let mut asked: Vec<(String, Option<PathBuf>)> = Vec::new();
        let changed = ledger.adopt_conversation_facts(|session, cwd| {
            asked.push((session.to_string(), cwd.map(PathBuf::from)));
            match session {
                "session-spoke" => Some(ConversationFacts {
                    title: Some("Gerçek bir konuşma".to_string()),
                    spoke: true,
                }),
                "session-purpose" => Some(ConversationFacts {
                    title: Some("konuşmanın başlığı".to_string()),
                    spoke: true,
                }),
                "session-silent" => Some(ConversationFacts {
                    title: None,
                    spoke: false,
                }),
                _ => None,
            }
        });

        fn of(ledger: &ClosedAgentLedger, id: &str) -> (String, bool) {
            ledger
                .entries()
                .find(|r| r.agent_id == id)
                .map(|r| (r.label.clone(), r.never_spoke))
                .expect("record")
        }
        assert_eq!(
            of(&ledger, "spoke"),
            ("Gerçek bir konuşma".to_string(), false)
        );
        assert_eq!(
            of(&ledger, "purpose"),
            ("tester".to_string(), false),
            "weighed, not renamed"
        );
        assert_eq!(of(&ledger, "silent"), ("tester".to_string(), true));
        assert_eq!(
            of(&ledger, "missing"),
            ("tester".to_string(), true),
            "no transcript anywhere"
        );
        assert!(
            !of(&ledger, "codex").1,
            "another agent's store is not ours to judge"
        );
        assert!(!of(&ledger, "pane-3").1);
        assert_eq!(changed, 3, "one renamed, two found silent");
        let mut ids_asked: Vec<&str> = asked.iter().map(|(id, _)| id.as_str()).collect();
        ids_asked.sort_unstable();
        assert_eq!(
            ids_asked,
            [
                "session-missing",
                "session-purpose",
                "session-silent",
                "session-spoke"
            ]
        );
        assert!(
            asked
                .iter()
                .all(|(_, cwd)| cwd.as_deref() == Some(std::path::Path::new("/tmp"))),
            "the record's directory rides along as the hint"
        );

        // Weighed again after someone spoke: the headstone comes back.
        let changed = ledger.adopt_conversation_facts(|session, _| {
            (session != "session-missing").then_some(ConversationFacts {
                title: None,
                spoke: true,
            })
        });
        assert_eq!(changed, 1);
        assert!(!of(&ledger, "silent").1);
        assert!(of(&ledger, "missing").1);
    }

    fn ids(ledger: &ClosedAgentLedger) -> Vec<String> {
        ledger.entries().map(|r| r.agent_id.clone()).collect()
    }

    // TP-AGPANEL-34: revival state describes THIS process, so it must not
    // survive one. A spawn that was in flight when the server was replaced did
    // not survive the replacement, and a row that came back claiming
    // `Reviving` would be inert forever — the claim it waits on belongs to a
    // process that no longer exists, and the row would refuse every click with
    // `RevivalInFlight`.
    #[test]
    fn a_loaded_ghost_starts_dormant_whatever_it_was_doing() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 1));
        assert!(
            ledger.try_begin_revival("a"),
            "precondition: the ghost is mid-revival when the store is written"
        );

        let mut restored = ClosedAgentLedger::default();
        restored.load_stored(ledger.to_stored());

        assert_eq!(
            restored.entries().next().map(|r| r.revival),
            Some(RevivalState::Dormant),
            "a ghost that comes back mid-revival could never be clicked again"
        );
    }

    // TP-AGPANEL-38: a month of loaded history is not thrown away by the next
    // agent that closes. The ring's capacity was eight, sized for the panel's
    // height — but the panel enforces its own height, and with a store behind
    // it that eight became silent data loss: load the month, close one agent,
    // and the month is gone. Found by mutation while the store was being
    // written, not by any test that existed.
    #[test]
    fn a_loaded_history_survives_the_next_close() {
        let mut written = ClosedAgentLedger::default();
        for i in 0..50u64 {
            written.record_closed(record(&format!("old{i}"), 1_000 + i));
        }
        let stored = written.to_stored();
        assert_eq!(stored.len(), 50, "precondition: fifty deaths were kept");

        let mut ledger = ClosedAgentLedger::default();
        ledger.load_stored(stored);
        ledger.record_closed(record("fresh", 9_999));

        assert_eq!(
            ledger.entries().count(),
            51,
            "the newest death joins the history instead of replacing it"
        );
        assert_eq!(
            ledger.entries().next().map(|r| r.agent_id.as_str()),
            Some("fresh"),
            "and it lands at the front"
        );
    }

    // TP-AGPANEL-37: everything a revival needs crosses the disk. The cwd is
    // the whole difference between reopening where the user worked and
    // reopening in `$HOME` (#46), and the session ref is what reattaches the
    // conversation instead of presenting a stranger — a ghost that survives a
    // restart without them is a row that can only refuse.
    #[test]
    fn a_round_trip_keeps_what_a_revival_needs() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("keep", 42));

        let mut restored = ClosedAgentLedger::default();
        restored.load_stored(ledger.to_stored());

        let before = ledger.entries().next().expect("written");
        let after = restored.entries().next().expect("read back");
        assert_eq!(after.agent_id, before.agent_id);
        assert_eq!(after.label, before.label);
        assert_eq!(
            after.cwd, before.cwd,
            "the revival directory crosses the disk"
        );
        assert_eq!(after.closed_at, before.closed_at);
        assert_eq!(
            after.session.as_ref().map(|s| s.session_ref.value.clone()),
            before.session.as_ref().map(|s| s.session_ref.value.clone()),
            "the resume key crosses the disk"
        );
        assert_eq!(
            after.session.as_ref().map(|s| s.session_ref.kind),
            before.session.as_ref().map(|s| s.session_ref.kind),
            "an id and a transcript path resume differently; the kind must survive"
        );
    }

    // TP-AGPANEL-07: the graveyard is newest first — "recently closed" is an
    // ordering claim, and without it the words on the panel lie.
    #[test]
    fn a_closed_agent_is_remembered_newest_first() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 1));
        ledger.record_closed(record("b", 2));
        assert_eq!(ids(&ledger), vec!["b", "a"]);
    }

    // TP-AGPANEL-08: one identity, one ghost. Closing the same agent again
    // refreshes the row (newest position, dormant again) instead of adding a
    // twin the user cannot tell from the original.
    #[test]
    fn closing_the_same_agent_again_refreshes_the_row_instead_of_duplicating() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 1));
        ledger.record_closed(record("b", 2));
        assert!(ledger.try_begin_revival("a"));
        let mut again = record("a", 3);
        again.revival = RevivalState::Reviving; // yazan taraf ne derse desin —
        ledger.record_closed(again); // taze bir ölüm Dormant başlar
        assert_eq!(ids(&ledger), vec!["a", "b"]);
        let a = ledger.entries().next().expect("a is present");
        assert_eq!(a.closed_at, 3);
        assert_eq!(a.revival, RevivalState::Dormant);
    }

    // TP-AGPANEL-09: the ring is bounded. Closing agents is a daily gesture
    // by the hundreds; without eviction the grey list is a leak with a UI.
    #[test]
    fn the_ledger_evicts_its_oldest_beyond_capacity() {
        let mut ledger = ClosedAgentLedger::default();
        for i in 0..=CLOSED_AGENT_CAPACITY {
            ledger.record_closed(record(&format!("g{i}"), i as u64));
        }
        let remembered = ids(&ledger);
        assert_eq!(remembered.len(), CLOSED_AGENT_CAPACITY);
        assert!(
            !remembered.contains(&"g0".to_string()),
            "en eski tahliye edilir"
        );
        // Derived from the capacity, not written out: the bound moved from
        // eight (the panel's row count) to the store's, and a hardcoded "g8"
        // pinned this test to a number that no longer means anything.
        let newest = format!("g{CLOSED_AGENT_CAPACITY}");
        assert_eq!(
            remembered.first().map(String::as_str),
            Some(newest.as_str())
        );
    }

    // TP-AGPANEL-10: revival is a one-way claim, not a debounce. The second
    // caller gets `false` from the state itself — no window, however fast the
    // clicks or slow the machine, in which two spawns can both win.
    #[test]
    fn a_second_revival_attempt_while_one_is_in_flight_is_inert() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 1));
        assert!(
            ledger.try_begin_revival("a"),
            "ilk tıklama spawn hakkını alır"
        );
        assert!(
            !ledger.try_begin_revival("a"),
            "uçuştayken ikinci tıklama atıldır"
        );
        assert!(!ledger.try_begin_revival("yok"), "bilinmeyen hedef atıldır");
    }

    // TP-AGPANEL-11: a landed revival leaves the graveyard; a failed one
    // returns to rest. Either way the row never sticks in `Reviving` — a
    // ghost bricked by one failure would be inert to every later click.
    #[test]
    fn a_finished_revival_leaves_and_a_failed_one_rests_again() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a", 1));
        ledger.record_closed(record("b", 2));

        assert!(ledger.try_begin_revival("a"));
        ledger.resolve_revival("a");
        assert_eq!(ids(&ledger), vec!["b"], "inen diriltme mezarlıktan çıkar");

        assert!(ledger.try_begin_revival("b"));
        ledger.abort_revival("b");
        assert!(
            ledger.try_begin_revival("b"),
            "başarısız diriltme yeniden tıklanabilir"
        );
    }

    // TP-AGPANEL-67 (the gesture): a headstone nobody spoke behind is not
    // revived — `claude --resume` on it answers "No conversation found"
    // (measured 2026-09-19), so the refusal says so instead of spawning a tab
    // that dies. But the flag is a fact about the transcript store AS OF the
    // last weighing, and a refusal from a stale fact would be a bug of its
    // own: the gesture weighs again first, and a conversation someone has
    // spoken in since then revives like any other.
    #[tokio::test]
    async fn a_conversation_nobody_spoke_in_is_weighed_again_before_it_is_refused() {
        let root = std::env::temp_dir().join(format!(
            "herdr-never-spoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = root.join("repo");
        let projects = root.join("projects");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&projects).expect("projects");
        let mut app = revival_test_app();
        // The conversation's home tab still stands with an idle shell, so a
        // revival that goes ahead is typed into it (TP-AGPANEL-56) — no agent
        // is ever launched by this test.
        let tab_idx = app.state.workspaces[0].test_add_tab(None);
        app.state.ensure_test_terminals();
        app.state.workspaces[0].tabs[tab_idx].resumed_session_id = Some("s-quiet".into());
        let tab = &app.state.workspaces[0].tabs[tab_idx];
        let terminal_id = tab.panes[&tab.root_pane].attached_terminal_id.clone();
        let (runtime, mut typed) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let mut quiet = record("s-quiet", 1);
        quiet.agent_id = "s-quiet".to_string();
        quiet.cwd = Some(cwd.clone());
        quiet.session = crate::agent_resume::AgentSessionRef::id("s-quiet").map(|session_ref| {
            crate::agent_resume::PersistedAgentSession {
                source: "herdr:claude".into(),
                agent: "claude".into(),
                session_ref,
            }
        });
        quiet.never_spoke = true;
        app.state.closed_agents.record_closed(quiet);

        // 1. Still nothing in the store: refused, with its own reason.
        assert_eq!(
            app.revive_closed_agent_in("s-quiet", Some(&projects)),
            Err(RevivalRefusal::NothingToResume)
        );
        assert_eq!(RevivalRefusal::NothingToResume.code(), "nothing_to_resume");
        assert!(typed.try_recv().is_err(), "nothing was launched");
        assert_eq!(
            app.state.closed_agents.entries().next().map(|r| r.revival),
            Some(RevivalState::Dormant),
            "a refusal leaves the row at rest"
        );
        // 2. No store to ask (a `--no-session` run): the flag is all there is.
        assert_eq!(
            app.revive_closed_agent_in("s-quiet", None),
            Err(RevivalRefusal::NothingToResume)
        );

        // 3. Someone has spoken since: the same gesture revives.
        let dir = projects.join(crate::claude_sessions::encode_project_path(
            &cwd.to_string_lossy(),
        ));
        std::fs::create_dir_all(&dir).expect("project dir");
        std::fs::write(
            dir.join("s-quiet.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"artık konuştuk\"}}\n",
        )
        .expect("transcript");
        assert_eq!(
            app.revive_closed_agent_in("s-quiet", Some(&projects)),
            Ok(())
        );
        assert!(
            app.state.closed_agents.entries().next().is_none(),
            "the landed revival left the graveyard"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn revival_test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("revive")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    // TP-AGPANEL-18: a revival that cannot answer for its directory is
    // refused with a reason, and the refusal leaves the row dormant — still
    // clickable for the moment the answer changes. Guessing instead of
    // refusing is the exact #46 failure ($HOME) this feature exists to bury.
    #[test]
    fn a_revival_without_a_livable_directory_is_refused_and_stays_clickable() {
        let mut app = revival_test_app();
        let mut no_cwd = record("no-cwd", 1);
        no_cwd.cwd = None;
        app.state.closed_agents.record_closed(no_cwd);
        let mut gone = record("gone", 2);
        gone.cwd = Some(PathBuf::from("/nonexistent/definitely-gone-herdr-test"));
        app.state.closed_agents.record_closed(gone);

        assert_eq!(
            app.revive_closed_agent("no-cwd"),
            Err(RevivalRefusal::NoCwd)
        );
        assert_eq!(
            app.revive_closed_agent("gone"),
            Err(RevivalRefusal::CwdGone)
        );
        assert_eq!(
            app.revive_closed_agent("nobody"),
            Err(RevivalRefusal::UnknownGhost)
        );
        // Every refusal left its row dormant: a later claim still succeeds…
        assert!(app.state.closed_agents.try_begin_revival("no-cwd"));
        assert!(app.state.closed_agents.try_begin_revival("gone"));
        // …and a row already claimed reports the flight instead of racing it.
        assert_eq!(
            app.revive_closed_agent("gone"),
            Err(RevivalRefusal::RevivalInFlight)
        );
    }

    // TP-AGPANEL-73: a headstone opens where it is labelled. A filing (a
    // move, or a module seat through the module's directory) outranks the
    // directory it closed in; that outranks a drawer that lists it; a
    // directory that is gone is passed over; and nothing left is `None`.
    #[test]
    fn a_headstone_opens_where_it_is_labelled() {
        use crate::persist::workspace_chats::{ChatRecord, WorkspaceChatLedger, WorkspaceChats};
        let closed = PathBuf::from("/work/closed");
        let listed = PathBuf::from("/work/listed");
        let module = PathBuf::from("/work/module");
        let everything = |_: &std::path::Path| true;
        let docs = |node: &str| (node == "docs").then(|| module.clone());
        let mut ledger = WorkspaceChatLedger::default();
        ledger.workspaces.insert(
            listed.to_string_lossy().into_owned(),
            WorkspaceChats {
                chats: vec![ChatRecord {
                    session_id: "s".into(),
                    agent: "claude".into(),
                    source: "herdr:claude".into(),
                    kind: "id".into(),
                    first_seen_ms: 1,
                    last_seen_ms: 2,
                }],
            },
        );
        assert_eq!(
            revival_home("s", Some(&closed), &ledger, &docs, &everything),
            Some(closed.clone()),
            "unfiled: where it closed outranks a drawer"
        );
        ledger.moves.insert("s".into(), "module:docs".into());
        assert_eq!(
            revival_home("s", Some(&closed), &ledger, &docs, &everything),
            Some(module.clone()),
            "a module seat is where it is labelled"
        );
        let only_listed = |dir: &std::path::Path| dir == listed.as_path();
        assert_eq!(
            revival_home("s", Some(&closed), &ledger, &docs, &only_listed),
            Some(listed.clone()),
            "gone directories are passed over"
        );
        let nothing = |_: &std::path::Path| false;
        assert_eq!(
            revival_home("s", Some(&closed), &ledger, &docs, &nothing),
            None,
            "and nothing left is nothing — never $HOME"
        );
        assert_eq!(
            revival_home(
                "s",
                None,
                &WorkspaceChatLedger::default(),
                &docs,
                &everything
            ),
            None
        );
    }

    // TP-AGPANEL-19: a ghost that closed without a session recipe has nothing
    // to resume. The click is refused with a reason instead of silently
    // launching a stranger and calling it a revival.
    #[test]
    fn a_revival_without_a_session_recipe_is_refused() {
        let mut app = revival_test_app();
        let mut ghost = record("recipe-less", 1);
        ghost.session = None;
        app.state.closed_agents.record_closed(ghost);
        assert_eq!(
            app.revive_closed_agent("recipe-less"),
            Err(RevivalRefusal::NoSessionRecipe)
        );
        assert!(app.state.closed_agents.try_begin_revival("recipe-less"));
    }

    // TP-AGPANEL-20: a ghost whose conversation already lives in a wired tab
    // must not spawn a twin (#45's lesson wearing grey). The revival rides
    // the chat road's own guard: it focuses the wired tab, and the ghost
    // leaves the graveyard — clicking again is no longer a revival at all.
    // TP-AGPANEL-56 sharpened "already lives": the agent must actually be
    // running there — a wired tab with an idle shell is the home the agent is
    // brought back into (see `projects.rs`), not a place to merely look at.
    #[test]
    fn a_ghost_whose_session_is_already_open_focuses_it_instead_of_twinning() {
        let mut app = revival_test_app();
        app.state.workspaces[0].test_add_tab(None);
        app.state.ensure_test_terminals();
        app.state.workspaces[0].tabs[1].resumed_session_id = Some("session-open".into());
        let running = app.state.workspaces[0].tabs[1].panes
            [&app.state.workspaces[0].tabs[1].root_pane]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&running)
            .expect("test terminal")
            .set_agent_name("session-open".to_string());
        let before = app.state.workspaces[0].tabs.len();

        app.state.closed_agents.record_closed(record("open", 1));
        assert_eq!(app.revive_closed_agent("open"), Ok(()));

        assert_eq!(app.state.workspaces[0].tabs.len(), before, "no twin tab");
        assert_eq!(
            app.state.workspaces[0].active_tab_index(),
            1,
            "the wired tab took focus"
        );
        assert!(
            app.state.closed_agents.entries().next().is_none(),
            "a revival that landed leaves the graveyard"
        );
        assert_eq!(
            app.revive_closed_agent("open"),
            Err(RevivalRefusal::UnknownGhost),
            "the next click is ordinary focus territory, not revival"
        );
    }

    // TP-AGPANEL-21: every ghost revives along its own agent's existing road —
    // claude with the chat drawer's exact launch (flags and env included),
    // codex with the restore road's plan. A revival argv invented here would
    // be a third road for the next change to forget.
    #[test]
    fn a_revival_launch_matches_the_agents_existing_road() {
        let claude = crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("abc").expect("valid id"),
        };
        let (argv, env) = revival_launch(&claude).expect("claude revives");
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".into(),
                "--resume".into(),
                "abc".into(),
            ]
        );
        assert!(env.iter().any(|(key, _)| key == "ENABLE_BACKGROUND_TASKS"));

        let codex = crate::agent_resume::PersistedAgentSession {
            source: "herdr:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("xyz").expect("valid id"),
        };
        let (argv, env) = revival_launch(&codex).expect("codex revives");
        assert_eq!(
            argv,
            vec!["codex".to_string(), "resume".into(), "xyz".into()]
        );
        assert!(env.is_empty());
    }
}

#[cfg(test)]
mod forget_tests {
    use super::{ClosedAgentLedger, ClosedAgentRecord, RevivalState};

    fn record(id: &str) -> ClosedAgentRecord {
        ClosedAgentRecord {
            agent_id: id.to_string(),
            label: format!("agent {id}"),
            cwd: Some(std::path::PathBuf::from("/tmp")),
            workspace_key: None,
            session: None,
            closed_at: 1,
            revival: RevivalState::Dormant,
            label_explicit: false,
            never_spoke: false,
        }
    }

    // G4 / TP-AGPANEL-45: one row goes, the rest stay. Removing a headstone is
    // not clearing the graveyard, and 62 records were standing on the machine
    // this was reported from with no way to remove any of them.
    #[test]
    fn forgetting_one_ghost_leaves_the_others() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a"));
        ledger.record_closed(record("b"));
        ledger.record_closed(record("c"));

        assert!(ledger.forget("b"));

        let ids: Vec<String> = ledger
            .entries()
            .map(|entry| entry.agent_id.clone())
            .collect();
        assert_eq!(ids, vec!["c".to_string(), "a".to_string()]);
    }

    // TP-AGPANEL-45: forgetting a row that is not there reports nothing rather
    // than claiming a removal — the menu can outlive the list it came from.
    #[test]
    fn forgetting_an_unknown_ghost_reports_nothing() {
        let mut ledger = ClosedAgentLedger::default();
        ledger.record_closed(record("a"));

        assert!(!ledger.forget("nobody"));
        assert_eq!(ledger.entries().count(), 1);
    }
}
