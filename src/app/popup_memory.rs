//! What a person made of a popup, kept for the next time the same thing
//! opens (TP-POPUP-MEMORY-01/02). In-session only: nothing here reaches
//! session.json, so the persisted format is unchanged.

use crate::app::popup::PopupGeometry;
use crate::popup_size::{PopupPosition, PopupSize};

/// Who opened the popup, as the thing that will open it again: the same
/// command is the same popup, whichever button or key launched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PopupOrigin {
    Argv(Vec<String>),
    Shell(String),
}

/// A frame as a person left it: where it was and how big, in the popup's
/// own terms so the resolver can clamp it into whatever screen comes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupRememberedGeometry {
    pub position: Option<PopupPosition>,
    pub width: Option<PopupSize>,
    pub height: Option<PopupSize>,
}

/// How many origins are kept. A session that opens a thousand different
/// popups remembers the thirty-two most recently adjusted.
pub(crate) const POPUP_GEOMETRY_MEMORY_CAPACITY: usize = 32;

/// The ledger, newest last. A vector rather than a map because it is tiny,
/// it is walked once per popup open, and the oldest entry has to be
/// findable when the cap is reached.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PopupGeometryMemory {
    entries: Vec<(PopupOrigin, PopupRememberedGeometry)>,
}

impl PopupGeometryMemory {
    /// Records what the person made of `origin`, replacing what was known
    /// and dropping the oldest origin once the cap is exceeded.
    pub(crate) fn remember(&mut self, origin: PopupOrigin, geometry: PopupRememberedGeometry) {
        self.entries.retain(|(known, _)| *known != origin);
        self.entries.push((origin, geometry));
        if self.entries.len() > POPUP_GEOMETRY_MEMORY_CAPACITY {
            let excess = self.entries.len() - POPUP_GEOMETRY_MEMORY_CAPACITY;
            self.entries.drain(..excess);
        }
    }

    pub(crate) fn recall(&self, origin: &PopupOrigin) -> Option<PopupRememberedGeometry> {
        self.entries
            .iter()
            .find(|(known, _)| known == origin)
            .map(|(_, geometry)| *geometry)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The geometry a popup opens with. What the person last made of this
/// origin beats what the caller asked for; an origin never adjusted opens
/// at the caller's size, centred (TP-POPUP-MEMORY-01).
pub(crate) fn geometry_to_open(
    requested: PopupGeometry,
    remembered: Option<PopupRememberedGeometry>,
) -> PopupRememberedGeometry {
    remembered.unwrap_or(PopupRememberedGeometry {
        position: None,
        width: requested.width,
        height: requested.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(name: &str) -> PopupOrigin {
        PopupOrigin::Shell(name.to_string())
    }

    fn at(x: u16, y: u16, width: u16, height: u16) -> PopupRememberedGeometry {
        PopupRememberedGeometry {
            position: Some(PopupPosition { x, y }),
            width: Some(PopupSize::Cells(width)),
            height: Some(PopupSize::Cells(height)),
        }
    }

    // TB4.1 · an origin is remembered once, its latest setting wins, and a
    // different origin knows nothing about it.
    #[test]
    fn the_latest_setting_of_an_origin_is_what_is_recalled() {
        let mut memory = PopupGeometryMemory::default();
        assert_eq!(memory.recall(&shell("btop")), None);
        memory.remember(shell("btop"), at(1, 1, 40, 12));
        memory.remember(shell("btop"), at(5, 2, 60, 20));
        assert_eq!(memory.recall(&shell("btop")), Some(at(5, 2, 60, 20)));
        assert_eq!(
            memory.len(),
            1,
            "an origin is one entry, however often it is adjusted"
        );
        assert_eq!(memory.recall(&shell("htop")), None);
        assert_eq!(
            memory.recall(&PopupOrigin::Argv(vec!["btop".to_string()])),
            None,
            "an argv and a shell string are different origins even when they read alike"
        );
    }

    // TB4.1 · the cap drops the oldest origin, and adjusting an old origin
    // again makes it the newest.
    #[test]
    fn the_oldest_origin_is_dropped_past_the_cap() {
        let mut memory = PopupGeometryMemory::default();
        for i in 0..POPUP_GEOMETRY_MEMORY_CAPACITY {
            memory.remember(shell(&format!("cmd{i}")), at(i as u16, 0, 20, 10));
        }
        memory.remember(shell("cmd0"), at(9, 9, 20, 10));
        memory.remember(shell("one more"), at(0, 0, 20, 10));
        assert_eq!(memory.len(), POPUP_GEOMETRY_MEMORY_CAPACITY);
        assert_eq!(
            memory.recall(&shell("cmd1")),
            None,
            "cmd1 was the oldest once cmd0 was adjusted again"
        );
        assert_eq!(memory.recall(&shell("cmd0")), Some(at(9, 9, 20, 10)));
        assert_eq!(memory.recall(&shell("one more")), Some(at(0, 0, 20, 10)));
    }

    // TB4.3 · what the person made of a popup beats what its caller asks
    // for; a caller never adjusted opens at its own size, centred.
    #[test]
    fn a_remembered_geometry_beats_the_callers_and_an_unknown_origin_opens_centred() {
        let requested = PopupGeometry {
            width: Some(PopupSize::Percent(80)),
            height: Some(PopupSize::Percent(50)),
        };
        assert_eq!(
            geometry_to_open(requested, None),
            PopupRememberedGeometry {
                position: None,
                width: Some(PopupSize::Percent(80)),
                height: Some(PopupSize::Percent(50)),
            }
        );
        assert_eq!(
            geometry_to_open(requested, Some(at(3, 4, 50, 20))),
            at(3, 4, 50, 20)
        );
    }
}
