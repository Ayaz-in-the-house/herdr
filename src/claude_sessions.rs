//! Claude Code session (chat) reader for the Projects sidebar tab.
//!
//! Claude Code stores each project's chat sessions under
//! `<home>/.claude/projects/<encoded>/*.jsonl`, where every `.jsonl` file is one
//! chat session and the directory name is the project's absolute path with every
//! non-ASCII-alphanumeric character replaced by `-`.
//!
//! The encoding was verified against real on-disk data (51/52 local projects
//! matched; the single miss was a session that `cd`-ed away from its start
//! directory, and `ssh-<uuid>` directories are remote sessions). Because the
//! encoding collapses `/`, `.`, ` `, `_`, `-`, … all to `-`, it is LOSSY and
//! therefore NEVER reversed — we only ever go project-path -> directory.
//!
//! This module is TUI/client-layer pure data (CLAUDE.md boundary): no PTYs, no
//! runtime state, no network. It never panics on malformed input.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One Claude Code chat session belonging to a project.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaudeSession {
    /// Session id = the `.jsonl` file stem (a UUID).
    pub id: String,
    /// Display title: `custom-title` > `ai-title` > first user message > "(untitled)".
    pub title: String,
    /// File modification time. An upper bound on activity: a resume that
    /// appends a bookkeeping line refreshes it without any message being sent.
    pub last_modified: SystemTime,
    /// TP-DRAW-15: when the last user/assistant message in the transcript was
    /// written, from the line's own `timestamp` field. `None` for a transcript
    /// with no timestamped messages (older formats). This — not the file's
    /// mtime and never the ledger's sighting — is what "how old is this chat"
    /// means to a person reading the list.
    pub last_message_at: Option<SystemTime>,
    /// Number of user + assistant turns (a rough activity signal).
    pub msg_count: usize,
    /// Normalised opening: the *shape* of the first user message.
    ///
    /// Separate from `title` on purpose. A title answers "what is this chat
    /// called" and prefers whatever a human or the model named it; an opening
    /// answers "how was this chat started", which is the only thing that tells
    /// a scheduled task apart from a person typing. When a chat carries an
    /// `ai-title`, the first user message never reaches the title at all — so
    /// deriving one from the other would lose exactly the cases that matter.
    ///
    /// `None` when the chat has no readable first user message.
    pub opening: Option<String>,
}

impl ClaudeSession {
    /// The moment this chat last moved, as a reader understands "moved":
    /// the last message's own timestamp when the transcript records one,
    /// else the file mtime (TP-DRAW-15).
    pub fn activity_time(&self) -> SystemTime {
        self.last_message_at.unwrap_or(self.last_modified)
    }
}

/// Placeholder shown when a session has no derivable title.
pub const UNTITLED: &str = "(untitled)";

/// Maximum displayed title length (in characters) before truncation.
const MAX_TITLE_CHARS: usize = 80;

/// How much of the first user message is kept after normalising.
///
/// Long enough to separate two automations that share an opening word, short
/// enough that a message which merely *starts* the same way still collapses to
/// one shape — the measured scheduled tasks differ within their first line.
const MAX_OPENING_CHARS: usize = 120;

/// How much raw text is examined before normalising, so a pathological single
/// line cannot make this scan expensive.
const OPENING_SCAN_CHARS: usize = 400;

/// Reduce a chat's first user message to the *shape* of how it was started.
///
/// Two runs of the same automation open with the same words and differ only in
/// the parts that change every run: a timestamp, a uuid, a path, a counter.
/// Masking those is what lets one shape be recognised across runs — measured on
/// real data, a single scheduled task appeared as two distinct spellings (39
/// and 32 occurrences) that differed only by a date.
///
/// The result is lowercase, whitespace-collapsed and truncated. It is a
/// grouping key, never shown to anyone, and never reversed.
pub fn normalise_opening(text: &str) -> String {
    let scanned: String = text.chars().take(OPENING_SCAN_CHARS).collect();

    let mut out = String::with_capacity(scanned.len());
    let mut rest = scanned.as_str();

    // Order matters: uuids before dates (a uuid contains dash groups a date
    // pattern could bite into), paths before numbers (a path carries digits),
    // numbers last. Each replacement writes a token that no later rule can
    // match again, which is what keeps this idempotent.
    while !rest.is_empty() {
        if let Some(len) = uuid_prefix_len(rest) {
            out.push_str("<id>");
            rest = &rest[len..];
        } else if let Some(len) = date_prefix_len(rest) {
            out.push_str("<date>");
            rest = &rest[len..];
        } else if let Some(len) = path_prefix_len(rest) {
            out.push_str("<path>");
            rest = &rest[len..];
        } else if let Some(len) = digits_prefix_len(rest) {
            out.push_str("<n>");
            rest = &rest[len..];
        } else {
            let ch = match rest.chars().next() {
                Some(ch) => ch,
                None => break,
            };
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }

    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .to_lowercase()
        .chars()
        .take(MAX_OPENING_CHARS)
        .collect()
}

/// Byte length of a `8-4-4-4-12` hex uuid at the start of `s`, if there is one.
fn uuid_prefix_len(s: &str) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = s.as_bytes();
    let mut at = 0usize;
    for (index, group) in GROUPS.iter().enumerate() {
        if index > 0 {
            if bytes.get(at) != Some(&b'-') {
                return None;
            }
            at += 1;
        }
        for _ in 0..*group {
            match bytes.get(at) {
                Some(byte) if byte.is_ascii_hexdigit() => at += 1,
                _ => return None,
            }
        }
    }
    // A longer hex run means this was not a uuid but some other token.
    match bytes.get(at) {
        Some(byte) if byte.is_ascii_hexdigit() => None,
        _ => Some(at),
    }
}

/// Byte length of a `YYYY-MM-DD` date at the start of `s`, if there is one.
fn date_prefix_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let digits = |at: usize, count: usize| {
        (0..count).all(|offset| bytes.get(at + offset).is_some_and(u8::is_ascii_digit))
    };
    if digits(0, 4) && bytes.get(4) == Some(&b'-') && digits(5, 2) && bytes.get(7) == Some(&b'-')
    // A date is exactly ten characters; anything longer is a different token.
        && digits(8, 2)
        && !bytes.get(10).is_some_and(u8::is_ascii_digit)
    {
        Some(10)
    } else {
        None
    }
}

/// Byte length of an absolute path at the start of `s`, if there is one.
///
/// Requires at least one character after the leading slash so a bare `/` (a
/// division sign, a sentence's slash) is left alone.
fn path_prefix_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'/') {
        return None;
    }
    let mut at = 1usize;
    while let Some(byte) = bytes.get(at) {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-') {
            at += 1;
        } else {
            break;
        }
    }
    (at > 1).then_some(at)
}

/// Byte length of a run of digits at the start of `s`, if there is one.
fn digits_prefix_len(s: &str) -> Option<usize> {
    let len = s.bytes().take_while(u8::is_ascii_digit).count();
    (len > 0).then_some(len)
}

/// Encode an absolute project path into its Claude Code storage directory name.
///
/// Rule (empirically verified): every character that is not ASCII alphanumeric
/// becomes `-`. This is LOSSY and NOT reversible — callers must only go
/// path -> directory, never the reverse.
pub fn encode_project_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Resolve `<home>/.claude/projects` using an injectable env lookup so the reader
/// is testable against a fake HOME without touching the real `~/.claude`.
pub(crate) fn claude_projects_dir(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = env("HOME").map(PathBuf::from)?;
    if home.as_os_str().is_empty() {
        return None;
    }
    Some(home.join(".claude").join("projects"))
}

/// Convenience wrapper over [`claude_projects_dir`] using the real process env.
pub fn default_claude_projects_dir() -> Option<PathBuf> {
    claude_projects_dir(|k| std::env::var_os(k))
}

/// Read every chat session for `project_path`, newest first.
///
/// `projects_dir` is the `.../.claude/projects` root (injected for testability).
/// Never panics: a missing/unreadable project directory yields an empty list,
/// and malformed session files are skipped individually.
// Production callers go through the cached/limited variant; this full read
// remains the reference behavior exercised by this module's tests.
#[cfg(test)]
pub fn read_sessions_for_project(projects_dir: &Path, project_path: &str) -> Vec<ClaudeSession> {
    read_recent_sessions_for_project(projects_dir, project_path, usize::MAX).0
}

#[cfg(test)]
pub fn read_recent_sessions_for_project(
    projects_dir: &Path,
    project_path: &str,
    limit: usize,
) -> (Vec<ClaudeSession>, usize) {
    read_recent_inner(projects_dir, project_path, limit, None)
}

/// What the store readers remember between passes: how far each transcript
/// has been read and what its lines added up to, and where every transcript
/// stands by id.
///
/// TP-CHATSYNC-01: a transcript is a log — Claude Code only ever appends to
/// it — and its summary ([`ClaudeSession`]) is a fold over its lines, so a
/// file that grew is folded on from where the last read stopped rather than
/// read again from its first byte. This used to be a map keyed by
/// `(mtime, size)` that re-read a file whole whenever either moved: right for
/// a transcript that sits still, and exactly wrong for the ones that matter,
/// because a live agent's moves on every message. Measured 2026-10-17 on the
/// reporting machine: transcripts of 96, 176 and 210 MB re-read whole on
/// every session save, on the app thread — herdr's loop stood still for
/// 5–7 s after every pane birth or death, and an agent start from herdr-web
/// gave up while it did.
///
/// TP-CHATSYNC-04: the by-id road answers from [`StoreIndex`] instead of
/// listing every project directory and probing each one, per id, per pass.
///
/// TP-CHATSYNC-07: the folds outlive the process. Measured 2026-10-17 22:49:
/// a delivery's new server merged once before it took over — 16.97 s, 543
/// transcripts read whole, 3.5 GB — because this cache was born empty with
/// every server. The App saves the folds with the session and loads them
/// before its first merge; every fold still proves it is the file's (identity
/// and anchor) before it is resumed, so a stale store file costs a whole read,
/// never a wrong answer.
#[derive(Debug, Default)]
pub struct SessionParseCache {
    transcripts: std::collections::HashMap<PathBuf, TranscriptFold>,
    store: StoreIndex,
    /// A fold moved since the store file was last written (TP-CHATSYNC-10).
    changed: bool,
}

/// The store file: every fold, with the format it was written in.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredFolds {
    version: u32,
    folds: Vec<(PathBuf, TranscriptFold)>,
    /// Codex's parse cache (TP-CHATSYNC-11). Absent in a file written before
    /// it was kept, which then loads with Codex's cache empty; a reader that
    /// predates it skips it as an unknown field (TP-CHATSYNC-13).
    #[serde(default)]
    codex: Vec<crate::codex_sessions::StoredRollout>,
}

impl SessionParseCache {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The summary of the transcript at `path`, whose listing reported
    /// `key`: from memory while the file has not moved, folded on from where
    /// the last read stopped when it only grew, read whole otherwise.
    fn read(
        &mut self,
        path: PathBuf,
        key: (SystemTime, u64),
        now: SystemTime,
    ) -> Option<ClaudeSession> {
        let previous = self.transcripts.get(&path);
        if let Some(fold) = previous {
            if fold.key == key {
                return fold.session.clone();
            }
        }
        let fold = read_transcript(&path, key, previous, now)?;
        let session = fold.session.clone();
        self.transcripts.insert(path, fold);
        self.changed = true;
        session
    }

    /// The folds as the store file holds them (TP-CHATSYNC-07), in path
    /// order so an unchanged cache writes the same bytes.
    fn stored_folds(&self) -> Vec<(PathBuf, TranscriptFold)> {
        let mut folds: Vec<(PathBuf, TranscriptFold)> = self
            .transcripts
            .iter()
            .map(|(path, fold)| (path.clone(), fold.clone()))
            .collect();
        folds.sort_by(|a, b| a.0.cmp(&b.0));
        folds
    }

    /// The store file's bytes for this cache alone, Codex's left empty.
    #[cfg(test)]
    pub(crate) fn to_store_bytes(&self) -> Vec<u8> {
        chat_folds_bytes(self, &crate::codex_sessions::CodexParseCache::default())
    }

    /// This cache from a store file's bytes (TP-CHATSYNC-09).
    #[cfg(test)]
    pub(crate) fn from_store_bytes(bytes: &[u8]) -> Self {
        chat_folds_from_bytes(bytes).0
    }

    /// Whether a fold moved since this was last asked (TP-CHATSYNC-10): the
    /// one question a save asks before writing the folds again.
    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// How many transcripts this cache holds a fold for.
    #[cfg(test)]
    fn folded_transcripts(&self) -> usize {
        self.transcripts.len()
    }

    /// Overwrite the title of every remembered answer: a test's way to prove
    /// an answer came from memory rather than from the file.
    #[cfg(test)]
    pub(crate) fn retitle_remembered_for_test(&mut self, title: &str) {
        for fold in self.transcripts.values_mut() {
            if let Some(session) = fold.session.as_mut() {
                session.title = title.to_string();
            }
        }
    }
}

/// The store file's format version (TP-CHATSYNC-09). Codex's part did not
/// move it: a file without that part reads as one whose Codex cache is empty.
pub(crate) const CHAT_FOLDS_VERSION: u32 = 1;

/// The folds file's bytes: both agents' parse caches (TP-CHATSYNC-07/11).
pub(crate) fn chat_folds_bytes(
    claude: &SessionParseCache,
    codex: &crate::codex_sessions::CodexParseCache,
) -> Vec<u8> {
    let stored = StoredFolds {
        version: CHAT_FOLDS_VERSION,
        folds: claude.stored_folds(),
        codex: codex.to_stored(),
    };
    serde_json::to_vec(&stored).unwrap_or_default()
}

/// Both caches from a folds file's bytes. Anything it cannot read — empty,
/// not JSON, another version, cut short — is two empty caches
/// (TP-CHATSYNC-09): the stores are then read as on a first run.
pub(crate) fn chat_folds_from_bytes(
    bytes: &[u8],
) -> (SessionParseCache, crate::codex_sessions::CodexParseCache) {
    match serde_json::from_slice::<StoredFolds>(bytes) {
        Ok(stored) if stored.version == CHAT_FOLDS_VERSION => (
            SessionParseCache {
                transcripts: stored.folds.into_iter().collect(),
                ..SessionParseCache::default()
            },
            crate::codex_sessions::CodexParseCache::from_stored(stored.codex),
        ),
        Ok(stored) => {
            tracing::debug!(
                version = stored.version,
                "claude_sessions: chat folds of another version; starting empty"
            );
            Default::default()
        }
        Err(err) => {
            if !bytes.is_empty() {
                tracing::debug!(%err, "claude_sessions: unreadable chat folds; starting empty");
            }
            Default::default()
        }
    }
}

/// Where the folds are kept between runs: state, not configuration — losing
/// the file costs one whole read of the store, nothing else.
pub(crate) fn default_chat_folds_path() -> PathBuf {
    crate::config::state_dir().join("chat-folds.json")
}

/// The caches a previous run saved at `path`; empty ones when there is no
/// file or it cannot be read (TP-CHATSYNC-07/09/11).
pub(crate) fn load_chat_folds(
    path: &Path,
) -> (SessionParseCache, crate::codex_sessions::CodexParseCache) {
    match fs::read(path) {
        Ok(bytes) => chat_folds_from_bytes(&bytes),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(?path, %err, "claude_sessions: chat folds unreadable");
            }
            Default::default()
        }
    }
}

/// The folds' bytes a save should carry: only when this run leaves things on
/// disk and either cache moved since the last write (TP-CHATSYNC-10/12). Both
/// marks are taken, so a Codex parse is not written again by the next save.
pub(crate) fn chat_folds_to_write(
    persist: bool,
    claude: &mut SessionParseCache,
    codex: &mut crate::codex_sessions::CodexParseCache,
) -> Option<Vec<u8>> {
    if !persist {
        return None;
    }
    let claude_moved = claude.take_changed();
    let codex_moved = codex.take_changed();
    if !(claude_moved || codex_moved) {
        return None;
    }
    Some(chat_folds_bytes(claude, codex))
}

/// Write the folds to `path` atomically (TP-CHATSYNC-07): a crash mid-write
/// leaves the previous file, never half of this one.
pub(crate) fn save_chat_folds(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::persist::durable::write_atomic(path, bytes)
}

/// Like [`read_recent_sessions_for_project`] through the readers' cache.
pub fn read_recent_sessions_for_project_cached(
    projects_dir: &Path,
    project_path: &str,
    limit: usize,
    cache: &mut SessionParseCache,
) -> (Vec<ClaudeSession>, usize) {
    read_recent_inner(projects_dir, project_path, limit, Some(cache))
}

/// Like [`read_sessions_for_project`] but parses only the `limit` newest
/// session files (ranked by mtime from directory metadata alone), returning
/// the parsed sessions plus the TOTAL session-file count. A busy project holds
/// hundreds of files and hundreds of megabytes, so it must never be parsed
/// whole just to list its newest few chats.
fn read_recent_inner(
    projects_dir: &Path,
    project_path: &str,
    limit: usize,
    mut cache: Option<&mut SessionParseCache>,
) -> (Vec<ClaudeSession>, usize) {
    let encoded = encode_project_path(project_path);
    let dir = projects_dir.join(&encoded);

    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            // A project with no chats yet is a normal, expected state; only log
            // at debug so it does not spam. NotFound is silent-by-design here.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(?dir, %err, "claude_sessions: read_dir failed");
            }
            return (Vec::new(), 0);
        }
    };

    // Rank candidates by mtime using directory metadata only; files are
    // opened just for the newest `limit` of them that missed the cache.
    let mut candidates: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let (modified, size) = entry
                .metadata()
                .map(|meta| {
                    (
                        meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                        meta.len(),
                    )
                })
                .unwrap_or((std::time::SystemTime::UNIX_EPOCH, 0));
            Some((modified, size, path))
        })
        .collect();
    let total = candidates.len();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));

    // One clock for the whole pass: the reference point of TP-DRAW-16's
    // future-date reject.
    let now = SystemTime::now();
    let mut sessions: Vec<ClaudeSession> = Vec::new();
    for (modified, size, path) in candidates.into_iter().take(limit) {
        let key = (modified, size);
        let session = match cache.as_deref_mut() {
            Some(cache) => cache.read(path.clone(), key, now),
            None => read_transcript(&path, key, None, now).and_then(|fold| fold.session),
        };
        match session {
            Some(session) => sessions.push(session),
            None => tracing::debug!(?path, "claude_sessions: skipped unreadable session file"),
        }
    }

    // Chronological: newest first (mtime desc). Ties broken by id for a stable,
    // deterministic order (important for reproducible tests and rendering).
    sessions.sort_by(|a, b| {
        b.activity_time()
            .cmp(&a.activity_time())
            .then_with(|| a.id.cmp(&b.id))
    });
    (sessions, total)
}

/// Where a transcript stands, given only its id.
///
/// The store is keyed `<slug>/<id>.jsonl` and the slug is the one thing a
/// ledger (TP-CHAT-MOVE-16) or a headstone (TP-AGPANEL-53) may not know: a
/// chat can be copied between project directories, and an agent can be closed
/// in a directory other than the one its chat started in. One stat per project
/// directory; the first hit in name order wins. A path-shaped id is refused
/// rather than probed outside the store.
fn find_session_file_by_id(projects_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    let mut project_dirs: Vec<PathBuf> = fs::read_dir(projects_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    project_dirs.sort();
    project_dirs
        .into_iter()
        .map(|dir| dir.join(&file_name))
        .find(|candidate| candidate.is_file())
}

/// What `chat.open` has to know about a transcript before resuming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLaunchFacts {
    /// Where the transcript stands.
    pub path: PathBuf,
    /// The directory the conversation recorded first — where it was started.
    pub cwd: Option<PathBuf>,
}

/// How much of a transcript's head is read for its launch directory. The
/// first lines carry it; a cap keeps one pathological line (an inlined
/// attachment) from turning an id lookup into a whole-file read.
const LAUNCH_FACTS_SCAN_BYTES: u64 = 1024 * 1024;

/// Whether a transcript with this id stands anywhere in the store, and the
/// directory it says it was started in.
///
/// TP-AGPANEL-59/60: the id is probed across every project directory — a chat
/// is filed where it STARTED, and the caller may only know where it was
/// REMEMBERED — and only the head is read. `None` means no transcript: there
/// is nothing to resume, whatever any ledger says.
pub fn read_launch_facts_for_session(
    projects_dir: &Path,
    session_id: &str,
) -> Option<SessionLaunchFacts> {
    use std::io::{BufRead, Read};

    let path = find_session_file_by_id(projects_dir, session_id)?;
    let file = fs::File::open(&path).ok()?;
    let mut cwd = None;
    for line in std::io::BufReader::new(file.take(LAUNCH_FACTS_SCAN_BYTES))
        .lines()
        .map_while(Result::ok)
    {
        if !line.contains("\"cwd\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(found) = value.get("cwd").and_then(|cwd| cwd.as_str()) else {
            continue;
        };
        let candidate = PathBuf::from(found);
        if candidate.is_absolute() {
            cwd = Some(candidate);
            break;
        }
    }
    Some(SessionLaunchFacts { path, cwd })
}

/// A string that can name a transcript: not empty, not path-shaped, not a
/// dotfile.
fn is_session_id(session_id: &str) -> bool {
    !(session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.'))
}

/// Parse one transcript through `cache`, re-reading it only when its
/// `(mtime, size)` moved — the same key the window fetch uses, so a file
/// either road parsed is served to the other from the cache.
fn parse_session_file_cached(
    path: PathBuf,
    cache: &mut SessionParseCache,
    now: SystemTime,
) -> Option<ClaudeSession> {
    let key = fs::metadata(&path)
        .map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        })
        .unwrap_or((SystemTime::UNIX_EPOCH, 0));
    cache.read(path, key, now)
}

/// The transcript the store index names for `session_id`, read through the
/// cache. The index is as fresh as its last refresh; a path it names that is
/// not (or no longer) a file is settled by the probe it replaced, so a stale
/// entry can cost a probe but never a wrong answer.
fn read_indexed(
    projects_dir: &Path,
    session_id: &str,
    cache: &mut SessionParseCache,
    now: SystemTime,
) -> Option<ClaudeSession> {
    let path = cache.store.by_id.get(session_id)?.clone();
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {
            let key = (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            );
            cache.read(path, key, now)
        }
        _ => parse_session_file_cached(
            find_session_file_by_id(projects_dir, session_id)?,
            cache,
            now,
        ),
    }
}

/// Every transcript among `session_ids` that stands anywhere under
/// `projects_dir`, keyed by id, parsed through the same cache the drawer
/// fetch uses.
///
/// TP-CHAT-MOVE-16: the drawer fetch is a recency window over one directory
/// (`read_recent_sessions_for_project_cached`), and a chat the ledger re-homed
/// can be older than that window in the directory it started in — then no
/// fetch ever produces its row and the re-home has nothing to carry. This is
/// the by-id road: the ledger knows the chat's id, the store is keyed by
/// `<slug>/<id>.jsonl`, and the slug is the one thing the ledger does not
/// know (a chat can be copied between project directories). The store index
/// answers it (TP-CHATSYNC-04): the first directory in name order holding
/// the id — exactly the probe's answer — at the cost of one stat per project
/// directory to keep the index current, rather than a stat per directory per
/// id.
///
/// TP-DRAW-17: the drawer's window is the newest few files of each open
/// directory, and a ledger row outside it had no transcript clock at all — it
/// fell back to the sighting, which a restart writes for every open chat at
/// once, so a dozen rows wore one age and only ever moved on the next restart.
/// The store is keyed by id, so the index is brought up to date ONCE and
/// every id is answered from it. First hit in name order wins, exactly as it
/// does for a single id, and a path-shaped id is skipped rather than probed
/// outside the store.
pub fn read_sessions_by_ids_cached(
    projects_dir: &Path,
    session_ids: &[String],
    cache: &mut SessionParseCache,
) -> std::collections::HashMap<String, ClaudeSession> {
    let mut found = std::collections::HashMap::new();
    if session_ids.is_empty() {
        return found;
    }
    let now = SystemTime::now();
    cache.store.refresh(projects_dir, now);
    for session_id in session_ids {
        if !is_session_id(session_id) {
            continue;
        }
        if let Some(session) = read_indexed(projects_dir, session_id, cache, now) {
            found.insert(session_id.clone(), session);
        }
    }
    found
}

/// How recently a directory may have changed for its listing to be taken
/// as whole (TP-CHATSYNC-05). An mtime is a coarse clock: a file created in
/// the same tick as a listing that missed it leaves the mtime where the
/// listing saw it. So a directory listed while its mtime was this fresh is
/// listed again on the next pass whatever its mtime says — git's "racily
/// clean" rule, applied to a directory.
const RACY_MTIME: std::time::Duration = std::time::Duration::from_secs(2);

/// Where every transcript in the store stands, by id (TP-CHATSYNC-04).
///
/// The by-id road listed the store root and probed `<dir>/<id>.jsonl` in
/// every project directory for every id it was asked about, every pass.
/// Measured 2026-10-17: 374 re-homed chats — 25 of them in no store at all —
/// against 693 directories, a third of a 5-second freeze. The index lists a
/// directory only when its mtime moved (a transcript created, removed or
/// renamed in it), so an unchanged store costs one stat per project
/// directory, whatever is asked.
#[derive(Debug, Default)]
struct StoreIndex {
    root: Option<PathBuf>,
    /// The root's mtime when it was listed; `None`: list it on the next pass.
    root_mtime: Option<SystemTime>,
    /// Every project directory, in name order.
    dirs: Vec<IndexedDir>,
    /// The first directory in name order holding each id, as `<dir>/<id>.jsonl`.
    by_id: std::collections::HashMap<String, PathBuf>,
}

#[derive(Debug)]
struct IndexedDir {
    path: PathBuf,
    /// The mtime the listing in `ids` answers for; `None`: list it again.
    mtime: Option<SystemTime>,
    ids: Vec<String>,
}

impl StoreIndex {
    /// Bring the index up to date with the store under `root`.
    fn refresh(&mut self, root: &Path, now: SystemTime) {
        if self.root.as_deref() != Some(root) {
            *self = Self {
                root: Some(root.to_path_buf()),
                ..Self::default()
            };
        }
        let mut changed = false;
        let root_mtime = fs::metadata(root).and_then(|meta| meta.modified()).ok();
        if self.root_mtime.is_none() || self.root_mtime != root_mtime {
            crate::render_prof::event("chat_sync.store_listings");
            let mut listed: Vec<PathBuf> = fs::read_dir(root)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.is_dir())
                        .collect()
                })
                .unwrap_or_default();
            listed.sort();
            let mut known: std::collections::HashMap<PathBuf, IndexedDir> = self
                .dirs
                .drain(..)
                .map(|dir| (dir.path.clone(), dir))
                .collect();
            self.dirs = listed
                .into_iter()
                .map(|path| {
                    known.remove(&path).unwrap_or(IndexedDir {
                        path,
                        mtime: None,
                        ids: Vec::new(),
                    })
                })
                .collect();
            self.root_mtime = trusted_mtime(root_mtime, now);
            changed = true;
        }
        for dir in &mut self.dirs {
            let mtime = fs::metadata(&dir.path)
                .and_then(|meta| meta.modified())
                .ok();
            if dir.mtime.is_some() && dir.mtime == mtime {
                continue;
            }
            crate::render_prof::event("chat_sync.store_listings");
            dir.ids = fs::read_dir(&dir.path)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| {
                            entry
                                .file_name()
                                .to_str()
                                .and_then(|name| name.strip_suffix(".jsonl"))
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            dir.mtime = trusted_mtime(mtime, now);
            changed = true;
        }
        if changed {
            self.by_id.clear();
            for dir in &self.dirs {
                for id in &dir.ids {
                    self.by_id
                        .entry(id.clone())
                        .or_insert_with(|| dir.path.join(format!("{id}.jsonl")));
                }
            }
        }
    }
}

/// `mtime`, when it is old enough that a listing taken now saw everything it
/// stands for (TP-CHATSYNC-05); `None` otherwise, which lists it again.
fn trusted_mtime(mtime: Option<SystemTime>, now: SystemTime) -> Option<SystemTime> {
    mtime.filter(|mtime| {
        now.duration_since(*mtime)
            .is_ok_and(|age| age >= RACY_MTIME)
    })
}

/// Parse a single `<uuid>.jsonl` session file into a [`ClaudeSession`].
///
/// Returns `None` only when the file has no usable id or cannot be read at all.
/// Malformed individual lines are skipped, never fatal.
/// How much of EACH END of a transcript [`read_headstone_facts_for_session`]
/// reads when the file is too big to read whole.
pub const HEADSTONE_WINDOW_BYTES: u64 = 64 * 1024;

/// How far back beyond the tail window [`read_headstone_facts_for_session`]
/// searches for a `custom-title` it cannot see. Measured 2026-09-19 on the
/// reporting machine: of 115 titled conversations the two whose winning title
/// had fallen out of the tail carried it 365 and 439 KiB before the end.
pub const HEADSTONE_DEEP_SCAN_BYTES: u64 = 1024 * 1024;

/// What a headstone needs to know about its conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadstoneFacts {
    /// `custom-title` > `ai-title` > first user message; `None` when the
    /// transcript names nothing — a placeholder is not an answer — or when
    /// the read cannot VOUCH for what it saw (TP-AGPANEL-68).
    pub title: Option<String>,
    /// Whether anyone ever spoke in it.
    pub spoke: bool,
}

/// A conversation's current title and whether it was ever spoken to, from the
/// two ENDS of its transcript — never the whole of it.
///
/// TP-AGPANEL-63: a headstone wears its conversation's current title, and the
/// lines that carry one (`custom-title`, `ai-title`) are appended — the cron
/// namer's rename, a person's `/rename` — so the end of the file is where they
/// stand; the first prompt, the store's own last-resort title, stands at the
/// start. Reading whole files for this would cost what the drawer's full parse
/// costs, once per headstone, on a startup path: one of those transcripts
/// measured 46 MB (2026-09-02). So a file over two windows is read as its
/// first and last [`HEADSTONE_WINDOW_BYTES`], the line each cut runs through is
/// dropped, and the precedence is the store's own — a custom title beats an AI
/// one, the last of each wins, the first prompt is the fallback. A transcript
/// that names nothing answers `title: None`, never [`UNTITLED`]: a placeholder
/// is not an answer, and the caller keeps what it has.
///
/// TP-AGPANEL-67: `spoke` is the same read — a `user` or `assistant` line in
/// either window. A file too big to read whole is not a silent one; what
/// could not be counted is never reported as empty.
///
/// `project_path` is a hint, not an address: a chat is filed where it STARTED
/// and a headstone remembers where the agent last WORKED. Measured 2026-09-02,
/// eight of seventeen placeholders had the two disagree, so a miss under the
/// hinted directory probes the store by id. `None` means no transcript stands
/// anywhere — which, to a caller asking whether anyone spoke, is an answer.
pub fn read_headstone_facts_for_session(
    projects_dir: &Path,
    project_path: &str,
    session_id: &str,
) -> Option<HeadstoneFacts> {
    use std::io::{Read, Seek, SeekFrom};
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        return None;
    }
    let hinted = projects_dir
        .join(encode_project_path(project_path))
        .join(format!("{session_id}.jsonl"));
    let path = if hinted.is_file() {
        hinted
    } else {
        find_session_file_by_id(projects_dir, session_id)?
    };
    let mut file = fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= HEADSTONE_WINDOW_BYTES * 2 {
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes).ok()?;
        let mut scan = HeadstoneScan::default();
        scan.read(&String::from_utf8_lossy(&bytes), true);
        return Some(HeadstoneFacts {
            title: named(scan.custom_title.or(scan.ai_title).or(scan.first_user)),
            spoke: scan.spoke,
        });
    }

    // A cut into the middle of a line leaves a fragment no parser should see:
    // the head ends at its last newline and the tail begins after its first,
    // and the two lines the cuts run through belong to the middle.
    let mut head_bytes = vec![0u8; HEADSTONE_WINDOW_BYTES as usize];
    file.read_exact(&mut head_bytes).ok()?;
    let head_end = head_bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let mut head = HeadstoneScan::default();
    head.read(&String::from_utf8_lossy(&head_bytes[..head_end]), true);

    let tail_start = len - HEADSTONE_WINDOW_BYTES;
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut tail_bytes = Vec::with_capacity(HEADSTONE_WINDOW_BYTES as usize);
    file.read_to_end(&mut tail_bytes).ok()?;
    let tail_line = tail_bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(tail_bytes.len(), |at| at + 1);
    let mut tail = HeadstoneScan::default();
    // Only the head can hold the FIRST prompt; a prompt in the tail is a
    // later one and never a title.
    tail.read(&String::from_utf8_lossy(&tail_bytes[tail_line..]), false);

    // TP-AGPANEL-68: the store's rule is "the LAST custom-title wins,
    // wherever it is". One in the tail IS the last. Otherwise the middle is
    // searched backwards for one, up to a budget — and an answer is given
    // only when something can vouch for it: a custom-title was found, or the
    // whole middle was searched and holds none. A window that merely failed to
    // see one is not evidence that there is none.
    let title = if tail.custom_title.is_some() {
        tail.custom_title
    } else {
        let middle = search_middle_backwards(
            &mut file,
            head_end as u64,
            tail_start + tail_line as u64,
            HEADSTONE_DEEP_SCAN_BYTES,
        )?;
        if middle.custom_title.is_some() {
            middle.custom_title
        } else if middle.exhausted {
            head.custom_title
                .or(tail.ai_title)
                .or(middle.ai_title)
                .or(head.ai_title)
                .or(head.first_user)
        } else {
            None
        }
    };
    Some(HeadstoneFacts {
        title: named(title),
        // A file too big to read whole is not a silent one; what could not
        // be counted is never reported as empty.
        spoke: true,
    })
}

/// A title as a headstone wears it: one line, bounded, and never empty.
fn named(title: Option<String>) -> Option<String> {
    title
        .map(|title| clean_title(&title))
        .filter(|title| !title.is_empty())
}

/// What a backward search of a transcript's middle found.
struct MiddleTitles {
    /// The last `custom-title` in the searched span — the search stops at it.
    custom_title: Option<String>,
    /// The last `ai-title` met on the way.
    ai_title: Option<String>,
    /// Whether the search reached the start of the middle (so "none found"
    /// means "none there") rather than running out of budget.
    exhausted: bool,
}

/// Search `[start, end)` of `file` backwards, a window at a time, for title
/// lines. `start` and `end` stand on line boundaries. At most `budget` bytes
/// are read; only lines that mention a title are ever parsed, and a line that
/// merely QUOTES one (a tool result) fails the parse's own `type` check.
fn search_middle_backwards(
    file: &mut fs::File,
    start: u64,
    end: u64,
    budget: u64,
) -> Option<MiddleTitles> {
    use std::io::{Read, Seek, SeekFrom};
    let mut found = MiddleTitles {
        custom_title: None,
        ai_title: None,
        exhausted: false,
    };
    let mut pos = end;
    let mut read = 0u64;
    // The fragment of a line whose beginning lies in the next (earlier) span.
    let mut carry: Vec<u8> = Vec::new();
    while pos > start && read < budget {
        let step = HEADSTONE_WINDOW_BYTES.min(pos - start).min(budget - read);
        pos -= step;
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut span = vec![0u8; step as usize];
        file.read_exact(&mut span).ok()?;
        span.extend_from_slice(&carry);
        read += step;
        let whole_from = if pos > start {
            match span.iter().position(|byte| *byte == b'\n') {
                Some(at) => at + 1,
                // One line longer than everything read so far: keep it all.
                None => span.len(),
            }
        } else {
            0
        };
        for line in span[whole_from..].split(|byte| *byte == b'\n').rev() {
            if !line
                .windows(TITLE_MARK.len())
                .any(|window| window == TITLE_MARK)
            {
                continue;
            }
            let mut scan = HeadstoneScan::default();
            scan.read(&String::from_utf8_lossy(line), false);
            if scan.custom_title.is_some() {
                found.custom_title = scan.custom_title;
                return Some(found);
            }
            if found.ai_title.is_none() {
                found.ai_title = scan.ai_title;
            }
        }
        span.truncate(whole_from);
        carry = span;
    }
    found.exhausted = pos <= start;
    Some(found)
}

/// What every title line carries, whatever spacing its writer used.
const TITLE_MARK: &[u8] = b"-title\"";

/// What [`read_headstone_facts_for_session`] gathers across its windows.
#[derive(Default)]
struct HeadstoneScan {
    custom_title: Option<String>,
    ai_title: Option<String>,
    first_user: Option<String>,
    spoke: bool,
}

impl HeadstoneScan {
    fn read(&mut self, text: &str, may_hold_first_prompt: bool) {
        for line in text.lines() {
            let line = line.trim();
            // Only a title or a message is worth parsing; the rest of a window
            // is tool output, and 64 KiB of it is not free to deserialise.
            if line.is_empty()
                || !(line.contains("-title\"")
                    || line.contains("\"user\"")
                    || line.contains("\"assistant\""))
            {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(obj) = value.as_object() else {
                continue;
            };
            match obj.get("type").and_then(|t| t.as_str()) {
                Some("custom-title") => {
                    if let Some(t) = nonempty_str(obj.get("customTitle")) {
                        self.custom_title = Some(t);
                    }
                }
                Some("ai-title") => {
                    if let Some(t) = nonempty_str(obj.get("aiTitle")) {
                        self.ai_title = Some(t);
                    }
                }
                Some("user") => {
                    self.spoke = true;
                    if may_hold_first_prompt && self.first_user.is_none() {
                        self.first_user = extract_user_text(obj);
                    }
                }
                Some("assistant") => self.spoke = true,
                _ => {}
            }
        }
    }
}

/// The normalised opening of one chat, read on its own.
///
/// TP-DAILY-27: the graveyard holds records for conversations no drawer is
/// listing right now — a chat closed under a checkout nobody has open still
/// has a headstone. Their openings are therefore never learned by the drawer
/// parse, and without an opening a rule has nothing to judge, so those rows
/// were the ones a person kept seeing.
///
/// Deliberately NOT `parse_session_file`: that reads the file whole to derive
/// a title, and this runs once per headstone at startup. This stops at the
/// first user message — the only line it needs — so a long conversation costs
/// the same as a short one.
pub fn read_opening_for_session(
    projects_dir: &Path,
    project_path: &str,
    session_id: &str,
) -> Option<String> {
    use std::io::BufRead;

    let path = projects_dir
        .join(encode_project_path(project_path))
        .join(format!("{session_id}.jsonl"));
    let file = fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        if obj.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let opening = extract_user_text(obj).map(|text| normalise_opening(&text))?;
        return (!opening.is_empty()).then_some(opening);
    }
    None
}

/// The jsonl line's own `timestamp` field ("2026-08-19T10:05:30.250Z") as a
/// SystemTime. RFC 3339; anything absent, non-string, unparseable, or
/// pre-epoch is `None` — a bad line must never poison the session (TP-DRAW-15).
fn parse_line_timestamp(obj: &serde_json::Map<String, serde_json::Value>) -> Option<SystemTime> {
    let raw = obj.get("timestamp")?.as_str()?;
    let parsed =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()?;
    let ns = parsed.unix_timestamp_nanos();
    u64::try_from(ns)
        .ok()
        .map(|ns| SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(ns))
}

/// TP-DRAW-16: `max` keeps the newest timestamp forever, so one corrupt
/// future-dated line would pin the chat at "now" for good. Allow a small
/// clock-skew window; beyond it the timestamp is treated as absent.
const FUTURE_TIMESTAMP_SLACK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// TP-DRAW-16, cc-l parity (`_is_noise`): a user line advances the chat clock
/// only when it is the human actually speaking — not an injected `<…>` block
/// and not a continuation/interrupt banner. Tool results never reach here:
/// they carry no text block, so extraction already returns `None` for them.
fn user_text_advances_clock(text: &str) -> bool {
    !text.starts_with('<')
        && !text.starts_with("This session is being continued")
        && !text.starts_with("[Request interrupted")
}

/// TP-DRAW-16, cc-l parity: an assistant line advances the clock only when it
/// carries visible text that is not an injected `<…>` block. While an agent
/// runs, tool_use-only lines stream in constantly; counting them shows every
/// agent chat as "1m ago" forever.
fn assistant_text_advances_clock(text: &str) -> bool {
    !text.starts_with('<')
}

/// One transcript as far as it has been read (TP-CHATSYNC-01).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TranscriptFold {
    /// The `(mtime, size)` this fold was last brought up to date against: a
    /// listing reporting the same pair is answered without opening the file.
    key: (SystemTime, u64),
    /// What the transcript said then. `None` when it did not read as text
    /// (invalid UTF-8) — which a whole read answers with nothing too.
    session: Option<ClaudeSession>,
    /// Which file this fold read, when the platform can say.
    identity: Option<crate::platform::FileIdentity>,
    /// Bytes folded so far — always just past a newline.
    folded: u64,
    /// The last bytes before `folded`, at most [`ANCHOR_BYTES`]: still there
    /// on the next read means the file only grew (TP-CHATSYNC-03).
    anchor: Vec<u8>,
    /// What the folded lines add up to.
    lines: LineFold,
}

/// How much of a fold's end is kept to recognise the file by next time.
const ANCHOR_BYTES: usize = 64;

/// What a transcript's lines add up to — everything [`ClaudeSession`] is
/// derived from, kept in a form where folding a file's head and then its
/// tail is folding the file.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct LineFold {
    /// The last `custom-title`: later lines override earlier ones.
    custom_title: Option<String>,
    /// The last `ai-title`.
    ai_title: Option<String>,
    /// The first user line's text, from the first user line that has one —
    /// already cut to a title by `extract_user_text`, so a pasted megabyte
    /// is at most a title long here and in the store file (TP-CHATSYNC-08).
    first_user: Option<String>,
    msg_count: usize,
    /// TP-DRAW-15/16: the newest clock of a message that advances it.
    last_message_at: Option<SystemTime>,
    /// Message clocks that stood beyond the reject line (TP-DRAW-16) when
    /// their lines were read. A whole read judges every line against the
    /// moment of the read, so these are judged again at every later read
    /// rather than dropped.
    ahead: Vec<SystemTime>,
    /// A line that was not UTF-8. A whole read of such a file answers
    /// nothing, and so does every fold holding the line.
    not_text: bool,
}

impl LineFold {
    fn fold(&mut self, line: &str, now: SystemTime) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // Malformed / partial JSON line -> skip (real jsonl files can be
        // truncated mid-write). Never crash the whole session for one bad line.
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return,
        };
        let Some(obj) = value.as_object() else {
            return;
        };

        match obj.get("type").and_then(|t| t.as_str()) {
            // Later title lines override earlier ones -> the last one wins.
            Some("custom-title") => {
                if let Some(t) = nonempty_str(obj.get("customTitle")) {
                    self.custom_title = Some(t);
                }
            }
            Some("ai-title") => {
                if let Some(t) = nonempty_str(obj.get("aiTitle")) {
                    self.ai_title = Some(t);
                }
            }
            Some("user") => {
                self.msg_count += 1;
                // One extraction serves both the title chain and the clock
                // sieve (tool_result-only lines extract to `None`).
                let text = extract_user_text(obj);
                if self.first_user.is_none() {
                    self.first_user = text.clone();
                }
                // TP-DRAW-15: the newest message timestamp wins, whatever
                // order the lines were appended in. TP-DRAW-16: but only a
                // substantive human message may advance the clock.
                if text.as_deref().is_some_and(user_text_advances_clock) {
                    self.clock(obj, now);
                }
            }
            Some("assistant") => {
                self.msg_count += 1;
                // TP-DRAW-16: tool_use-only assistant lines are machinery,
                // not conversation — only visible text advances the clock.
                if extract_user_text(obj)
                    .as_deref()
                    .is_some_and(assistant_text_advances_clock)
                {
                    self.clock(obj, now);
                }
            }
            _ => {}
        }
    }

    fn clock(&mut self, obj: &serde_json::Map<String, serde_json::Value>, now: SystemTime) {
        let Some(ts) = parse_line_timestamp(obj) else {
            return;
        };
        if ts <= now + FUTURE_TIMESTAMP_SLACK {
            self.last_message_at = Some(self.last_message_at.map_or(ts, |cur| cur.max(ts)));
        } else {
            self.ahead.push(ts);
        }
    }

    /// Take into the clock what the reject line no longer rejects at `now`.
    fn settle(&mut self, now: SystemTime) {
        let last = &mut self.last_message_at;
        self.ahead.retain(|ts| {
            if *ts <= now + FUTURE_TIMESTAMP_SLACK {
                *last = Some(last.map_or(*ts, |cur| cur.max(*ts)));
                false
            } else {
                true
            }
        });
    }

    fn finish(
        mut self,
        id: String,
        last_modified: SystemTime,
        now: SystemTime,
    ) -> Option<ClaudeSession> {
        if self.not_text {
            return None;
        }
        self.settle(now);
        // The opening is taken before `derive_title` consumes `first_user`: a
        // chat with an `ai-title` never lets its first message reach the
        // title, and those are precisely the chats an automation produces.
        let opening = self
            .first_user
            .as_deref()
            .map(normalise_opening)
            .filter(|opening| !opening.is_empty());
        let title = derive_title(self.custom_title, self.ai_title, self.first_user);
        Some(ClaudeSession {
            id,
            title,
            last_modified,
            last_message_at: self.last_message_at,
            msg_count: self.msg_count,
            opening,
        })
    }
}

/// Bring a transcript's fold up to date with the file at `path`: from where
/// `previous` stopped when the file only grew since (TP-CHATSYNC-01), from
/// its first byte when it is another file, is shorter, or no longer holds
/// the bytes the fold ended on (TP-CHATSYNC-03).
///
/// A line is folded once its newline is read. An unfinished last line is
/// counted in this read's answer — a whole read counts it — but not kept,
/// so the next read reads it again (TP-CHATSYNC-02). The file is streamed a
/// line at a time: what is held is the longest line, not the transcript.
///
/// `None` only when the file cannot be opened or read; `key` is the pair the
/// caller's listing reported, remembered so an unmoved file is not reopened.
fn read_transcript(
    path: &Path,
    key: (SystemTime, u64),
    previous: Option<&TranscriptFold>,
    now: SystemTime,
) -> Option<TranscriptFold> {
    use std::io::{BufRead as _, Read as _, Seek as _};

    let id = path.file_stem()?.to_str()?.to_string();
    let mut file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let identity = crate::platform::file_identity(path, &meta).ok();
    let mut read: u64 = 0;
    let resumed = previous.filter(|fold| {
        if fold.folded == 0 || fold.identity != identity || fold.folded > meta.len() {
            return false;
        }
        let mut ended_on = vec![0; fold.anchor.len()];
        let at = fold.folded - fold.anchor.len() as u64;
        read += ended_on.len() as u64;
        file.seek(std::io::SeekFrom::Start(at)).is_ok()
            && file.read_exact(&mut ended_on).is_ok()
            && ended_on == fold.anchor
    });
    let (mut lines, mut folded, mut anchor) = match resumed {
        Some(fold) => {
            crate::render_prof::event("chat_sync.tail_parses");
            (fold.lines.clone(), fold.folded, fold.anchor.clone())
        }
        None => {
            crate::render_prof::event("chat_sync.full_parses");
            (LineFold::default(), 0, Vec::new())
        }
    };
    file.seek(std::io::SeekFrom::Start(folded)).ok()?;

    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::new();
    let mut unfinished = None;
    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(err) => {
                crate::render_prof::counter("chat_sync.bytes_read", read);
                tracing::debug!(?path, %err, "claude_sessions: transcript read failed");
                return None;
            }
        };
        if n == 0 {
            break;
        }
        read += n as u64;
        if line.last() != Some(&b'\n') {
            unfinished = Some(std::mem::take(&mut line));
            break;
        }
        match std::str::from_utf8(&line[..n - 1]) {
            Ok(text) => lines.fold(text, now),
            Err(_) => lines.not_text = true,
        }
        folded += n as u64;
        if n >= ANCHOR_BYTES {
            anchor.clear();
            anchor.extend_from_slice(&line[n - ANCHOR_BYTES..]);
        } else {
            anchor.extend_from_slice(&line);
            let excess = anchor.len().saturating_sub(ANCHOR_BYTES);
            anchor.drain(..excess);
        }
    }
    crate::render_prof::counter("chat_sync.bytes_read", read);

    // Metadata failure must not panic; fall back to the epoch (sorts oldest).
    let last_modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut answer = lines.clone();
    if let Some(rest) = unfinished {
        match std::str::from_utf8(&rest) {
            Ok(text) => answer.fold(text, now),
            Err(_) => answer.not_text = true,
        }
    }
    let session = answer.finish(id, last_modified, now);
    lines.settle(now);
    Some(TranscriptFold {
        key,
        session,
        identity,
        folded,
        anchor,
        lines,
    })
}

/// Title precedence: user-set custom-title > AI-generated ai-title >
/// first user message > `UNTITLED`.
fn derive_title(
    custom_title: Option<String>,
    ai_title: Option<String>,
    first_user: Option<String>,
) -> String {
    custom_title
        .or(ai_title)
        .or(first_user)
        .map(|t| clean_title(&t))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string())
}

/// Extract the first text of a user message. `message.content` may be a plain
/// string or an array of typed blocks (`{type:"text", text:"…"}`).
fn extract_user_text(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let content = obj.get("message")?.as_object()?.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks.iter().find_map(|block| {
            let block = block.as_object()?;
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })?,
        _ => return None,
    };
    let cleaned = clean_title(&text);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Read a string field, returning `Some` only when present and non-blank.
fn nonempty_str(value: Option<&serde_json::Value>) -> Option<String> {
    let s = value?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Collapse whitespace to a single line and truncate for display.
fn clean_title(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX_TITLE_CHARS {
        let truncated: String = one_line.chars().take(MAX_TITLE_CHARS - 1).collect();
        format!("{truncated}…")
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn unique() -> u64 {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// Isolated fake `.claude/projects` root. Never touches the real `~/.claude`;
    /// cleaned up on drop.
    struct TempProjects {
        root: PathBuf,
    }

    impl TempProjects {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-cs-test-{}-{}-{}",
                std::process::id(),
                tag,
                unique()
            ));
            fs::create_dir_all(&root).expect("create temp projects root");
            Self { root }
        }

        /// Write a session file for `project` with the given raw jsonl `lines`.
        fn write_session(&self, project: &str, session_id: &str, lines: &[&str]) -> PathBuf {
            let dir = self.root.join(encode_project_path(project));
            fs::create_dir_all(&dir).expect("create project dir");
            let path = dir.join(format!("{session_id}.jsonl"));
            let mut file = fs::File::create(&path).expect("create session file");
            for line in lines {
                writeln!(file, "{line}").expect("write session line");
            }
            path
        }
    }

    /// The title half of [`read_headstone_facts_for_session`].
    fn headstone_title(projects_dir: &Path, project_path: &str, id: &str) -> Option<String> {
        read_headstone_facts_for_session(projects_dir, project_path, id)
            .and_then(|facts| facts.title)
    }

    impl Drop for TempProjects {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    // TP-AGPANEL-53: a headstone's title is read without reading the
    // transcript whole. The title lines a chat gains after it closed are
    // appended at the end (the cron namer's `custom-title`, a late `/rename`),
    // so the last 64 KiB is where they stand — and a 46 MB transcript
    // (measured 2026-09-02, one of the fifteen `Claude` headstones) must not
    // be read whole on a startup path for a name. (The reader gained a head
    // window with TP-AGPANEL-63; what this test owns is the tail half.)
    #[test]
    fn a_title_is_read_from_the_transcripts_tail_without_reading_it_whole() {
        let temp = TempProjects::new("tail-title");
        let filler = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":\"{}\"}}}}",
            "x".repeat(4000)
        );
        // 160 KiB of body, then the title lines: only the tail sees them.
        let mut lines: Vec<String> = vec![filler; 40];
        lines.push(r#"{"type":"ai-title","aiTitle":"ai first"}"#.to_string());
        lines.push(r#"{"type":"custom-title","customTitle":"HERDR: MEZAR TAŞI"}"#.to_string());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        temp.write_session("/home/x/proj", "s-tail", &refs);
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "s-tail").as_deref(),
            Some("HERDR: MEZAR TAŞI"),
            "a custom title in the tail names the chat, and beats the AI one"
        );

        // The last of each wins, and custom beats AI whatever the order.
        temp.write_session(
            "/home/x/proj",
            "s-order",
            &[
                r#"{"type":"custom-title","customTitle":"older"}"#,
                r#"{"type":"custom-title","customTitle":"newer"}"#,
                r#"{"type":"ai-title","aiTitle":"ai later"}"#,
            ],
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "s-order").as_deref(),
            Some("newer")
        );

        // An AI title alone still names.
        temp.write_session(
            "/home/x/proj",
            "s-ai",
            &[r#"{"type":"ai-title","aiTitle":"only ai"}"#],
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "s-ai").as_deref(),
            Some("only ai")
        );

        // No title line: the first prompt — the store's own fallback, the one
        // the drawer wears for the same chat (TP-AGPANEL-63) — and never
        // "(untitled)".
        temp.write_session(
            "/home/x/proj",
            "s-none",
            &[r#"{"type":"user","message":{"role":"user","content":"hello there"}}"#],
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "s-none").as_deref(),
            Some("hello there")
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "s-missing"),
            None,
            "a transcript that does not exist names nothing"
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/proj", "../s-tail"),
            None,
            "a path-shaped id never leaves the store"
        );
    }

    // TP-AGPANEL-53: the record's directory is a hint, not an address. A chat
    // is filed under the directory it was STARTED in, and a headstone carries
    // the directory the agent last WORKED in — measured 2026-09-02 on eight
    // of the seventeen placeholders left after the first repair: the cwd said
    // `private-project-3`, the transcript stood under `private-project-4`
    // with two `ai-title` lines in it, and the tail read looked in the wrong
    // place and answered nothing. So a miss under the hinted directory probes
    // the store by id, the way the drawer's by-id road does.
    // TP-DRAW-17: many ids, one directory listing. A path-shaped id is
    // skipped rather than probed outside the store, and an id no directory
    // holds simply has no entry.
    #[test]
    fn many_ids_are_found_in_one_listing_and_a_path_shaped_id_is_skipped() {
        let temp = TempProjects::new("by-ids");
        temp.write_session(
            "/home/x/one",
            "s-one",
            &[r#"{"type":"custom-title","customTitle":"first"}"#],
        );
        temp.write_session(
            "/home/x/two",
            "s-two",
            &[
                r#"{"type":"custom-title","customTitle":"second"}"#,
                r#"{"type":"user","timestamp":"2026-09-01T10:00:00.000Z","message":{"content":"hi"}}"#,
            ],
        );
        let mut cache = SessionParseCache::new();
        let ids = [
            "s-one".to_string(),
            "s-two".to_string(),
            "s-missing".to_string(),
            "../s-one".to_string(),
        ];

        let found = read_sessions_by_ids_cached(&temp.root, &ids, &mut cache);

        assert_eq!(found.len(), 2, "{:?}", found.keys().collect::<Vec<_>>());
        assert_eq!(found["s-one"].title, "first");
        assert_eq!(found["s-two"].title, "second");
        assert_eq!(
            found["s-two"].last_message_at,
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_788_256_800)),
            "the message clock rides along"
        );
        assert!(
            !found.contains_key("../s-one"),
            "a path-shaped id never leaves the store"
        );
        assert_eq!(
            cache.folded_transcripts(),
            2,
            "both parses are cached for the next road"
        );
    }

    #[test]
    fn a_title_is_found_by_id_when_the_transcript_stands_under_another_directory() {
        let temp = TempProjects::new("tail-title-by-id");
        temp.write_session(
            "/home/x/where-it-started",
            "s-moved",
            &[r#"{"type":"ai-title","aiTitle":"Adres listesi diff analizi"}"#],
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/where-it-worked", "s-moved").as_deref(),
            Some("Adres listesi diff analizi"),
            "the hinted directory misses; the id still finds the transcript"
        );
        assert_eq!(
            headstone_title(&temp.root, "module:project:herdr", "s-moved").as_deref(),
            Some("Adres listesi diff analizi"),
            "a hint that is not even a directory still resolves by id"
        );
        assert_eq!(
            headstone_title(&temp.root, "/home/x/where-it-worked", "s-nowhere"),
            None,
            "an id with no transcript anywhere names nothing"
        );
    }

    // TP-AGPANEL-63 / TP-AGPANEL-67: what a headstone needs from its
    // transcript comes from the file's TWO ENDS, never the whole of it. The
    // title lines are appended (the cron namer, a late `/rename`) and stand in
    // the tail; the first prompt — the store's own last-resort title, the one
    // the drawer and the PWA both fall back to — stands in the head, which the
    // tail-only read could never see. That was the third place the two
    // surfaces disagreed about a name. And "did anyone ever speak" is the same
    // read: a transcript with no message in it is not a conversation.
    #[test]
    fn a_headstones_facts_come_from_the_two_ends_of_its_transcript() {
        let temp = TempProjects::new("headstone-facts");
        let filler = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":\"{}\"}}}}",
            "x".repeat(4000)
        );
        let first =
            r#"{"type":"user","message":{"role":"user","content":"ilk soru: mezar taşı adları"}}"#;

        // 1. 160 KiB of body between the first prompt and the title lines:
        //    only the tail sees the titles, and custom beats AI.
        let mut lines: Vec<String> = vec![first.to_string()];
        lines.extend(std::iter::repeat_n(filler.clone(), 40));
        lines.push(r#"{"type":"ai-title","aiTitle":"ai first"}"#.to_string());
        lines.push(r#"{"type":"custom-title","customTitle":"HERDR: MEZAR TAŞI"}"#.to_string());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        temp.write_session("/home/x/proj", "s-titled", &refs);
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "s-titled"),
            Some(HeadstoneFacts {
                title: Some("HERDR: MEZAR TAŞI".to_string()),
                spoke: true,
            })
        );

        // 2. No title line anywhere: the first prompt, from the HEAD window —
        //    and a later prompt in the tail is never mistaken for it.
        let mut lines: Vec<String> = vec![first.to_string()];
        lines.extend(std::iter::repeat_n(filler.clone(), 40));
        lines.push(
            r#"{"type":"user","message":{"role":"user","content":"son soru, ilk değil"}}"#
                .to_string(),
        );
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        temp.write_session("/home/x/proj", "s-prompt", &refs);
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "s-prompt"),
            Some(HeadstoneFacts {
                title: Some("ilk soru: mezar taşı adları".to_string()),
                spoke: true,
            })
        );

        // 3. Opened and never spoken to (measured 2026-09-19: two such
        //    `(untitled)` rows stood at the top of the panel).
        temp.write_session(
            "/home/x/proj",
            "s-silent",
            &[
                r#"{"type":"mode","mode":"default"}"#,
                r#"{"type":"system","content":"x"}"#,
            ],
        );
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "s-silent"),
            Some(HeadstoneFacts {
                title: None,
                spoke: false,
            }),
            "a transcript that names nothing answers no title — never `(untitled)`"
        );

        // 4. A small file is read whole: a title in its middle still counts.
        temp.write_session(
            "/home/x/proj",
            "s-small",
            &[
                first,
                r#"{"type":"ai-title","aiTitle":"küçük dosya"}"#,
                r#"{"type":"assistant","message":{"content":"cevap"}}"#,
            ],
        );
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "s-small")
                .and_then(|facts| facts.title)
                .as_deref(),
            Some("küçük dosya")
        );

        // 5. The record's directory is a hint, not an address (TP-AGPANEL-53).
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/where-it-worked", "s-small")
                .and_then(|facts| facts.title)
                .as_deref(),
            Some("küçük dosya"),
            "the hinted directory misses; the id still finds the transcript"
        );

        // 6. No transcript anywhere is `None` — the caller's "nobody spoke".
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "s-missing"),
            None
        );
        assert_eq!(
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", "../s-small"),
            None,
            "a path-shaped id never leaves the store"
        );
    }

    // TP-AGPANEL-68: a windowed read never answers a title it cannot stand
    // behind. The store's rule is "the LAST custom-title wins, wherever it
    // is", and a window only sees the ends: measured 2026-09-19 on the
    // reporting machine, 2 of 115 titled conversations carried their winning
    // `custom-title` 365 and 439 KiB before the end (the namer wrote it, then
    // the conversation went on and Claude kept appending `ai-title` lines).
    // The tail-only view of those answers the AI title — and a headstone that
    // had frozen the RIGHT name at close would be renamed to the wrong one.
    // So the middle is searched backwards for a `custom-title`, up to a
    // budget, and when neither that search nor the windows can vouch for the
    // answer the reader says nothing: the caller keeps what it has.
    #[test]
    fn a_title_the_windows_cannot_vouch_for_is_not_answered() {
        let temp = TempProjects::new("headstone-certainty");
        let filler = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":\"{}\"}}}}",
            "x".repeat(4000)
        );
        let first = r#"{"type":"user","message":{"role":"user","content":"ilk soru"}}"#;
        let custom =
            r#"{"type": "custom-title", "customTitle": "HERDR: ASIL AD", "sessionId": "s"}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"sonradan eklenen ai başlığı"}"#;
        let build = |before: usize, after: usize| -> Vec<String> {
            let mut lines = vec![first.to_string()];
            lines.extend(std::iter::repeat_n(filler.clone(), before));
            lines.push(custom.to_string());
            lines.extend(std::iter::repeat_n(filler.clone(), after));
            lines.push(ai.to_string());
            lines
        };
        let title = |id: &str, lines: &[String]| {
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            temp.write_session("/home/x/proj", id, &refs);
            read_headstone_facts_for_session(&temp.root, "/home/x/proj", id)
                .expect("the transcript stands")
        };

        // 1. The winning custom-title sits ~400 KiB before the end, the way
        //    the measured ones did: found by the backward search, and it beats
        //    the AI title that stands in the tail.
        let facts = title("s-deep", &build(40, 100));
        assert_eq!(facts.title.as_deref(), Some("HERDR: ASIL AD"));
        assert!(facts.spoke);

        // 2. Deeper than the budget (~2 MiB before the end) with more file
        //    below it than the search may read: the windows hold an AI title,
        //    but nothing can vouch that no custom-title outranks it — so no
        //    title is answered at all.
        let facts = title("s-too-deep", &build(40, 520));
        assert_eq!(
            facts.title, None,
            "an answer nobody can stand behind is not given"
        );
        assert!(
            facts.spoke,
            "silence is still judged: the file is no empty one"
        );

        // 3. No custom-title anywhere, and a middle small enough to search
        //    whole: the AI title is vouched for.
        let mut lines = vec![first.to_string()];
        lines.extend(std::iter::repeat_n(filler.clone(), 100));
        lines.push(ai.to_string());
        let facts = title("s-ai-only", &lines);
        assert_eq!(facts.title.as_deref(), Some("sonradan eklenen ai başlığı"));

        // 4. A tool result that QUOTES a title line is not one.
        let quoted = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"content\":\"{}\"}}]}}}}",
            r#"{\"type\":\"custom-title\",\"customTitle\":\"SAHTE\"}"#
        );
        let mut lines = vec![first.to_string()];
        lines.extend(std::iter::repeat_n(filler.clone(), 20));
        lines.push(quoted);
        lines.extend(std::iter::repeat_n(filler.clone(), 40));
        lines.push(ai.to_string());
        let facts = title("s-quoted", &lines);
        assert_eq!(facts.title.as_deref(), Some("sonradan eklenen ai başlığı"));
    }

    // ---- T1.1a: basic encoding ------------------------------------------------
    #[test]
    fn encode_basic_path() {
        assert_eq!(
            encode_project_path("/home/user/projects/x"),
            "-home-user-projects-x"
        );
    }

    // ---- T1.1b: real edge cases — space and dot also become '-' --------------
    #[test]
    fn encode_space_and_dot_and_underscore() {
        assert_eq!(
            encode_project_path("/Users/a/The Planner"),
            "-Users-a-The-Planner"
        );
        assert_eq!(encode_project_path("/home/a/.config"), "-home-a--config");
        // underscore is non-alphanumeric -> '-'
        assert_eq!(encode_project_path("/home/a/my_proj"), "-home-a-my-proj");
        // digits are preserved
        assert_eq!(encode_project_path("/srv/app2"), "-srv-app2");
    }

    // ---- T1.1c: malformed / partial jsonl lines are skipped, not fatal -------
    #[test]
    fn malformed_lines_are_skipped_and_valid_session_still_reads() {
        let tp = TempProjects::new("malformed");
        tp.write_session(
            "/home/x/proj",
            "1111",
            &[
                "{ this is not valid json",
                r#"{"type":"ai-title","aiTitle":"real title"}"#,
                "",
                r#"{"type":"user","message":{"content":"hi"}}"#,
                "{truncated mid-write",
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "real title");
        assert_eq!(sessions[0].msg_count, 1);
    }

    // ---- TP-DRAW-15: a chat's age is its last message's own time -------------

    #[test]
    fn the_last_message_timestamp_comes_from_the_last_user_or_assistant_line() {
        let tp = TempProjects::new("last-msg");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"summary","summary":"s"}"#,
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T10:05:30.250Z","message":{"content":[{"type":"text","text":"hello back"}]}}"#,
                r#"{"type":"custom-title","customTitle":"t"}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions.len(), 1);
        // The LAST message line wins, and non-message lines after it change
        // nothing — the reader asks "when did someone last speak here".
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_930_250);
        assert_eq!(sessions[0].last_message_at, Some(expected));
        assert_eq!(sessions[0].activity_time(), expected);
    }

    #[test]
    fn a_transcript_without_messages_carries_no_last_message_time() {
        let tp = TempProjects::new("no-msg");
        let path = tp.write_session(
            "/home/x/proj",
            "bbbb",
            &[r#"{"type":"ai-title","aiTitle":"only a title"}"#],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].last_message_at, None);
        // Honest fallback: with no timestamped message the file's own mtime
        // still answers, so the row is dated rather than blank.
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(sessions[0].activity_time(), mtime);
    }

    #[test]
    fn a_malformed_timestamp_does_not_poison_the_last_good_one() {
        let tp = TempProjects::new("bad-ts");
        tp.write_session(
            "/home/x/proj",
            "cccc",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"not-a-date"}"#,
                r#"{"type":"user","message":{"content":"no timestamp field at all"}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions.len(), 1);
        // Unparseable or absent timestamps are skipped, never zeroed in.
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn the_listing_orders_by_the_message_clock_not_the_file_clock() {
        // Two files written back-to-back (mtimes practically equal, "old"
        // freshly written): the one whose LAST MESSAGE is newer must lead.
        // This is the restart scenario — a resume refreshes mtimes wholesale,
        // and only the message clock keeps the order truthful.
        let tp = TempProjects::new("order");
        // Written and named so every OTHER clock gives the wrong order:
        // z-fresh is written FIRST (older mtime) and sorts LAST by id — only
        // the message clock can put it first.
        tp.write_session(
            "/home/x/proj",
            "z-fresh",
            &[r#"{"type":"user","timestamp":"2026-08-19T00:00:00.000Z","message":{"content":"b"}}"#],
        );
        std::thread::sleep(Duration::from_millis(25));
        tp.write_session(
            "/home/x/proj",
            "a-stale",
            &[r#"{"type":"user","timestamp":"2026-08-01T00:00:00.000Z","message":{"content":"a"}}"#],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["z-fresh", "a-stale"]);
    }

    #[test]
    fn an_out_of_order_transcript_still_reports_the_newest_message() {
        // Lines appended out of chronological order (a merge, a repair, an
        // imported chat): the NEWEST message timestamp wins, not the last line.
        let tp = TempProjects::new("ooo");
        tp.write_session(
            "/home/x/proj",
            "dddd",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:05:30.250Z","message":{"content":"late"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":[{"type":"text","text":"earlier reply"}]}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_930_250);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    // ---- TP-DRAW-16: only substantive messages advance the chat clock --------

    #[test]
    fn tool_use_only_assistant_lines_do_not_advance_the_clock() {
        // The "always 1m ago" bug: while an agent runs, every tool call
        // appends an assistant line with no visible text. Counting those
        // pins the row at "now" forever, so they must not touch the clock.
        let tp = TempProjects::new("tool-only");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:05:00.000Z"}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn an_assistant_line_with_text_advances_the_clock() {
        // The sieve must not overreach: a real reply is real activity.
        let tp = TempProjects::new("asst-text");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":[{"type":"text","text":"done"}]}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_144_400_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn injected_angle_bracket_blocks_do_not_advance_the_clock() {
        // System reminders and command transcripts arrive as ordinary
        // user/assistant lines whose text starts with "<". cc-l ignores
        // them and so do we.
        let tp = TempProjects::new("angle");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":"<system-reminder>ping</system-reminder>"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:05:00.000Z","message":{"content":[{"type":"text","text":"<system>injected</system>"}]}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn continuation_and_interrupt_banners_do_not_advance_the_clock() {
        // Compact/resume machinery writes these as user lines; they are not
        // the human speaking (cc-l's noise-prefix parity).
        let tp = TempProjects::new("banner");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":"This session is being continued from a previous conversation"}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:05:00.000Z","message":{"content":"[Request interrupted by user]"}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn tool_result_user_lines_do_not_advance_the_clock() {
        // While an agent runs, tool results stream back as user-typed lines
        // with no text block; they are machinery, not conversation.
        let tp = TempProjects::new("tool-result");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn a_future_dated_line_is_ignored_by_the_clock() {
        // `max` keeps the newest timestamp forever, so a single corrupt
        // future-dated line would pin the chat at "now" for good. Reject
        // anything past the present (plus a small clock-skew allowance).
        let tp = TempProjects::new("future");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":"hi"}}"#,
                r#"{"type":"user","timestamp":"2099-01-01T00:00:00.000Z","message":{"content":"from the future"}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    #[test]
    fn the_skew_allowance_admits_minutes_and_rejects_hours() {
        // Clocks drift: a message stamped seconds ahead of this machine is
        // real activity, not corruption — only the far future is corrupt.
        // Pinned as a bound so neither a zeroed nor an enormous allowance
        // can slip through: minutes pass, hours do not. (The `time` crate is
        // built with `parsing` only, so the product road is exercised with a
        // fixed far-future stamp in the test above.)
        let now = SystemTime::now();
        assert!(now + Duration::from_secs(60) <= now + FUTURE_TIMESTAMP_SLACK);
        assert!(now + Duration::from_secs(2 * 60 * 60) > now + FUTURE_TIMESTAMP_SLACK);
    }

    #[test]
    fn the_clock_matches_the_cc_l_content_sieve_on_a_mixed_transcript() {
        // Fixture parity with `private-project-5 list` (cc-l): on a realistic
        // agent-session tail — real message, then a burst of tool traffic and
        // injected blocks — both tools must date the chat at the last REAL
        // message. cc-l's sieve: user counts when non-noise; assistant counts
        // when it has text not starting with "<".
        let tp = TempProjects::new("ccl-parity");
        tp.write_session(
            "/home/x/proj",
            "aaaa",
            &[
                r#"{"type":"user","timestamp":"2026-08-19T09:00:00.000Z","message":{"content":"start the job"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T10:00:00.000Z","message":{"content":[{"type":"text","text":"working on it"}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:00:00.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:00:05.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"out"}]}}"#,
                r#"{"type":"user","timestamp":"2026-08-19T13:01:00.000Z","message":{"content":"<local-command-stdout>ok</local-command-stdout>"}}"#,
                r#"{"type":"assistant","timestamp":"2026-08-19T13:02:00.000Z"}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        // 10:00:00Z — the assistant's last real reply, exactly what cc-l shows.
        let expected = std::time::UNIX_EPOCH + Duration::from_millis(1_787_133_600_000);
        assert_eq!(sessions[0].last_message_at, Some(expected));
    }

    // ---- T1.1d: missing / empty project dir -> empty list, no panic ----------
    #[test]
    fn missing_project_dir_returns_empty() {
        let tp = TempProjects::new("missing");
        let sessions = read_sessions_for_project(&tp.root, "/home/x/never-opened");
        assert!(sessions.is_empty());
    }

    #[test]
    fn empty_project_dir_returns_empty() {
        let tp = TempProjects::new("empty");
        // create the encoded dir but no jsonl files
        fs::create_dir_all(tp.root.join(encode_project_path("/home/x/proj"))).unwrap();
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert!(sessions.is_empty());
    }

    // ---- T1.1e: title fallback chain -----------------------------------------
    #[test]
    fn title_falls_back_to_first_user_then_untitled() {
        let tp = TempProjects::new("fallback");
        // no titles -> first user message
        tp.write_session(
            "/home/x/proj",
            "user-only",
            &[r#"{"type":"user","message":{"content":"open the preview"}}"#],
        );
        // no titles, no user -> UNTITLED
        tp.write_session(
            "/home/x/proj",
            "empty-meta",
            &[r#"{"type":"assistant","message":{"content":"hello"}}"#],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        let by_id = |id: &str| sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(by_id("user-only").title, "open the preview");
        assert_eq!(by_id("empty-meta").title, UNTITLED);
    }

    // ---- T1.1g: custom-title beats ai-title; last value wins ------------------
    #[test]
    fn custom_title_beats_ai_title_and_last_wins() {
        let tp = TempProjects::new("precedence");
        tp.write_session(
            "/home/x/proj",
            "titled",
            &[
                r#"{"type":"ai-title","aiTitle":"ai first"}"#,
                r#"{"type":"custom-title","customTitle":"user pick"}"#,
                r#"{"type":"ai-title","aiTitle":"ai second"}"#,
                r#"{"type":"custom-title","customTitle":"final pick"}"#,
                r#"{"type":"user","message":{"content":"ignored for title"}}"#,
            ],
        );
        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions[0].title, "final pick");
    }

    // ---- T1.1f: chronological ordering (newest first by mtime) ----------------
    #[test]
    fn sessions_sorted_newest_first() {
        let tp = TempProjects::new("order");
        let older = tp.write_session(
            "/home/x/proj",
            "aaa-older",
            &[r#"{"type":"custom-title","customTitle":"older"}"#],
        );
        let newer = tp.write_session(
            "/home/x/proj",
            "bbb-newer",
            &[r#"{"type":"custom-title","customTitle":"newer"}"#],
        );
        // Force a deterministic mtime ordering regardless of write speed.
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        set_mtime(&older, base);
        set_mtime(&newer, base + Duration::from_secs(60));

        let sessions = read_sessions_for_project(&tp.root, "/home/x/proj");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].title, "newer");
        assert_eq!(sessions[1].title, "older");
    }

    // Deterministic mtime via std (File::set_modified, stable since Rust 1.75) —
    // no external `filetime` dep, cross-platform, clippy-clean.
    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open session file to set mtime");
        file.set_modified(when).expect("set mtime");
    }

    // ---- claude_projects_dir env resolution ----------------------------------
    #[test]
    fn projects_dir_resolves_from_home() {
        let dir = claude_projects_dir(|k| {
            if k == "HOME" {
                Some(OsString::from("/home/tester"))
            } else {
                None
            }
        });
        assert_eq!(dir, Some(PathBuf::from("/home/tester/.claude/projects")));
    }

    #[test]
    fn projects_dir_none_without_home() {
        assert_eq!(claude_projects_dir(|_| None), None);
        assert_eq!(claude_projects_dir(|_| Some(OsString::new())), None);
    }

    // ---- Opening extraction and normalisation (O1-O6) -----------------------

    /// O4: the whole point of normalising. Two runs of one automation differ
    /// only in the parts that change every run; masking those is what makes
    /// them one shape. Measured on real data: a single scheduled task appeared
    /// as two spellings, 39 and 32 times, differing only by a date.
    #[test]
    fn normalising_masks_the_parts_that_change_every_run() {
        let first = normalise_opening(
            "<scheduled-task name=\"sor\" file=\"/home/user/.claude/x.md\" at=\"2026-08-17\" run=42>",
        );
        let second = normalise_opening(
            "<scheduled-task name=\"sor\" file=\"/home/user/.claude/x.md\" at=\"2026-07-03\" run=7>",
        );
        assert_eq!(first, second, "two runs of one task must share one shape");
        assert!(first.contains("<date>"), "{first}");
        assert!(first.contains("<path>"), "{first}");
        assert!(first.contains("<n>"), "{first}");
    }

    /// O4b: a uuid is masked as one token rather than being chopped into a
    /// date and several numbers, which would make two runs disagree.
    #[test]
    fn normalising_masks_a_uuid_as_one_token() {
        let opening = normalise_opening("resume 3f33db7a-797d-4bcc-b60a-fabb4ada10ae now");
        assert_eq!(opening, "resume <id> now");
    }

    /// O5: normalising is idempotent. If it were not, the same chat could
    /// produce two different shapes depending on how often it was processed,
    /// and the repeat counter would split one automation across two buckets.
    #[test]
    fn normalising_is_idempotent() {
        for raw in [
            "<scheduled-task name=\"x\" file=\"/a/b.md\" at=\"2026-08-17\">",
            "resume 3f33db7a-797d-4bcc-b60a-fabb4ada10ae",
            "plain words with 42 numbers",
            "",
        ] {
            let once = normalise_opening(raw);
            assert_eq!(normalise_opening(&once), once, "not idempotent for {raw:?}");
        }
    }

    /// O4c: masking must not eat ordinary text. A guard that flattens prose
    /// makes unrelated chats collide and hides real conversations.
    #[test]
    fn normalising_leaves_ordinary_prose_recognisable() {
        let opening = normalise_opening("Bu sohbette neler yaptık? Özetle lütfen.");
        assert_eq!(opening, "bu sohbette neler yaptık? özetle lütfen.");
    }

    /// O4d: a lone slash is a slash, not a path. Requiring a character after
    /// it keeps division signs and sentence slashes out of the mask.
    #[test]
    fn a_bare_slash_is_not_a_path() {
        assert_eq!(normalise_opening("a / b"), "a / b");
        assert_eq!(normalise_opening("see /tmp/x now"), "see <path> now");
    }

    /// TP-DAILY-27 (O1/O3/O5): a single chat's opening, read on its own.
    ///
    /// The graveyard needs this for conversations no drawer is listing: the
    /// headstone outlives the drawer, and a row with no opening is a row no
    /// rule can judge — which is exactly the set a person kept seeing. A
    /// missing directory, a missing file and a chat that never spoke all
    /// answer `None` rather than costing anything.
    #[test]
    fn one_chats_opening_can_be_read_without_parsing_the_whole_file() {
        let temp = TempProjects::new("lone-opening");
        temp.write_session(
            "/repo/thing",
            "s1",
            &[
                r#"{"type":"user","message":{"content":"Review this change for security vulnerabilities. Changed files: a.rs"}}"#,
                r#"{"type":"assistant","message":{"content":"ok"}}"#,
                r#"{"type":"user","message":{"content":"and a second message that must not win"}}"#,
            ],
        );

        assert_eq!(
            read_opening_for_session(&temp.root, "/repo/thing", "s1").as_deref(),
            Some("review this change for security vulnerabilities. changed files: a.rs"),
            "the FIRST user message is the opening"
        );
        assert_eq!(
            read_opening_for_session(&temp.root, "/repo/thing", "missing"),
            None,
            "a chat with no file costs nothing"
        );
        assert_eq!(
            read_opening_for_session(&temp.root, "/repo/elsewhere", "s1"),
            None,
            "and neither does a directory nothing was written to"
        );

        temp.write_session(
            "/repo/thing",
            "quiet",
            &[r#"{"type":"assistant","message":{"content":"nobody asked anything"}}"#],
        );
        assert_eq!(
            read_opening_for_session(&temp.root, "/repo/thing", "quiet"),
            None,
            "a chat that never spoke has no shape to remember"
        );
    }

    /// O1/O2: the opening is read from the first user message and survives an
    /// `ai-title`. This is the case that matters most: an automation's chat is
    /// titled by the model, so the first message never reaches the title, and
    /// deriving the opening from the title would lose exactly those chats.
    #[test]
    fn the_opening_is_read_even_when_an_ai_title_wins_the_title() {
        let temp = TempProjects::new("opening-ai-title");
        temp.write_session(
            "/p",
            "s1",
            &[
                r#"{"type":"user","message":{"content":"<scheduled-task name=\"nightly\">"}}"#,
                r#"{"type":"assistant","message":{"content":"ok"}}"#,
                r#"{"type":"ai-title","aiTitle":"Nightly report"}"#,
            ],
        );

        let sessions = read_sessions_for_project(&temp.root, "/p");
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].title, "Nightly report",
            "title precedence is unchanged"
        );
        assert_eq!(
            sessions[0].opening.as_deref(),
            Some("<scheduled-task name=\"nightly\">"),
            "the opening must survive an ai-title"
        );
    }

    /// O3: adding the opening must not move the title. Titles are what people
    /// read; a silent change there would be a regression nobody asked for.
    #[test]
    fn adding_the_opening_does_not_move_the_title() {
        assert_eq!(
            derive_title(
                Some("custom".into()),
                Some("ai".into()),
                Some("first".into())
            ),
            "custom"
        );
        assert_eq!(
            derive_title(None, Some("ai".into()), Some("first".into())),
            "ai"
        );
        assert_eq!(derive_title(None, None, Some("first".into())), "first");
        assert_eq!(derive_title(None, None, None), UNTITLED);
    }

    /// O6: a chat with nothing readable gets no opening at all. Inventing a
    /// shape for it would put unrelated chats in one bucket and let the repeat
    /// rule hide them together.
    #[test]
    fn a_chat_with_no_readable_first_message_has_no_opening() {
        let temp = TempProjects::new("opening-none");
        temp.write_session(
            "/p",
            "s1",
            &[
                r#"{"type":"assistant","message":{"content":"hi"}}"#,
                r#"{"type":"user","message":{"content":"   "}}"#,
            ],
        );

        let sessions = read_sessions_for_project(&temp.root, "/p");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].opening, None);
    }

    // ---- TP-CHATSYNC: the cost of reading a store that never stops growing ---

    /// Every line shape a live transcript carries, in the order a session
    /// produces them: a tool result before the first words (so "the first
    /// user line with text" is exercised), both message forms, both title
    /// kinds twice, the machinery the clock ignores, a multi-byte name, a
    /// Windows line ending, and the damage a real file can hold.
    fn live_transcript_lines() -> Vec<&'static str> {
        vec![
            r#"{"type":"summary","summary":"an earlier conversation"}"#,
            r#"{"type":"user","timestamp":"2026-10-17T08:59:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t0","content":"ok"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-17T09:00:00.000Z","message":{"role":"user","content":"herdr donmasını profille"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-17T09:00:05.000Z","message":{"content":[{"type":"text","text":"Profil çıkarıyorum — şğüıöç"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-17T09:00:06.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-17T09:00:07.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Profiling the stall"}"#,
            "{ this line was cut mid-write",
            "",
            "   ",
            r#"{"type":"user","timestamp":"2026-10-17T09:01:00.000Z","message":{"content":"<system-reminder>ping</system-reminder>"}}"#,
            r#"{"type":"custom-title","customTitle":"first pick"}"#,
            "{\"type\":\"assistant\",\"timestamp\":\"2026-10-17T09:02:00.000Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"a windows line ending\"}]}}\r",
            r#"{"type":"user","timestamp":"2099-01-01T00:00:00.000Z","message":{"content":"from the future"}}"#,
            r#"{"type":"user","timestamp":"not-a-date","message":{"content":"a broken clock"}}"#,
            "[1,2,3]",
            r#""a bare string""#,
            r#"{"type":"ai-title","aiTitle":"Profiling the stall, again"}"#,
            r#"{"type":"custom-title","customTitle":"final pick"}"#,
            r#"{"type":"assistant","timestamp":"2026-10-17T09:03:00.000Z"}"#,
            r#"{"type":"user","timestamp":"2026-10-17T09:04:00.000Z","message":{"content":"This session is being continued from a previous conversation"}}"#,
            r#"{"type":"user","timestamp":"2026-10-17T09:05:00.000Z","message":{"content":[{"type":"text","text":"  last human words  "}]}}"#,
        ]
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open the transcript for appending");
        file.write_all(bytes).expect("append to the transcript");
    }

    /// Push a directory's mtime into the past, so the store index may trust
    /// it — a directory changed in the last moments is listed again on the
    /// next pass whatever its mtime says (see TP-CHATSYNC-05).
    fn age(path: &Path) {
        fs::File::open(path)
            .expect("open the directory")
            .set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .expect("age the directory");
    }

    /// One read of `project` through `cache`, with the reader's counters.
    fn read_counted(
        root: &Path,
        project: &str,
        cache: &mut SessionParseCache,
    ) -> (Vec<ClaudeSession>, crate::render_prof::TestRenderProfile) {
        let ((sessions, _), profile) = crate::render_prof::observe_for_test(|| {
            read_recent_sessions_for_project_cached(root, project, 12, cache)
        });
        (sessions, profile)
    }

    // TP-CHATSYNC-01: a transcript that grew reads exactly like one read
    // whole. Claude Code only ever appends, so the reader folds a grown file
    // from where it stopped rather than from its first byte — and that is
    // only a speed-up if the answer is the one a whole read gives. Split at
    // EVERY byte (inside a line, inside a multi-byte character, on a line
    // end), fold the head, append the rest, fold again: same chat, same
    // title, same count, same clock, both with and without a final newline.
    #[test]
    fn a_grown_transcript_reads_like_a_whole_one_at_every_split() {
        let tp = TempProjects::new("grown-every-split");
        let project = "/home/x/live";
        let dir = tp.root.join(encode_project_path(project));
        fs::create_dir_all(&dir).expect("project dir");
        let path = dir.join("live.jsonl");
        let joined = live_transcript_lines().join("\n");
        for full in [format!("{joined}\n"), joined] {
            let bytes = full.as_bytes();
            for split in 0..=bytes.len() {
                fs::write(&path, &bytes[..split]).expect("write the head");
                let mut cache = SessionParseCache::new();
                let _ = read_recent_sessions_for_project_cached(&tp.root, project, 12, &mut cache);
                append(&path, &bytes[split..]);
                let (grown, _) =
                    read_recent_sessions_for_project_cached(&tp.root, project, 12, &mut cache);
                let whole = read_sessions_for_project(&tp.root, project);
                assert_eq!(grown, whole, "split at byte {split} of {}", bytes.len());
            }
        }
        // The fixture is not vacuous: every fold above had this to agree on.
        let whole = read_sessions_for_project(&tp.root, project);
        assert_eq!(whole[0].title, "final pick");
        assert_eq!(whole[0].msg_count, 12);
        assert_eq!(
            whole[0].last_message_at,
            Some(stamp("2026-10-17T09:05:00.000Z"))
        );
        assert_eq!(
            whole[0].opening.as_deref(),
            Some("herdr donmasını profille")
        );
    }

    fn stamp(rfc3339: &str) -> SystemTime {
        let parsed =
            time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
                .expect("a test timestamp parses");
        SystemTime::UNIX_EPOCH
            + Duration::from_nanos(
                u64::try_from(parsed.unix_timestamp_nanos()).expect("post-epoch"),
            )
    }

    // TP-CHATSYNC-01: TP-DRAW-16's reject line holds under the fold too. A
    // whole read judges every message clock against the moment of the read,
    // so a clock that stood ahead of the first read is not thrown away: the
    // next read that finds it no longer ahead takes it, as a whole read at
    // that moment would.
    #[test]
    fn a_clock_ahead_of_its_first_read_is_judged_again_at_the_next() {
        let tp = TempProjects::new("ahead");
        let path = tp.write_session(
            "/home/x/ahead",
            "s",
            &[
                r#"{"type":"user","timestamp":"2026-10-17T10:00:00.000Z","message":{"content":"early"}}"#,
                r#"{"type":"user","timestamp":"2026-10-17T12:00:00.000Z","message":{"content":"ahead"}}"#,
            ],
        );
        let key = |path: &Path| {
            let meta = fs::metadata(path).expect("stat");
            (meta.modified().expect("mtime"), meta.len())
        };
        let first_read = stamp("2026-10-17T10:30:00.000Z");
        let first = read_transcript(&path, key(&path), None, first_read).expect("first read");
        assert_eq!(
            first.session.as_ref().and_then(|s| s.last_message_at),
            Some(stamp("2026-10-17T10:00:00.000Z")),
            "90 minutes ahead is past the reject line"
        );

        append(
            &path,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-17T10:20:00.000Z","message":{"content":"late"}}"#,
                "\n"
            )
            .as_bytes(),
        );
        let later = stamp("2026-10-17T12:30:00.000Z");
        let grown = read_transcript(&path, key(&path), Some(&first), later).expect("grown read");
        let whole = read_transcript(&path, key(&path), None, later).expect("whole read");
        assert_eq!(grown.session, whole.session);
        assert_eq!(
            grown.session.and_then(|s| s.last_message_at),
            Some(stamp("2026-10-17T12:00:00.000Z")),
            "the clock the first read rejected is taken once the reject line passed it"
        );
    }

    // TP-CHATSYNC-01: and the point of it — a re-read costs what was
    // appended, not what the file holds. Measured 2026-10-17 on the reporting
    // machine: the live agents' transcripts stood at 96, 176 and 210 MB, every
    // session save re-read them whole, and herdr's loop froze 5–7 s after
    // every pane birth or death while it did.
    #[test]
    fn a_grown_transcript_costs_what_was_appended_not_its_size() {
        let tp = TempProjects::new("grown-cost");
        let project = "/home/x/live";
        let body = format!(
            r#"{{"type":"assistant","timestamp":"2026-10-17T09:00:00.000Z","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(2_000)
        );
        let lines = [body.as_str(); 512];
        let path = tp.write_session(project, "live", &lines);
        let size = fs::metadata(&path).expect("stat").len();
        let mut cache = SessionParseCache::new();

        let (_, first) = read_counted(&tp.root, project, &mut cache);
        assert_eq!(
            first.counter("chat_sync.full_parses"),
            1,
            "the first read is whole"
        );
        assert!(first.counter("chat_sync.bytes_read") >= size);

        let appended = concat!(
            r#"{"type":"user","timestamp":"2026-10-17T09:10:00.000Z","message":{"content":"one more"}}"#,
            "\n"
        );
        append(&path, appended.as_bytes());
        let (sessions, second) = read_counted(&tp.root, project, &mut cache);

        assert_eq!(second.counter("chat_sync.tail_parses"), 1);
        assert_eq!(second.counter("chat_sync.full_parses"), 0);
        let read = second.counter("chat_sync.bytes_read");
        let new = appended.len() as u64;
        assert!(
            (new..=new + 4_096).contains(&read),
            "read {read} bytes of a {size}-byte transcript to learn {new} new ones"
        );
        assert_eq!(sessions, read_sessions_for_project(&tp.root, project));
        assert_eq!(sessions[0].msg_count, 513);
    }

    // TP-CHATSYNC-02: a line still being written is read again, never kept.
    // An agent appends a line in more than one write. A reader that took the
    // bytes it saw as done would lose the line when the rest landed (half a
    // line parses as nothing, and so does the other half); one that folded a
    // finished-looking last line into what it keeps would count it again
    // when its newline landed. The whole read counts a finished-looking last
    // line, so this one does too — and still reads it again next time.
    #[test]
    fn a_line_still_being_written_is_read_again_not_counted_twice() {
        let tp = TempProjects::new("half-line");
        let project = "/home/x/live";
        let path = tp.write_session(
            project,
            "live",
            &[r#"{"type":"user","timestamp":"2026-10-17T09:00:00.000Z","message":{"content":"hi"}}"#],
        );
        let pending = r#"{"type":"assistant","timestamp":"2026-10-17T09:00:09.000Z","message":{"content":[{"type":"text","text":"half"}]}}"#;
        let (head, rest) = pending.split_at(pending.len() / 2);
        let mut cache = SessionParseCache::new();

        append(&path, head.as_bytes());
        let (first, _) = read_counted(&tp.root, project, &mut cache);
        assert_eq!(first, read_sessions_for_project(&tp.root, project));
        assert_eq!(first[0].msg_count, 1, "half a line is no line yet");

        append(&path, rest.as_bytes());
        let (second, _) = read_counted(&tp.root, project, &mut cache);
        assert_eq!(second, read_sessions_for_project(&tp.root, project));
        assert_eq!(
            second[0].msg_count, 2,
            "a whole read counts the finished-looking line"
        );

        append(&path, b"\n");
        append(
            &path,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-17T09:01:00.000Z","message":{"content":"next"}}"#,
                "\n"
            )
            .as_bytes(),
        );
        let (third, profile) = read_counted(&tp.root, project, &mut cache);
        assert_eq!(third, read_sessions_for_project(&tp.root, project));
        assert_eq!(
            third[0].msg_count, 3,
            "the line is counted once when its newline lands"
        );
        assert!(
            profile.counter("chat_sync.bytes_read") > pending.len() as u64,
            "the unfinished line was read again, not trusted"
        );
    }

    // TP-CHATSYNC-03: a transcript that did not only grow is read whole. The
    // fold trusts what it already read only while the file is the same file
    // (identity), at least as long (size), and still holds the bytes it ended
    // on (anchor); a truncation, a replacement, an in-place rewrite and an
    // insertion before the fold's end each break one of those, and a reader
    // that trusted the stale head would serve the old title forever.
    #[test]
    fn a_rewritten_transcript_is_read_whole_again() {
        let original = [
            r#"{"type":"user","timestamp":"2026-10-17T09:00:00.000Z","message":{"content":"hi"}}"#,
            r#"{"type":"custom-title","customTitle":"alpha"}"#,
        ];
        let tail = concat!(
            r#"{"type":"user","timestamp":"2026-10-17T09:30:00.000Z","message":{"content":"more"}}"#,
            "\n"
        );
        // A last line longer than the fold's anchor: a file that keeps it can
        // end exactly where the old one ended and still be another file.
        let pad = concat!(
            r#"{"type":"summary","summary":""#,
            "pppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppppp",
            r#""}"#
        );
        for case in [
            "shrunk",
            "replaced",
            "replaced-same-end",
            "rewritten-in-place",
            "inserted-before",
        ] {
            let tp = TempProjects::new(case);
            let project = "/home/x/rewritten";
            let mut written = original.to_vec();
            if case == "replaced-same-end" {
                written.push(pad);
            }
            let path = tp.write_session(project, "chat", &written);
            let mut cache = SessionParseCache::new();
            let _ = read_counted(&tp.root, project, &mut cache);

            match case {
                "shrunk" => fs::write(&path, format!("{}\n", original[0])).expect("shrink"),
                "replaced" => {
                    let staged = path.with_extension("staged");
                    fs::write(
                        &staged,
                        format!(
                            "{}\n{}\n{tail}",
                            original[0], r#"{"type":"custom-title","customTitle":"omega"}"#
                        ),
                    )
                    .expect("stage the replacement");
                    fs::rename(&staged, &path).expect("replace");
                }
                // Another file (a new inode) holding every byte the fold ended
                // on, and longer — but its first words are not the old file's.
                // Only the file's identity tells the two apart.
                "replaced-same-end" => {
                    let staged = path.with_extension("staged");
                    fs::write(
                        &staged,
                        format!(
                            "{}\n{}\n{pad}\n{tail}",
                            r#"{"type":"user","timestamp":"2026-10-17T09:00:00.000Z","message":{"content":"yo"}}"#,
                            original[1]
                        ),
                    )
                    .expect("stage the replacement");
                    fs::rename(&staged, &path).expect("replace");
                }
                // Same inode, same first line; the line the fold ended on
                // now says something else, and the file is longer.
                "rewritten-in-place" => fs::write(
                    &path,
                    format!(
                        "{}\n{}\n{tail}",
                        original[0], r#"{"type":"custom-title","customTitle":"omega"}"#
                    ),
                )
                .expect("rewrite in place"),
                "inserted-before" => fs::write(
                    &path,
                    format!(
                        "{}\n{}\n{}\n{tail}",
                        r#"{"type":"custom-title","customTitle":"inserted at the top"}"#,
                        original[0],
                        original[1]
                    ),
                )
                .expect("insert before"),
                _ => unreachable!(),
            }

            let (got, profile) = read_counted(&tp.root, project, &mut cache);
            assert_eq!(got, read_sessions_for_project(&tp.root, project), "{case}");
            assert_eq!(
                profile.counter("chat_sync.full_parses"),
                1,
                "{case}: a file that did not only grow is read whole"
            );
            assert_eq!(profile.counter("chat_sync.tail_parses"), 0, "{case}");
        }
    }

    // TP-CHATSYNC-04: the by-id road answers from an index of the store, and
    // an unchanged store is not listed again. The road used to list every
    // project directory and stat `<id>.jsonl` in each, per id, every sync:
    // measured 2026-10-17, 374 re-homed chats (25 of them in no store at all)
    // against 693 directories, a third of a 5-second freeze. The answer is
    // the probe's — the first directory in name order holding the id — and
    // the id no store holds costs nothing on the next pass either.
    #[test]
    fn an_id_lookup_answers_from_the_store_index_and_lists_nothing_twice() {
        let tp = TempProjects::new("store-index");
        tp.write_session(
            "/home/x/a",
            "shared",
            &[r#"{"type":"custom-title","customTitle":"from a"}"#],
        );
        tp.write_session(
            "/home/x/c",
            "shared",
            &[r#"{"type":"custom-title","customTitle":"from c"}"#],
        );
        tp.write_session(
            "/home/x/b",
            "only-b",
            &[r#"{"type":"custom-title","customTitle":"b"}"#],
        );
        for i in 0..40 {
            tp.write_session(
                &format!("/home/x/other-{i:02}"),
                &format!("filler-{i:02}"),
                &[r#"{"type":"custom-title","customTitle":"filler"}"#],
            );
        }
        for entry in fs::read_dir(&tp.root).expect("list the store").flatten() {
            age(&entry.path());
        }
        age(&tp.root);
        let ids: Vec<String> = ["shared", "only-b", "ghost", "../shared"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut cache = SessionParseCache::new();

        let (first, built) = crate::render_prof::observe_for_test(|| {
            read_sessions_by_ids_cached(&tp.root, &ids, &mut cache)
        });
        assert_eq!(
            first["shared"].title, "from a",
            "the first directory in name order wins, as the probe's did"
        );
        assert_eq!(
            find_session_file_by_id(&tp.root, "shared"),
            Some(
                tp.root
                    .join(encode_project_path("/home/x/a"))
                    .join("shared.jsonl")
            )
        );
        assert_eq!(first["only-b"].title, "b");
        assert!(!first.contains_key("ghost"));
        assert!(!first.contains_key("../shared"));
        assert!(
            built.counter("chat_sync.store_listings") > 0,
            "the first pass builds the index"
        );

        let (second, again) = crate::render_prof::observe_for_test(|| {
            read_sessions_by_ids_cached(&tp.root, &ids, &mut cache)
        });
        assert_eq!(second, first);
        assert_eq!(
            again.counter("chat_sync.store_listings"),
            0,
            "an unchanged store is not listed again, the ghost included"
        );
    }

    // TP-CHATSYNC-05: the index misses nothing the probe would find. A
    // transcript written into an existing directory moves that directory's
    // mtime; a new project directory moves the root's; a removed transcript
    // is never answered. And a directory whose mtime was still fresh when it
    // was listed is listed again on the next pass even if the mtime did not
    // move — an mtime is a coarse clock, and a file created in the same tick
    // as the listing would otherwise stay invisible (git's "racy" entries).
    #[test]
    fn the_store_index_sees_new_and_removed_transcripts_and_distrusts_a_racy_directory() {
        let tp = TempProjects::new("store-index-fresh");
        tp.write_session(
            "/home/x/a",
            "known",
            &[r#"{"type":"custom-title","customTitle":"known"}"#],
        );
        age(&tp.root.join(encode_project_path("/home/x/a")));
        age(&tp.root);
        let ids: Vec<String> = ["known", "late", "in-a-new-dir", "same-tick"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut cache = SessionParseCache::new();
        let found = read_sessions_by_ids_cached(&tp.root, &ids, &mut cache);
        assert_eq!(found.len(), 1);
        assert!(found.contains_key("known"));

        tp.write_session(
            "/home/x/a",
            "late",
            &[r#"{"type":"custom-title","customTitle":"late"}"#],
        );
        tp.write_session(
            "/home/x/brand-new",
            "in-a-new-dir",
            &[r#"{"type":"custom-title","customTitle":"new dir"}"#],
        );
        let found = read_sessions_by_ids_cached(&tp.root, &ids, &mut cache);
        assert!(
            found.contains_key("late"),
            "a new transcript in a known directory"
        );
        assert!(
            found.contains_key("in-a-new-dir"),
            "a new project directory"
        );

        let dir_a = tp.root.join(encode_project_path("/home/x/a"));
        fs::remove_file(dir_a.join("known.jsonl")).expect("remove");
        let found = read_sessions_by_ids_cached(&tp.root, &ids, &mut cache);
        assert!(
            !found.contains_key("known"),
            "a removed transcript is not answered"
        );

        // `a` was listed just now, while its mtime was fresh. Write into it
        // and put the mtime back where it was: a coarse clock that did not
        // tick. The fresh listing was not trusted, so the next pass looks.
        let tick = fs::metadata(&dir_a)
            .and_then(|m| m.modified())
            .expect("mtime");
        tp.write_session(
            "/home/x/a",
            "same-tick",
            &[r#"{"type":"custom-title","customTitle":"same tick"}"#],
        );
        fs::File::open(&dir_a)
            .expect("open the directory")
            .set_modified(tick)
            .expect("put the mtime back");
        let found = read_sessions_by_ids_cached(&tp.root, &ids, &mut cache);
        assert!(
            found.contains_key("same-tick"),
            "a directory listed in its own tick is listed again"
        );
    }

    // ---- TP-CHATSYNC-07..10: a restart starts warm -------------------------

    /// Two projects, three transcripts, read once through `cache`.
    fn warm_store(tag: &str) -> (TempProjects, SessionParseCache) {
        let tp = TempProjects::new(tag);
        tp.write_session("/home/x/a", "a1", &live_transcript_lines());
        tp.write_session(
            "/home/x/a",
            "a2",
            &[r#"{"type":"user","timestamp":"2026-10-17T08:00:00.000Z","message":{"content":"second chat"}}"#],
        );
        tp.write_session(
            "/home/x/b",
            "b1",
            &[r#"{"type":"ai-title","aiTitle":"another project"}"#],
        );
        let mut cache = SessionParseCache::new();
        for project in ["/home/x/a", "/home/x/b"] {
            let _ = read_recent_sessions_for_project_cached(&tp.root, project, 12, &mut cache);
        }
        (tp, cache)
    }

    /// Both projects through `cache`, with the reader's counters summed.
    fn read_both(
        root: &Path,
        cache: &mut SessionParseCache,
    ) -> (Vec<ClaudeSession>, u64, u64, u64) {
        let (mut sessions, mut full, mut tail, mut bytes) = (Vec::new(), 0, 0, 0);
        for project in ["/home/x/a", "/home/x/b"] {
            let (found, profile) = read_counted(root, project, cache);
            sessions.extend(found);
            full += profile.counter("chat_sync.full_parses");
            tail += profile.counter("chat_sync.tail_parses");
            bytes += profile.counter("chat_sync.bytes_read");
        }
        (sessions, full, tail, bytes)
    }

    fn whole_both(root: &Path) -> Vec<ClaudeSession> {
        let mut whole = read_sessions_for_project(root, "/home/x/a");
        whole.extend(read_sessions_for_project(root, "/home/x/b"));
        whole
    }

    // TP-CHATSYNC-07: what one run folded, the next run starts from.
    // Measured 2026-10-17 22:49 on the reporting machine: a delivery's new
    // server merged once before it took over — 16.97 s, 543 transcripts read
    // whole, 3.5 GB — because the fold cache lived and died with the process.
    // Saved and loaded, an unchanged store is answered without opening a
    // transcript, and a grown one costs its tail.
    #[test]
    fn a_saved_cache_reads_an_unchanged_store_without_parsing_it() {
        let (tp, cache) = warm_store("saved-warm");
        let bytes = cache.to_store_bytes();
        let mut loaded = SessionParseCache::from_store_bytes(&bytes);

        let (sessions, full, tail, read) = read_both(&tp.root, &mut loaded);
        assert_eq!(sessions, whole_both(&tp.root));
        assert_eq!((full, tail, read), (0, 0, 0), "nothing moved, nothing read");

        let a1 = tp
            .root
            .join(encode_project_path("/home/x/a"))
            .join("a1.jsonl");
        append(
            &a1,
            concat!(
                r#"{"type":"custom-title","customTitle":"after the restart"}"#,
                "\n"
            )
            .as_bytes(),
        );
        let (sessions, full, tail, _) = read_both(&tp.root, &mut loaded);
        assert_eq!((full, tail), (0, 1), "the grown transcript costs its tail");
        assert_eq!(sessions, whole_both(&tp.root));
        assert!(sessions.iter().any(|s| s.title == "after the restart"));
    }

    // TP-CHATSYNC-08: the store file holds what the list shows, not what was
    // typed. A first message can be a pasted megabyte; the fold keeps it cut
    // to a title (`extract_user_text`), so the file stays small — and a fold
    // resumed from the file still reads like a whole read when it grows.
    #[test]
    fn a_pasted_first_message_is_kept_as_its_title_and_opening() {
        let tp = TempProjects::new("pasted");
        let pasted = format!(
            r#"{{"type":"user","timestamp":"2026-10-17T09:00:00.000Z","message":{{"content":"{}"}}}}"#,
            "log line with words ".repeat(52_000)
        );
        let path = tp.write_session("/home/x/a", "pasted", &[pasted.as_str()]);
        assert!(fs::metadata(&path).expect("stat").len() > 1_000_000);
        let mut cache = SessionParseCache::new();
        let _ = read_recent_sessions_for_project_cached(&tp.root, "/home/x/a", 12, &mut cache);

        let bytes = cache.to_store_bytes();
        assert!(
            bytes.len() <= 4_096,
            "a one-transcript store keeps {} bytes for a 1 MB first message",
            bytes.len()
        );
        let mut loaded = SessionParseCache::from_store_bytes(&bytes);
        assert_eq!(loaded.folded_transcripts(), 1, "the fold came back");
        let (sessions, profile) = read_counted(&tp.root, "/home/x/a", &mut loaded);
        assert_eq!(
            profile.counter("chat_sync.full_parses"),
            0,
            "answered from the store"
        );
        assert_eq!(sessions, read_sessions_for_project(&tp.root, "/home/x/a"));

        append(
            &path,
            concat!(r#"{"type":"assistant","timestamp":"2026-10-17T09:01:00.000Z","message":{"content":[{"type":"text","text":"read it"}]}}"#, "\n").as_bytes(),
        );
        let (sessions, profile) = read_counted(&tp.root, "/home/x/a", &mut loaded);
        assert_eq!(profile.counter("chat_sync.tail_parses"), 1);
        assert_eq!(sessions, read_sessions_for_project(&tp.root, "/home/x/a"));
        assert!(sessions[0].title.starts_with("log line with words"));
    }

    // TP-CHATSYNC-09: the store file is input like any other. Empty, not
    // JSON, another version, cut short: the cache starts empty and the store
    // is read whole — never a panic, never an answer from a file the reader
    // could not vouch for.
    #[test]
    fn a_damaged_store_file_starts_an_empty_cache() {
        let (tp, cache) = warm_store("damaged");
        let good = cache.to_store_bytes();
        let other_version = String::from_utf8(good.clone())
            .expect("the store file is text")
            .replacen(
                &format!("\"version\":{}", CHAT_FOLDS_VERSION),
                &format!("\"version\":{}", CHAT_FOLDS_VERSION + 1),
                1,
            );
        assert_ne!(
            other_version.as_bytes(),
            good.as_slice(),
            "the version is in the file"
        );
        for bytes in [
            Vec::new(),
            b"not json".to_vec(),
            other_version.into_bytes(),
            good[..good.len() / 2].to_vec(),
        ] {
            let mut loaded = SessionParseCache::from_store_bytes(&bytes);
            let (sessions, full, tail, _) = read_both(&tp.root, &mut loaded);
            assert_eq!(sessions, whole_both(&tp.root));
            assert_eq!(
                (full, tail),
                (3, 0),
                "{} bytes read as an empty cache",
                bytes.len()
            );
        }
    }

    // TP-CHATSYNC-10: a save writes the folds only when a fold moved. Nobody
    // looking means nothing written (the resource doctrine): an unchanged
    // store has nothing to say twice.
    #[test]
    fn only_a_cache_that_moved_has_anything_to_write() {
        let tp = TempProjects::new("moved");
        let path = tp.write_session("/home/x/a", "a1", &live_transcript_lines());
        let mut cache = SessionParseCache::new();
        assert!(!cache.take_changed(), "an empty cache has nothing to write");

        let _ = read_recent_sessions_for_project_cached(&tp.root, "/home/x/a", 12, &mut cache);
        assert!(cache.take_changed(), "a first read is new");
        assert!(!cache.take_changed(), "taken once");

        let _ = read_recent_sessions_for_project_cached(&tp.root, "/home/x/a", 12, &mut cache);
        assert!(
            !cache.take_changed(),
            "an unmoved transcript changes nothing"
        );

        append(
            &path,
            b"{\"type\":\"custom-title\",\"customTitle\":\"moved\"}\n",
        );
        let _ = read_recent_sessions_for_project_cached(&tp.root, "/home/x/a", 12, &mut cache);
        assert!(cache.take_changed(), "a grown transcript is new");

        let loaded = SessionParseCache::from_store_bytes(&cache.to_store_bytes());
        let mut loaded = loaded;
        assert!(!loaded.take_changed(), "what was loaded is what is on disk");
    }

    // TP-CHATSYNC-10: what a save carries. A run that leaves nothing on disk
    // carries nothing even when a fold moved — and does not consume the move,
    // so nothing is lost if persistence is the question asked first.
    #[test]
    fn a_save_carries_the_folds_only_when_one_moved_and_the_run_persists() {
        let (_tp, mut cache) = warm_store("carry");
        let mut codex = crate::codex_sessions::CodexParseCache::default();
        assert_eq!(
            chat_folds_to_write(false, &mut cache, &mut codex),
            None,
            "--no-session"
        );
        let carried =
            chat_folds_to_write(true, &mut cache, &mut codex).expect("a first read moved");
        assert_eq!(carried, cache.to_store_bytes());
        assert_eq!(
            chat_folds_to_write(true, &mut cache, &mut codex),
            None,
            "nothing moved since"
        );
    }

    // TP-CHATSYNC-07: the file round trip the App uses — written atomically,
    // read back whole; a missing file is an empty cache.
    #[test]
    fn the_folds_file_round_trips_and_a_missing_one_is_empty() {
        let (tp, cache) = warm_store("file");
        let file = tp.root.join("state").join("chat-folds.json");
        assert_eq!(
            load_chat_folds(&file).0.folded_transcripts(),
            0,
            "no file yet"
        );
        save_chat_folds(&file, &cache.to_store_bytes()).expect("write the folds");
        let (mut loaded, _) = load_chat_folds(&file);
        assert_eq!(loaded.folded_transcripts(), 3);
        let (sessions, full, tail, _) = read_both(&tp.root, &mut loaded);
        assert_eq!(sessions, whole_both(&tp.root));
        assert_eq!((full, tail), (0, 0));
    }

    /// A Codex store with one rollout standing in `cwd`, read once through a
    /// fresh cache — which a parse has marked.
    fn codex_store(
        tp: &TempProjects,
        id: &str,
        cwd: &str,
    ) -> crate::codex_sessions::CodexParseCache {
        let dir = tp.root.join("codex").join("2026/10/18");
        fs::create_dir_all(&dir).expect("codex store");
        fs::write(
            dir.join(format!("rollout-2026-10-18T10-00-00-{id}.jsonl")),
            format!(
                concat!(
                    r#"{{"timestamp":"2026-10-18T08:00:00.000Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-10-18T08:00:01.000Z","type":"event_msg","payload":{{"type":"user_message","message":"codex chat"}}}}"#,
                    "\n"
                ),
                id = id,
                cwd = cwd
            ),
        )
        .expect("rollout");
        let mut codex = crate::codex_sessions::CodexParseCache::default();
        let _ = crate::codex_sessions::read_recent_sessions_for_cwd_cached(
            &tp.root.join("codex"),
            Path::new(cwd),
            12,
            &mut codex,
        );
        codex
    }

    // TP-CHATSYNC-12: a save asks both caches. A Codex parse alone is worth a
    // write, and the file it writes carries it; both marks are taken by one
    // question, so the next save has nothing to write.
    #[test]
    fn a_save_carries_the_folds_when_either_cache_moved() {
        let (tp, mut claude) = warm_store("either");
        let mut codex = codex_store(&tp, "019cbcf7-7800-7002-a7e4-562e4595cb84", "/home/x/a");
        assert!(
            chat_folds_to_write(true, &mut claude, &mut codex).is_some(),
            "both moved"
        );
        assert_eq!(
            chat_folds_to_write(true, &mut claude, &mut codex),
            None,
            "one question took both marks"
        );

        let mut codex = codex_store(&tp, "019cbcf7-7800-7002-a7e4-562e4595cb85", "/home/x/b");
        let carried = chat_folds_to_write(true, &mut claude, &mut codex)
            .expect("a Codex parse alone is worth a write");
        let (folds, rollouts) = chat_folds_from_bytes(&carried);
        assert_eq!(folds.folded_transcripts(), 3, "Claude's folds ride along");
        // The window for b read both heads in the store: both are carried.
        assert_eq!(rollouts.len(), 2, "and every rollout the window parsed");
        assert_eq!(chat_folds_to_write(true, &mut claude, &mut codex), None);
        assert_eq!(chat_folds_to_write(false, &mut claude, &mut codex), None);
    }

    // TP-CHATSYNC-13: the file across the change. One written before Codex's
    // part was kept loads its Claude folds (Codex's cache empty); one written
    // now reads in the shape the previous reader knew, so a rollback keeps the
    // folds too.
    #[test]
    fn a_folds_file_without_codex_loads_and_a_previous_reader_reads_a_new_one() {
        #[derive(serde::Deserialize)]
        struct PreviousStoredFolds {
            version: u32,
            folds: Vec<(PathBuf, TranscriptFold)>,
        }
        let (tp, claude) = warm_store("compat");
        let codex = codex_store(&tp, "019cbcf7-7800-7002-a7e4-562e4595cb84", "/home/x/a");
        let now = chat_folds_bytes(&claude, &codex);

        let mut previous: serde_json::Value = serde_json::from_slice(&now).expect("json");
        assert!(
            previous.get("codex").is_some(),
            "the new file carries Codex's part"
        );
        previous.as_object_mut().expect("an object").remove("codex");
        let previous = serde_json::to_vec(&previous).expect("bytes");
        let (folds, rollouts) = chat_folds_from_bytes(&previous);
        assert_eq!(
            folds.folded_transcripts(),
            3,
            "the folds of a file from before"
        );
        assert_eq!(rollouts.len(), 0);

        let read_back: PreviousStoredFolds =
            serde_json::from_slice(&now).expect("the previous reader reads the new file");
        assert_eq!(read_back.version, CHAT_FOLDS_VERSION);
        assert_eq!(read_back.folds.len(), 3);
    }
}
