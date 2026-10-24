//! The tailnet device picker behind a bar section's `devices` action.
//!
//! The Taildrop picker beside it (`file_manager::open_tailscale_send`) is the
//! model, and deliberately so: one box, one geometry, one set of keys, so a
//! person who has learned one has learned the other. The tailnet is read once,
//! when the picker opens, and nothing polls while it is up — a list nobody is
//! looking at costs nothing.
//!
//! What differs is the verb. Enter here does not send a file; it attaches to
//! the highlighted machine's own herdr, in a new tab running
//! `herdr --remote <machine>`. So a row that cannot host a server — offline,
//! Windows, iOS — is shown (it still answers "where is my laptop?") but refuses
//! the jump on the status line rather than opening a tab that can only fail.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::state::{AppState, DevicePickerState};
use crate::app::{App, Mode};
use crate::tailscale::TailscaleDevice;

/// What one key did, for the caller that owns the things pure state cannot
/// do: open a tab, or write the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevicePickerKeyOutcome {
    /// Consumed, and nothing outside the picker has to happen.
    Nothing,
    /// The picker closed on this machine: attach to it.
    Jump(TailscaleDevice),
    /// The pin list changed: persist it.
    Pinned(Vec<String>),
}

/// Open the picker over whatever is on screen, reading the tailnet once.
pub(crate) fn open_device_picker(state: &mut AppState) -> bool {
    open_device_picker_with(state, crate::tailscale::load_devices())
}

/// The half of [`open_device_picker`] that never touches the tailnet.
///
/// Split for the same reason the Taildrop picker is: a test that ran
/// `tailscale` would pass everywhere except where it matters.
pub(crate) fn open_device_picker_with(
    state: &mut AppState,
    loaded: Result<Vec<TailscaleDevice>, String>,
) -> bool {
    // A second press while the picker is up would record the picker itself as
    // the mode to return to, and Esc would then close it onto itself.
    if state.mode == Mode::DevicePicker {
        return false;
    }
    let pinned = state.tailscale_pinned_devices.clone();
    let loaded = loaded.map(|mut devices| {
        crate::tailscale::sort_devices(&mut devices, &pinned);
        devices
    });
    let (devices, status) = match loaded {
        Ok(devices) if devices.is_empty() => (
            Vec::new(),
            // An empty tailnet and a failed read look the same on screen
            // unless one of them says so.
            Some("  no other devices on this tailnet".to_owned()),
        ),
        Ok(devices) => (devices, None),
        Err(reason) => (Vec::new(), Some(format!("  {reason}"))),
    };
    state.device_picker = Some(DevicePickerState {
        devices,
        selected: 0,
        status,
    });
    state.enter_overlay_mode(Mode::DevicePicker);
    true
}

/// Close the picker and hand the keyboard back to whoever had it.
pub(crate) fn close_device_picker(state: &mut AppState) -> bool {
    if state.device_picker.take().is_none() {
        return false;
    }
    super::leave_modal(state);
    true
}

/// Why this machine cannot be attached to right now, or `None` when it can.
///
/// A sentence rather than a hidden row: a machine missing from the list reads
/// as "not on the tailnet", which is a different and worse answer than
/// "asleep" or "the wrong kind of machine".
pub(crate) fn jump_refusal(device: &TailscaleDevice) -> Option<String> {
    if !device.online {
        return Some(format!("  {} is offline", device.label));
    }
    let os = device.os.trim();
    if ["windows", "ios", "ipados", "android", "tvos"]
        .iter()
        .any(|cannot| os.eq_ignore_ascii_case(cannot))
    {
        return Some(format!(
            "  {} runs {os}, which cannot host a herdr server",
            device.label
        ));
    }
    None
}

/// The name the remote tab wears: an arrow and the machine, so the tab strip
/// says where the pane went without the reader opening it.
pub(crate) fn remote_tab_name(device: &TailscaleDevice) -> String {
    format!("↗ {}", device.label)
}

/// The command the remote tab runs: this very binary, told to attach over
/// there. `program` is the caller's business — the running executable when it
/// can be named, `herdr` from the path when it cannot.
pub(crate) fn remote_tab_argv(program: &str, device: &TailscaleDevice) -> Vec<String> {
    vec![
        program.to_owned(),
        "--remote".to_owned(),
        device.target.clone(),
    ]
}

/// Handle one key while the picker is open.
///
/// Everything unrecognised is swallowed: this is a blocking overlay, and a key
/// that fell through would act on a surface the reader cannot see.
pub(crate) fn handle_device_picker_key(
    state: &mut AppState,
    key: KeyEvent,
) -> DevicePickerKeyOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q'), KeyModifiers::NONE) => {
            let _ = close_device_picker(state);
        }
        (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
            if let Some(picker) = state.device_picker.as_mut() {
                let _ = picker.move_selection(true);
            }
        }
        (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
            if let Some(picker) = state.device_picker.as_mut() {
                let _ = picker.move_selection(false);
            }
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if let Some(device) = jump_to_selected_device(state) {
                return DevicePickerKeyOutcome::Jump(device);
            }
        }
        (KeyCode::Char('p'), KeyModifiers::NONE) => {
            if let Some(pinned) = toggle_pin_on_selected_device(state) {
                return DevicePickerKeyOutcome::Pinned(pinned);
            }
        }
        _ => {}
    }
    DevicePickerKeyOutcome::Nothing
}

/// Take the highlighted machine as the place to go and close the picker — or
/// say on the status line why this row cannot be attached to, and stay open.
///
/// Closing on success is the opposite of what the Taildrop picker does, and
/// on purpose: a send leaves the reader waiting for an outcome the picker
/// reports, whereas a jump's outcome is the tab that opens, which the picker
/// would only cover.
pub(crate) fn jump_to_selected_device(state: &mut AppState) -> Option<TailscaleDevice> {
    let picker = state.device_picker.as_mut()?;
    let device = picker.selected_device()?.clone();
    if let Some(reason) = jump_refusal(&device) {
        picker.status = Some(reason);
        return None;
    }
    let _ = close_device_picker(state);
    Some(device)
}

/// Pin or unpin the highlighted machine and follow it to its new row — the
/// Taildrop picker's rule, kept identical so the two lists agree on what `p`
/// does. The pin list is shared with that picker on purpose: a machine
/// somebody keeps sending files to is one they keep attaching to.
pub(crate) fn toggle_pin_on_selected_device(state: &mut AppState) -> Option<Vec<String>> {
    let picker = state.device_picker.as_mut()?;
    let device = picker.selected_device()?.clone();
    let pinned = crate::tailscale::toggle_pin(&state.tailscale_pinned_devices, &device.target);
    crate::tailscale::sort_devices(&mut picker.devices, &pinned);
    picker.selected = picker
        .devices
        .iter()
        .position(|candidate| candidate.target == device.target)
        .unwrap_or(0);
    state.tailscale_pinned_devices = pinned.clone();
    Some(pinned)
}

impl App {
    /// Route one key while the picker owns the keyboard.
    ///
    /// One body for both roads — the monolithic loop and the headless server
    /// call this and nothing else — so the two cannot drift apart on what
    /// Enter or `p` does.
    pub(crate) fn route_device_picker_key(&mut self, key: KeyEvent) {
        match handle_device_picker_key(&mut self.state, key) {
            DevicePickerKeyOutcome::Jump(device) => self.open_remote_tab(&device),
            DevicePickerKeyOutcome::Pinned(pinned) => self.save_tailscale_pinned_devices(&pinned),
            DevicePickerKeyOutcome::Nothing => {}
        }
    }

    /// A row click already moved the highlight (mouse road); attach to it, as
    /// Enter would. Selecting on one click and attaching on a second would
    /// make the two paths disagree.
    pub(crate) fn activate_device_picker_selection(&mut self) {
        if let Some(device) = jump_to_selected_device(&mut self.state) {
            self.open_remote_tab(&device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(label: &str, os: &str, online: bool) -> TailscaleDevice {
        TailscaleDevice {
            label: label.to_owned(),
            target: format!("{label}.ts.net"),
            os: os.to_owned(),
            online,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn open(state: &mut AppState, devices: Vec<TailscaleDevice>) {
        assert!(open_device_picker_with(state, Ok(devices)));
    }

    // TP-JUMP-03: the picker opens on the tailnet as it is at the press —
    // pinned machines first — and an empty or unreadable tailnet says which
    // it is instead of showing a blank box. A second press does not stack a
    // second picker on the first.
    #[test]
    fn the_picker_opens_pinned_first_and_says_why_when_there_is_nothing_to_pick() {
        let mut state = AppState::test_new();
        state.tailscale_pinned_devices = vec!["dell.ts.net".to_owned()];
        open(
            &mut state,
            vec![device("air", "macOS", true), device("dell", "linux", true)],
        );
        assert_eq!(state.mode, Mode::DevicePicker);
        let picker = state.device_picker.as_ref().expect("picker state");
        assert_eq!(
            picker.devices[0].label, "dell",
            "the pinned machine leads, whatever the alphabet says"
        );
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.status, None, "nothing has gone wrong yet");
        assert!(
            !open_device_picker_with(&mut state, Ok(Vec::new())),
            "a press while the picker is up must not open a second one"
        );

        let mut empty = AppState::test_new();
        open(&mut empty, Vec::new());
        assert_eq!(
            empty
                .device_picker
                .as_ref()
                .and_then(|picker| picker.status.as_deref()),
            Some("  no other devices on this tailnet")
        );

        let mut failed = AppState::test_new();
        assert!(open_device_picker_with(
            &mut failed,
            Err("tailscale is not running".to_owned())
        ));
        assert_eq!(
            failed
                .device_picker
                .as_ref()
                .and_then(|picker| picker.status.as_deref()),
            Some("  tailscale is not running"),
            "a failed read names its reason rather than looking like an empty tailnet"
        );
    }

    // TP-JUMP-04: Enter on a machine that can host a herdr hands the jump to
    // the caller and closes the picker — the tab that opens is the outcome,
    // and a picker left over it would only cover what it produced.
    #[test]
    fn enter_on_a_hosting_machine_hands_over_the_jump_and_closes_the_picker() {
        let mut state = AppState::test_new();
        let before = state.mode;
        open(&mut state, vec![device("lab", "linux", true)]);
        let outcome = handle_device_picker_key(&mut state, key(KeyCode::Enter));
        assert_eq!(
            outcome,
            DevicePickerKeyOutcome::Jump(device("lab", "linux", true))
        );
        assert!(
            state.device_picker.is_none(),
            "the picker closes on the jump"
        );
        assert_eq!(
            state.mode, before,
            "the keyboard goes back to whoever had it"
        );
    }

    // TP-JUMP-04: what the remote tab is — this binary attaching over there,
    // and a name that says where the pane went.
    #[test]
    fn a_remote_tab_runs_herdr_remote_for_the_machine_and_wears_its_name() {
        let lab = device("lab", "linux", true);
        assert_eq!(
            remote_tab_argv("/usr/bin/herdr", &lab),
            vec!["/usr/bin/herdr", "--remote", "lab.ts.net"]
        );
        assert_eq!(remote_tab_name(&lab), "↗ lab");
    }

    // TP-JUMP-05: a machine that is offline, or one that cannot host a herdr
    // server, refuses the jump on the status line and keeps the picker open —
    // a tab that can only fail is worse than a sentence saying why.
    #[test]
    fn a_machine_that_cannot_be_attached_to_refuses_on_the_status_line_and_stays_open() {
        let cases = [
            (device("dell", "linux", false), "  dell is offline"),
            (
                device("win", "windows", true),
                "  win runs windows, which cannot host a herdr server",
            ),
            (
                device("phone", "iOS", true),
                "  phone runs iOS, which cannot host a herdr server",
            ),
        ];
        for (machine, expected) in cases {
            let mut state = AppState::test_new();
            open(&mut state, vec![machine.clone()]);
            let outcome = handle_device_picker_key(&mut state, key(KeyCode::Enter));
            assert_eq!(
                outcome,
                DevicePickerKeyOutcome::Nothing,
                "{} must not be jumped to",
                machine.label
            );
            let picker = state
                .device_picker
                .as_ref()
                .expect("the picker stays open to say why");
            assert_eq!(picker.status.as_deref(), Some(expected));
            assert_eq!(state.mode, Mode::DevicePicker);
        }
    }

    // TP-JUMP-06: Esc and q close the picker and hand the keyboard back; a key
    // the picker does not know is swallowed, never passed through.
    #[test]
    fn esc_closes_the_picker_and_hands_the_keyboard_back() {
        let mut state = AppState::test_new();
        let before = state.mode;
        open(&mut state, vec![device("lab", "linux", true)]);
        assert_eq!(
            handle_device_picker_key(&mut state, key(KeyCode::Esc)),
            DevicePickerKeyOutcome::Nothing
        );
        assert!(state.device_picker.is_none());
        assert_eq!(state.mode, before);

        open(&mut state, vec![device("lab", "linux", true)]);
        handle_device_picker_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(
            state.mode,
            Mode::DevicePicker,
            "an unknown key is consumed, not a close"
        );
        handle_device_picker_key(&mut state, key(KeyCode::Char('q')));
        assert_eq!(state.mode, before);
    }

    // TP-JUMP-06: p pins the highlighted machine into the list the Taildrop
    // picker also reads, and the highlight follows the machine to the top —
    // Enter is one keystroke away, and it must still land on the row the
    // reader just acted on.
    #[test]
    fn p_pins_the_highlighted_machine_shares_the_list_with_taildrop_and_follows_it() {
        let mut state = AppState::test_new();
        open(
            &mut state,
            vec![device("air", "macOS", true), device("lab", "linux", true)],
        );
        handle_device_picker_key(&mut state, key(KeyCode::Down));
        let outcome = handle_device_picker_key(&mut state, key(KeyCode::Char('p')));
        assert_eq!(
            outcome,
            DevicePickerKeyOutcome::Pinned(vec!["lab.ts.net".to_owned()])
        );
        let picker = state.device_picker.as_ref().expect("picker");
        assert_eq!(
            picker.devices[0].label, "lab",
            "a pinned machine moves to the top"
        );
        assert_eq!(picker.selected, 0, "and the highlight follows it");
        assert_eq!(
            state.tailscale_pinned_devices,
            vec!["lab.ts.net".to_owned()],
            "the same list the Taildrop picker reads"
        );
    }
}
