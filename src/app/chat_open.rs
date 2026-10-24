//! Opening a conversation by its identity — `chat.open`.
//!
//! The TUI opens a chat from a drawer row, and the row knows where the chat
//! lives. A client without those drawers (the PWA's history list) knows only
//! the conversation's id, and until this door existed its one road back was
//! the graveyard (`closed_agent.revive`). The graveyard is incomplete by
//! design — a chore never earns a headstone (TP-DAILY-25), a chat that never
//! ran under herdr has none — and measured on the reporting machine
//! (2026-09-19) seventeen of fifty-eight listed conversations answered with a
//! refusal instead of an agent.
//!
//! An identity is enough: every fact a resume needs is either frozen in a
//! headstone, written in the ledger by the person who filed the chat, or
//! recorded by the transcript itself. This module resolves them in that order
//! and then walks the road every drawer row walks
//! (`open_project_chat_tab_with_argv`, TP-AGPANEL-56) — no second spawn path.

use std::path::{Path, PathBuf};

use crate::api::schema::ChatOpenRoad;
use crate::app::closed_agents::RevivalRefusal;
use crate::persist::workspace_chats::WorkspaceChatLedger;

/// Longest id `chat.open` accepts. A Claude session id is a 36-character
/// uuid; the ceiling only exists so a hostile caller cannot make the store
/// probe with a megabyte of "id".
pub(crate) const MAX_CHAT_OPEN_ID_LEN: usize = 128;

/// Why a conversation was not opened. Every variant is a reason a person can
/// read — the alternative to refusing is guessing, and #46 measured where a
/// guessed directory lands (`$HOME`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatOpenRefusal {
    /// The id is not shaped like one; nothing was looked up.
    InvalidSessionId,
    /// No transcript carries this id: there is nothing to resume.
    TranscriptMissing,
    /// Nothing says where the conversation lived, or that place is gone.
    CwdUnknown,
    /// A headstone remembered the conversation and its revival refused.
    Revival(RevivalRefusal),
    /// The road produced no tab wired to the conversation.
    SpawnFailed,
}

impl ChatOpenRefusal {
    /// Stable machine-readable code for the API surface.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidSessionId => "invalid_session_id",
            Self::TranscriptMissing => "chat_transcript_missing",
            Self::CwdUnknown => "chat_cwd_unknown",
            Self::Revival(refusal) => refusal.code(),
            Self::SpawnFailed => "chat_open_spawn_failed",
        }
    }

    /// The visible reason behind the code.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidSessionId => "the session id is not shaped like one",
            Self::TranscriptMissing => "no transcript carries this session id",
            Self::CwdUnknown => "nothing records a directory this conversation can resume in",
            Self::Revival(refusal) => refusal.message(),
            Self::SpawnFailed => "opening the conversation produced no tab",
        }
    }
}

/// What opening a conversation came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatOpened {
    pub road: ChatOpenRoad,
    /// The root pane of the tab that hosts the conversation now, so a client
    /// can go there without waiting for the agent to report itself.
    pub pane_id: Option<String>,
}

/// Whether `id` may be looked up at all.
///
/// The launch can be TYPED into an idle shell (TP-AGPANEL-56), and the id
/// names a file in the transcript store. Both are quoted and probed safely
/// already; this is the second lock on the same door, and it costs nothing.
pub(crate) fn valid_chat_open_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CHAT_OPEN_ID_LEN
        && !id.starts_with('.')
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// Where a person — or a plan they applied — FILED a conversation: the
/// ledger's `moves`, a module seat resolving through the module's own
/// directory. `None` when it was never filed, or the filing no longer names a
/// directory. The first answer of [`resolve_chat_directory`], and the first
/// answer a revival asks (TP-AGPANEL-73): a headstone opens where it is
/// labelled, not only where it happened to close.
pub(crate) fn filed_chat_directory(
    session_id: &str,
    ledger: &WorkspaceChatLedger,
    module_dir: &dyn Fn(&str) -> Option<PathBuf>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let key = ledger.moves.get(session_id)?;
    let filed = match crate::persist::workspace_chats::module_node_key(key) {
        Some(node) => module_dir(node),
        None => Some(PathBuf::from(key)),
    };
    filed.filter(|dir| is_dir(dir))
}

/// The directories whose drawers list a conversation, newest sighting first
/// (the key breaks ties so the answer never depends on map order); the
/// second answer of [`resolve_chat_directory`].
pub(crate) fn listed_chat_directory(
    session_id: &str,
    ledger: &WorkspaceChatLedger,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut listed: Vec<(&String, u64)> = ledger
        .workspaces
        .iter()
        .filter(|(key, _)| crate::persist::workspace_chats::module_node_key(key).is_none())
        .filter_map(|(key, workspace)| {
            workspace
                .chats
                .iter()
                .find(|chat| chat.session_id == session_id)
                .map(|chat| (key, chat.last_seen_ms))
        })
        .collect();
    listed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    listed
        .into_iter()
        .map(|(key, _)| PathBuf::from(key))
        .find(|dir| is_dir(dir))
}

/// Where a conversation is resumed when no headstone remembers.
///
/// In the order of who knows best: the person (or plan) who FILED the chat
/// (`moves`, the same directory the TUI's module/branch door opens it in),
/// then the directory whose drawer lists it (newest sighting first, the
/// `open_workspace_chat` meaning), then the transcript's own launch
/// directory. A candidate that is not a directory any more is passed over —
/// and when nothing is left the answer is `None`, never a guess.
pub(crate) fn resolve_chat_directory(
    session_id: &str,
    ledger: &WorkspaceChatLedger,
    transcript_cwd: Option<&Path>,
    module_dir: &dyn Fn(&str) -> Option<PathBuf>,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // 1. Where a person — or a plan they applied — filed it.
    if let Some(dir) = filed_chat_directory(session_id, ledger, module_dir, is_dir) {
        return Some(dir);
    }
    // 2. The directory whose drawer lists it, newest sighting first.
    if let Some(dir) = listed_chat_directory(session_id, ledger, is_dir) {
        return Some(dir);
    }
    // 3. Where the transcript says the conversation started.
    transcript_cwd
        .filter(|dir| is_dir(dir))
        .map(Path::to_path_buf)
}

impl super::App {
    /// Open the conversation `session_id` names, wherever it has to come from.
    ///
    /// `launch` builds the resume recipe; production passes the chat drawer's
    /// own (`project_chat_launch`), tests pass a harmless one.
    pub(crate) fn open_chat_by_session_with(
        &mut self,
        session_id: &str,
        projects_dir: Option<&Path>,
        launch: &dyn Fn(&str) -> crate::app::closed_agents::LaunchRecipe,
    ) -> Result<ChatOpened, ChatOpenRefusal> {
        if !valid_chat_open_id(session_id) {
            return Err(ChatOpenRefusal::InvalidSessionId);
        }
        // One session, one surface: a conversation that is running is looked
        // at, never resumed a second time against the same transcript.
        if let Some((ws_idx, tab_idx)) = self.state.find_running_chat_tab(session_id) {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
            return Ok(ChatOpened {
                road: ChatOpenRoad::Focused,
                pane_id: self.chat_tab_root_pane_id(session_id),
            });
        }
        // A headstone froze the directory and the recipe at close time; no
        // lookup can derive better than that. Whether it is a Claude
        // conversation decides if the transcript store has a say first.
        let ghost = self
            .state
            .closed_agents
            .entries()
            .find(|record| {
                record.agent_id == session_id || record.session_value() == Some(session_id)
            })
            .map(|record| {
                let claude = record
                    .session
                    .as_ref()
                    .is_none_or(|session| session.agent == "claude");
                (record.agent_id.clone(), claude)
            });
        if let Some((agent_id, false)) = &ghost {
            self.revive_closed_agent_in(agent_id, projects_dir)
                .map_err(ChatOpenRefusal::Revival)?;
            return Ok(ChatOpened {
                road: ChatOpenRoad::Revived,
                pane_id: self.chat_tab_root_pane_id(session_id),
            });
        }
        // A conversation nobody wrote down cannot be resumed: `claude
        // --resume` answers "No conversation found" (measured), and a tab
        // opened onto that error reads as the product being broken.
        let facts = projects_dir
            .and_then(|dir| crate::claude_sessions::read_launch_facts_for_session(dir, session_id))
            .ok_or(ChatOpenRefusal::TranscriptMissing)?;
        if let Some((agent_id, _)) = &ghost {
            self.revive_closed_agent_in(agent_id, projects_dir)
                .map_err(ChatOpenRefusal::Revival)?;
            return Ok(ChatOpened {
                road: ChatOpenRoad::Revived,
                pane_id: self.chat_tab_root_pane_id(session_id),
            });
        }
        let project_path = resolve_chat_directory(
            session_id,
            &self.workspace_chat_ledger,
            facts.cwd.as_deref(),
            &|node| self.state.module_directory_for_key(node),
            &|dir| dir.is_dir(),
        )
        .ok_or(ChatOpenRefusal::CwdUnknown)?;
        let (argv, extra_env) = launch(session_id);
        self.open_project_chat_tab_with_argv(
            crate::app::state::ProjectChatTabRequest {
                project_path,
                session_id: Some(session_id.to_string()),
            },
            &argv,
            extra_env,
        );
        // The road wires whichever tab it spawned to the session; that wiring
        // is its own receipt, exactly as it is for a revival.
        if self.state.find_resumed_chat_tab(session_id).is_none() {
            return Err(ChatOpenRefusal::SpawnFailed);
        }
        Ok(ChatOpened {
            road: ChatOpenRoad::Resumed,
            pane_id: self.chat_tab_root_pane_id(session_id),
        })
    }

    /// The public id of the root pane of the tab wired to `session_id`.
    fn chat_tab_root_pane_id(&self, session_id: &str) -> Option<String> {
        let (ws_idx, tab_idx) = self.state.find_resumed_chat_tab(session_id)?;
        let tab = self.state.workspaces.get(ws_idx)?.tabs.get(tab_idx)?;
        self.public_pane_id(ws_idx, tab.root_pane)
    }

    /// The production door: Claude's own resume recipe.
    pub(crate) fn open_chat_by_session(
        &mut self,
        session_id: &str,
        projects_dir: Option<&Path>,
    ) -> Result<ChatOpened, ChatOpenRefusal> {
        self.open_chat_by_session_with(session_id, projects_dir, &|id| {
            super::projects::project_chat_launch("claude", Some(id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::persist::workspace_chats::{ChatRecord, WorkspaceChats};
    use crate::workspace::Workspace;

    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-chat-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// A transcript store with one conversation that says where it started.
    fn store_with_transcript(root: &Path, session_id: &str, cwd: &Path) -> PathBuf {
        let projects = root.join("projects");
        let slug = crate::claude_sessions::encode_project_path(&cwd.to_string_lossy());
        std::fs::create_dir_all(projects.join(&slug)).expect("create project dir");
        let line = serde_json::json!({
            "type": "user",
            "sessionId": session_id,
            "cwd": cwd.to_string_lossy(),
            "message": { "role": "user", "content": "merhaba" },
        });
        std::fs::write(
            projects.join(slug).join(format!("{session_id}.jsonl")),
            format!("{line}\n"),
        )
        .expect("write transcript");
        projects
    }

    fn sh_launch(_: &str) -> crate::app::closed_agents::LaunchRecipe {
        (
            vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            Vec::new(),
        )
    }

    fn record(session_id: &str, last_seen_ms: u64) -> ChatRecord {
        ChatRecord {
            session_id: session_id.to_string(),
            agent: "claude".to_string(),
            source: "herdr:claude".to_string(),
            kind: "id".to_string(),
            first_seen_ms: 1,
            last_seen_ms,
        }
    }

    // TP-AGPANEL-57: the id is looked at before anything is looked up. It can
    // be typed into a shell and it names a file in the store; both roads are
    // safe on their own, and this is the lock that does not depend on them.
    #[test]
    fn a_chat_open_id_is_a_plain_token_or_it_is_nothing() {
        for good in [SESSION, "rollout-2026_09.19", "a"] {
            assert!(valid_chat_open_id(good), "{good:?} is a plain token");
        }
        let too_long = "a".repeat(MAX_CHAT_OPEN_ID_LEN + 1);
        for bad in [
            "",
            ".hidden",
            "../escape",
            "a/b",
            "a\\b",
            "a b",
            "a;rm -rf x",
            "a$(id)",
            "a\nb",
            "çok",
            too_long.as_str(),
        ] {
            assert!(!valid_chat_open_id(bad), "{bad:?} must be refused");
        }
    }

    // TP-AGPANEL-58: where a conversation resumes, in the order of who knows
    // best — the person who filed it, the drawer that lists it, the
    // transcript itself — and never a guess (#46 measured where guesses land).
    #[test]
    fn a_conversation_resumes_where_it_was_filed_then_listed_then_started() {
        let filed = PathBuf::from("/work/filed");
        let listed_old = PathBuf::from("/work/listed-old");
        let listed_new = PathBuf::from("/work/listed-new");
        let started = PathBuf::from("/work/started");
        let module = PathBuf::from("/work/module-dir");
        let everything_exists = |_: &Path| true;
        let no_module = |_: &str| None;

        let mut ledger = WorkspaceChatLedger::default();
        for (dir, seen) in [(&listed_old, 10), (&listed_new, 20)] {
            ledger.workspaces.insert(
                dir.to_string_lossy().into_owned(),
                WorkspaceChats {
                    chats: vec![record(SESSION, seen), record("someone-else", 99)],
                },
            );
        }
        let resolve = |ledger: &WorkspaceChatLedger,
                       cwd: Option<&Path>,
                       module_dir: &dyn Fn(&str) -> Option<PathBuf>,
                       is_dir: &dyn Fn(&Path) -> bool| {
            resolve_chat_directory(SESSION, ledger, cwd, module_dir, is_dir)
        };

        assert_eq!(
            resolve(&ledger, Some(&started), &no_module, &everything_exists),
            Some(listed_new.clone()),
            "the drawer that saw it last outranks where it started"
        );

        ledger
            .moves
            .insert(SESSION.to_string(), filed.to_string_lossy().into_owned());
        assert_eq!(
            resolve(&ledger, Some(&started), &no_module, &everything_exists),
            Some(filed.clone()),
            "a filing decision outranks every observation"
        );

        ledger
            .moves
            .insert(SESSION.to_string(), "module:docs".to_string());
        let docs = |node: &str| (node == "docs").then(|| module.clone());
        assert_eq!(
            resolve(&ledger, Some(&started), &docs, &everything_exists),
            Some(module.clone()),
            "a module seat resolves through the module's own directory"
        );
        assert_eq!(
            resolve(&ledger, Some(&started), &no_module, &everything_exists),
            Some(listed_new.clone()),
            "a module without a directory is passed over, not guessed"
        );

        let only_started_exists = |path: &Path| path == started.as_path();
        assert_eq!(
            resolve(&ledger, Some(&started), &docs, &only_started_exists),
            Some(started.clone()),
            "directories that are gone are passed over"
        );
        let nothing_exists = |_: &Path| false;
        assert_eq!(
            resolve(&ledger, Some(&started), &docs, &nothing_exists),
            None,
            "and when nothing is left the answer is nothing — never $HOME"
        );
        assert_eq!(
            resolve(
                &WorkspaceChatLedger::default(),
                None,
                &no_module,
                &everything_exists
            ),
            None
        );
    }

    // TP-AGPANEL-59: refusals are reasons. A malformed id looks nothing up, a
    // conversation nobody wrote down is not resumed into a doomed
    // `claude --resume`, and a conversation with nowhere to live is refused
    // rather than opened in whatever directory happens to be active.
    #[tokio::test]
    async fn chat_open_refuses_with_a_reason_and_spawns_nothing() {
        let root = unique_dir("refuse");
        let gone = root.join("gone");
        let projects = store_with_transcript(&root, SESSION, &gone);
        let mut app = test_app();
        app.state.workspaces = vec![Workspace::test_new("proj")];
        app.state.active = Some(0);
        let tabs_before = app.state.workspaces[0].tabs.len();

        assert_eq!(
            app.open_chat_by_session_with("../x", Some(&projects), &sh_launch),
            Err(ChatOpenRefusal::InvalidSessionId)
        );
        assert_eq!(
            app.open_chat_by_session_with(
                "99999999-0000-4000-8000-000000000000",
                Some(&projects),
                &sh_launch
            ),
            Err(ChatOpenRefusal::TranscriptMissing)
        );
        assert_eq!(
            app.open_chat_by_session_with(SESSION, None, &sh_launch),
            Err(ChatOpenRefusal::TranscriptMissing),
            "no store at all is a missing transcript, not a crash"
        );
        assert_eq!(
            app.open_chat_by_session_with(SESSION, Some(&projects), &sh_launch),
            Err(ChatOpenRefusal::CwdUnknown),
            "the transcript's directory is gone and nothing else knows one"
        );
        assert_eq!(
            app.state.workspaces[0].tabs.len(),
            tabs_before,
            "a refusal spawns nothing"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // TP-AGPANEL-60: a conversation no headstone remembers still opens — in
    // the directory its transcript recorded, wired to its session, through
    // the road every drawer row walks. Asking again focuses what is there:
    // one session, one surface.
    #[tokio::test]
    async fn a_conversation_without_a_headstone_opens_where_its_transcript_started() {
        let root = unique_dir("resume");
        let home = root.join("checkout");
        std::fs::create_dir_all(&home).expect("create checkout");
        let projects = store_with_transcript(&root, SESSION, &home);
        let mut app = test_app();
        let mut ws = Workspace::test_new("proj");
        ws.identity_cwd = home.clone();
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;

        let opened = app
            .open_chat_by_session_with(SESSION, Some(&projects), &sh_launch)
            .expect("the conversation opens");
        assert_eq!(opened.road, ChatOpenRoad::Resumed);
        let (ws_idx, tab_idx) = app
            .state
            .find_resumed_chat_tab(SESSION)
            .expect("a tab is wired to the conversation");
        let tab = &app.state.workspaces[ws_idx].tabs[tab_idx];
        assert_eq!(
            opened.pane_id,
            app.public_pane_id(ws_idx, tab.root_pane),
            "the answer names the pane a client can go to"
        );
        assert!(opened.pane_id.is_some());
        let tabs_after_first = app.state.workspaces[ws_idx].tabs.len();

        // The wired tab's agent is "running" for as long as its launch is
        // pending; a second ask must not grow a twin.
        let again = app.open_chat_by_session_with(SESSION, Some(&projects), &sh_launch);
        assert!(again.is_ok(), "asking twice is not an error: {again:?}");
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            tabs_after_first,
            "one session, one surface — no twin tab"
        );
        crate::app::api::test_support::shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(root);
    }

    // TP-AGPANEL-61: a headstone knows more than any lookup can derive — the
    // directory and the recipe frozen at close — so when one remembers the
    // conversation, `chat.open` IS its revival: same road, same refusals, and
    // the landed revival leaves the graveyard.
    #[tokio::test]
    async fn chat_open_rides_the_headstone_when_one_remembers_the_conversation() {
        let root = unique_dir("ghost");
        let frozen = root.join("frozen-cwd");
        let elsewhere = root.join("transcript-cwd");
        std::fs::create_dir_all(&frozen).expect("create frozen cwd");
        std::fs::create_dir_all(&elsewhere).expect("create transcript cwd");
        let projects = store_with_transcript(&root, SESSION, &elsewhere);
        let mut app = test_app();
        let mut ws = Workspace::test_new("proj");
        ws.identity_cwd = frozen.clone();
        ws.test_add_tab(None);
        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        // The conversation's home tab still stands with an idle shell: the
        // revival is typed back into it (TP-AGPANEL-56) and lands as success.
        app.state.workspaces[0].tabs[1].resumed_session_id = Some(SESSION.into());
        let tab = &app.state.workspaces[0].tabs[1];
        let terminal_id = tab.panes[&tab.root_pane].attached_terminal_id.clone();
        let (runtime, _typed) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);
        app.state
            .closed_agents
            .record_closed(crate::app::closed_agents::ClosedAgentRecord {
                agent_id: SESSION.into(),
                label: "bir konuşma".into(),
                cwd: Some(frozen.clone()),
                workspace_key: None,
                session: crate::agent_resume::AgentSessionRef::id(SESSION).map(|session_ref| {
                    crate::agent_resume::PersistedAgentSession {
                        source: "herdr:claude".into(),
                        agent: "claude".into(),
                        session_ref,
                    }
                }),
                closed_at: 7,
                revival: crate::app::closed_agents::RevivalState::Dormant,
                label_explicit: false,
                never_spoke: false,
            });

        let opened = app
            .open_chat_by_session_with(SESSION, Some(&projects), &sh_launch)
            .expect("the headstone revives");
        assert_eq!(opened.road, ChatOpenRoad::Revived);
        assert!(
            app.state.closed_agents.entries().next().is_none(),
            "the landed revival left the graveyard"
        );

        // A headstone whose directory is gone refuses with the revival's own
        // reason — the transcript's directory is NOT quietly used instead:
        // the record is the better witness and it says the place is gone.
        let mut gone = app_record_with_cwd(SESSION, root.join("missing"));
        gone.closed_at = 9;
        app.state.workspaces[0].tabs[1].resumed_session_id = None;
        app.state.closed_agents.record_closed(gone);
        assert_eq!(
            app.open_chat_by_session_with(SESSION, Some(&projects), &sh_launch),
            Err(ChatOpenRefusal::Revival(RevivalRefusal::CwdGone))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // TP-AGPANEL-62: the door is a protocol method like revival is
    // (TP-AGPANEL-24): it parses from the wire by its own name, counts as a
    // request that changes what people see, answers a refusal with a stable
    // code, and says which road it took when it lands.
    #[tokio::test]
    async fn chat_open_travels_the_api_road() {
        let request: crate::api::schema::Request = serde_json::from_str(
            r#"{"id":"req_open","method":"chat.open","params":{"session_id":"../x"}}"#,
        )
        .expect("chat.open parses from the wire");
        assert!(matches!(
            request.method,
            crate::api::schema::Method::ChatOpen(_)
        ));
        assert!(crate::api::request_changes_ui(&request));

        let mut app = test_app();
        let response = app.handle_api_request(request);
        assert!(
            response.contains("invalid_session_id"),
            "a refusal names its reason: {response}"
        );

        let landed = serde_json::to_value(crate::api::schema::ResponseResult::ChatOpened {
            road: ChatOpenRoad::Resumed,
            pane_id: Some("w1:p2".into()),
        })
        .expect("serialises");
        assert_eq!(
            landed,
            serde_json::json!({ "type": "chat_opened", "road": "resumed", "pane_id": "w1:p2" })
        );
    }

    fn app_record_with_cwd(
        session_id: &str,
        cwd: PathBuf,
    ) -> crate::app::closed_agents::ClosedAgentRecord {
        crate::app::closed_agents::ClosedAgentRecord {
            agent_id: session_id.into(),
            label: "bir konuşma".into(),
            cwd: Some(cwd),
            workspace_key: None,
            session: crate::agent_resume::AgentSessionRef::id(session_id).map(|session_ref| {
                crate::agent_resume::PersistedAgentSession {
                    source: "herdr:claude".into(),
                    agent: "claude".into(),
                    session_ref,
                }
            }),
            closed_at: 1,
            revival: crate::app::closed_agents::RevivalState::Dormant,
            label_explicit: false,
            never_spoke: false,
        }
    }
}
