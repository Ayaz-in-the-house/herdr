//! The tailnet picker a bar section's `devices` action opens.
//!
//! The same centred box as the Taildrop picker, drawn by the same geometry:
//! one `layout`, so a row here hit-tests exactly where a row there does, and
//! TP-FSEND-TS-26's offset rule holds for both without a second proof.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::state::AppState;

pub(crate) use super::tailscale_send::{
    device_row_at as device_picker_row_at, tailscale_send_popup_rect as device_picker_popup_rect,
};

/// The box's title. Static: the title names the list and the footer names the
/// verb, so neither is recomputed per frame.
const TITLE: &str = "Devices on the tailnet";

/// What one machine's row says: a pin mark, a presence mark, the name, the OS.
///
/// Two marks rather than the Taildrop picker's three — nothing here has "been
/// sent to" — and the OS is kept because it is the one column that tells the
/// reader in advance which rows will refuse the jump.
pub(crate) fn device_row(device: &crate::tailscale::TailscaleDevice, pinned: bool) -> String {
    let pin = if pinned { "*" } else { " " };
    let presence = if device.online { " " } else { "·" };
    if device.os.is_empty() {
        format!("{pin}{presence} {}", device.label)
    } else {
        format!("{pin}{presence} {}  ({})", device.label, device.os)
    }
}

pub(crate) fn render_device_picker(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(picker) = app.device_picker.as_ref() else {
        return;
    };
    let Some((popup, _)) = super::tailscale_send::layout(area, picker.devices.len()) else {
        return;
    };

    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(TITLE);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::with_capacity(picker.devices.len() + 3);
    lines.push(Line::from(""));

    if picker.devices.is_empty() {
        // An empty list still says something. The status carries the reason:
        // no other machines, or what went wrong reading the tailnet.
        lines.push(Line::from(Span::styled(
            picker
                .status
                .clone()
                .unwrap_or_else(|| "  no other devices on this tailnet".to_owned()),
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (index, device) in picker
            .devices
            .iter()
            .take(super::tailscale_send::MAX_ROWS as usize)
            .enumerate()
        {
            let mut style = Style::default();
            if !device.online {
                style = style.add_modifier(Modifier::DIM);
            }
            if index == picker.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let pinned = app
                .tailscale_pinned_devices
                .iter()
                .any(|target| target == &device.target);
            lines.push(Line::from(Span::styled(device_row(device, pinned), style)));
        }
    }

    lines.push(Line::from(""));
    let footer = if picker.devices.is_empty() {
        "  esc closes".to_owned()
    } else {
        picker
            .status
            .clone()
            .unwrap_or_else(|| "  enter attaches · p pins · esc closes".to_owned())
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().add_modifier(Modifier::DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::DevicePickerState;
    use crate::tailscale::TailscaleDevice;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    /// Draw the picker exactly as the overlay layer does, and hand back the
    /// cells: what reaches the screen, not what the state says.
    fn render(app: &AppState, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_device_picker(app, frame, app.view.terminal_area))
            .expect("render picker");
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn device(label: &str, os: &str, online: bool) -> TailscaleDevice {
        TailscaleDevice {
            label: label.to_owned(),
            target: format!("{label}.ts.net"),
            os: os.to_owned(),
            online,
        }
    }

    fn picker_app(devices: Vec<TailscaleDevice>) -> AppState {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 80, 24);
        let status = devices
            .is_empty()
            .then(|| "  no other devices on this tailnet".to_owned());
        app.device_picker = Some(DevicePickerState {
            devices,
            selected: 0,
            status,
        });
        app
    }

    /// The column at which `needle` starts on the drawn line, counted in
    /// cells rather than bytes — a `·` before it is one cell and two bytes.
    fn column_of(line: &str, needle: &str) -> Option<u16> {
        let cells: Vec<char> = line.chars().collect();
        let want: Vec<char> = needle.chars().collect();
        cells
            .windows(want.len())
            .position(|window| window == want.as_slice())
            .map(|column| column as u16)
    }

    // TP-JUMP-07: the picker names the list, draws one row per machine with
    // its OS, marks an offline machine in text and dims it, highlights the
    // selected row, and says what Enter does.
    #[test]
    fn the_picker_draws_the_tailnet_with_offline_machines_dimmed_and_says_what_enter_does() {
        let app = picker_app(vec![
            device("air", "macOS", true),
            device("dell", "linux", false),
        ]);
        let buffer = render(&app, 80, 24);
        let text = buffer_text(&buffer);
        assert!(text.contains("Devices on the tailnet"), "{text}");
        assert!(text.contains("air  (macOS)"), "{text}");
        assert!(
            text.contains("· dell  (linux)"),
            "an offline machine is marked in text, not colour alone: {text}"
        );
        assert!(
            text.contains("enter attaches · p pins · esc closes"),
            "{text}"
        );

        let (_, rows) = super::super::tailscale_send::layout(app.view.terminal_area, 2)
            .expect("the box fits an 80x24 screen");
        let selected = &buffer[(rows[0].x + 3, rows[0].y)];
        assert!(
            selected.modifier.contains(Modifier::REVERSED),
            "the highlight sits on the first row: {:?}",
            selected.modifier
        );
        let offline = &buffer[(rows[1].x + 3, rows[1].y)];
        assert!(
            offline.modifier.contains(Modifier::DIM),
            "the offline row is dimmed as well as marked: {:?}",
            offline.modifier
        );
    }

    // TP-JUMP-07: the hit-test answers with the machine that was drawn under
    // the pointer — the same geometry draws and resolves, so the two cannot
    // drift a row apart.
    #[test]
    fn a_row_click_lands_on_the_machine_it_was_drawn_on() {
        let app = picker_app(vec![
            device("air", "macOS", true),
            device("dell", "linux", false),
        ]);
        let buffer = render(&app, 80, 24);
        let area = app.view.terminal_area;
        let text = buffer_text(&buffer);
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("dell"))
            .expect("the dell row is drawn");
        let x = column_of(line, "dell").expect("dell's column");
        assert_eq!(device_picker_row_at(area, 2, x, y as u16), Some(1));
        assert_eq!(
            device_picker_row_at(area, 2, x, y as u16 + 5),
            None,
            "below the list is not a row"
        );
    }

    // TP-JUMP-07: an empty tailnet says so and offers only a way out — a
    // footer promising Enter over nothing would be a lie.
    #[test]
    fn an_empty_tailnet_says_so_and_offers_only_a_way_out() {
        let app = picker_app(Vec::new());
        let text = buffer_text(&render(&app, 80, 24));
        assert!(text.contains("no other devices on this tailnet"), "{text}");
        assert!(text.contains("esc closes"), "{text}");
        assert!(
            !text.contains("enter attaches"),
            "nothing to attach to: {text}"
        );
    }
}
