use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::widgets::modal_stack_areas;
use super::widgets::panel_contrast_fg;
use super::widgets::render_action_button;
use super::widgets::render_modal_shell_or_notice;
use crate::app::AppState;

pub(crate) const ONBOARDING_TITLE: &str = "  herdr";
pub(crate) const ONBOARDING_SUBTITLE: &str = "  terminal workspace manager for coding agents";
pub(crate) const ONBOARDING_DESCRIPTION: [&str; 3] = [
    "  this is a mouse-first terminal.",
    "  click the sidebar to switch workspaces, drag pane",
    "  borders to resize, right-click for context menus.",
];
pub(crate) const ONBOARDING_PREFIX_LABEL: &str = "ctrl+b";
pub(crate) const ONBOARDING_PREFIX_SUFFIX: &str = " enters prefix mode · ";
pub(crate) const ONBOARDING_HELP_LABEL: &str = "?";
pub(crate) const ONBOARDING_HELP_SUFFIX: &str = " shows keybinds and settings";
pub(crate) const ONBOARDING_NEXT: &str =
    "  next: install optional agent integrations for more reliable state";

pub(super) fn render_onboarding_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    super::dim_background(frame, area);
    render_onboarding_welcome(app, frame, area);
}

pub(crate) fn onboarding_welcome_continue_rect(area: Rect) -> Rect {
    super::widgets::continue_button_rect(area)
}

fn render_onboarding_welcome(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(inner) =
        render_modal_shell_or_notice(frame, area, 64, 16, "welcome", (0, 11), &app.palette)
    else {
        return;
    };

    let stack = modal_stack_areas(inner, 2, 0, 1, 1);
    let header_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas::<2>(stack.header);
    let content_rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<4>(stack.content);

    frame.render_widget(
        Paragraph::new(ONBOARDING_TITLE).style(
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        header_rows[0],
    );
    frame.render_widget(
        Paragraph::new(ONBOARDING_SUBTITLE).style(Style::default().fg(app.palette.overlay0)),
        header_rows[1],
    );

    frame.render_widget(
        Paragraph::new(ONBOARDING_DESCRIPTION.join("\n"))
            .style(Style::default().fg(app.palette.overlay1)),
        content_rows[0],
    );

    let key_line = Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            ONBOARDING_PREFIX_LABEL,
            Style::default()
                .fg(app.palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            ONBOARDING_PREFIX_SUFFIX,
            Style::default().fg(app.palette.overlay1),
        ),
        Span::styled(
            ONBOARDING_HELP_LABEL,
            Style::default()
                .fg(app.palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            ONBOARDING_HELP_SUFFIX,
            Style::default().fg(app.palette.overlay1),
        ),
    ]);
    frame.render_widget(Paragraph::new(key_line), content_rows[2]);

    frame.render_widget(
        Paragraph::new(ONBOARDING_NEXT).style(Style::default().fg(app.palette.overlay1)),
        content_rows[3],
    );

    let continue_rect = onboarding_welcome_continue_rect(stack.actions.unwrap_or_default());
    render_action_button(
        frame,
        continue_rect,
        Some("↵"),
        "continue",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
}
