//! Agents started OUTSIDE herdr: noticed while they live, remembered when
//! they leave.
//!
//! The graveyard used to learn about a conversation from two places only: a
//! herdr road closing one of its own panes, and the drawer rows read once at
//! startup (TP-AGPANEL-39). A `claude` opened in another terminal and closed
//! an hour later reached neither — the server that saw it leave had started
//! days before. Measured 2026-09-24: the server had run five days and 277 of
//! 284 conversations touched since its start had no headstone.
//!
//! Claude Code keeps one file per live interactive process in
//! `~/.claude/sessions/<pid>.json` (`sessionId`, `cwd`, `name`, `kind`,
//! `procStart`) and removes it when the process exits. That directory is the
//! contract read here. Nothing polls it: a native watcher marks it dirty, and
//! the next scheduler pass rescans a directory of a few dozen small files.
//! What left since the previous look — and was not hosted by a herdr pane,
//! whose own close road already births the headstone — is recorded like any
//! other close (TP-AGPANEL-69..72).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_full::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer, RecommendedCache,
};
use tokio::sync::Notify;

/// Claude writes its session file several times per turn; one look per quiet
/// second is plenty for a question answered in minutes.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(1000);

/// One live interactive Claude process, as its own session file describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveClaudeSession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    /// The name Claude shows for the conversation (user-given or derived).
    pub name: Option<String>,
    /// Whether the process runs inside a herdr pane (its env carries
    /// `HERDR_PANE_ID`). Such an agent's close is herdr's own road.
    pub herdr_hosted: bool,
}

/// `<home>/.claude/sessions`, beside the transcript store.
pub(crate) fn claude_sessions_dir(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    crate::claude_sessions::claude_projects_dir(env)
        .and_then(|projects| projects.parent().map(|claude| claude.join("sessions")))
}

/// What a process probe answers about one session file.
pub(crate) struct ProcessProbe<'a> {
    /// Whether `pid` is still the process that wrote the file (`procStart`,
    /// when the file carries one, must match the live process's start time).
    pub alive: &'a dyn Fn(u32, Option<&str>) -> bool,
    /// Whether `pid` runs inside a herdr pane.
    pub herdr_hosted: &'a dyn Fn(u32) -> bool,
}

/// Read every live interactive session in `dir`.
///
/// Only `<digits>.json` files count (Syncthing's `*.sync-conflict-*.json`
/// copies are stale history, not processes); the file's `pid` must name the
/// file; `kind` must be `interactive` (a scripted `claude -p` is nobody's
/// agent); the session id must be a plain token and the cwd absolute. A file
/// whose process is gone is a crash leftover and is not live. Never panics:
/// an unreadable directory or file is simply not there.
pub(crate) fn read_live_sessions(dir: &Path, probe: &ProcessProbe<'_>) -> Vec<LiveClaudeSession> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(stem) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(session) = parse_session_file(pid, &content, probe) {
            live.push(session);
        }
    }
    live.sort_by_key(|session| session.pid);
    live
}

fn parse_session_file(
    pid: u32,
    content: &str,
    probe: &ProcessProbe<'_>,
) -> Option<LiveClaudeSession> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    if value.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(pid)) {
        return None;
    }
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("interactive") {
        return None;
    }
    let session_id = value.get("sessionId").and_then(serde_json::Value::as_str)?;
    // The id names a transcript file and may be typed into a shell on
    // revival: the same plain-token lock `chat.open` uses (TP-AGPANEL-57).
    if !crate::app::chat_open::valid_chat_open_id(session_id) {
        return None;
    }
    let cwd = PathBuf::from(value.get("cwd").and_then(serde_json::Value::as_str)?);
    if !cwd.is_absolute() {
        return None;
    }
    let proc_start = value.get("procStart").and_then(serde_json::Value::as_str);
    if !(probe.alive)(pid, proc_start) {
        return None;
    }
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    Some(LiveClaudeSession {
        pid,
        session_id: session_id.to_string(),
        cwd,
        name,
        herdr_hosted: (probe.herdr_hosted)(pid),
    })
}

/// The real probe: `/proc` on Linux. Elsewhere a session file is taken at its
/// word — Claude removes it on a clean exit, so only a crash leftover lies.
pub(crate) fn system_probe() -> ProcessProbe<'static> {
    ProcessProbe {
        alive: &process_alive,
        herdr_hosted: &process_herdr_hosted,
    }
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32, proc_start: Option<&str>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(expected) = proc_start else {
        return true;
    };
    // Field 2 (comm) may hold spaces and parentheses; everything after the
    // LAST ')' starts at field 3, so starttime (field 22) is the 20th token.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .is_some_and(|start| start == expected)
}

#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32, _proc_start: Option<&str>) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn process_herdr_hosted(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
        environ
            .split(|byte| *byte == 0)
            .any(|var| var.starts_with(b"HERDR_PANE_ID="))
    })
}

#[cfg(not(target_os = "linux"))]
fn process_herdr_hosted(_pid: u32) -> bool {
    false
}

/// What the tracker remembers about the processes it saw alive.
#[derive(Default)]
pub(crate) struct ExternalAgentTracker {
    seen: HashMap<u32, LiveClaudeSession>,
    watch: Option<SessionsWatch>,
    started: bool,
}

struct SessionsWatch {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    dirty: Arc<AtomicBool>,
}

impl ExternalAgentTracker {
    /// Fold one look at the live sessions in; answer the ones that LEFT and
    /// were not herdr's own (TP-AGPANEL-69/70).
    ///
    /// Keyed by process, not by session: a process that compacts or forks its
    /// conversation keeps its pid and reports the newer id, and the headstone
    /// must carry the conversation it was in when it left. Hosting is sticky —
    /// once a process was seen inside a herdr pane it stays herdr's.
    pub(crate) fn observe(&mut self, live: Vec<LiveClaudeSession>) -> Vec<LiveClaudeSession> {
        let mut next: HashMap<u32, LiveClaudeSession> = HashMap::with_capacity(live.len());
        for mut session in live {
            if let Some(previous) = self.seen.get(&session.pid) {
                session.herdr_hosted |= previous.herdr_hosted;
                if session.name.is_none() {
                    session.name.clone_from(&previous.name);
                }
            }
            next.insert(session.pid, session);
        }
        let mut departed: Vec<LiveClaudeSession> = self
            .seen
            .drain()
            .filter(|(pid, _)| !next.contains_key(pid))
            .map(|(_, session)| session)
            .filter(|session| !session.herdr_hosted)
            .collect();
        departed.sort_by_key(|session| session.pid);
        self.seen = next;
        departed
    }

    /// Whether the directory changed since the last look (always true for the
    /// very first one). Starts the watcher on first use.
    fn take_due(&mut self, dir: &Path, wake: &Arc<Notify>) -> bool {
        if !self.started {
            self.started = true;
            self.watch = start_watch(dir, wake.clone());
            return true;
        }
        self.watch
            .as_ref()
            .is_some_and(|watch| watch.dirty.swap(false, Ordering::AcqRel))
    }
}

fn start_watch(dir: &Path, wake: Arc<Notify>) -> Option<SessionsWatch> {
    let dirty = Arc::new(AtomicBool::new(false));
    let flag = dirty.clone();
    let mut debouncer = new_debouncer(WATCH_DEBOUNCE, None, move |result: DebounceEventResult| {
        if result.is_ok_and(|events| !events.is_empty()) {
            flag.store(true, Ordering::Release);
            wake.notify_one();
        }
    })
    .map_err(|err| tracing::warn!(%err, "external agents: watcher unavailable"))
    .ok()?;
    debouncer
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(|err| {
            tracing::warn!(dir = %dir.display(), %err, "external agents: cannot watch sessions dir");
        })
        .ok()?;
    Some(SessionsWatch {
        _debouncer: debouncer,
        dirty,
    })
}

/// The headstone an external departure leaves: the same shape the drawer seed
/// derives (TP-AGPANEL-39), closed now, labelled with Claude's own name for the
/// conversation until the transcript offers a title.
pub(crate) fn headstone_for(
    session: &LiveClaudeSession,
    closed_at: u64,
) -> crate::app::closed_agents::ClosedAgentRecord {
    let label = session.name.clone().unwrap_or_else(|| {
        session
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "claude".to_string())
    });
    crate::app::closed_agents::ClosedAgentRecord {
        agent_id: session.session_id.clone(),
        label,
        cwd: Some(session.cwd.clone()),
        workspace_key: None,
        session: Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef {
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: session.session_id.clone(),
            },
        }),
        closed_at,
        revival: crate::app::closed_agents::RevivalState::Dormant,
        label_explicit: false,
        never_spoke: false,
    }
}

impl super::App {
    /// Record the external agents that left since the last look.
    ///
    /// Runs on the scheduler pass but costs nothing between events: the
    /// watcher's flag gates the rescan. Suppressed on `--no-session`, the
    /// same terms the store's reads and writes use (TP-AGPANEL-35/36).
    pub(crate) fn sync_external_agents(&mut self) -> bool {
        if self.no_session() {
            return false;
        }
        let Some(dir) = claude_sessions_dir(|key| std::env::var_os(key)) else {
            return false;
        };
        if !self.external_agents.take_due(&dir, &self.render_notify) {
            return false;
        }
        let projects_dir = self.headstone_projects_dir();
        let probe = system_probe();
        let recorded = self
            .note_external_departures_in(read_live_sessions(&dir, &probe), projects_dir.as_deref());
        if recorded > 0 {
            self.save_closed_agents();
        }
        recorded > 0
    }

    /// Fold one look in and give each departure its headstone; answers how
    /// many were recorded. The store is not written here (the caller owns the
    /// trigger's write), so a test never reaches a real config directory.
    pub(crate) fn note_external_departures_in(
        &mut self,
        live: Vec<LiveClaudeSession>,
        projects_dir: Option<&Path>,
    ) -> usize {
        let departed = self.external_agents.observe(live);
        let now = crate::persist::workspace_chats::now_ms();
        let mut recorded = 0;
        for session in departed {
            self.state
                .closed_agents
                .record_closed(headstone_for(&session, now));
            if let Some(dir) = projects_dir {
                self.state
                    .closed_agents
                    .weigh(&session.session_id, |id, cwd| {
                        crate::app::closed_agents::conversation_facts_in(dir, id, cwd)
                    });
            }
            recorded += 1;
        }
        if recorded > 0 {
            self.state.sync_closed_agent_labels();
        }
        recorded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(pid: u32, session: &str, hosted: bool) -> LiveClaudeSession {
        LiveClaudeSession {
            pid,
            session_id: session.to_string(),
            cwd: PathBuf::from("/work/proj"),
            name: Some(format!("chat {pid}")),
            herdr_hosted: hosted,
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-external-agents-{tag}-{}-{}",
            std::process::id(),
            crate::persist::workspace_chats::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session_json(pid: u32, session: &str, kind: &str, cwd: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{session}","cwd":"{cwd}","procStart":"42","kind":"{kind}","name":"Named chat"}}"#
        )
    }

    // TP-AGPANEL-69: the sessions directory is read by its contract — only
    // `<pid>.json` files, only interactive processes, only a pid that names
    // its own file, only a live process — and nothing else becomes an agent.
    #[test]
    fn only_live_interactive_session_files_are_agents() {
        let dir = unique_dir("read");
        let session = "11111111-2222-4333-8444-555555555555";
        std::fs::write(
            dir.join("100.json"),
            session_json(100, session, "interactive", "/w"),
        )
        .unwrap();
        std::fs::write(
            dir.join("101.sync-conflict-20260803-193632-X.json"),
            session_json(101, session, "interactive", "/w"),
        )
        .unwrap();
        std::fs::write(
            dir.join("102.json"),
            session_json(102, session, "print", "/w"),
        )
        .unwrap();
        std::fs::write(
            dir.join("103.json"),
            session_json(999, session, "interactive", "/w"),
        )
        .unwrap();
        std::fs::write(
            dir.join("104.json"),
            session_json(104, session, "interactive", "rel"),
        )
        .unwrap();
        std::fs::write(
            dir.join("105.json"),
            session_json(105, "../x", "interactive", "/w"),
        )
        .unwrap();
        std::fs::write(
            dir.join("106.json"),
            session_json(106, session, "interactive", "/w"),
        )
        .unwrap();
        std::fs::write(dir.join("107.json"), "{ torn").unwrap();
        std::fs::write(dir.join("100.abc.key"), "k").unwrap();

        let alive = |pid: u32, start: Option<&str>| pid != 106 && start == Some("42");
        let hosted = |pid: u32| pid == 100;
        let probe = ProcessProbe {
            alive: &alive,
            herdr_hosted: &hosted,
        };
        let found = read_live_sessions(&dir, &probe);
        assert_eq!(
            found,
            vec![LiveClaudeSession {
                pid: 100,
                session_id: session.to_string(),
                cwd: PathBuf::from("/w"),
                name: Some("Named chat".to_string()),
                herdr_hosted: true,
            }]
        );
        assert!(read_live_sessions(&dir.join("missing"), &probe).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    // TP-AGPANEL-70: a departure is a process that was seen alive and is no
    // longer; herdr's own panes are left to herdr's close road, and hosting,
    // once seen, is sticky.
    #[test]
    fn departures_are_the_external_processes_that_left() {
        let mut tracker = ExternalAgentTracker::default();
        assert!(tracker
            .observe(vec![live(1, "s-one", false), live(2, "s-two", true)])
            .is_empty());
        // pid 2 reports unhosted this time; it stays herdr's.
        assert!(tracker
            .observe(vec![live(1, "s-one", false), live(2, "s-two", false)])
            .is_empty());
        let departed = tracker.observe(Vec::new());
        assert_eq!(
            departed.len(),
            1,
            "only the external one leaves a headstone"
        );
        assert_eq!(departed[0].session_id, "s-one");
        assert!(
            tracker.observe(Vec::new()).is_empty(),
            "a departure is reported once"
        );
    }

    // TP-AGPANEL-70: keyed by process — a process that moved to a newer
    // conversation leaves with the conversation it was last in.
    #[test]
    fn a_process_leaves_with_its_latest_conversation() {
        let mut tracker = ExternalAgentTracker::default();
        tracker.observe(vec![live(7, "s-old", false)]);
        tracker.observe(vec![live(7, "s-new", false)]);
        let departed = tracker.observe(Vec::new());
        assert_eq!(departed.len(), 1);
        assert_eq!(departed[0].session_id, "s-new");
    }

    // TP-AGPANEL-71: an external departure becomes an ordinary headstone —
    // directory, Claude recipe and name frozen — newest first, and a
    // conversation already remembered is refreshed rather than twinned.
    #[tokio::test]
    async fn an_external_departure_leaves_a_revivable_headstone() {
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        assert_eq!(
            app.note_external_departures_in(vec![live(5, "s-five", false)], None),
            0,
            "a first sighting is not a death"
        );
        assert_eq!(app.note_external_departures_in(Vec::new(), None), 1);
        let ghost = app
            .state
            .closed_agents
            .entries()
            .next()
            .expect("the departure is remembered")
            .clone();
        assert_eq!(ghost.agent_id, "s-five");
        assert_eq!(ghost.label, "chat 5");
        assert_eq!(ghost.cwd.as_deref(), Some(Path::new("/work/proj")));
        let recipe = ghost.session.expect("a claude recipe");
        assert_eq!(recipe.agent, "claude");
        assert_eq!(recipe.session_ref.value, "s-five");
        assert!(!ghost.label_explicit);

        app.note_external_departures_in(vec![live(6, "s-five", false)], None);
        app.note_external_departures_in(Vec::new(), None);
        assert_eq!(
            app.state
                .closed_agents
                .entries()
                .filter(|record| record.agent_id == "s-five")
                .count(),
            1,
            "the same conversation closing again refreshes its one row"
        );
    }

    // TP-AGPANEL-72: `--no-session` never reads the machine's real sessions
    // directory, and never writes the machine's real store.
    #[tokio::test]
    async fn a_sessionless_run_does_not_watch_external_agents() {
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        assert!(!app.sync_external_agents());
        assert!(!app.external_agents.started);
    }

    #[test]
    fn the_sessions_dir_sits_beside_the_transcript_store() {
        let env = |key: &str| (key == "HOME").then(|| std::ffi::OsString::from("/fixture-home"));
        assert_eq!(
            claude_sessions_dir(env),
            Some(PathBuf::from("/fixture-home/.claude/sessions"))
        );
    }
}
