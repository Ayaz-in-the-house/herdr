use std::borrow::Cow;

use ratatui::layout::Rect;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupSize {
    Cells(u16),
    Percent(u8),
}

impl PopupSize {
    pub(crate) fn resolve(self, available: u16) -> u16 {
        match self {
            Self::Cells(cells) => cells,
            Self::Percent(percent) => ((available as u32 * percent as u32) / 100) as u16,
        }
    }

    pub(crate) fn parse_cli(value: &str) -> Result<Self, String> {
        if let Some(percent) = value.strip_suffix('%') {
            let percent = percent
                .parse::<u8>()
                .map_err(|_| "must be a number of cells or a percentage like 80%".to_string())?;
            if !(1..=100).contains(&percent) {
                return Err("percentage must be between 1% and 100%".to_string());
            }
            return Ok(Self::Percent(percent));
        }
        value
            .parse::<u16>()
            .map(Self::Cells)
            .map_err(|_| "must be a number of cells or a percentage like 80%".to_string())
    }

    fn parse_percent_string(value: &str) -> Result<Self, String> {
        if value.ends_with('%') {
            return Self::parse_cli(value);
        }
        Err("string sizes must be percentages like 80%; use a number for cells".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupResolvedGeometry {
    pub outer: Rect,
    pub inner: Rect,
}

/// Where a popup's outer frame sits when a person has put it somewhere:
/// the top-left cell, in screen coordinates. `None` on the popup means
/// "centred", which is where every popup starts (TP-POPUP-MOVE-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupPosition {
    pub x: u16,
    pub y: u16,
}

/// The one place a popup's frame is worked out from its size and position.
///
/// TP-CHROME-48: both ends of the range are reachable from a config file, so
/// a size past the screen is clamped and an area with no room is refused.
/// `None` for the position is centred, which is where every popup starts. A
/// `position` is honoured cell for cell while the frame fits and clamped to
/// the area's edges when it does not — a terminal that shrank never leaves
/// the popup half off-screen, and a position that asks for the top-left
/// corner gets exactly it (TP-POPUP-MOVE-01, TP-POPUP-RESIZE-02).
pub(crate) fn resolve_popup_geometry_at(
    width: Option<PopupSize>,
    height: Option<PopupSize>,
    position: Option<PopupPosition>,
    area: Rect,
) -> Option<PopupResolvedGeometry> {
    let default_width = area.width.saturating_div(2).max(6);
    let default_height = area.height.saturating_div(2).max(4);
    let outer_width = width
        .map(|width| width.resolve(area.width))
        .unwrap_or(default_width)
        .max(6)
        .min(area.width);
    let outer_height = height
        .map(|height| height.resolve(area.height))
        .unwrap_or(default_height)
        .max(4)
        .min(area.height);
    if outer_width < 6 || outer_height < 4 {
        return None;
    }

    // `outer_*` never exceeds the area, so the slack is a plain difference.
    let slack_x = area.width.saturating_sub(outer_width);
    let slack_y = area.height.saturating_sub(outer_height);
    let (outer_x, outer_y) = match position {
        None => (
            area.x.saturating_add(slack_x / 2),
            area.y.saturating_add(slack_y / 2),
        ),
        Some(position) => (
            position.x.max(area.x).min(area.x.saturating_add(slack_x)),
            position.y.max(area.y).min(area.y.saturating_add(slack_y)),
        ),
    };
    let pane_inner_width = outer_width.saturating_sub(2);
    let pane_inner_height = outer_height.saturating_sub(2);
    let terminal_cols = if pane_inner_width <= 4 {
        pane_inner_width
    } else {
        pane_inner_width.saturating_sub(1)
    };
    let inner = Rect::new(
        outer_x.saturating_add(1),
        outer_y.saturating_add(1),
        terminal_cols,
        pane_inner_height,
    );
    Some(PopupResolvedGeometry {
        outer: Rect::new(outer_x, outer_y, outer_width, outer_height),
        inner,
    })
}

/// The centred frame: `resolve_popup_geometry_at` with no position.
///
/// The client-rendered shell (opt-in, U2a) asks under upstream's name. Its
/// protocol carries a popup's size and not the position a person dragged it
/// to (TP-POPUP-MOVE-01), so both ends of a shell connection agree on the
/// centre. Size never depends on position, so the popup terminal's grid is the
/// same whichever client drew it.
pub(crate) fn resolve_popup_geometry(
    width: Option<PopupSize>,
    height: Option<PopupSize>,
    area: Rect,
) -> Option<PopupResolvedGeometry> {
    resolve_popup_geometry_at(width, height, None, area)
}

impl Serialize for PopupSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Cells(cells) => serializer.serialize_u16(*cells),
            Self::Percent(percent) => serializer.serialize_str(&format!("{percent}%")),
        }
    }
}

impl<'de> Deserialize<'de> for PopupSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PopupSizeVisitor;

        impl serde::de::Visitor<'_> for PopupSizeVisitor {
            type Value = PopupSize;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a cell count or percentage string like 80%")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let value =
                    u16::try_from(value).map_err(|_| E::custom("cell count must fit in u16"))?;
                Ok(PopupSize::Cells(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let value = u16::try_from(value)
                    .map_err(|_| E::custom("cell count must be between 0 and 65535"))?;
                Ok(PopupSize::Cells(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                PopupSize::parse_percent_string(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(PopupSizeVisitor)
    }
}

impl schemars::JsonSchema for PopupSize {
    fn schema_name() -> Cow<'static, str> {
        "PopupSize".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 65535,
                    "description": "Outer popup size in terminal cells, including the border."
                },
                {
                    "type": "string",
                    "pattern": "^(100|[1-9][0-9]?)%$",
                    "description": "Outer popup size as a percentage of the terminal area, for example 80%."
                }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_popup_geometry_at, PopupPosition, PopupResolvedGeometry, PopupSize};
    use ratatui::layout::Rect;

    /// The centred frame: what every popup opens with (TP-CHROME-48).
    fn centred(
        width: Option<PopupSize>,
        height: Option<PopupSize>,
        area: Rect,
    ) -> Option<PopupResolvedGeometry> {
        resolve_popup_geometry_at(width, height, None, area)
    }

    #[test]
    fn parses_cells_and_percent() {
        assert_eq!(PopupSize::parse_cli("120"), Ok(PopupSize::Cells(120)));
        assert_eq!(PopupSize::parse_cli("80%"), Ok(PopupSize::Percent(80)));
        assert_eq!(PopupSize::Percent(80).resolve(100), 80);
    }

    #[test]
    fn rejects_invalid_percent() {
        assert!(PopupSize::parse_cli("0%").is_err());
        assert!(PopupSize::parse_cli("101%").is_err());
        assert!(PopupSize::parse_cli("%").is_err());
    }

    #[test]
    fn string_deserialization_requires_percent() {
        assert!(serde_json::from_value::<PopupSize>(serde_json::json!("120")).is_err());
        assert_eq!(
            serde_json::from_value::<PopupSize>(serde_json::json!("80%")).unwrap(),
            PopupSize::Percent(80)
        );
    }

    #[test]
    fn serializes_percent_as_string() {
        assert_eq!(
            serde_json::to_value(PopupSize::Percent(80)).unwrap(),
            serde_json::json!("80%")
        );
        assert_eq!(
            serde_json::to_value(PopupSize::Cells(120)).unwrap(),
            serde_json::json!(120)
        );
    }

    #[test]
    fn resolves_requested_outer_size_and_inner_terminal_area() {
        let resolved = centred(
            Some(PopupSize::Percent(80)),
            Some(PopupSize::Percent(40)),
            ratatui::layout::Rect::new(0, 0, 100, 30),
        )
        .unwrap();
        assert_eq!(resolved.outer, ratatui::layout::Rect::new(10, 9, 80, 12));
        assert_eq!(resolved.inner, ratatui::layout::Rect::new(11, 10, 77, 10));
    }

    // TC-B4 · a size now reaches this resolver from a config file rather than
    // only from a keybind somebody typed, so the two ends of the range are
    // worth pinning: a number far larger than the screen must be brought back
    // inside it, and a screen too small for any popup must still be a refusal
    // rather than a rectangle with no room in it.
    #[test]
    fn an_absurd_size_is_clamped_and_a_hopeless_area_is_refused() {
        let resolved = centred(
            Some(PopupSize::Cells(60_000)),
            Some(PopupSize::Cells(60_000)),
            ratatui::layout::Rect::new(0, 0, 100, 30),
        )
        .expect("a size larger than the screen is clamped, not refused");
        assert_eq!(
            (resolved.outer.width, resolved.outer.height),
            (100, 30),
            "the popup must stay inside the area it is drawn in"
        );
        assert!(
            resolved.inner.width > 0 && resolved.inner.height > 0,
            "a clamped popup must still have somewhere to put its terminal"
        );

        for area in [
            ratatui::layout::Rect::new(0, 0, 5, 30),
            ratatui::layout::Rect::new(0, 0, 100, 3),
            ratatui::layout::Rect::new(0, 0, 0, 0),
        ] {
            assert!(
                centred(
                    Some(PopupSize::Percent(100)),
                    Some(PopupSize::Percent(100)),
                    area,
                )
                .is_none(),
                "an area with no room for a popup must be refused, not squeezed: {area:?}"
            );
        }
    }

    #[test]
    fn allows_full_terminal_outer_size() {
        let resolved = centred(
            Some(PopupSize::Percent(100)),
            Some(PopupSize::Percent(100)),
            ratatui::layout::Rect::new(4, 2, 100, 30),
        )
        .unwrap();

        assert_eq!(resolved.outer, ratatui::layout::Rect::new(4, 2, 100, 30));
        assert_eq!(resolved.inner, ratatui::layout::Rect::new(5, 3, 97, 28));
    }

    #[test]
    fn enforces_runtime_minimum_terminal_width() {
        let resolved = centred(
            Some(PopupSize::Cells(4)),
            None,
            ratatui::layout::Rect::new(0, 0, 80, 24),
        )
        .unwrap();
        assert_eq!(resolved.outer.width, 6);
        assert_eq!(resolved.inner.width, 4);

        assert!(centred(None, None, ratatui::layout::Rect::new(0, 0, 5, 24),).is_none());
    }

    // TB1.2 — a position that fits is honoured cell for cell.
    #[test]
    fn a_fitting_position_is_honoured() {
        let area = Rect::new(0, 0, 80, 24);
        let geometry = resolve_popup_geometry_at(
            Some(PopupSize::Cells(20)),
            Some(PopupSize::Cells(10)),
            Some(PopupPosition { x: 5, y: 3 }),
            area,
        )
        .expect("fits");
        assert_eq!(geometry.outer, Rect::new(5, 3, 20, 10));
        assert_eq!(geometry.inner, Rect::new(6, 4, 17, 8));
    }

    // TB1.3 — a position past the edge is clamped to it, never refused and
    // never allowed to hang off-screen; the area's own origin is respected.
    #[test]
    fn a_position_past_the_edge_is_clamped_inside_the_area() {
        let area = Rect::new(2, 1, 80, 24);
        let geometry = resolve_popup_geometry_at(
            Some(PopupSize::Cells(20)),
            Some(PopupSize::Cells(10)),
            Some(PopupPosition { x: 200, y: 200 }),
            area,
        )
        .expect("clamped, not refused");
        assert_eq!(geometry.outer, Rect::new(2 + 80 - 20, 1 + 24 - 10, 20, 10));
        let geometry = resolve_popup_geometry_at(
            Some(PopupSize::Cells(20)),
            Some(PopupSize::Cells(10)),
            Some(PopupPosition { x: 0, y: 0 }),
            area,
        )
        .expect("fits");
        assert_eq!(
            geometry.outer,
            Rect::new(2, 1, 20, 10),
            "below the area's origin snaps to it"
        );
    }

    // TB1.4 — the gesture works in cells; a percentage resolves once.
    #[test]
    fn a_percentage_resolves_to_cells_against_the_available_span() {
        assert_eq!(PopupSize::Percent(50).resolve(80), 40);
        assert_eq!(PopupSize::Cells(33).resolve(80), 33);
    }
}
