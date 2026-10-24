//! Wire protocol for herdr server/client communication.
//!
//! Defines the message types, framing, version negotiation, and safety
//! constraints for the binary protocol over local sockets.
//!
//! `PROTOCOL_VERSION` still guards same-install direct-terminal and internal
//! operations. Client-owned shells negotiate the independent stable endpoint
//! contract in [`super::endpoint`].

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Current protocol version. Bumped when wire format changes incompatibly.
///
/// Still 22 after the U1 upstream sync (2026-09-26): upstream's own 20 -> 21 (#3206,
/// explicit workspace group close) changed no wire type — the socket API gained an
/// optional `close_group` field that old payloads still satisfy. The merge leaves the
/// fork's frames byte-for-byte as they were, so running clients keep talking.
///
/// 22, not 21: the handshake now carries a capability announcement
/// (`Hello.capabilities` / `Welcome.accepted`), which changes the frame shape
/// once. It is meant to be the **last** media-driven bump: capabilities are
/// names, so a future audio or video feature adds a name and leaves this
/// number alone. Reaching for a bump because "media needs a new field" is the
/// mistake this field was added to retire — see `docs/patterns/
/// remote-media-transport.md` RA7, and the 20 -> 21 bump below, which cost the
/// whole client fleet for one boolean.
///
/// 21, not 20: protocol 20 is already published in the preview channel, and
/// `Hello` now carries the client's pixel-mouse capability. The check below is
/// exact-match, so reusing 20 would let a released client speak a different
/// dialect under the same number.
///
/// 20, not 19: the latest released tag (preview-2026-08-04) already shipped a
/// wire that calls itself 19 while this source line said 18. Numbers are
/// cheap; collisions are not.
pub const PROTOCOL_VERSION: u32 = 22;

/// Maximum allowed frame payload size (2 MB). Frames larger than this are
/// rejected to prevent denial-of-service via oversized length prefixes.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Maximum allowed server-to-client frame payload when Kitty graphics are enabled.
/// Normal traffic keeps `MAX_FRAME_SIZE`; this larger cap is only for explicit
/// image payloads that are naturally much larger after base64 encoding.
pub const MAX_GRAPHICS_FRAME_SIZE: usize = 32 * 1024 * 1024;

/// Maximum clipboard image payload size for remote paste bridging.
pub const MAX_CLIPBOARD_IMAGE_PAYLOAD: usize = 16 * 1024 * 1024;

/// Length of the u32 little-endian length prefix in bytes.
const LENGTH_PREFIX_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// Capability negotiation
// ---------------------------------------------------------------------------

/// Well-known capability names.
///
/// Names, not enum variants, and that is the whole point. `bincode` is not
/// self-describing: an unknown enum discriminant does not skip a field, it
/// fails the decode of the entire message. So a capability enum would make
/// every new media feature a wire change, and every wire change a
/// `PROTOCOL_VERSION` bump — the exact loop this negotiation exists to end.
/// A name list stays the same shape forever; a reader that has never heard of
/// a name simply never asks for it.
///
/// This is the shape SSH (RFC 4253 §7.1, algorithm name lists), TLS
/// (RFC 8446 §4.2, "clients MUST ignore unrecognized extensions") and HTTP/2
/// (RFC 9113 §6.5.2, unknown SETTINGS identifiers MUST be ignored) all
/// converged on independently.
pub mod capability {
    // The names exist before their first consumer on purpose: F0 builds the
    // negotiation and F1 is the phase that announces `MEDIA_STREAMS` and
    // `AUDIO_SINK` for real. Defining them here keeps every later phase from
    // inventing its own spelling of the same name, which is the failure two
    // herdr-browser plugins already produced independently.
    #![allow(dead_code)]

    /// Client can receive timestamped media streams and take part in clock sync.
    pub const MEDIA_STREAMS: &str = "media.streams";
    /// Client has a local audio sink. Values are the codecs it can decode.
    pub const AUDIO_SINK: &str = "media.audio.sink";
    /// Client can decode video and draw it itself. Values are the codecs.
    pub const VIDEO_DECODE: &str = "media.video.decode";
    /// Client can open a second transport for media. Values are the kinds.
    pub const SIDE_CHANNEL: &str = "media.side_channel";
    /// Client wants pane rectangles so it can place drawing of its own.
    ///
    /// Gated rather than always-on for the reason the resource doctrine gives:
    /// a client that never draws over a pane must pay nothing for the ones that
    /// do. A client that does not announce this receives no geometry at all.
    pub const PANE_GEOMETRY: &str = "pane.geometry";
}

/// Codec names, shared by the capability values and the wire.
///
/// One vocabulary, not two. `AUDIO_SINK`'s values are drawn from here and so
/// is `MediaOpen::codec`, which is what makes the handshake's answer directly
/// usable as the stream's codec without a translation table nobody maintains.
pub mod codec {
    #![allow(dead_code)]

    /// Opus, the only real-time audio codec both clients decode natively.
    /// 48 kHz, 20 ms frames, 64-128 kbps (canonical design L3).
    pub const OPUS: &str = "opus";
    /// Uncompressed little-endian signed 16-bit PCM. Local and debugging use;
    /// 32x the bytes of Opus at the same quality, measured, not estimated.
    pub const PCM_S16LE: &str = "pcm-s16le";
    /// H.264 in Annex-B byte-stream form: start codes, no container.
    ///
    /// Annex-B rather than a container because the consumer is a decoder, not
    /// a demuxer — `mpv -` and every browser decoder read this shape directly,
    /// and a container would add a parser on both ends to carry timing that
    /// `MediaChunk::pts_us` already carries.
    ///
    /// Chosen over AV1 despite the patent story, on a measurement: the encode
    /// hardware present here does H.264 and HEVC and **refuses AV1**
    /// (`av1_nvenc`: "No capable devices found" — Ampere decodes AV1 but does
    /// not encode it). A royalty-free codec nothing can encode is not an
    /// option, it is a plan.
    pub const H264: &str = "h264";
}

/// Stream parameters, one variant per media kind.
///
/// Audio is the only kind F1 carries. Video appends a variant in F2 — appends,
/// because a variant inserted before this one would re-tag every stream that
/// is already open on a released client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaParams {
    /// Sample rate and channel count of an audio stream.
    Audio { sample_rate_hz: u32, channels: u8 },
    /// Picture size and nominal rate of a video stream.
    ///
    /// Appended, exactly as the note above promised it would be. The size is
    /// the encoder's, not the pane's: a pane that resizes gets a new stream
    /// rather than a changed one, because a decoder mid-GOP cannot be told its
    /// picture just changed shape.
    ///
    /// `fps` is nominal — what the encoder was configured for, not what the
    /// source delivered. Real timing rides `MediaChunk::pts_us`, which is the
    /// only clock A/V sync rests on; a consumer that trusted this number
    /// instead would drift the moment the source stuttered.
    Video { width: u32, height: u32, fps: u32 },
}

/// Why a media stream ended.
///
/// Deliberately two values and a free-form detail, rather than a growing list
/// of reasons: a receiver that meets an unknown discriminant cannot decode the
/// message at all, so the closed set carries the meaning and the string carries
/// the specifics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaCloseReason {
    /// The source finished normally.
    Ended,
    /// The stream stopped because something went wrong. `detail` says what.
    Failed,
}

/// One pane's place on the client's screen, in the client's own pixels.
///
/// Pixels rather than cells, deliberately: the consumer of this is a player,
/// and making it multiply by a cell size would put the same rounding rule in
/// two places. Two places that round are two places that can disagree, and the
/// disagreement shows up as a picture one pixel off its own border.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRect {
    /// Public pane id — the same string `ServerMessage::MediaOpen::pane_id`
    /// carries, so a consumer joins a stream to its rectangle without a second
    /// lookup and without a second id space to keep in step.
    pub pane_id: String,
    /// Outer box, borders included.
    pub x: u32,
    /// Outer box, borders included.
    pub y: u32,
    /// Outer box, borders included.
    pub width: u32,
    /// Outer box, borders included.
    pub height: u32,
    /// Content box, borders excluded. ⭐ This is the clip rectangle: anything
    /// drawn outside it paints over the pane's own frame, which is the whole
    /// reason the outer box alone is not enough.
    pub inner_x: u32,
    /// Content box, borders excluded.
    pub inner_y: u32,
    /// Content box, borders excluded.
    pub inner_width: u32,
    /// Content box, borders excluded.
    pub inner_height: u32,
    /// Whether this pane holds focus right now.
    pub focused: bool,
}

/// One announced capability: a name plus its parameter values.
///
/// Values are free-form strings whose meaning belongs to the name (codec ids
/// for [`capability::AUDIO_SINK`], transport kinds for
/// [`capability::SIDE_CHANNEL`]). Keeping them untyped here is deliberate: a
/// typed payload would put the forward-compatibility problem back into the
/// wire format, one level down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    /// Capability name. See [`capability`] for the well-known ones.
    pub name: String,
    /// Parameter values, in the announcer's order of preference.
    pub values: Vec<String>,
}

impl CapabilityEntry {
    // Constructors are used by tests and by the phases that announce real
    // capabilities; production code in F0 only ever passes empty lists.
    #![allow(dead_code)]

    /// A capability with no parameters.
    pub fn flag(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            values: Vec::new(),
        }
    }

    /// A capability carrying ordered parameter values.
    pub fn with_values<I, S>(name: impl Into<String>, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            name: name.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }
}

/// A read-only view over announced capabilities.
///
/// The reading contract is the negotiation's safety property: asking about a
/// name nobody announced is an ordinary `false`, never an error, so a peer
/// speaking a richer dialect costs this side nothing. TP-MEDIA-CAP-02
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    entries: Vec<CapabilityEntry>,
}

impl CapabilitySet {
    // `has` and `values_of` are the reading half of the contract. F0 negotiates
    // and stores; F1 is the first phase that asks. Keeping them beside
    // `intersect` means the reading rule is written and tested once, not
    // rediscovered by whichever feature needs it first.
    #![allow(dead_code)]

    /// Builds a set from announced entries.
    ///
    /// A repeated name keeps its first announcement: a malformed or hostile
    /// peer must not be able to make the handshake ambiguous.
    pub fn from_entries(entries: impl IntoIterator<Item = CapabilityEntry>) -> Self {
        let mut kept: Vec<CapabilityEntry> = Vec::new();
        for entry in entries {
            if !kept.iter().any(|existing| existing.name == entry.name) {
                kept.push(entry);
            }
        }
        Self { entries: kept }
    }

    /// Whether `name` was announced.
    pub fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name == name)
    }

    /// Values announced for `name`, or an empty slice when absent.
    pub fn values_of(&self, name: &str) -> &[String] {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map_or(&[], |entry| entry.values.as_slice())
    }

    /// The value both sides agreed on for `name`, if there is one.
    ///
    /// The question `has` cannot answer. `intersect` keeps a name whose values
    /// do not overlap — it has to, because a flag capability has no values and
    /// an empty list is therefore not evidence of disagreement. That leaves
    /// `has(AUDIO_SINK) == true` with no codec in common, which is exactly the
    /// state where opening a stream produces silence and no error.
    ///
    /// So valued capabilities ask this instead, and flags keep asking `has`.
    /// `None` here means "no usable agreement", never "not supported".
    pub fn negotiated_value(&self, name: &str) -> Option<&str> {
        self.values_of(name).first().map(String::as_str)
    }

    /// The capabilities both sides support.
    ///
    /// Names present on only one side are dropped, and the surviving values are
    /// the ones both sides named, in `self`'s announced order. Reflecting an
    /// announcement back instead would accept capabilities this side cannot
    /// serve — and would leave every test green while doing it.
    pub fn intersect(&self, other: &Self) -> Self {
        let entries = self
            .entries
            .iter()
            .filter_map(|entry| {
                let theirs = other
                    .entries
                    .iter()
                    .find(|candidate| candidate.name == entry.name)?;
                let values = entry
                    .values
                    .iter()
                    .filter(|value| theirs.values.contains(value))
                    .cloned()
                    .collect();
                Some(CapabilityEntry {
                    name: entry.name.clone(),
                    values,
                })
            })
            .collect();
        Self { entries }
    }

    /// The announced entries, for putting back on the wire.
    pub fn entries(&self) -> &[CapabilityEntry] {
        &self.entries
    }

    /// Consumes the set into its entries.
    pub fn into_entries(self) -> Vec<CapabilityEntry> {
        self.entries
    }
}

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

/// Render payload encoding negotiated during client handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderEncoding {
    /// Send full semantic FrameData values. This is the local/default mode.
    SemanticFrame,
    /// Send already-diffed terminal ANSI byte streams.
    TerminalAnsi,
}

/// Keybinding profile requested by an attached app client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeybindings {
    /// Use the server's own keybinding config.
    Server,
    /// Use this attached client's normalized local `[keys]` config.
    Local { keys_toml: String },
}

/// Client behavior requested at connection time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientLaunchMode {
    /// Full app client rendered by the server.
    App,
    /// Full app client eligible for audited local direct graphics.
    AppDirectGraphics,
    /// Direct terminal attach client.
    TerminalAttach,
    /// Media-only listener: it wants a pane's audio and nothing else.
    ///
    /// Appended, never inserted — a variant placed before an existing one
    /// re-tags every message a released client already knows how to read.
    ///
    /// Its own mode rather than an App client with a flag, because connecting
    /// as an App client is what carries the three costs a listener must not
    /// pay: it takes foreground from the user's terminal, it resizes the
    /// shared runtime to its own size, and it becomes a render target whose
    /// frames are then discarded. None of the three reports anything.
    MediaListen,
}

/// Size of the pane surface requested by a client-owned shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSurfaceSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeyKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    Char(char),
    F(u8),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseKind {
    Down(ClientMouseButton),
    Up(ClientMouseButton),
    Drag(ClientMouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMousePosition {
    Cell {
        column: u16,
        row: u16,
    },
    Pixels {
        x: u32,
        y: u32,
        column: u16,
        row: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMouseGeometry {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientInputEvent {
    Key {
        code: ClientKeyCode,
        modifiers: u8,
        kind: ClientKeyKind,
        repeat_count: u16,
        generated_text: Option<String>,
        source: ClientKeySource,
    },
    TextCommit(String),
    Mouse {
        kind: ClientMouseKind,
        column: u16,
        row: u16,
        modifiers: u8,
    },
    Paste {
        text: String,
    },
    FocusGained,
    FocusLost,
}

/// Pane-domain input after the client has classified and consumed shell actions.
///
/// Keys are semantic rather than outer-terminal VT bytes so the target pane can
/// encode them for the child application's negotiated keyboard protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientPaneInputEvent {
    Key {
        code: ClientKeyCode,
        modifiers: u8,
        kind: ClientKeyKind,
        repeat_count: u16,
        shifted_codepoint: Option<u32>,
        generated_text: Option<String>,
        tracks_release: bool,
        physical_key_id: Option<u32>,
        windows_record: Option<crate::input::WindowsKeyRecord>,
    },
    TextCommit(String),
    Mouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    },
    Paste(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeySource {
    Synthesized,
    Vt {
        bytes: Vec<u8>,
    },
    WindowsConsole {
        record: crate::input::WindowsKeyRecord,
    },
}

impl ClientKeyKind {
    pub(crate) fn from_crossterm(kind: crossterm::event::KeyEventKind) -> Self {
        match kind {
            crossterm::event::KeyEventKind::Press => Self::Press,
            crossterm::event::KeyEventKind::Repeat => Self::Repeat,
            crossterm::event::KeyEventKind::Release => Self::Release,
        }
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::KeyEventKind {
        match self {
            Self::Press => crossterm::event::KeyEventKind::Press,
            Self::Repeat => crossterm::event::KeyEventKind::Repeat,
            Self::Release => crossterm::event::KeyEventKind::Release,
        }
    }
}

impl ClientKeyCode {
    pub(crate) fn from_crossterm(code: crossterm::event::KeyCode) -> Option<Self> {
        use crossterm::event::KeyCode;
        Some(match code {
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::Esc => Self::Esc,
            KeyCode::Char(ch) => Self::Char(ch),
            KeyCode::F(n) => Self::F(n),
            KeyCode::Null => Self::Null,
            _ => return None,
        })
    }

    pub(crate) fn to_crossterm(&self) -> crossterm::event::KeyCode {
        use crossterm::event::KeyCode;
        match self {
            Self::Backspace => KeyCode::Backspace,
            Self::Enter => KeyCode::Enter,
            Self::Left => KeyCode::Left,
            Self::Right => KeyCode::Right,
            Self::Up => KeyCode::Up,
            Self::Down => KeyCode::Down,
            Self::Home => KeyCode::Home,
            Self::End => KeyCode::End,
            Self::PageUp => KeyCode::PageUp,
            Self::PageDown => KeyCode::PageDown,
            Self::Tab => KeyCode::Tab,
            Self::BackTab => KeyCode::BackTab,
            Self::Delete => KeyCode::Delete,
            Self::Insert => KeyCode::Insert,
            Self::Esc => KeyCode::Esc,
            Self::Char(ch) => KeyCode::Char(*ch),
            Self::F(n) => KeyCode::F(*n),
            Self::Null => KeyCode::Null,
        }
    }
}

impl ClientMouseButton {
    pub(crate) fn from_crossterm(button: crossterm::event::MouseButton) -> Self {
        match button {
            crossterm::event::MouseButton::Left => Self::Left,
            crossterm::event::MouseButton::Right => Self::Right,
            crossterm::event::MouseButton::Middle => Self::Middle,
        }
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::MouseButton {
        match self {
            Self::Left => crossterm::event::MouseButton::Left,
            Self::Right => crossterm::event::MouseButton::Right,
            Self::Middle => crossterm::event::MouseButton::Middle,
        }
    }
}

impl ClientMouseKind {
    pub(crate) fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self> {
        use crossterm::event::MouseEventKind;
        Some(match kind {
            MouseEventKind::Down(button) => Self::Down(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Up(button) => Self::Up(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Drag(button) => Self::Drag(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Moved => Self::Moved,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            MouseEventKind::ScrollLeft => Self::ScrollLeft,
            MouseEventKind::ScrollRight => Self::ScrollRight,
        })
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::MouseEventKind {
        use crossterm::event::MouseEventKind;
        match self {
            Self::Down(button) => MouseEventKind::Down(button.to_crossterm()),
            Self::Up(button) => MouseEventKind::Up(button.to_crossterm()),
            Self::Drag(button) => MouseEventKind::Drag(button.to_crossterm()),
            Self::Moved => MouseEventKind::Moved,
            Self::ScrollUp => MouseEventKind::ScrollUp,
            Self::ScrollDown => MouseEventKind::ScrollDown,
            Self::ScrollLeft => MouseEventKind::ScrollLeft,
            Self::ScrollRight => MouseEventKind::ScrollRight,
        }
    }
}

impl ClientPaneInputEvent {
    pub(crate) fn from_terminal_key(key: crate::input::TerminalKey) -> Option<Self> {
        let tracks_release = key.generated_text.is_none() || key.has_physical_identity();
        let physical_key_id = key.physical_key_id();
        let windows_record = key.windows_record();
        Some(Self::Key {
            code: ClientKeyCode::from_crossterm(key.code)?,
            modifiers: key.modifiers.bits(),
            kind: ClientKeyKind::from_crossterm(key.kind),
            repeat_count: key.repeat_count,
            shifted_codepoint: key.shifted_codepoint,
            generated_text: key.generated_text,
            tracks_release,
            physical_key_id,
            windows_record,
        })
    }

    pub(crate) fn to_raw_input_event(&self) -> crate::raw_input::RawInputEvent {
        self.to_raw_input_event_with_windows_source(cfg!(any(windows, test)))
    }

    fn to_raw_input_event_with_windows_source(
        &self,
        attach_windows_source: bool,
    ) -> crate::raw_input::RawInputEvent {
        match self {
            Self::Key {
                code,
                modifiers,
                kind,
                repeat_count,
                shifted_codepoint,
                generated_text,
                tracks_release,
                windows_record,
                ..
            } => {
                let mut key = crate::input::TerminalKey::new(
                    code.to_crossterm(),
                    crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
                )
                .with_kind(kind.to_crossterm())
                .with_repeat_count(*repeat_count)
                .with_generated_text(generated_text.clone())
                .with_physical_identity_hint(*tracks_release && generated_text.is_some())
                .with_windows_composition_hint(*windows_record);
                if let Some(shifted_codepoint) = shifted_codepoint {
                    key = key.with_shifted_codepoint(*shifted_codepoint);
                }
                #[cfg(any(windows, test))]
                if attach_windows_source {
                    if let Some(record) = windows_record {
                        key = key.with_windows_record(*record);
                    }
                }
                #[cfg(not(any(windows, test)))]
                let _ = (attach_windows_source, windows_record);
                crate::raw_input::RawInputEvent::Key(key)
            }
            Self::TextCommit(text) => {
                crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new(text.clone()))
            }
            Self::Mouse {
                kind,
                position,
                modifiers,
                ..
            } => {
                let (column, row) = match position {
                    ClientMousePosition::Cell { column, row }
                    | ClientMousePosition::Pixels { column, row, .. } => (*column, *row),
                };
                crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: kind.to_crossterm(),
                    column,
                    row,
                    modifiers: crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
                })
            }
            Self::Paste(text) => crate::raw_input::RawInputEvent::Paste(text.clone()),
        }
    }
}

impl ClientInputEvent {
    #[cfg(any(windows, test))]
    pub(crate) fn from_crossterm(event: crossterm::event::Event) -> Option<Self> {
        match event {
            crossterm::event::Event::Key(key) => Some(Self::Key {
                code: ClientKeyCode::from_crossterm(key.code)?,
                modifiers: key.modifiers.bits(),
                kind: ClientKeyKind::from_crossterm(key.kind),
                repeat_count: 1,
                generated_text: None,
                source: ClientKeySource::Synthesized,
            }),
            crossterm::event::Event::Mouse(mouse) => Some(Self::Mouse {
                kind: ClientMouseKind::from_crossterm(mouse.kind)?,
                column: mouse.column,
                row: mouse.row,
                modifiers: mouse.modifiers.bits(),
            }),
            crossterm::event::Event::Paste(text) => Some(Self::Paste { text }),
            crossterm::event::Event::FocusGained => Some(Self::FocusGained),
            crossterm::event::Event::FocusLost => Some(Self::FocusLost),
            crossterm::event::Event::Resize(_, _) => None,
        }
    }

    pub(crate) fn to_raw_input_event(&self) -> crate::raw_input::RawInputEvent {
        match self {
            Self::Key {
                code,
                modifiers,
                kind,
                repeat_count,
                generated_text,
                source,
            } => {
                let mut key = crate::input::TerminalKey::new(
                    code.to_crossterm(),
                    crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
                )
                .with_generated_text(generated_text.clone());
                key = match source {
                    ClientKeySource::Synthesized => key,
                    ClientKeySource::Vt { bytes } => key.with_vt_bytes(bytes.clone()),
                    ClientKeySource::WindowsConsole { record } => key.with_windows_record(*record),
                };
                key = key
                    .with_repeat_count(*repeat_count)
                    .with_kind(kind.to_crossterm());
                crate::raw_input::RawInputEvent::Key(key)
            }
            Self::TextCommit(text) => {
                crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new(text.clone()))
            }
            Self::Mouse {
                kind,
                column,
                row,
                modifiers,
            } => crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind: kind.to_crossterm(),
                column: *column,
                row: *row,
                modifiers: crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
            }),
            Self::Paste { text } => crate::raw_input::RawInputEvent::Paste(text.clone()),
            Self::FocusGained => crate::raw_input::RawInputEvent::OuterFocusGained,
            Self::FocusLost => crate::raw_input::RawInputEvent::OuterFocusLost,
        }
    }
}

/// Messages sent from the client to the server over the client protocol socket.
///
/// Variant order is frozen for endpoint generation 1. Add compatible endpoint
/// behavior through `EndpointControl` or advertised API methods, not new enum variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Handshake: client announces its protocol version and terminal dimensions.
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        /// Render encoding requested by the client.
        requested_encoding: RenderEncoding,
        /// Keybinding profile requested by the client.
        keybindings: ClientKeybindings,
        /// Whether this connection will render the full app or attach directly to one terminal.
        launch_mode: ClientLaunchMode,
        /// Whether this client can report sub-cell (DECSET 1016) mouse positions.
        ///
        /// Independent of `launch_mode`: direct graphics ships whole frames and is refused over
        /// relayed transports, while a pixel mouse report only widens numbers already present in
        /// every mouse event.
        pixel_mouse: bool,
        /// Optional capabilities this client announces (see [`capability`]).
        ///
        /// An empty list is the ordinary case and means "behave exactly as
        /// before". Names the server does not know are ignored, which is what
        /// lets later media features ship without touching this shape again.
        capabilities: Vec<CapabilityEntry>,
    },

    /// Raw input bytes read from the client's stdin.
    Input {
        /// Raw terminal input (possibly multi-byte escape sequences).
        data: Vec<u8>,
    },

    /// Image bytes read from the client's local clipboard for remote paste bridging.
    ClipboardImage {
        /// Stable terminal target selected by the client that read the clipboard.
        target: ClientClipboardImageTarget,
        /// Image file extension without a leading dot.
        extension: String,
        /// Raw image bytes.
        data: Vec<u8>,
    },

    /// Terminal resize notification from the client.
    Resize {
        /// New terminal width in columns.
        cols: u16,
        /// New terminal height in rows.
        rows: u16,
        /// Width of a terminal cell in physical pixels, or 0 when client-side Kitty graphics are disabled.
        cell_width_px: u32,
        /// Height of a terminal cell in physical pixels, or 0 when unavailable.
        cell_height_px: u32,
        /// Whether this resize carries coherent exact geometry for SGR pixel mouse input.
        pixel_mouse: bool,
    },

    /// Graceful disconnect request.
    Detach,

    /// Switch this connection into direct terminal attach mode.
    AttachTerminal {
        /// Terminal id to attach to.
        terminal_id: String,
        /// Replace an existing writable attach owner for this terminal.
        takeover: bool,
    },

    /// Scroll input handled by a direct terminal attach client.
    AttachScroll {
        /// Original input source for routing.
        source: AttachScrollSource,
        /// Scroll direction.
        direction: AttachScrollDirection,
        /// Number of terminal rows to move when using host scrollback.
        lines: u16,
        /// Mouse column relative to the attached terminal, when available.
        column: Option<u16>,
        /// Mouse row relative to the attached terminal, when available.
        row: Option<u16>,
        /// Crossterm-compatible modifier bits for forwarded mouse wheel events.
        modifiers: u8,
    },

    /// Structured input events from platform clients that do not expose Unix-style raw bytes.
    InputEvents { events: Vec<ClientInputEvent> },

    /// Switch this connection into read-only terminal observe mode.
    ObserveTerminal {
        /// Pane, terminal, or agent target to observe.
        target: String,
    },

    /// Switch this connection into writable terminal control mode.
    ControlTerminal {
        /// Pane, terminal, or agent target to control.
        target: String,
        /// Replace an existing writable controller for this terminal.
        takeover: bool,
    },

    /// Result of the one armed Herdr-owned direct Kitty transmission.
    GraphicsTransmissionResult {
        transfer_id: u64,
        image_id: u32,
        success: bool,
    },

    /// One confirmed SGR 1016 mouse report with read-time host geometry.
    InputPixels {
        data: Vec<u8>,
        cols: u16,
        rows: u16,
        width_px: u32,
        height_px: u32,
    },

    /// The direct command was written and flushed; terminal response timing starts now.
    GraphicsTransmissionStarted { transfer_id: u64, image_id: u32 },

    // -- Media streams (F1) -------------------------------------------------
    /// How many further chunks the client can hold for this stream.
    ///
    /// Without it a slow client grows the server's memory instead of its own
    /// queue, and the first sign of trouble is the server, not the playback.
    MediaCredit { stream_id: u32, chunks: u16 },

    /// Clock-sync probe. The client stamps it and the server answers with
    /// `ServerMessage::TimeSyncReply`.
    TimeSync { client_send_us: u64 },

    /// Experimental handshake for a client-owned shell around one pane surface.
    ClientShellHello {
        version: u32,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        requested_encoding: RenderEncoding,
        surface_size: ClientSurfaceSize,
        pixel_mouse: bool,
        direct_graphics: bool,
        /// Whether the endpoint's keymap, rather than the client's, owns shell bindings.
        endpoint_keybindings: bool,
        /// Whether this client wants shell mouse capture even without pane demand.
        mouse_capture: bool,
    },

    /// Resize the pane viewport of a client-owned shell.
    ClientShellResize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        surface_size: ClientSurfaceSize,
        /// Whether this resize carries coherent exact geometry for SGR pixel mouse input.
        pixel_mouse: bool,
    },

    /// Deliver client-classified semantic input directly to a stable pane target.
    ClientShellPaneInput {
        pane_id: String,
        events: Vec<ClientPaneInputEvent>,
    },

    /// Deliver client-classified semantic input to the active popup terminal.
    ClientShellPopupInput {
        terminal_id: String,
        events: Vec<ClientPaneInputEvent>,
    },

    /// Invoke one endpoint operation through this client shell's selected connection.
    ClientShellEndpointRequest { boot_id: String, request: String },

    /// Deliver one structured mouse event to a directly attached terminal.
    AttachMouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    },

    /// Publish one host terminal color or appearance update observed by a client-owned shell.
    ClientShellHostTheme { update: ClientHostThemeUpdate },

    /// Publish whether the outer terminal containing a client shell has focus.
    ClientShellFocus { focused: bool },

    /// Update this client's shell mouse-capture preference after config reload.
    ClientShellMouseCapture { enabled: bool },

    /// Extensible named control message for the stable client-owned endpoint protocol.
    ///
    /// This variant is append-only. Its bincode tag and two-string payload are part
    /// of endpoint generation 1 and must not change.
    EndpointControl { kind: String, data: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHostColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<crate::terminal_theme::RgbColor> for ClientHostColor {
    fn from(color: crate::terminal_theme::RgbColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<ClientHostColor> for crate::terminal_theme::RgbColor {
    fn from(color: ClientHostColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostDefaultColorKind {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostAppearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostThemeUpdate {
    DefaultColor {
        kind: ClientHostDefaultColorKind,
        color: ClientHostColor,
    },
    PaletteColors(Vec<(u8, ClientHostColor)>),
    Appearance(ClientHostAppearance),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientClipboardImageTarget {
    DirectTerminal,
    Pane(String),
    Popup(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollSource {
    Wheel,
    PageKey {
        /// Original key bytes to forward when the child application owns page keys.
        input: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

/// Wheel routing states, as the wire spells them.
///
/// A wire-owned copy rather than `crate::pane::WheelRouting` because the
/// protocol layer must not depend upward on pane internals; `pane` maps into
/// this with a `From` impl and the compiler keeps the two in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalWheelRouting {
    /// The wheel scrolls the host's scrollback.
    HostScroll,
    /// The wheel becomes arrow keys (alternate screen + alternate scroll).
    AlternateScroll,
    /// The application asked for the mouse and receives real wheel events.
    MouseReport,
}

/// A single cell in a rendered frame, serialized independently from ratatui's
/// `Cell` type to keep the wire protocol stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellData {
    /// Grapheme cluster displayed in this cell (usually 1–2 chars).
    pub symbol: String,
    /// Foreground color as a packed u32 (0xAARRGGBB or ratatui Color index).
    pub fg: u32,
    /// Background color as a packed u32.
    pub bg: u32,
    /// Bitmask of style modifiers (bold, italic, etc.) plus Herdr extension bits.
    pub modifier: u16,
    /// Whether this cell should be skipped during diff-based rendering.
    pub skip: bool,
    /// Index into `FrameData::hyperlinks` for this cell's OSC 8 target, if any.
    pub hyperlink: Option<u32>,
}

impl CellData {
    pub(crate) fn from_ratatui_cell(cell: &ratatui::buffer::Cell) -> Self {
        Self {
            symbol: cell.symbol().to_owned(),
            fg: color_to_u32(cell.fg),
            bg: color_to_u32(cell.bg),
            modifier: modifier_to_u16(cell.modifier),
            skip: cell.skip,
            hyperlink: None,
        }
    }
}

/// Cursor shape encoded as a DECSCUSR parameter.
///
/// 0 = terminal default, 1 = blinking block, 2 = steady block,
/// 3 = blinking underline, 4 = steady underline, 5 = blinking bar,
/// 6 = steady bar.
pub type CursorShapeParam = u8;

/// Cursor position within a rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Column offset (0-based) of the cursor.
    pub x: u16,
    /// Row offset (0-based) of the cursor.
    pub y: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
    /// Cursor shape as a DECSCUSR parameter.
    #[serde(default)]
    pub shape: CursorShapeParam,
}

/// A rendered frame to be displayed by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameData {
    /// Cells in row-major order. Length must equal `width * height`.
    pub cells: Vec<CellData>,
    /// Frame width in columns.
    pub width: u16,
    /// Frame height in rows.
    pub height: u16,
    /// Cursor state for this frame, if applicable.
    pub cursor: Option<CursorState>,
    /// OSC 8 hyperlink URIs referenced by cells.
    pub hyperlinks: Vec<String>,
    /// Kitty graphics protocol bytes to apply after the text frame.
    pub graphics: Vec<u8>,
}

impl FrameData {
    /// Creates a `FrameData` from a ratatui `Buffer` and optional cursor.
    ///
    /// This converts ratatui's internal cell representation into the
    /// wire-protocol cell format. The conversion is lossless for all
    /// commonly used cell attributes.
    #[cfg(test)]
    pub fn from_ratatui_buffer(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) -> Self {
        Self::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &[])
    }

    pub fn from_ratatui_buffer_with_hyperlinks(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
        hyperlinks: &[((u16, u16), String, String)],
    ) -> Self {
        let area = buffer.area;
        let width = area.width;
        let height = area.height;

        let mut hyperlink_uris = Vec::<String>::new();
        let mut hyperlink_indices = HashMap::<&str, u32>::new();
        let mut hyperlink_by_position = HashMap::<(u16, u16), (&str, &str)>::new();
        for ((x, y), symbol, uri) in hyperlinks {
            hyperlink_by_position.insert((*x, *y), (symbol.as_str(), uri.as_str()));
        }
        let mut cells = Vec::with_capacity((width as usize) * (height as usize));
        for row in 0..height {
            for col in 0..width {
                let cell = buffer.cell((col, row)).expect("cell within bounds");
                let hyperlink = hyperlink_by_position
                    .get(&(col, row))
                    .and_then(|(symbol, uri)| {
                        if *symbol != cell.symbol() {
                            return None;
                        }
                        Some(*hyperlink_indices.entry(*uri).or_insert_with(|| {
                            let index = hyperlink_uris.len() as u32;
                            hyperlink_uris.push((*uri).to_owned());
                            index
                        }))
                    });
                let mut cell = CellData::from_ratatui_cell(cell);
                cell.hyperlink = hyperlink;
                cells.push(cell);
            }
        }

        FrameData {
            cells,
            width,
            height,
            cursor,
            hyperlinks: hyperlink_uris,
            graphics: Vec::new(),
        }
    }

    pub(crate) fn replace_from_ratatui_buffer_preserving_effects(
        &mut self,
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) {
        let width = self.width;
        let hyperlinks = if width == 0 {
            Vec::new()
        } else {
            self.cells
                .iter()
                .enumerate()
                .filter_map(|(index, cell)| {
                    let uri = self.hyperlinks.get(cell.hyperlink? as usize)?;
                    let x = u16::try_from(index % usize::from(width)).ok()?;
                    let y = u16::try_from(index / usize::from(width)).ok()?;
                    Some(((x, y), cell.symbol.clone(), uri.clone()))
                })
                .collect::<Vec<_>>()
        };
        let graphics = std::mem::take(&mut self.graphics);
        let mut replacement =
            Self::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &hyperlinks);
        replacement.graphics = graphics;
        *self = replacement;
    }

    /// Reconstructs a ratatui `Buffer` from this frame data.
    ///
    /// Returns `None` if the cells vector length doesn't match `width * height`.
    pub(crate) fn to_ratatui_buffer(&self) -> Option<ratatui::buffer::Buffer> {
        let expected = (self.width as usize) * (self.height as usize);
        if self.cells.len() != expected {
            return None;
        }

        let area = ratatui::layout::Rect::new(0, 0, self.width, self.height);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));

        for row in 0..self.height {
            for col in 0..self.width {
                let idx = (row as usize) * (self.width as usize) + (col as usize);
                let cell_data = &self.cells[idx];
                let cell = buffer.cell_mut((col, row)).expect("cell within bounds");
                cell.set_symbol(&cell_data.symbol);
                cell.fg = u32_to_color(cell_data.fg);
                cell.bg = u32_to_color(cell_data.bg);
                cell.modifier = u16_to_modifier(cell_data.modifier);
                cell.skip = cell_data.skip;
            }
        }

        Some(buffer)
    }
}

fn deserialize_client_shell_agent_status<'de, D>(
    deserializer: D,
) -> Result<crate::api::schema::AgentStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    if !deserializer.is_human_readable() {
        return crate::api::schema::AgentStatus::deserialize(deserializer);
    }
    let value = String::deserialize(deserializer)?;
    Ok(match value.as_str() {
        "idle" => crate::api::schema::AgentStatus::Idle,
        "working" => crate::api::schema::AgentStatus::Working,
        "blocked" => crate::api::schema::AgentStatus::Blocked,
        "done" => crate::api::schema::AgentStatus::Done,
        _ => crate::api::schema::AgentStatus::Unknown,
    })
}

/// Initial resource projection used by the stable client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSnapshot {
    /// Changes whenever the endpoint process restarts.
    pub boot_id: String,
    /// Monotonic replacement revision within one endpoint boot.
    pub revision: u64,
    /// Endpoint startup/reload config warning, filtered for client-owned keybindings.
    pub config_diagnostic: Option<String>,
    /// Unseen announcement owned and persisted by this endpoint.
    pub product_announcement: Option<ClientShellProductAnnouncement>,
    /// Future-version update advertised by the endpoint.
    pub update_available: Option<String>,
    /// Endpoint-specific command shown in update instructions.
    pub update_install_command: String,
    /// Endpoint's normalized built-in keybindings, used only when a remote client selects server bindings.
    pub server_keybindings_toml: Option<String>,
    /// Whether the endpoint has a What's New entry, even if its body is unavailable.
    pub latest_release_notes_available: bool,
    /// Whether endpoint-owned integration assets need an update.
    pub integration_updates_available: bool,
    /// Endpoint-owned base directory used for new linked worktree checkouts.
    pub worktree_directory: String,
    /// Cached endpoint-owned notes used by the client-rendered overlay.
    pub release_notes: Option<ClientShellReleaseNotes>,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub tab_bar_right: Vec<ClientShellTabStatusSegment>,
    pub tab_bar_right_separator: String,
    pub agent_view_label: Option<String>,
    pub agent_order: Vec<String>,
    pub workspaces: Vec<ClientShellWorkspace>,
    pub tabs: Vec<ClientShellTab>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
    pub commands: Vec<ClientShellCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellProductAnnouncement {
    pub version: String,
    pub id: String,
    pub title: String,
    pub body: String,
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellReleaseNotes {
    pub version: String,
    pub body: String,
    pub preview: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientShellCommandAction {
    Shell,
    Pane,
    Popup,
    PluginAction,
    /// A future endpoint action kind that this client cannot execute.
    #[serde(other)]
    Unknown,
}

impl From<crate::config::CustomCommandAction> for ClientShellCommandAction {
    fn from(action: crate::config::CustomCommandAction) -> Self {
        match action {
            crate::config::CustomCommandAction::Shell => Self::Shell,
            crate::config::CustomCommandAction::Pane => Self::Pane,
            crate::config::CustomCommandAction::Popup => Self::Popup,
            crate::config::CustomCommandAction::PluginAction => Self::PluginAction,
        }
    }
}

impl TryFrom<ClientShellCommandAction> for crate::config::CustomCommandAction {
    type Error = ();

    fn try_from(action: ClientShellCommandAction) -> Result<Self, Self::Error> {
        match action {
            ClientShellCommandAction::Shell => Ok(Self::Shell),
            ClientShellCommandAction::Pane => Ok(Self::Pane),
            ClientShellCommandAction::Popup => Ok(Self::Popup),
            ClientShellCommandAction::PluginAction => Ok(Self::PluginAction),
            ClientShellCommandAction::Unknown => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellCommand {
    pub command_id: String,
    pub binding_label: String,
    pub binding_labels: Vec<String>,
    pub action: ClientShellCommandAction,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTabStatusSegment {
    pub text: String,
    pub accent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorkspace {
    pub workspace_id: String,
    pub active_tab_id: String,
    pub new_workspace_cwd: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub branch: Option<String>,
    pub git_ahead_behind: Option<(usize, usize)>,
    pub tokens: Vec<(String, String)>,
    pub worktree: Option<ClientShellWorktree>,
    pub focused: bool,
    #[serde(deserialize_with = "deserialize_client_shell_agent_status")]
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorktree {
    pub key: String,
    pub label: String,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTab {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub zoomed: bool,
    pub focused: bool,
    #[serde(deserialize_with = "deserialize_client_shell_agent_status")]
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub focused: bool,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub name: Option<String>,
    pub display_agent: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    #[serde(deserialize_with = "deserialize_client_shell_agent_status")]
    pub agent_status: crate::api::schema::AgentStatus,
    pub state_change_seq: u64,
    pub state_labels: Vec<(String, String)>,
    pub tokens: Vec<(String, String)>,
    pub focused: bool,
}

/// Origin-relative geometry for one pane in a rendered pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePane {
    pub pane_id: String,
    pub content_revision: u64,
    pub rect: SurfaceRect,
    pub inner_rect: SurfaceRect,
    pub scrollbar_rect: Option<SurfaceRect>,
    pub scroll: Option<PaneSurfaceScrollMetrics>,
    pub focused: bool,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceScrollMetrics {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

/// One draggable BSP split handle relative to a pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceSplit {
    pub direction: PaneSurfaceSplitDirection,
    pub pos: u16,
    pub area: SurfaceRect,
    pub hit_rect: SurfaceRect,
    pub path: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSurfaceSplitDirection {
    Horizontal,
    Vertical,
}

/// Wire-safe rectangle relative to a pane surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl From<ratatui::layout::Rect> for SurfaceRect {
    fn from(rect: ratatui::layout::Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsTarget {
    Pane { pane_id: String },
    Popup { terminal_id: String },
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsSource {
    Terminal {
        target: SurfaceGraphicsTarget,
        image_id: u32,
    },
    PaneLayer {
        pane_id: String,
        layer_id: String,
    },
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsFormat {
    Rgb,
    Rgba,
    Png,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGraphicsAssetKey {
    pub source: SurfaceGraphicsSource,
    pub image_width: u32,
    pub image_height: u32,
    pub format: SurfaceGraphicsFormat,
    pub data_len: u64,
    pub data_fingerprint: u64,
}

/// Image bytes newly needed by this connection's complete desired scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGraphicsAsset {
    pub key: SurfaceGraphicsAssetKey,
    pub data: Vec<u8>,
}

/// One already-clipped desired placement relative to its target surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGraphicsPlacement {
    pub asset: SurfaceGraphicsAssetKey,
    pub logical_placement_id: u32,
    pub x: u16,
    pub y: u16,
    pub cols: u32,
    pub rows: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub z: i32,
    pub scrollback_offset: u32,
}

/// Complete desired placements plus only the image bytes not already sent for
/// the current live scene on this connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGraphicsScene {
    pub assets: Vec<SurfaceGraphicsAsset>,
    pub placements: Vec<SurfaceGraphicsPlacement>,
    /// Direct-uploaded assets that remain live for this client even while their
    /// pane is outside the selected scene.
    pub retained_assets: Vec<SurfaceGraphicsAssetKey>,
}

/// One server-rendered active-tab surface without sidebar, tab bar, or overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceFrame {
    /// Endpoint process identity that produced this surface.
    pub boot_id: String,
    /// Projection revision whose focused IDs and topology produced this surface.
    pub projection_revision: u64,
    /// Monotonic revision for full surfaces and incremental patches on one connection.
    pub surface_revision: u64,
    pub frame: FrameData,
    pub panes: Vec<PaneSurfacePane>,
    pub splits: Vec<PaneSurfaceSplit>,
    pub popup: Option<Box<ClientShellPopupSurface>>,
    pub graphics: SurfaceGraphicsScene,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientShellPopupSize {
    Cells(u16),
    Percent(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatchRow {
    /// Origin-relative surface column where this changed cell span starts.
    pub x: u16,
    /// Origin-relative surface row.
    pub y: u16,
    pub cells: Vec<CellData>,
}

/// Incremental terminal-cell update against one committed complete pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatch {
    pub boot_id: String,
    pub projection_revision: u64,
    pub base_surface_revision: u64,
    pub surface_revision: u64,
    pub rows: Vec<PaneSurfacePatchRow>,
    /// Updated metadata for panes whose terminal content changed.
    pub panes: Vec<PaneSurfacePane>,
    /// Final cursor relative to the pane surface.
    pub cursor: Option<CursorState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPopupSurface {
    pub terminal_id: String,
    pub title: String,
    pub width: Option<ClientShellPopupSize>,
    pub height: Option<ClientShellPopupSize>,
    pub frame: FrameData,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// Terminal ANSI bytes encoded by the server for network-efficient clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    /// Monotonic per-client frame sequence.
    pub seq: u64,
    /// Frame width in columns.
    pub width: u16,
    /// Frame height in rows.
    pub height: u16,
    /// Whether bytes contain a full redraw rather than an incremental diff.
    pub full: bool,
    /// Terminal escape bytes ready to write directly to stdout.
    pub bytes: Vec<u8>,
}

/// Notification kind forwarded from server to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyKind {
    /// Play a sound (bell/agent-done, etc.).
    Sound,
    /// Display a toast message through the outer terminal.
    Toast,
    /// Display a toast message through the host OS notification service.
    SystemToast,
}

/// A client-rendered notification category. The server reports the semantic
/// event; each connected shell client chooses how (or whether) to present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticNotificationKind {
    NeedsAttention,
    Finished,
    UpdateInstalled,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticNotificationSound {
    Done,
    Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticNotification {
    pub kind: SemanticNotificationKind,
    pub title: String,
    pub body: Option<String>,
    pub sound: Option<SemanticNotificationSound>,
    pub agent: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub position: Option<crate::config::ToastHerdrPosition>,
}

/// Messages sent from the server to the client over the client protocol socket.
///
/// Variant order is frozen for endpoint generation 1. Add compatible endpoint
/// behavior through `EndpointControl`, and ignore unrecognized named controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Handshake response: server acknowledges (or rejects) the client.
    Welcome {
        /// Protocol version the server speaks.
        version: u32,
        /// Render encoding selected by the server for this connection.
        encoding: RenderEncoding,
        /// If present, the handshake failed and this describes why.
        /// The client should exit with a clear error message.
        error: Option<String>,
        /// Capabilities the server accepted for this connection.
        ///
        /// This is the intersection of what the client announced and what this
        /// server supports — never a reflection of the client's list. The
        /// server uses nothing outside it, so an empty list is exactly the
        /// pre-negotiation behaviour.
        accepted: Vec<CapabilityEntry>,
    },

    /// A rendered frame to be displayed by a semantic-frame client.
    Frame(FrameData),

    /// Terminal bytes to write directly for a terminal-ANSI client.
    Terminal(TerminalFrame),

    /// Client-local Kitty graphics bytes to write directly to the host terminal.
    Graphics {
        /// Raw Kitty graphics protocol bytes.
        bytes: Vec<u8>,
    },

    /// Server is shutting down. Clients should exit gracefully.
    ServerShutdown {
        /// Optional reason for the shutdown.
        reason: Option<String>,
    },

    /// A notification event (sound/toast) to be rendered locally by the client.
    Notify {
        /// What kind of notification.
        kind: NotifyKind,
        /// Human-readable title or sound label.
        message: String,
        /// Optional human-readable notification body.
        body: Option<String>,
    },

    /// OSC 52 clipboard data forwarded from a PTY through the server.
    Clipboard {
        /// Base64-encoded clipboard data.
        data: String,
    },

    /// Set the foreground client's outer terminal window title.
    WindowTitle {
        /// Sanitized title to write with OSC 0. `None` restores Herdr's default title.
        title: Option<String>,
    },

    /// Client-local runtime config changed on disk; refresh it without reconnecting.
    ReloadSoundConfig,

    /// Whether the client should currently capture host mouse input.
    MouseCapture {
        /// True when Herdr mouse UI is enabled or the focused pane app requests mouse reporting.
        enabled: bool,
        /// True only while the focused pane requests DEC SGR pixel mode 1016.
        sgr_pixels: bool,
    },

    /// Whether the focused terminal requests Kitty report-all keyboard input.
    KittyKeyboardReportAll {
        /// True only while the focused pane requests `REPORT_ALL_KEYS_AS_ESCAPE_CODES`.
        enabled: bool,
    },

    /// Apply the prefix-mode ASCII input-source change on the foreground client.
    /// `active = true` → switch to an ASCII-capable source (saving the current one);
    /// `active = false` → restore the saved source.
    PrefixInputSource {
        /// Whether the ASCII input source should be active.
        active: bool,
    },
    /// Where a wheel over this attached terminal would go, sent on attach and
    /// again whenever it changes.
    ///
    /// The routing decision itself has always been the server's
    /// (`wheel_routing()`): a pane whose application asked for the mouse gets
    /// real wheel events, an alternate screen with alternate-scroll gets
    /// arrow keys, everything else scrolls the host's scrollback. What was
    /// missing was the client's ability to KNOW which of those it is talking
    /// to — a web client deciding whether an upward gesture may be taken
    /// locally (a reading mode) cannot make that call blind without stealing
    /// vim's wheel or feeding claude's history-jog.
    TerminalRouting {
        /// The current wheel routing for the attached terminal.
        routing: TerminalWheelRouting,
    },

    /// Ring the foreground client's outer terminal for pane-originated BEL characters.
    TerminalBell {
        /// Number of BEL characters parsed from one PTY read.
        count: u16,
    },

    /// One validated Herdr-owned Kitty regular-file RGBA transmission.
    GraphicsFile {
        path: String,
        expected_len: u64,
        image_id: u32,
        transfer_id: u64,
        leading: Vec<u8>,
        control: String,
        /// ClientShell upload identity. `None` targets a direct terminal client.
        surface_asset: Option<SurfaceGraphicsAssetKey>,
    },

    /// Suppress a direct command that expired before terminal delivery.
    GraphicsTransmissionRetired { transfer_id: u64, image_id: u32 },

    // -- Media streams (F1) -------------------------------------------------
    //
    // Appended, and every later media message appends too. `Graphics` stays
    // exactly as it is: a one-shot picture is the right primitive for a TUI
    // drawing a cover, and it costs nothing. These carry *continuous* sources.
    /// A media stream opened for one pane.
    MediaOpen {
        /// Identifies the stream for its whole life. Bound to the pane, not to
        /// a frame: waypipe's measured mistake was binding a stream to the
        /// buffer, which made surfaces flicker as buffers rotated.
        stream_id: u32,
        pane_id: String,
        /// Codec name from `codec`, drawn from what the handshake agreed on.
        codec: String,
        params: MediaParams,
        /// How long after `pts_us` the client should play a chunk. The server's
        /// opening suggestion; the client raises it as it measures jitter.
        target_latency_us: u32,
    },

    /// One chunk of an open stream.
    MediaChunk {
        stream_id: u32,
        /// Monotonic per stream. A gap is a loss, and the client conceals it
        /// rather than stalling — stalling turns a lost packet into silence.
        seq: u64,
        /// Presentation time in the server's clock. The only thing A/V sync
        /// rests on, and the reason audio and video need no separate alignment.
        pts_us: u64,
        data: Vec<u8>,
    },

    /// A stream ended.
    MediaClose {
        stream_id: u32,
        reason: MediaCloseReason,
        /// Free-form specifics. Empty for a normal end.
        detail: String,
    },

    /// Answer to `ClientMessage::TimeSync`, carrying the server's two stamps.
    ///
    /// Three stamps travel and the client takes the fourth on arrival: the
    /// NTP four-timestamp shape, which is what lets the client separate the
    /// clock offset from the round trip instead of confusing the two.
    TimeSyncReply {
        client_send_us: u64,
        server_recv_us: u64,
        server_send_us: u64,
    },

    /// Where every visible pane is, in the client's own pixels.
    ///
    /// Appended rather than inserted, for the reason `MediaParams` already
    /// records: a variant placed before an existing one re-tags every message
    /// a released client is already able to decode.
    ///
    /// Sent on change and never on a tick. A client looking at a still screen
    /// must cost nothing, which is the resource doctrine's whole claim, and a
    /// geometry that repeats itself sixty times a second would break it while
    /// looking like progress.
    ///
    /// The whole tab travels in one message rather than one message per pane:
    /// a layout change moves several panes at once, and delivering them
    /// separately would let a consumer draw half of the old layout beside half
    /// of the new one.
    PaneGeometry {
        /// Bumped once per layout change. A consumer that sees a gap knows it
        /// missed one and can redraw from scratch instead of trusting a
        /// rectangle that may have moved twice since.
        generation: u64,
        /// Cell size the rectangles were computed with, as this client last
        /// reported it. Carried rather than assumed, because a `Resize` can
        /// land between two geometries and a consumer holding the old cell
        /// size would scale every box wrongly.
        cell_width_px: u32,
        /// Cell size the rectangles were computed with.
        cell_height_px: u32,
        /// Every pane of the active tab.
        ///
        /// Panes of background tabs are absent rather than flagged. Absence is
        /// the visibility signal on purpose: a `visible: bool` is a second
        /// thing that has to be kept true, and this repository has already
        /// paid for one of those going stale.
        panes: Vec<PaneRect>,
    },

    /// Initial metadata for a client-owned shell.
    ClientShellSnapshot(Box<ClientShellSnapshot>),

    /// Active-tab pane content rendered at a client-requested origin-relative size.
    PaneSurface(PaneSurfaceFrame),

    /// Ephemeral semantic notification delivered over the private control lane.
    /// It is sent only to currently connected client-rendered shells.
    SemanticNotification(SemanticNotification),

    /// Immediate endpoint error that the client-rendered shell must show regardless of notification policy.
    ClientShellError { message: String },

    /// One ordered chunk of the final response to an endpoint operation.
    ClientShellEndpointResponseChunk {
        boot_id: String,
        request_id: String,
        final_chunk: bool,
        data: Vec<u8>,
    },

    /// Exact Kitty keyboard flags requested by a directly attached terminal.
    /// Zero restores the host terminal's previous keyboard mode.
    DirectTerminalKeyboardProtocol {
        flags: u16,
        modify_other_keys_level: u8,
    },

    /// Whether the focused pane or popup needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// Incremental terminal-cell update for a previously committed pane surface.
    /// Appended LAST: every ordinal a trunk client already knows stays put (U2c-2).
    PaneSurfacePatch(PaneSurfacePatch),

    /// Extensible named control message for the stable client-owned endpoint protocol.
    ///
    /// This variant is append-only. Its bincode tag and two-string payload are part
    /// of endpoint generation 1 and must not change.
    EndpointControl { kind: String, data: String },
}

// ---------------------------------------------------------------------------
// Color / Modifier conversion helpers
// ---------------------------------------------------------------------------

/// Converts a ratatui `Color` to a packed u32 for wire transport.
///
/// Encoding:
/// - Named colors (Reset, Black, …, White) → `0x00_00_00_XX` where XX is 0..=16
/// - Indexed palette → `0x01_00_00_XX` where XX is the palette index
/// - RGB → `0x02_RR_GG_BB` with components in the lower 3 bytes
pub(crate) fn color_to_u32(color: ratatui::style::Color) -> u32 {
    match color {
        ratatui::style::Color::Reset => 0x00_00_00_00,
        ratatui::style::Color::Black => 0x00_00_00_01,
        ratatui::style::Color::Red => 0x00_00_00_02,
        ratatui::style::Color::Green => 0x00_00_00_03,
        ratatui::style::Color::Yellow => 0x00_00_00_04,
        ratatui::style::Color::Blue => 0x00_00_00_05,
        ratatui::style::Color::Magenta => 0x00_00_00_06,
        ratatui::style::Color::Cyan => 0x00_00_00_07,
        ratatui::style::Color::Gray => 0x00_00_00_08,
        ratatui::style::Color::DarkGray => 0x00_00_00_09,
        ratatui::style::Color::LightRed => 0x00_00_00_0A,
        ratatui::style::Color::LightGreen => 0x00_00_00_0B,
        ratatui::style::Color::LightYellow => 0x00_00_00_0C,
        ratatui::style::Color::LightBlue => 0x00_00_00_0D,
        ratatui::style::Color::LightMagenta => 0x00_00_00_0E,
        ratatui::style::Color::LightCyan => 0x00_00_00_0F,
        ratatui::style::Color::White => 0x00_00_00_10,
        ratatui::style::Color::Indexed(i) => 0x01_00_00_00 | (i as u32),
        ratatui::style::Color::Rgb(r, g, b) => {
            0x02_00_00_00 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        }
    }
}

/// Converts a packed u32 back to a ratatui `Color`.
fn u32_to_color(val: u32) -> ratatui::style::Color {
    match val >> 24 {
        0x00 => match val & 0xFF {
            0x00 => ratatui::style::Color::Reset,
            0x01 => ratatui::style::Color::Black,
            0x02 => ratatui::style::Color::Red,
            0x03 => ratatui::style::Color::Green,
            0x04 => ratatui::style::Color::Yellow,
            0x05 => ratatui::style::Color::Blue,
            0x06 => ratatui::style::Color::Magenta,
            0x07 => ratatui::style::Color::Cyan,
            0x08 => ratatui::style::Color::Gray,
            0x09 => ratatui::style::Color::DarkGray,
            0x0A => ratatui::style::Color::LightRed,
            0x0B => ratatui::style::Color::LightGreen,
            0x0C => ratatui::style::Color::LightYellow,
            0x0D => ratatui::style::Color::LightBlue,
            0x0E => ratatui::style::Color::LightMagenta,
            0x0F => ratatui::style::Color::LightCyan,
            0x10 => ratatui::style::Color::White,
            _ => ratatui::style::Color::Reset, // unknown named → Reset
        },
        0x01 => ratatui::style::Color::Indexed((val & 0xFF) as u8),
        0x02 => {
            let r = ((val >> 16) & 0xFF) as u8;
            let g = ((val >> 8) & 0xFF) as u8;
            let b = (val & 0xFF) as u8;
            ratatui::style::Color::Rgb(r, g, b)
        }
        _ => ratatui::style::Color::Reset, // unknown tag → Reset
    }
}

const UNDERLINE_STYLE_SHIFT: u16 = 12;
const UNDERLINE_STYLE_MASK: u16 = 0xF000;

/// Converts a ratatui `Modifier` bitmask to a u16 for wire transport.
pub(crate) fn modifier_to_u16(modifier: ratatui::style::Modifier) -> u16 {
    modifier.bits()
}

pub(crate) fn underline_style_from_modifier(modifier: u16) -> u8 {
    ((modifier & UNDERLINE_STYLE_MASK) >> UNDERLINE_STYLE_SHIFT) as u8
}

pub(crate) fn modifier_with_underline_style(
    modifier: ratatui::style::Modifier,
    underline_style: u8,
) -> ratatui::style::Modifier {
    let bits = modifier.bits() | ((u16::from(underline_style) & 0x0F) << UNDERLINE_STYLE_SHIFT);
    ratatui::style::Modifier::from_bits_retain(bits)
}

/// Converts a u16 back to a ratatui `Modifier`.
fn u16_to_modifier(val: u16) -> ratatui::style::Modifier {
    ratatui::style::Modifier::from_bits_truncate(val & !UNDERLINE_STYLE_MASK)
}

// ---------------------------------------------------------------------------
// Framing: length-prefixed binary messages
// ---------------------------------------------------------------------------

/// Errors that can occur during framing operations.
#[derive(Debug)]
pub enum FramingError {
    /// The decoded payload length exceeds the configured maximum frame size.
    Oversized { claimed: usize, max: usize },
    /// An I/O error occurred while reading or writing.
    Io(io::Error),
    /// Bincode serialization or deserialization failed.
    Bincode(String),
    /// The connection was closed before a complete frame could be read.
    UnexpectedEof,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::Oversized { claimed, max } => {
                write!(f, "frame size {claimed} exceeds maximum {max}")
            }
            FramingError::Io(e) => write!(f, "I/O error: {e}"),
            FramingError::Bincode(e) => write!(f, "bincode error: {e}"),
            FramingError::UnexpectedEof => write!(f, "unexpected end of stream"),
        }
    }
}

impl std::error::Error for FramingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FramingError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FramingError {
    fn from(e: io::Error) -> Self {
        FramingError::Io(e)
    }
}

/// Serializes a message and writes it as a length-prefixed frame:
/// `[u32LE length][bincode payload]`.
///
/// This is a blocking/synchronous write suitable for use with `std::os::unix::net::UnixStream`
/// in blocking mode, or with any `Write` implementor.
///
/// # Errors
///
/// Returns `FramingError::Bincode` if the payload length exceeds `u32::MAX`
/// (would be truncated by the length prefix cast).
pub fn write_message<W: Write, M: Serialize>(writer: &mut W, msg: &M) -> Result<(), FramingError> {
    let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard())
        .map_err(|e| FramingError::Bincode(e.to_string()))?;

    let len = payload.len();
    if len > u32::MAX as usize {
        return Err(FramingError::Bincode(format!(
            "payload length {len} exceeds u32::MAX ({}), would be truncated by length prefix",
            u32::MAX
        )));
    }

    writer.write_all(&(len as u32).to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Reads and deserializes a length-prefixed frame from a reader.
///
/// Reassembles partial reads correctly. Rejects frames whose declared
/// length exceeds `max_frame_size` without panicking or allocating
/// oversized buffers.
pub fn read_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_frame_size: usize,
) -> Result<M, FramingError> {
    // Read the 4-byte length prefix, reassembling partial reads.
    let mut len_buf = [0u8; LENGTH_PREFIX_BYTES];
    read_exact_or_eof(reader, &mut len_buf)?;
    let claimed_len = u32::from_le_bytes(len_buf) as usize;

    if claimed_len > max_frame_size {
        return Err(FramingError::Oversized {
            claimed: claimed_len,
            max: max_frame_size,
        });
    }

    // Read the payload, reassembling partial reads.
    let mut payload = vec![0u8; claimed_len];
    read_exact_or_eof(reader, &mut payload)?;

    let (msg, consumed) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
        .map_err(|e| FramingError::Bincode(e.to_string()))?;

    // Enforce that the decoder consumed the full payload.
    // Trailing bytes after the decoded message indicate a protocol violation
    // (e.g., a corrupted length prefix or concatenated payloads).
    if consumed != claimed_len {
        return Err(FramingError::Bincode(format!(
            "decoded {} bytes but payload length was {claimed_len}; trailing bytes are not allowed",
            consumed
        )));
    }

    Ok(msg)
}

/// Like `Read::read_exact`, but returns `FramingError::UnexpectedEof`
/// when the reader hits end-of-stream before filling the buffer, instead
/// of the generic `io::ErrorKind::UnexpectedEof`.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FramingError> {
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FramingError::UnexpectedEof
        } else {
            FramingError::Io(e)
        }
    })
}

// ---------------------------------------------------------------------------
// Version negotiation
// ---------------------------------------------------------------------------

/// Result of checking a client's protocol version against the server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// Versions are compatible. The server should reply with a successful Welcome.
    Compatible,
    /// Versions are incompatible. The server should reply with a Welcome error and close.
    Incompatible(String),
}

/// Checks whether a client's protocol version is compatible with this server.
///
/// Current rules:
/// - Version 0 (pre-persistence client) is always rejected.
/// - Matching major versions are accepted.
/// - A client with a newer version than the server is rejected.
/// - A client with an older version than the server is rejected
///   (backward compatibility is not yet supported).
pub fn check_client_version(client_version: u32) -> VersionCheck {
    if client_version == 0 {
        return VersionCheck::Incompatible(
            "pre-persistence client (version 0) is not supported".to_owned(),
        );
    }

    if client_version == PROTOCOL_VERSION {
        VersionCheck::Compatible
    } else if client_version < PROTOCOL_VERSION {
        VersionCheck::Incompatible(format!(
            "client version {client_version} is older than server version {PROTOCOL_VERSION}; please upgrade your herdr client"
        ))
    } else {
        VersionCheck::Incompatible(format!(
            "client version {client_version} is newer than server version {PROTOCOL_VERSION}; please upgrade the herdr server"
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};
    use sha2::{Digest, Sha256};

    fn encoded_sha256(value: &impl Serialize) -> String {
        let encoded = bincode::serde::encode_to_vec(value, bincode::config::standard()).unwrap();
        format!("{:x}", Sha256::digest(encoded))
    }

    /// The fork's enum ordinals differ from upstream's (fork variants come first), so a frozen upstream
    /// generation-1 digest is checked against the fork's encoding with only the tag byte swapped: the
    /// payload body must be upstream's byte for byte, and the tag must be the fork's own (PRD U3 D1).
    fn upstream_tagged_sha256(value: &impl Serialize, fork_tag: u8, upstream_tag: u8) -> String {
        let mut encoded =
            bincode::serde::encode_to_vec(value, bincode::config::standard()).unwrap();
        assert_eq!(encoded[0], fork_tag, "fork ordinal");
        encoded[0] = upstream_tag;
        format!("{:x}", Sha256::digest(encoded))
    }

    // These digests freeze representative generation-1 bincode payloads. A mismatch
    // requires a new named codec; do not update a v1 digest to bless a wire change.
    // They do not detect appended enum variants, so every type reachable from a v1
    // payload is also append-closed.

    // ---- Round-trip: ClientMessage ----

    // TP-WHEELMODE-01
    #[test]
    fn terminal_routing_roundtrip() {
        // The variant is appended at the end of ServerMessage on purpose —
        // bincode discriminants are positional — and this pins that a mode
        // message survives the wire byte-for-byte.
        for routing in [
            TerminalWheelRouting::HostScroll,
            TerminalWheelRouting::AlternateScroll,
            TerminalWheelRouting::MouseReport,
        ] {
            let msg = ServerMessage::TerminalRouting { routing };
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ServerMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            match decoded {
                ServerMessage::TerminalRouting { routing: got } => assert_eq!(got, routing),
                other => panic!("expected TerminalRouting, got {other:?}"),
            }
        }
    }

    // -- Capability negotiation (F0) ---------------------------------------
    //
    // These pin the one promise the whole design rests on: a capability the
    // reader has never heard of must cost nothing. If that breaks, every later
    // media feature needs a protocol bump again, which is the trap this fork
    // already fell into once (PROTOCOL_VERSION 20 -> 21 for a single bool).

    #[test]
    fn capability_entries_survive_a_hello_roundtrip() {
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::App,
            pixel_mouse: true,
            capabilities: vec![
                CapabilityEntry::flag(capability::MEDIA_STREAMS),
                CapabilityEntry::with_values(capability::AUDIO_SINK, ["opus", "pcm_s16"]),
            ],
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn an_empty_capability_announcement_survives_a_roundtrip() {
        // A client that announces nothing is the "behaves exactly like today"
        // case, so it has to be representable and stable on the wire.
        let msg = ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: RenderEncoding::SemanticFrame,
            error: None,
            accepted: Vec::new(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        match decoded {
            ServerMessage::Welcome { accepted, .. } => assert!(accepted.is_empty()),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_capability_name_is_ignored_rather_than_fatal() {
        let set = CapabilitySet::from_entries(vec![
            CapabilityEntry::flag("future.thing.nobody.here.knows"),
            CapabilityEntry::flag(capability::MEDIA_STREAMS),
        ]);
        assert!(set.has(capability::MEDIA_STREAMS));
        assert!(!set.has(capability::AUDIO_SINK));
        // The unknown name is carried, not rejected: a reader that does not know
        // it simply never asks for it.
        assert!(!set.has("something.else.entirely"));
    }

    #[test]
    fn capability_values_are_read_in_announced_order() {
        let set = CapabilitySet::from_entries(vec![CapabilityEntry::with_values(
            capability::AUDIO_SINK,
            ["opus", "pcm_s16", "flac"],
        )]);
        assert_eq!(
            set.values_of(capability::AUDIO_SINK),
            ["opus", "pcm_s16", "flac"]
        );
        // An absent capability yields no values instead of panicking.
        assert!(set.values_of(capability::VIDEO_DECODE).is_empty());
    }

    #[test]
    fn a_repeated_capability_name_keeps_the_first_announcement() {
        // A malformed or hostile client must not be able to make the handshake
        // ambiguous; first announcement wins and nothing panics.
        let set = CapabilitySet::from_entries(vec![
            CapabilityEntry::with_values(capability::AUDIO_SINK, ["opus"]),
            CapabilityEntry::with_values(capability::AUDIO_SINK, ["mp3"]),
        ]);
        assert_eq!(set.values_of(capability::AUDIO_SINK), ["opus"]);
    }

    #[test]
    fn capability_intersection_drops_what_only_one_side_offers() {
        // This is the rule the server's handshake depends on: reflecting the
        // client's announcement back would silently accept capabilities the
        // server cannot serve, and every test would still be green.
        let client = CapabilitySet::from_entries(vec![
            CapabilityEntry::flag(capability::MEDIA_STREAMS),
            CapabilityEntry::with_values(capability::AUDIO_SINK, ["opus"]),
            CapabilityEntry::flag("client.only.extension"),
        ]);
        let server = CapabilitySet::from_entries(vec![
            CapabilityEntry::with_values(capability::AUDIO_SINK, ["opus", "pcm_s16"]),
            CapabilityEntry::flag(capability::MEDIA_STREAMS),
            CapabilityEntry::flag("server.only.extension"),
        ]);

        let accepted = client.intersect(&server);

        assert!(accepted.has(capability::MEDIA_STREAMS));
        assert!(accepted.has(capability::AUDIO_SINK));
        assert!(!accepted.has("client.only.extension"));
        assert!(!accepted.has("server.only.extension"));
        // The values that survive are the ones both sides named, in the
        // client's announced order: the client cannot be handed a codec it
        // never claimed to decode.
        assert_eq!(accepted.values_of(capability::AUDIO_SINK), ["opus"]);
    }

    #[test]
    fn adding_a_capability_does_not_shift_the_wire_shape() {
        // Guards the promise directly: more capabilities must only make the
        // frame longer, never move the enum tag or the fields around it.
        // (A variant-ordering slip cost this repo a 9.4s test timeout once.)
        let hello = |caps: Vec<CapabilityEntry>| ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::App,
            pixel_mouse: false,
            capabilities: caps,
        };

        let one = bincode::serde::encode_to_vec(
            hello(vec![CapabilityEntry::flag(capability::MEDIA_STREAMS)]),
            bincode::config::standard(),
        )
        .unwrap();
        let three = bincode::serde::encode_to_vec(
            hello(vec![
                CapabilityEntry::flag(capability::MEDIA_STREAMS),
                CapabilityEntry::with_values(capability::AUDIO_SINK, ["opus"]),
                CapabilityEntry::flag("third.name"),
            ]),
            bincode::config::standard(),
        )
        .unwrap();

        assert_eq!(one.first(), three.first(), "enum tag must not move");
        assert!(
            three.len() > one.len(),
            "more capabilities means more bytes"
        );
        // Both must decode with the same reader.
        for encoded in [&one, &three] {
            let (_decoded, _): (ClientMessage, _) =
                bincode::serde::decode_from_slice(encoded, bincode::config::standard())
                    .expect("both announcements decode with one reader");
        }
    }

    #[test]
    fn client_hello_roundtrip() {
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::App,
            pixel_mouse: true,
            capabilities: Vec::new(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_shell_hello_roundtrip() {
        let msg = ClientMessage::ClientShellHello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 29,
            requested_encoding: RenderEncoding::SemanticFrame,
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: ClientSurfaceSize { cols: 80, rows: 29 },
            pixel_mouse: true,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn endpoint_control_roundtrip() {
        let msg = ClientMessage::EndpointControl {
            kind: "endpoint.hello.v1".into(),
            data: r#"{"generation":1}"#.into(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        assert_eq!(
            bincode::serde::encode_to_vec(
                ClientMessage::EndpointControl {
                    kind: String::new(),
                    data: String::new(),
                },
                bincode::config::standard(),
            )
            .unwrap(),
            [24, 0, 0]
        );
    }

    #[test]
    fn client_shell_resize_roundtrip() {
        let msg = ClientMessage::ClientShellResize {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: ClientSurfaceSize { cols: 74, rows: 29 },
            pixel_mouse: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        assert_eq!(
            encoded_sha256(&msg),
            // The fork's resize carries the terminal cols/rows next to the surface (trunk shell clients
            // during handoff), so its generation-1 payload is the fork's own; frozen here once (U3a).
            "7ab814ccdd4387aad6ceb8e2dc79759a1cda542c1576a98906a6c9ce9f92cc14"
        );
    }

    #[test]
    fn client_input_roundtrip() {
        let msg = ClientMessage::Input {
            data: vec![0x1b, 0x5b, 0x41], // ESC [ A (up arrow)
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_message_wire_tags_preserve_protocol_15_order() {
        fn tag(msg: &ClientMessage) -> u8 {
            *bincode::serde::encode_to_vec(msg, bincode::config::standard())
                .unwrap()
                .first()
                .expect("encoded client message should include enum tag")
        }

        assert_eq!(
            tag(&ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
                requested_encoding: RenderEncoding::SemanticFrame,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::App,
                pixel_mouse: true,
                capabilities: Vec::new(),
            }),
            0
        );
        assert_eq!(tag(&ClientMessage::Input { data: Vec::new() }), 1);
        assert_eq!(
            tag(&ClientMessage::ClipboardImage {
                target: ClientClipboardImageTarget::DirectTerminal,
                extension: "png".to_owned(),
                data: Vec::new(),
            }),
            2
        );
        assert_eq!(
            tag(&ClientMessage::Resize {
                cols: 80,
                rows: 24,
                pixel_mouse: false,
                cell_width_px: 8,
                cell_height_px: 16,
            }),
            3
        );
        assert_eq!(tag(&ClientMessage::Detach), 4);
        assert_eq!(
            tag(&ClientMessage::AttachTerminal {
                terminal_id: "term".to_owned(),
                takeover: false,
            }),
            5
        );
        assert_eq!(
            tag(&ClientMessage::AttachScroll {
                source: AttachScrollSource::Wheel,
                direction: AttachScrollDirection::Up,
                lines: 1,
                column: None,
                row: None,
                modifiers: 0,
            }),
            6
        );
        assert_eq!(tag(&ClientMessage::InputEvents { events: Vec::new() }), 7);
        assert_eq!(
            tag(&ClientMessage::ObserveTerminal {
                target: "w1:p1".to_owned(),
            }),
            8
        );
        assert_eq!(
            tag(&ClientMessage::ControlTerminal {
                target: "w1:p1".to_owned(),
                takeover: false,
            }),
            9
        );
    }

    #[test]
    fn client_input_events_roundtrip() {
        let msg = ClientMessage::InputEvents {
            events: vec![
                ClientInputEvent::Key {
                    code: ClientKeyCode::Char('N'),
                    modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
                    kind: ClientKeyKind::Press,

                    repeat_count: 1,
                    generated_text: None,
                    source: crate::protocol::ClientKeySource::Synthesized,
                },
                ClientInputEvent::Key {
                    code: ClientKeyCode::Backspace,
                    modifiers: 0,
                    kind: ClientKeyKind::Press,
                    repeat_count: 3,
                    generated_text: None,
                    source: crate::protocol::ClientKeySource::Vt {
                        bytes: b"\x1b[127;1u".to_vec(),
                    },
                },
                ClientInputEvent::Key {
                    code: ClientKeyCode::Esc,
                    modifiers: 0,
                    kind: ClientKeyKind::Release,
                    repeat_count: 1,
                    generated_text: None,
                    source: crate::protocol::ClientKeySource::WindowsConsole {
                        record: crate::input::WindowsKeyRecord {
                            key_down: false,
                            repeat_count: 1,
                            virtual_key_code: 27,
                            virtual_scan_code: 1,
                            unicode: 27,
                            control_key_state: 0,
                        },
                    },
                },
                ClientInputEvent::TextCommit("你🙂".to_owned()),
                ClientInputEvent::Mouse {
                    kind: ClientMouseKind::Down(ClientMouseButton::Left),
                    column: 3,
                    row: 4,
                    modifiers: 0,
                },
            ],
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        // Freeze the protocol 20 input envelope before it is published.
        assert_eq!(
            encoded,
            vec![
                7, 5, 0, 15, 78, 1, 0, 1, 0, 0, 0, 0, 0, 0, 3, 0, 1, 8, 27, 91, 49, 50, 55, 59, 49,
                117, 0, 14, 0, 2, 1, 0, 2, 0, 1, 27, 1, 27, 0, 1, 7, 228, 189, 160, 240, 159, 153,
                130, 2, 0, 0, 3, 4, 0,
            ]
        );
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_message_wire_tags_reflect_current_order() {
        fn tag(msg: &ClientMessage) -> u8 {
            *bincode::serde::encode_to_vec(msg, bincode::config::standard())
                .unwrap()
                .first()
                .expect("encoded client message should include enum tag")
        }

        assert_eq!(
            tag(&ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
                requested_encoding: RenderEncoding::SemanticFrame,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::App,
                pixel_mouse: false,
                capabilities: Vec::new(),
            }),
            0
        );
        assert_eq!(
            tag(&ClientMessage::ClientShellHello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 29,
                requested_encoding: RenderEncoding::SemanticFrame,
                cell_width_px: 8,
                cell_height_px: 16,
                surface_size: ClientSurfaceSize { cols: 80, rows: 29 },
                pixel_mouse: false,
                direct_graphics: false,
                endpoint_keybindings: false,
                mouse_capture: false,
            }),
            15
        );
        assert_eq!(tag(&ClientMessage::Input { data: Vec::new() }), 1);
        assert_eq!(
            tag(&ClientMessage::ClipboardImage {
                target: ClientClipboardImageTarget::DirectTerminal,
                extension: "png".to_owned(),
                data: Vec::new(),
            }),
            2
        );
        assert_eq!(
            tag(&ClientMessage::Resize {
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: false,
            }),
            3
        );
        assert_eq!(tag(&ClientMessage::Detach), 4);
        assert_eq!(
            tag(&ClientMessage::AttachTerminal {
                terminal_id: "term".to_owned(),
                takeover: false,
            }),
            5
        );
        assert_eq!(
            tag(&ClientMessage::AttachScroll {
                source: AttachScrollSource::Wheel,
                direction: AttachScrollDirection::Up,
                lines: 1,
                column: None,
                row: None,
                modifiers: 0,
            }),
            6
        );
        assert_eq!(
            tag(&ClientMessage::ObserveTerminal {
                target: "w1:p1".to_owned(),
            }),
            8
        );
        assert_eq!(
            tag(&ClientMessage::ControlTerminal {
                target: "w1:p1".to_owned(),
                takeover: false,
            }),
            9
        );
        assert_eq!(
            tag(&ClientMessage::AttachMouse {
                kind: ClientMouseKind::Down(ClientMouseButton::Left),
                position: ClientMousePosition::Cell { column: 10, row: 5 },
                geometry: None,
                modifiers: 0,
                lines: 1,
            }),
            20
        );
        assert_eq!(
            tag(&ClientMessage::ClientShellHostTheme {
                update: ClientHostThemeUpdate::Appearance(ClientHostAppearance::Dark),
            }),
            21
        );
        assert_eq!(tag(&ClientMessage::ClientShellFocus { focused: true }), 22);
        assert_eq!(
            tag(&ClientMessage::ClientShellMouseCapture { enabled: true }),
            23
        );
        // Appended LAST (U3a). Upstream freezes this tag at 20 for its endpoint generation 1; the fork's
        // generation 1 keeps the fork's ordinals, so a running App client never changes dialect (PRD U3 D1).
        assert_eq!(
            tag(&ClientMessage::EndpointControl {
                kind: String::new(),
                data: String::new(),
            }),
            24
        );
    }

    #[test]
    fn client_shell_pane_input_roundtrips_semantic_and_windows_keys() {
        let windows_record = crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x37,
            virtual_scan_code: 0x08,
            unicode: 0,
            control_key_state: 0x0008,
        };
        let message = ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p2".into(),
            events: vec![
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('l'),
                    modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
                    kind: ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: Some('L' as u32),
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('7'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: Some(0x08),
                    windows_record: Some(windows_record),
                },
            ],
        };
        let encoded = bincode::serde::encode_to_vec(&message, bincode::config::standard())
            .expect("encode targeted semantic input");
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode targeted semantic input");
        assert_eq!(decoded, message);
        assert_eq!(
            upstream_tagged_sha256(&message, 17, 13),
            "f558384bb53dfd2baf1fa72e1709d88be6891da79e51f88513905afc085065e6"
        );
        let ClientMessage::ClientShellPaneInput { events, .. } = decoded else {
            panic!("expected targeted semantic input");
        };
        let crate::raw_input::RawInputEvent::Key(semantic) = events[0].to_raw_input_event() else {
            panic!("expected semantic key");
        };
        assert_eq!(semantic.shifted_codepoint, Some('L' as u32));
        assert_eq!(semantic.kind, crossterm::event::KeyEventKind::Release);
        let crate::raw_input::RawInputEvent::Key(key) = events[1].to_raw_input_event() else {
            panic!("expected Windows key");
        };
        assert_eq!(key.code, crossterm::event::KeyCode::Char('7'));
        assert_eq!(key.modifiers, crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(key.windows_record(), Some(windows_record));
    }

    #[test]
    fn client_shell_pane_input_reconstructs_windows_dead_key_without_native_source() {
        let event = ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char('6'),
            modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
            kind: ClientKeyKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: true,
            physical_key_id: Some(0x07),
            windows_record: Some(crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 0x36,
                virtual_scan_code: 0x07,
                unicode: 0,
                control_key_state: 0x0030,
            }),
        };
        let crate::raw_input::RawInputEvent::Key(key) =
            event.to_raw_input_event_with_windows_source(false)
        else {
            panic!("pane dead key should remain a key");
        };
        assert!(key.is_windows_shift_dead_key());
        assert_eq!(key.windows_record(), None);
        assert!(crate::input::encode_terminal_key(
            key,
            crate::input::KeyboardProtocol::Kitty { flags: 1 },
        )
        .is_empty());
    }

    #[test]
    fn client_shell_key_roundtrip_preserves_physical_generated_text_encoding() {
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::SHIFT,
        )
        .with_generated_text(Some("/".into()))
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x37,
            virtual_scan_code: 0x08,
            unicode: '/' as u16,
            control_key_state: 0,
        });
        let event = ClientPaneInputEvent::from_terminal_key(key).expect("semantic pane key");
        assert!(matches!(
            event,
            ClientPaneInputEvent::Key {
                tracks_release: true,
                ..
            }
        ));
        let crate::raw_input::RawInputEvent::Key(roundtripped) = event.to_raw_input_event() else {
            panic!("pane key should remain a key");
        };

        assert!(roundtripped.has_physical_identity());
        let encoded = crate::input::encode_terminal_key(
            roundtripped,
            crate::input::KeyboardProtocol::Kitty { flags: 8 },
        );
        assert_ne!(encoded, b"/");
        assert!(encoded.starts_with(b"\x1b["));
    }

    #[test]
    fn client_shell_focus_roundtrip() {
        let message = ClientMessage::ClientShellFocus { focused: false };
        let encoded = bincode::serde::encode_to_vec(&message, bincode::config::standard())
            .expect("encode client shell focus");
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode client shell focus");
        assert_eq!(decoded, message);
    }

    #[test]
    fn client_shell_mouse_capture_roundtrip() {
        let message = ClientMessage::ClientShellMouseCapture { enabled: false };
        let encoded = bincode::serde::encode_to_vec(&message, bincode::config::standard())
            .expect("encode client shell mouse capture");
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode client shell mouse capture");
        assert_eq!(decoded, message);
    }

    #[test]
    fn client_shell_host_theme_roundtrip() {
        let message = ClientMessage::ClientShellHostTheme {
            update: ClientHostThemeUpdate::PaletteColors(vec![(
                4,
                ClientHostColor {
                    r: 10,
                    g: 20,
                    b: 30,
                },
            )]),
        };
        let encoded = bincode::serde::encode_to_vec(&message, bincode::config::standard())
            .expect("encode host theme update");
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode host theme update");
        assert_eq!(decoded, message);
    }

    #[test]
    fn client_shell_endpoint_messages_roundtrip() {
        let request = ClientMessage::ClientShellEndpointRequest {
            boot_id: "boot-a".into(),
            request: r#"{"id":"request-a","method":"session.snapshot","params":{}}"#.into(),
        };
        let encoded = bincode::serde::encode_to_vec(&request, bincode::config::standard())
            .expect("encode endpoint request");
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode endpoint request");
        assert_eq!(decoded, request);
        assert_eq!(
            upstream_tagged_sha256(&request, 19, 15),
            "de5693585a01f6b0d5ee07c51b6ddf79ee9f67dbf183255822d31f35210f5ffb"
        );

        let response = ServerMessage::ClientShellEndpointResponseChunk {
            boot_id: "boot-a".into(),
            request_id: "request-a".into(),
            final_chunk: true,
            data: br#"{"id":"request-a","result":{"type":"ok"}}"#.to_vec(),
        };
        let encoded = bincode::serde::encode_to_vec(&response, bincode::config::standard())
            .expect("encode endpoint response");
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode endpoint response");
        assert_eq!(decoded, response);
        assert_eq!(
            upstream_tagged_sha256(&response, 25, 18),
            "bc14dbb5263d3097fe6d3e70a4b6d71aa9c2fa4ae3206d692a7182512bffdd1d"
        );
    }

    #[test]
    fn wire_release_cannot_restore_a_grouped_repeat_count() {
        let event = ClientInputEvent::Key {
            code: ClientKeyCode::Esc,
            modifiers: 0,
            kind: ClientKeyKind::Release,
            repeat_count: 3,
            generated_text: Some("ignored".to_owned()),
            source: ClientKeySource::Synthesized,
        };

        match event.to_raw_input_event() {
            crate::raw_input::RawInputEvent::Key(key) => {
                assert_eq!(key.kind, crossterm::event::KeyEventKind::Release);
                assert_eq!(key.repeat_count, 1);
                assert_eq!(key.generated_text, None);
            }
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[test]
    fn client_input_events_convert_to_raw_keys() {
        let record = crate::input::WindowsKeyRecord {
            key_down: false,
            repeat_count: 1,
            virtual_key_code: 78,
            virtual_scan_code: 49,
            unicode: 78,
            control_key_state: 16,
        };
        let shifted = ClientInputEvent::Key {
            code: ClientKeyCode::Char('N'),
            modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
            kind: ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: None,
            source: ClientKeySource::WindowsConsole { record },
        }
        .to_raw_input_event();
        match shifted {
            crate::raw_input::RawInputEvent::Key(key) => {
                assert_eq!(key.code, crossterm::event::KeyCode::Char('N'));
                assert_eq!(key.modifiers, crossterm::event::KeyModifiers::SHIFT);
                assert_eq!(key.kind, crossterm::event::KeyEventKind::Press);
                assert_eq!(
                    key.windows_record().map(|record| record.key_down),
                    Some(false)
                );
            }
            other => panic!("expected shifted key event, got {other:?}"),
        }

        let backspace = ClientInputEvent::Key {
            code: ClientKeyCode::Backspace,
            modifiers: 0,
            kind: ClientKeyKind::Press,

            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }
        .to_raw_input_event();
        match backspace {
            crate::raw_input::RawInputEvent::Key(key) => {
                assert_eq!(key.code, crossterm::event::KeyCode::Backspace);
                assert_eq!(key.modifiers, crossterm::event::KeyModifiers::empty());
                assert_eq!(key.kind, crossterm::event::KeyEventKind::Press);
            }
            other => panic!("expected backspace key event, got {other:?}"),
        }
    }

    #[test]
    fn client_clipboard_image_roundtrip() {
        let msg = ClientMessage::ClipboardImage {
            target: ClientClipboardImageTarget::Pane("w1:p1".into()),
            extension: "png".to_owned(),
            data: vec![0x89, b'P', b'N', b'G'],
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        assert_eq!(
            encoded_sha256(&msg),
            "1c02110be0671faf318b3f4d8f507748d5a0a98cd474832669c5290d97ef19ab"
        );
    }

    #[test]
    fn client_input_large_multilingual_payload_roundtrip() {
        let text = "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.🙂".repeat(1024);
        assert!(text.len() > 64 * 1024);
        assert!(text.len() < MAX_FRAME_SIZE);
        let msg = ClientMessage::Input {
            data: text.as_bytes().to_vec(),
        };

        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, consumed): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, msg);
    }

    #[test]
    fn client_resize_roundtrip() {
        let msg = ClientMessage::Resize {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_detach_roundtrip() {
        let msg = ClientMessage::Detach;
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_attach_terminal_roundtrip() {
        let msg = ClientMessage::AttachTerminal {
            terminal_id: "term_123".to_owned(),
            takeover: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_observe_terminal_roundtrip() {
        let msg = ClientMessage::ObserveTerminal {
            target: "w1:p1".to_owned(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_control_terminal_roundtrip() {
        let msg = ClientMessage::ControlTerminal {
            target: "w1:p1".to_owned(),
            takeover: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_attach_scroll_roundtrip() {
        let msg = ClientMessage::AttachScroll {
            source: AttachScrollSource::Wheel,
            direction: AttachScrollDirection::Up,
            lines: 3,
            column: Some(12),
            row: Some(7),
            modifiers: 4,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    // ---- Round-trip: ServerMessage ----

    #[test]
    fn server_welcome_roundtrip() {
        let msg = ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: RenderEncoding::SemanticFrame,
            error: None,
            accepted: Vec::new(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_welcome_with_error_roundtrip() {
        let msg = ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: RenderEncoding::SemanticFrame,
            error: Some("incompatible version".to_owned()),
            accepted: Vec::new(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_frame_roundtrip_nontrivial() {
        // Build a 3×2 frame with varied styles (≥2×2).
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "H".into(),
                    fg: color_to_u32(Color::Red),
                    bg: color_to_u32(Color::Black),
                    modifier: Modifier::BOLD.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "i".into(),
                    fg: color_to_u32(Color::Green),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::ITALIC.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "!".into(),
                    fg: color_to_u32(Color::Rgb(255, 128, 0)),
                    bg: color_to_u32(Color::Indexed(220)),
                    modifier: (Modifier::BOLD | Modifier::UNDERLINED).bits(),
                    skip: false,
                    hyperlink: Some(0),
                },
                CellData {
                    symbol: " ".into(),
                    fg: color_to_u32(Color::Reset),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::empty().bits(),
                    skip: true,
                    hyperlink: None,
                },
                CellData {
                    symbol: "→".into(), // multi-byte grapheme
                    fg: color_to_u32(Color::Cyan),
                    bg: color_to_u32(Color::Blue),
                    modifier: Modifier::REVERSED.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "🦀".into(), // emoji, wide grapheme cluster
                    fg: color_to_u32(Color::Yellow),
                    bg: color_to_u32(Color::Magenta),
                    modifier: Modifier::empty().bits(),
                    skip: false,
                    hyperlink: None,
                },
            ],
            width: 3,
            height: 2,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 6,
            }),
            hyperlinks: vec!["https://example.com".to_owned()],
            graphics: Vec::new(),
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: frame.clone(),
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene::default(),
        });
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        assert_eq!(
            upstream_tagged_sha256(&msg, 22, 13),
            "7c016f7b21ddb5ac79212cf65a968b93eb292b5305b941263e89ffaa40158ee3"
        );
        match decoded {
            ServerMessage::PaneSurface(surface) => {
                assert_eq!(surface.frame.cells[2].hyperlink, Some(0));
                assert_eq!(
                    surface.frame.hyperlinks,
                    vec!["https://example.com".to_owned()]
                );
            }
            other => panic!("expected pane surface, got {other:?}"),
        }
    }

    #[test]
    fn server_frame_roundtrip_nontrivial_app_client() {
        // Build a 3×2 frame with varied styles (≥2×2).
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "H".into(),
                    fg: color_to_u32(Color::Red),
                    bg: color_to_u32(Color::Black),
                    modifier: Modifier::BOLD.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "i".into(),
                    fg: color_to_u32(Color::Green),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::ITALIC.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "!".into(),
                    fg: color_to_u32(Color::Rgb(255, 128, 0)),
                    bg: color_to_u32(Color::Indexed(220)),
                    modifier: (Modifier::BOLD | Modifier::UNDERLINED).bits(),
                    skip: false,
                    hyperlink: Some(0),
                },
                CellData {
                    symbol: " ".into(),
                    fg: color_to_u32(Color::Reset),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::empty().bits(),
                    skip: true,
                    hyperlink: None,
                },
                CellData {
                    symbol: "→".into(), // multi-byte grapheme
                    fg: color_to_u32(Color::Cyan),
                    bg: color_to_u32(Color::Blue),
                    modifier: Modifier::REVERSED.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "🦀".into(), // emoji, wide grapheme cluster
                    fg: color_to_u32(Color::Yellow),
                    bg: color_to_u32(Color::Magenta),
                    modifier: Modifier::empty().bits(),
                    skip: false,
                    hyperlink: None,
                },
            ],
            width: 3,
            height: 2,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 6,
            }),
            hyperlinks: vec!["https://example.com".to_owned()],
            graphics: Vec::new(),
        };
        let msg = ServerMessage::Frame(frame.clone());
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        match decoded {
            ServerMessage::Frame(frame) => {
                assert_eq!(frame.cells[2].hyperlink, Some(0));
                assert_eq!(frame.hyperlinks, vec!["https://example.com".to_owned()]);
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn pane_surface_patch_roundtrip() {
        let msg = ServerMessage::PaneSurfacePatch(PaneSurfacePatch {
            boot_id: "boot-1".into(),
            projection_revision: 3,
            base_surface_revision: 7,
            surface_revision: 8,
            rows: vec![PaneSurfacePatchRow {
                x: 2,
                y: 4,
                cells: vec![CellData {
                    symbol: "x".into(),
                    fg: 1,
                    bg: 2,
                    modifier: 3,
                    skip: false,
                    hyperlink: None,
                }],
            }],
            panes: Vec::new(),
            cursor: Some(CursorState {
                x: 2,
                y: 4,
                visible: true,
                shape: 2,
            }),
        });
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(
            upstream_tagged_sha256(&msg, 28, 19),
            "0814b99a1dc6eaf7918424aa416c066509cbfb73b72344a809c27cde78cb6dbd"
        );
    }

    #[test]
    fn client_shell_graphics_payload_codec_is_frozen() {
        let key = SurfaceGraphicsAssetKey {
            source: SurfaceGraphicsSource::Terminal {
                target: SurfaceGraphicsTarget::Pane {
                    pane_id: "w1:p1".into(),
                },
                image_id: 7,
            },
            image_width: 2,
            image_height: 1,
            format: SurfaceGraphicsFormat::Rgba,
            data_len: 8,
            data_fingerprint: 42,
        };
        let message = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 2,
            surface_revision: 3,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene {
                assets: vec![SurfaceGraphicsAsset {
                    key: key.clone(),
                    data: vec![255, 0, 0, 255, 0, 255, 0, 255],
                }],
                placements: vec![SurfaceGraphicsPlacement {
                    asset: key,
                    logical_placement_id: 9,
                    x: 1,
                    y: 2,
                    cols: 2,
                    rows: 1,
                    source_x: 0,
                    source_y: 0,
                    source_width: 2,
                    source_height: 1,
                    x_offset: 0,
                    y_offset: 0,
                    z: -1,
                    scrollback_offset: 0,
                }],
                retained_assets: Vec::new(),
            },
        });

        assert_eq!(
            upstream_tagged_sha256(&message, 22, 13),
            "49c4efec0f1456c8ca4112ddf6ead1ab75d0224007576c2ccc18c3fca55a69f0"
        );
    }

    #[test]
    fn server_endpoint_control_tag_is_frozen() {
        let message = ServerMessage::EndpointControl {
            kind: "endpoint.welcome.v1".into(),
            data: r#"{"generation":1}"#.into(),
        };
        let encoded = bincode::serde::encode_to_vec(&message, bincode::config::standard()).unwrap();
        assert_eq!(encoded.first(), Some(&29));
        assert_eq!(
            bincode::serde::encode_to_vec(
                ServerMessage::EndpointControl {
                    kind: String::new(),
                    data: String::new(),
                },
                bincode::config::standard(),
            )
            .unwrap(),
            [29, 0, 0]
        );
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn client_shell_server_message_tags_are_frozen() {
        // Fork ordinals: fork variants come first, every upstream variant is appended in
        // upstream's order (PRD U3 D1).
        fn tag(message: &ServerMessage) -> u8 {
            *bincode::serde::encode_to_vec(message, bincode::config::standard())
                .unwrap()
                .first()
                .expect("encoded server message should include enum tag")
        }

        let empty_frame = || PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene::default(),
        };
        assert_eq!(tag(&ServerMessage::PaneSurface(empty_frame())), 22);
        assert_eq!(
            tag(&ServerMessage::ClientShellError {
                message: String::new(),
            }),
            24
        );
        assert_eq!(
            tag(&ServerMessage::ClientShellKeyboardReportAll { enabled: false }),
            27
        );
        assert_eq!(
            tag(&ServerMessage::ClientShellEndpointResponseChunk {
                boot_id: String::new(),
                request_id: String::new(),
                final_chunk: true,
                data: Vec::new(),
            }),
            25
        );
        assert_eq!(
            tag(&ServerMessage::PaneSurfacePatch(PaneSurfacePatch {
                boot_id: String::new(),
                projection_revision: 0,
                base_surface_revision: 0,
                surface_revision: 0,
                rows: Vec::new(),
                panes: Vec::new(),
                cursor: None,
            })),
            28
        );
        assert_eq!(
            tag(&ServerMessage::EndpointControl {
                kind: String::new(),
                data: String::new(),
            }),
            29
        );
    }

    #[test]
    fn client_shell_snapshot_roundtrip() {
        let msg = ServerMessage::ClientShellSnapshot(Box::new(ClientShellSnapshot {
            boot_id: "boot-1".into(),
            revision: 1,
            config_diagnostic: Some("endpoint config warning".into()),
            product_announcement: Some(ClientShellProductAnnouncement {
                version: "0.8.2".into(),
                id: "client-shell".into(),
                title: "Client shell".into(),
                body: "### New\n- Client-owned chrome".into(),
                preview: false,
            }),
            update_available: Some("0.8.3".into()),
            update_install_command: "herdr update".into(),
            server_keybindings_toml: Some("[keys]\nprefix = \"ctrl+a\"\n".into()),
            latest_release_notes_available: true,
            integration_updates_available: true,
            worktree_directory: "/tmp/herdr-worktrees".into(),
            release_notes: Some(ClientShellReleaseNotes {
                version: "0.8.3".into(),
                body: "### New\n- Update ready".into(),
                preview: true,
            }),
            focused_workspace_id: Some("w1".into()),
            focused_tab_id: Some("w1:t1".into()),
            focused_pane_id: Some("w1:p1".into()),
            tab_bar_right: vec![ClientShellTabStatusSegment {
                text: "host".into(),
                accent: false,
            }],
            tab_bar_right_separator: " · ".into(),
            agent_view_label: None,
            agent_order: Vec::new(),
            workspaces: vec![ClientShellWorkspace {
                workspace_id: "w1".into(),
                active_tab_id: "w1:t1".into(),
                new_workspace_cwd: "/tmp".into(),
                number: 1,
                label: "shell".into(),
                custom_label: false,
                branch: Some("main".into()),
                git_ahead_behind: None,
                tokens: Vec::new(),
                worktree: None,
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            tabs: vec![ClientShellTab {
                tab_id: "w1:t1".into(),
                workspace_id: "w1".into(),
                number: 1,
                label: "main".into(),
                custom_label: true,
                zoomed: false,
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            panes: vec![ClientShellPane {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                label: None,
                cwd: Some("/repo".into()),
                foreground_cwd: Some("/repo".into()),
                focused: true,
                right_click_passthrough: false,
            }],
            agents: Vec::new(),
            commands: vec![ClientShellCommand {
                command_id: "cmd_0123456789abcdef0123456789abcdef".into(),
                binding_label: "prefix+z".into(),
                binding_labels: vec!["prefix+z".into()],
                action: ClientShellCommandAction::Shell,
                description: Some("deploy".into()),
            }],
        }));
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_shutdown_roundtrip() {
        let msg = ServerMessage::ServerShutdown {
            reason: Some("updating".to_owned()),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn semantic_notification_roundtrip() {
        let msg = ServerMessage::SemanticNotification(SemanticNotification {
            kind: SemanticNotificationKind::NeedsAttention,
            title: "codex needs attention".into(),
            body: Some("repo · 1".into()),
            sound: Some(SemanticNotificationSound::Request),
            agent: Some("codex".into()),
            workspace_id: Some("w1".into()),
            tab_id: Some("w1:t1".into()),
            pane_id: Some("w1:p1".into()),
            position: Some(crate::config::ToastHerdrPosition::TopRight),
        });
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_notify_roundtrip() {
        for kind in [
            NotifyKind::Sound,
            NotifyKind::Toast,
            NotifyKind::SystemToast,
        ] {
            let msg = ServerMessage::Notify {
                kind,
                message: "agent done".to_owned(),
                body: None,
            };
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ServerMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn server_clipboard_roundtrip() {
        let msg = ServerMessage::Clipboard {
            data: "dGVzdA==".to_owned(), // base64 "test"
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_window_title_roundtrip() {
        for title in [Some("herdr api".to_owned()), None] {
            let msg = ServerMessage::WindowTitle { title };
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ServerMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn server_graphics_roundtrip() {
        let msg = ServerMessage::Graphics {
            bytes: b"\x1b_Ga=d,d=A,q=2;\x1b\\".to_vec(),
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
        assert_eq!(
            upstream_tagged_sha256(&msg, 3, 2),
            "28a420f92e0e05e6760a8c140baf307c360c6d1b1aa68027481b324f87e22c44"
        );
    }

    #[test]
    fn server_terminal_frame_roundtrip() {
        let msg = ServerMessage::Terminal(TerminalFrame {
            seq: 7,
            width: 120,
            height: 40,
            full: false,
            bytes: b"\x1b[1;1Hhello".to_vec(),
        });
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_reload_sound_config_roundtrip() {
        let msg = ServerMessage::ReloadSoundConfig;
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_mouse_capture_roundtrip() {
        let msg = ServerMessage::MouseCapture {
            enabled: true,
            sgr_pixels: true,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_kitty_keyboard_report_all_roundtrip() {
        let msg = ServerMessage::KittyKeyboardReportAll { enabled: true };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_shell_keyboard_report_all_roundtrip() {
        let msg = ServerMessage::ClientShellKeyboardReportAll { enabled: true };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn direct_terminal_keyboard_mode_roundtrip() {
        let msg = ServerMessage::DirectTerminalKeyboardProtocol {
            flags: 15,
            modify_other_keys_level: 1,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn direct_graphics_messages_roundtrip() {
        let client = ClientMessage::GraphicsTransmissionResult {
            transfer_id: 7,
            image_id: 42,
            success: false,
        };
        let encoded = bincode::serde::encode_to_vec(&client, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(client, decoded);

        let server = ServerMessage::GraphicsFile {
            path: "/run/user/1000/herdr/source/frame".into(),
            expected_len: 4,
            image_id: 42,
            transfer_id: 7,
            leading: b"\x1b[2;3H".to_vec(),
            control: "a=T,f=32,i=42,q=0".into(),
            surface_asset: None,
        };
        let encoded = bincode::serde::encode_to_vec(&server, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(server, decoded);

        let pixels = ClientMessage::InputPixels {
            data: b"\x1b[<35;321;241M".to_vec(),
            cols: 80,
            rows: 24,
            width_px: 800,
            height_px: 480,
        };
        let encoded = bincode::serde::encode_to_vec(&pixels, bincode::config::standard()).unwrap();
        let (decoded, _): (ClientMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(pixels, decoded);
    }

    #[test]
    fn server_prefix_input_source_roundtrip() {
        for active in [true, false] {
            let msg = ServerMessage::PrefixInputSource { active };
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ServerMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn server_terminal_bell_roundtrip() {
        let msg = ServerMessage::TerminalBell { count: 3 };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(msg, decoded);
    }

    // ---- Framing ----

    #[test]
    fn framing_small_message_roundtrip() {
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::App,
            pixel_mouse: false,
            capabilities: Vec::new(),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_large_payload_roundtrip() {
        // Create a pane-surface message that is ≥128 KB.
        // Use a large frame with verbose cell data to exceed 128 KB after bincode encoding.
        // 200×50 = 10000 cells. With varied symbols and styles, this should easily exceed 128 KB.
        let width: u16 = 200;
        let height: u16 = 50;
        let cells: Vec<CellData> = (0..(width as usize) * (height as usize))
            .map(|i| CellData {
                symbol: if i % 256 < 32 {
                    " ".to_owned()
                } else {
                    format!("{:03}", i % 1000)
                },
                fg: color_to_u32(Color::Rgb((i % 256) as u8, ((i / 256) % 256) as u8, 128)),
                bg: color_to_u32(Color::Indexed((i % 256) as u8)),
                modifier: ((i % 16) as u16),
                skip: i % 100 == 0,
                hyperlink: None,
            })
            .collect();

        let frame = FrameData {
            cells,
            width,
            height,
            cursor: Some(CursorState {
                x: 10,
                y: 5,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame,
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene::default(),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        // Verify the payload is at least 128 KB
        assert!(
            buf.len() >= 128 * 1024,
            "framed payload should be >= 128 KB, got {} bytes",
            buf.len()
        );

        let decoded: ServerMessage = read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_large_payload_roundtrip_app_client() {
        // Create a Frame message that is ≥128 KB.
        // Use a large frame with verbose cell data to exceed 128 KB after bincode encoding.
        // 200×50 = 10000 cells. With varied symbols and styles, this should easily exceed 128 KB.
        let width: u16 = 200;
        let height: u16 = 50;
        let cells: Vec<CellData> = (0..(width as usize) * (height as usize))
            .map(|i| CellData {
                symbol: if i % 256 < 32 {
                    " ".to_owned()
                } else {
                    format!("{:03}", i % 1000)
                },
                fg: color_to_u32(Color::Rgb((i % 256) as u8, ((i / 256) % 256) as u8, 128)),
                bg: color_to_u32(Color::Indexed((i % 256) as u8)),
                modifier: ((i % 16) as u16),
                skip: i % 100 == 0,
                hyperlink: None,
            })
            .collect();

        let frame = FrameData {
            cells,
            width,
            height,
            cursor: Some(CursorState {
                x: 10,
                y: 5,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let msg = ServerMessage::Frame(frame);

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        // Verify the payload is at least 128 KB
        assert!(
            buf.len() >= 128 * 1024,
            "framed payload should be >= 128 KB, got {} bytes",
            buf.len()
        );

        let decoded: ServerMessage = read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_multiple_messages_sequential() {
        // Write 100+ messages of varying types and read them back.
        let mut buf = Vec::new();
        let mut expected = Vec::new();

        for i in 0..150u32 {
            let msg = match i % 5 {
                0 => ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    cols: (80 + (i % 40) as u16),
                    rows: (24 + (i % 20) as u16),
                    cell_width_px: 8,
                    cell_height_px: 16,
                    requested_encoding: RenderEncoding::SemanticFrame,
                    keybindings: ClientKeybindings::Server,
                    launch_mode: ClientLaunchMode::App,
                    pixel_mouse: i % 2 == 0,
                    capabilities: Vec::new(),
                },
                1 => ClientMessage::Input {
                    data: vec![(i % 256) as u8; (i as usize % 50) + 1],
                },
                2 => ClientMessage::ClipboardImage {
                    target: ClientClipboardImageTarget::DirectTerminal,
                    extension: "png".to_owned(),
                    data: vec![0x89, b'P', b'N', b'G', (i % 256) as u8],
                },
                3 => ClientMessage::Resize {
                    cols: (100 + (i % 30) as u16),
                    rows: (30 + (i % 10) as u16),
                    cell_width_px: 8,
                    cell_height_px: 16,
                    pixel_mouse: i % 2 == 0,
                },
                4 => ClientMessage::Detach,
                _ => unreachable!(),
            };
            write_message(&mut buf, &msg).unwrap();
            expected.push(msg);
        }

        let mut cursor = buf.as_slice();
        for expected_msg in &expected {
            let decoded: ClientMessage = read_message(&mut cursor, MAX_FRAME_SIZE).unwrap();
            assert_eq!(*expected_msg, decoded);
        }
    }

    #[test]
    fn framing_oversized_rejected_without_panic() {
        // Craft a frame with a huge length prefix (4 GB claim).
        let mut buf: Vec<u8> = (u32::MAX).to_le_bytes().to_vec();
        // Add a few garbage bytes after the length prefix.
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::Oversized { claimed, max }) => {
                assert_eq!(claimed, u32::MAX as usize);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected Oversized error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_malformed_payload_rejected_without_panic() {
        // Valid length prefix pointing to garbage data.
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let mut buf = (payload.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err(), "malformed payload should be rejected");
        match result {
            Err(FramingError::Bincode(_)) => {} // expected
            other => panic!("expected Bincode error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_truncated_stream_returns_unexpected_eof() {
        // Write a length prefix claiming 100 bytes, but only provide 4.
        let mut buf: Vec<u8> = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got: {other:?}"),
        }
    }

    #[test]
    fn framing_zero_length_message() {
        // A 1-byte message (smallest possible valid bincode payload).
        // Actually, let's test with the smallest real message: Detach.
        let msg = ClientMessage::Detach;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();

        // Verify the length prefix is correct
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(
            len,
            buf.len() - 4,
            "length prefix should match payload size"
        );

        let decoded: ClientMessage = read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_partial_read_reassembly() {
        // Simulate partial reads by using a reader that yields small chunks.
        let msg = ClientMessage::Input {
            data: vec![42; 500], // 500-byte input payload
        };
        let mut full_buf = Vec::new();
        write_message(&mut full_buf, &msg).unwrap();

        // Wrap in a chunked reader that only yields 7 bytes at a time.
        let mut chunked = ChunkedReader::new(full_buf, 7);
        let decoded: ClientMessage = read_message(&mut chunked, MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    // ---- Version negotiation ----

    #[test]
    fn version_compatible() {
        assert_eq!(
            check_client_version(PROTOCOL_VERSION),
            VersionCheck::Compatible
        );
    }

    #[test]
    fn version_older_client_rejected() {
        let result = check_client_version(PROTOCOL_VERSION - 1);
        assert!(matches!(result, VersionCheck::Incompatible(_)));
        if let VersionCheck::Incompatible(msg) = result {
            assert!(msg.contains("older"), "error should mention older version");
        }
    }

    #[test]
    fn version_newer_client_rejected() {
        let result = check_client_version(PROTOCOL_VERSION + 1);
        assert!(matches!(result, VersionCheck::Incompatible(_)));
        if let VersionCheck::Incompatible(msg) = result {
            assert!(msg.contains("newer"), "error should mention newer version");
        }
    }

    // ---- Pre-persistence client rejection ----

    #[test]
    fn prepersistence_version_zero_rejected() {
        let result = check_client_version(0);
        match result {
            VersionCheck::Incompatible(msg) => {
                assert!(
                    msg.contains("pre-persistence"),
                    "error should mention pre-persistence: {msg}"
                );
            }
            _ => panic!("version 0 should be rejected as incompatible"),
        }
    }

    #[test]
    fn prepersistence_version_zero_welcome_has_error() {
        // Simulating what the server would send to a v0 client.
        let check = check_client_version(0);
        let response = match check {
            VersionCheck::Compatible => ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: None,
                accepted: Vec::new(),
            },
            VersionCheck::Incompatible(reason) => ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: Some(reason),
                accepted: Vec::new(),
            },
        };

        match response {
            ServerMessage::Welcome { error: Some(_), .. } => {}
            other => panic!("expected Welcome with error, got: {other:?}"),
        }
    }

    // ---- Malformed/oversized input ----

    #[test]
    fn oversized_frame_does_not_panic() {
        // Claim 4GB payload — should return Oversized error, not panic.
        let mut buf: Vec<u8> = 0xFFC00000u32.to_le_bytes().to_vec(); // ~4 GB claim
        buf.extend_from_slice(&[0; 8]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err());
        // Did not panic — test passing is proof.
    }

    #[test]
    fn malformed_frame_does_not_panic() {
        // Random garbage bytes after a valid-ish length prefix.
        let garbage: Vec<u8> = (0..200).map(|i| (i ^ 0xAA) as u8).collect();
        let mut buf = (garbage.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&garbage);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err());
        // Did not panic.
    }

    #[test]
    fn oversized_input_rejected_custom_max() {
        // Verify a custom (small) max_frame_size is enforced.
        let msg = ClientMessage::Input {
            data: vec![0x41; 1000],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice(), 64);
        // The actual bincode payload for 1000 bytes of input will be > 64 bytes.
        assert!(
            matches!(result, Err(FramingError::Oversized { .. })),
            "expected Oversized with small max_frame_size"
        );
    }

    // ---- FrameData ↔ ratatui Buffer conversion ----

    #[test]
    fn frame_data_roundtrip_through_ratatui_buffer() {
        let area = ratatui::layout::Rect::new(0, 0, 5, 3);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));

        // Write some styled content.
        buffer.cell_mut((0, 0)).unwrap().set_symbol("H");
        buffer.cell_mut((0, 0)).unwrap().fg = Color::Red;
        buffer.cell_mut((0, 0)).unwrap().modifier = Modifier::BOLD;

        buffer.cell_mut((1, 0)).unwrap().set_symbol("i");
        buffer.cell_mut((1, 0)).unwrap().fg = Color::Green;
        buffer.cell_mut((1, 0)).unwrap().modifier = Modifier::ITALIC;

        buffer.cell_mut((2, 0)).unwrap().set_symbol("!");
        buffer.cell_mut((2, 0)).unwrap().fg = Color::Rgb(255, 128, 0);
        buffer.cell_mut((2, 0)).unwrap().bg = Color::Indexed(220);

        let cursor = CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: 0,
        };
        let frame = FrameData::from_ratatui_buffer(&buffer, Some(cursor.clone()));

        // Verify frame dimensions.
        assert_eq!(frame.width, 5);
        assert_eq!(frame.height, 3);
        assert_eq!(frame.cells.len(), 15);
        assert_eq!(frame.cursor, Some(cursor));

        // Verify specific cells survived the conversion.
        assert_eq!(frame.cells[0].symbol, "H");
        assert_eq!(frame.cells[0].fg, color_to_u32(Color::Red));
        assert_eq!(frame.cells[0].modifier, Modifier::BOLD.bits());

        assert_eq!(frame.cells[1].symbol, "i");
        assert_eq!(frame.cells[1].fg, color_to_u32(Color::Green));
        assert_eq!(frame.cells[1].modifier, Modifier::ITALIC.bits());

        assert_eq!(frame.cells[2].symbol, "!");
        assert_eq!(frame.cells[2].fg, color_to_u32(Color::Rgb(255, 128, 0)));
        assert_eq!(frame.cells[2].bg, color_to_u32(Color::Indexed(220)));

        let with_links = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            None,
            &[((1, 0), "i".to_owned(), "https://example.com".to_owned())],
        );
        assert_eq!(with_links.cells[1].hyperlink, Some(0));
        assert_eq!(
            with_links.hyperlinks,
            vec!["https://example.com".to_owned()]
        );

        // Convert back to ratatui buffer and compare.
        let restored = frame.to_ratatui_buffer().expect("should reconstruct");
        assert_eq!(restored.area, area);
        assert_eq!(restored.cell((0, 0)).unwrap().symbol(), "H");
        assert_eq!(restored.cell((0, 0)).unwrap().fg, Color::Red);
        assert_eq!(restored.cell((0, 0)).unwrap().modifier, Modifier::BOLD);
        assert_eq!(restored.cell((1, 0)).unwrap().symbol(), "i");
        assert_eq!(restored.cell((2, 0)).unwrap().symbol(), "!");
        assert_eq!(restored.cell((2, 0)).unwrap().fg, Color::Rgb(255, 128, 0));
    }

    #[test]
    fn frame_data_rejects_mismatched_cell_count() {
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "X".into(),
                    fg: 0,
                    bg: 0,
                    modifier: 0,
                    skip: false,
                    hyperlink: None,
                };
                5
            ], // 5 cells but 3×2 = 6 expected
            width: 3,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        assert!(frame.to_ratatui_buffer().is_none());
    }

    // ---- Color conversion coverage ----

    #[test]
    fn color_roundtrip_all_named_colors() {
        let named = [
            Color::Reset,
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        for c in named {
            assert_eq!(
                u32_to_color(color_to_u32(c)),
                c,
                "roundtrip failed for {c:?}"
            );
        }
    }

    #[test]
    fn color_roundtrip_indexed() {
        for i in 0..=255u8 {
            let c = Color::Indexed(i);
            assert_eq!(
                u32_to_color(color_to_u32(c)),
                c,
                "roundtrip failed for Indexed({i})"
            );
        }
    }

    #[test]
    fn color_roundtrip_rgb() {
        let c = Color::Rgb(0xAB, 0xCD, 0xEF);
        assert_eq!(u32_to_color(color_to_u32(c)), c);

        let c = Color::Rgb(0, 0, 0);
        assert_eq!(u32_to_color(color_to_u32(c)), c);

        let c = Color::Rgb(255, 255, 255);
        assert_eq!(u32_to_color(color_to_u32(c)), c);
    }

    // ---- Modifier conversion ----

    #[test]
    fn modifier_roundtrip() {
        let all_mods = [
            Modifier::BOLD,
            Modifier::ITALIC,
            Modifier::REVERSED,
            Modifier::UNDERLINED,
            Modifier::DIM,
            Modifier::SLOW_BLINK,
            Modifier::CROSSED_OUT,
            Modifier::BOLD | Modifier::ITALIC,
            Modifier::BOLD | Modifier::UNDERLINED | Modifier::REVERSED,
            Modifier::empty(),
        ];
        for m in all_mods {
            assert_eq!(
                u16_to_modifier(modifier_to_u16(m)),
                m,
                "roundtrip failed for {m:?}"
            );
        }
    }

    #[test]
    fn read_message_rejects_trailing_bytes() {
        // Encode a valid message, then append an extra byte after it.
        let msg = ClientMessage::Detach;
        let mut payload = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let original_len = payload.len();
        payload.push(0xDE); // trailing garbage

        // Frame it with the inflated length (original + 1).
        let mut buf = (payload.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::Bincode(msg)) => {
                assert!(
                    msg.contains("trailing bytes"),
                    "error should mention trailing bytes: {msg}"
                );
                assert!(
                    msg.contains(&format!("decoded {original_len}")),
                    "error should mention decoded byte count: {msg}"
                );
            }
            other => panic!("expected Bincode error about trailing bytes, got: {other:?}"),
        }
    }

    #[test]
    fn read_message_accepts_exact_payload() {
        // A normally-framed message should decode without error.
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::App,
            pixel_mouse: false,
            capabilities: Vec::new(),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let decoded: ClientMessage = read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn write_message_rejects_oversized_payload() {
        // We can't easily create a message that exceeds u32::MAX in a test,
        // but we can verify the check exists by testing that normal messages
        // have lengths well within the limit and the function doesn't fail.
        let msg = ClientMessage::Detach;
        let mut buf = Vec::new();
        assert!(write_message(&mut buf, &msg).is_ok());
    }

    // ---- Unix socketpair integration test ----

    #[cfg(unix)]
    #[test]
    fn framing_over_unix_socketpair() {
        use std::os::unix::net::UnixStream;

        let (mut a, mut b) = UnixStream::pair().expect("socketpair");

        let messages = vec![
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 200,
                rows: 60,
                cell_width_px: 8,
                cell_height_px: 16,
                requested_encoding: RenderEncoding::SemanticFrame,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::App,
                pixel_mouse: true,
                capabilities: Vec::new(),
            },
            ClientMessage::Input {
                data: b"hello world".to_vec(),
            },
            ClientMessage::ClipboardImage {
                target: ClientClipboardImageTarget::DirectTerminal,
                extension: "png".to_owned(),
                data: vec![0x89, b'P', b'N', b'G'],
            },
            ClientMessage::Resize {
                cols: 100,
                rows: 30,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            },
            ClientMessage::Detach,
        ];

        // Set non-blocking so we can write and read in the same test.
        a.set_nonblocking(false).unwrap();
        b.set_nonblocking(false).unwrap();

        for msg in &messages {
            write_message(&mut a, msg).unwrap();
        }

        for expected in &messages {
            let decoded: ClientMessage = read_message(&mut b, MAX_FRAME_SIZE).unwrap();
            assert_eq!(*expected, decoded);
        }
    }

    // ---- Helper: chunked reader for simulating partial reads ----

    /// A `Read` wrapper that yields at most `chunk_size` bytes per `read()` call,
    /// simulating partial reads on a real socket.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk_size,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let remaining = self.data.len() - self.pos;
            let to_read = buf.len().min(remaining).min(self.chunk_size);
            buf[..to_read].copy_from_slice(&self.data[self.pos..self.pos + to_read]);
            self.pos += to_read;
            Ok(to_read)
        }
    }
    // TP-MEDIA-CAP-06
    #[test]
    fn a_shared_name_with_no_shared_value_is_not_a_usable_capability() {
        // The trap this exists to close: `intersect` keeps a name whose values
        // do not overlap, because a flag capability legitimately has no values
        // and the set cannot tell the two apart. So after negotiating "audio
        // sink: opus" against "audio sink: pcm", `has` answers true and
        // `values_of` answers empty — and a caller that asks the first question
        // opens a stream the far side cannot decode a single byte of.
        //
        // Nothing about that failure is loud. The handshake succeeds, the
        // stream opens, chunks flow, and the only symptom is silence.
        let client = CapabilitySet::from_entries(vec![CapabilityEntry::with_values(
            capability::AUDIO_SINK,
            ["opus"],
        )]);
        let server = CapabilitySet::from_entries(vec![CapabilityEntry::with_values(
            capability::AUDIO_SINK,
            ["pcm-s16le"],
        )]);

        let accepted = client.intersect(&server);

        assert!(
            accepted.has(capability::AUDIO_SINK),
            "the name survives — this is the trap, not a bug"
        );
        assert!(accepted.values_of(capability::AUDIO_SINK).is_empty());
        assert_eq!(
            accepted.negotiated_value(capability::AUDIO_SINK),
            None,
            "the question a caller should be asking answers no"
        );
    }

    // TP-MEDIA-CAP-06
    #[test]
    fn a_negotiated_value_is_the_first_the_client_named() {
        // Order is preference. `intersect` keeps the announcing side's order,
        // and the announcing side of a sink is the client, so the first
        // surviving value is the client's favourite codec that the server can
        // also produce. Reversing this picks a codec both sides support and
        // neither prefers, silently and forever.
        let client = CapabilitySet::from_entries(vec![CapabilityEntry::with_values(
            capability::AUDIO_SINK,
            [codec::OPUS, codec::PCM_S16LE],
        )]);
        let server = CapabilitySet::from_entries(vec![CapabilityEntry::with_values(
            capability::AUDIO_SINK,
            [codec::PCM_S16LE, codec::OPUS],
        )]);

        let accepted = client.intersect(&server);

        assert_eq!(
            accepted.values_of(capability::AUDIO_SINK),
            [codec::OPUS, codec::PCM_S16LE]
        );
        assert_eq!(
            accepted.negotiated_value(capability::AUDIO_SINK),
            Some(codec::OPUS)
        );
    }

    // TP-MEDIA-CAP-06
    #[test]
    fn a_flag_capability_has_no_negotiated_value_and_that_is_not_a_failure() {
        // The other half of the same distinction: `media.streams` carries no
        // values by design, so `negotiated_value` answering None must not be
        // read as "not supported". `has` is the question for a flag; the two
        // are not interchangeable and this pins which is which.
        let both =
            CapabilitySet::from_entries(vec![CapabilityEntry::flag(capability::MEDIA_STREAMS)]);
        let accepted = both.intersect(&both);

        assert!(accepted.has(capability::MEDIA_STREAMS));
        assert_eq!(accepted.negotiated_value(capability::MEDIA_STREAMS), None);
    }

    // -- Media streams (F1) ------------------------------------------------
    //
    // Everything below guards one property: a media message must not be able
    // to move an existing message's wire tag, and must not be able to carry a
    // value the reader cannot survive. bincode is not self-describing — the
    // variant's position IS the tag — so an insertion in the middle of either
    // enum re-labels every message after it. That failure is silent: it
    // compiles, and the wrong variant decodes as a neighbour.

    /// The leading varint of a bincode-encoded enum is its discriminant.
    /// Values below 251 are a single byte, which is every tag we have.
    fn wire_tag(encoded: &[u8]) -> u8 {
        assert!(
            encoded[0] < 251,
            "tag {} left the single-byte varint range; this helper needs to grow",
            encoded[0]
        );
        encoded[0]
    }

    fn server_tag(msg: &ServerMessage) -> u8 {
        wire_tag(&bincode::serde::encode_to_vec(msg, bincode::config::standard()).unwrap())
    }

    fn client_tag(msg: &ClientMessage) -> u8 {
        wire_tag(&bincode::serde::encode_to_vec(msg, bincode::config::standard()).unwrap())
    }

    fn sample_media_open() -> ServerMessage {
        ServerMessage::MediaOpen {
            stream_id: 7,
            pane_id: "w1:p3".to_string(),
            codec: codec::OPUS.to_string(),
            params: MediaParams::Audio {
                sample_rate_hz: 48_000,
                channels: 2,
            },
            target_latency_us: 120_000,
        }
    }

    // TP-MEDIA-WIRE-01
    #[test]
    fn media_variants_are_appended_after_every_existing_one() {
        // The tags below are not a wish list: they are what the wire already
        // says. Adding a variant anywhere but the end changes one of these,
        // and this test is the only thing that notices before a released
        // client decodes a Graphics frame as something else.
        assert_eq!(
            server_tag(&ServerMessage::ReloadSoundConfig),
            8,
            "an existing ServerMessage tag moved"
        );
        assert_eq!(
            server_tag(&ServerMessage::GraphicsTransmissionRetired {
                transfer_id: 1,
                image_id: 2,
            }),
            15,
            "GraphicsTransmissionRetired was the last variant before media"
        );
        assert_eq!(server_tag(&sample_media_open()), 16);
        assert_eq!(
            server_tag(&ServerMessage::MediaChunk {
                stream_id: 7,
                seq: 1,
                pts_us: 2,
                data: vec![9],
            }),
            17
        );
        assert_eq!(
            server_tag(&ServerMessage::MediaClose {
                stream_id: 7,
                reason: MediaCloseReason::Ended,
                detail: String::new(),
            }),
            18
        );
        assert_eq!(
            server_tag(&ServerMessage::TimeSyncReply {
                client_send_us: 1,
                server_recv_us: 2,
                server_send_us: 3,
            }),
            19
        );

        assert_eq!(
            client_tag(&ClientMessage::Detach),
            4,
            "an existing ClientMessage tag moved"
        );
        assert_eq!(
            client_tag(&ClientMessage::GraphicsTransmissionStarted {
                transfer_id: 1,
                image_id: 2,
            }),
            12,
            "GraphicsTransmissionStarted was the last variant before media"
        );
        assert_eq!(
            client_tag(&ClientMessage::MediaCredit {
                stream_id: 7,
                chunks: 4,
            }),
            13
        );
        assert_eq!(
            client_tag(&ClientMessage::TimeSync { client_send_us: 1 }),
            14
        );
    }

    // TP-MEDIA-WIRE-01
    #[test]
    fn media_messages_survive_the_wire_field_for_field() {
        let open = sample_media_open();
        let encoded = bincode::serde::encode_to_vec(&open, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        match decoded {
            ServerMessage::MediaOpen {
                stream_id,
                pane_id,
                codec,
                params,
                target_latency_us,
            } => {
                assert_eq!(stream_id, 7);
                assert_eq!(pane_id, "w1:p3");
                assert_eq!(codec, codec::OPUS);
                assert_eq!(
                    params,
                    MediaParams::Audio {
                        sample_rate_hz: 48_000,
                        channels: 2,
                    }
                );
                assert_eq!(target_latency_us, 120_000);
            }
            other => panic!("expected MediaOpen, got {other:?}"),
        }

        for msg in [
            ServerMessage::MediaChunk {
                stream_id: 7,
                seq: u64::MAX,
                pts_us: 1_234_567,
                data: vec![1, 2, 3],
            },
            ServerMessage::MediaClose {
                stream_id: 7,
                reason: MediaCloseReason::Failed,
                detail: "encoder gone".to_string(),
            },
            ServerMessage::TimeSyncReply {
                client_send_us: 10,
                server_recv_us: 20,
                server_send_us: 30,
            },
        ] {
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ServerMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            assert_eq!(decoded, msg);
        }

        for msg in [
            ClientMessage::MediaCredit {
                stream_id: 7,
                chunks: 3,
            },
            ClientMessage::TimeSync { client_send_us: 99 },
        ] {
            let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
            let (decoded, _): (ClientMessage, _) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    // TP-MEDIA-WIRE-01
    #[test]
    fn an_audio_chunk_larger_than_a_control_frame_is_refused_not_decoded() {
        // A 20 ms Opus packet is roughly 160 bytes. A megabyte-sized "audio
        // chunk" is a malformed or hostile peer, and the reader must answer
        // with a framing error rather than allocating whatever the header
        // claims. MAX_FRAME_SIZE, not MAX_GRAPHICS_FRAME_SIZE, is the ceiling
        // that applies here: media is not a graphics frame.
        let msg = ServerMessage::MediaChunk {
            stream_id: 1,
            seq: 0,
            pts_us: 0,
            data: vec![0u8; MAX_FRAME_SIZE + 1],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();

        let err = read_message::<_, ServerMessage>(&mut buf.as_slice(), MAX_FRAME_SIZE)
            .expect_err("an oversized media chunk must not decode");
        assert!(
            matches!(err, FramingError::Oversized { .. }),
            "expected Oversized, got {err:?}"
        );
    }

    // TP-MEDIA-WIRE-01
    #[test]
    fn an_unknown_codec_name_decodes_instead_of_killing_the_message() {
        // The codec travels as a name for the same reason capabilities do: a
        // bincode enum cannot decode a discriminant it has never seen, and the
        // whole message dies with it — a connection-level failure for what
        // should be one refusable stream. With a name, an old client decodes
        // the open, does not recognise the codec, and can decline the stream.
        let msg = ServerMessage::MediaOpen {
            stream_id: 1,
            pane_id: "w1:p1".to_string(),
            codec: "a-codec-from-2030".to_string(),
            params: MediaParams::Audio {
                sample_rate_hz: 48_000,
                channels: 2,
            },
            target_latency_us: 0,
        };
        let encoded = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let (decoded, _): (ServerMessage, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("an unknown codec name must still decode");
        match decoded {
            ServerMessage::MediaOpen { codec, .. } => assert_eq!(codec, "a-codec-from-2030"),
            other => panic!("expected MediaOpen, got {other:?}"),
        }
    }
}
