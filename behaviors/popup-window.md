# Popup window — moving, resizing and remembering the popup pane

Upstream's popup is a centred, fixed frame: it opens where the resolver puts it
and stays there until it closes. This fork makes it a window a person can take
hold of — the top border moves it, the corners and the other borders resize it
— and remembers where each popup was left for the rest of the session.
Everything here is ours to lose on a merge.

Design and the decisions behind it:
`.local/prd/2026-09-16-popup-window-ergonomics-and-fm-leftovers-PRD.md` (§3 K1–K6).

## Gestures on the frame

| ID | Behavior | What breaks if it is lost | Verified by |
|---|---|---|---|
| TP-POPUP-MOVE-01 | A plain left press on the top border takes hold of the frame; a drag carries it by the delta from the press, clamped inside the terminal area; the release commits the position. Every cell of the frame has exactly one meaning, the gesture belongs to the display that pressed, and a position that no longer fits is clamped rather than refused | Deltas taken from the previous motion drift when the terminal coalesces events; a gesture shared across displays would apply one display's drag to geometry the other never saw; a refused position would leave a moved popup unable to open on a smaller screen | `dragging_the_title_bar_moves_the_popup_and_the_release_commits_it`, `move_translates_by_the_press_delta_and_clamps_to_the_area`, `zones_cover_corners_title_edges_inside_and_outside`, `a_fitting_position_is_honoured`, `a_position_past_the_edge_is_clamped_inside_the_area`, `a_gesture_belongs_to_the_display_that_pressed`, `test_the_title_bar_drags_the_popup_a_corner_resizes_it_and_the_next_open_remembers` |
| TP-POPUP-MOVE-02 | Only a plain left press holds the frame — other buttons and modified presses are swallowed by the border as before; taking hold of the frame cancels a pending click-outside dismissal; a held frame lets nothing through to the program or to the dismissal; and a click that never moved commits nothing | A right or modified press that started a drag would fight the passthrough and selection gestures; a frame press that left the dismissal armed would close the popup on the very next click outside; a click committing a position would turn every centred popup into an absolutely placed one | `only_a_plain_left_press_holds_the_frame_and_holding_it_is_using_the_popup` |
| TP-POPUP-RESIZE-01 | The corners and the left, right and bottom borders resize: only the pressed edges move, the opposite edge stays put, the release commits the size in cells, and the program is told its new size once, at the release | A resize that moved the opposite edge would jump the frame under the pointer; resizing the PTY on every motion sends a full-screen program a storm of window-size signals it redraws on | `dragging_a_corner_resizes_the_popup_and_the_release_commits_cells`, `a_popup_pty_is_resized_once_when_the_frame_is_let_go`, `bottom_right_resize_keeps_the_origin_and_grows`, `top_left_resize_moves_the_origin_and_grows_toward_it` |
| TP-POPUP-RESIZE-02 | A resize never collapses the frame below the resolver's minimum (6×4) and never grows it past the terminal area; a resize commits the size as cells, so a percentage the caller asked for is replaced by what the person made of it | A frame below the minimum is refused by the resolver and vanishes; a percentage kept after a resize would snap the popup back to the caller's size on the next terminal resize | `resize_is_clamped_to_the_minimum_and_the_area`, `dragging_a_corner_resizes_the_popup_and_the_release_commits_cells` |
| TP-POPUP-FEEDBACK-01 | A held frame is drawn inverted and its title carries the position or size the drag has reached; let go, it is the plain frame again. Static — set by the press, cleared by the release, nothing ticks for it | Without it a drag looks like nothing until the release and a resize gives no number to aim for; an animated cue would cost every idle session for a gesture that happens once | `a_held_popup_frame_is_drawn_inverted_with_its_size_in_the_title` |

## Remembering where a popup was left

| ID | Behavior | What breaks if it is lost | Verified by |
|---|---|---|---|
| TP-POPUP-MEMORY-01 | The release records the frame the person made against the command that opened the popup (argv or shell string), and the next open of the same origin starts there — the person's last adjustment beats the caller's size; an origin never adjusted opens at the caller's size, centred; a popup that knows no origin records nothing | A popup that forgets its place is re-dragged every time it opens; a memory keyed on anything but the command would hand one tool's frame to another | `the_release_remembers_the_frame_for_the_popups_origin`, `the_latest_setting_of_an_origin_is_what_is_recalled`, `a_remembered_geometry_beats_the_callers_and_an_unknown_origin_opens_centred`, `test_the_title_bar_drags_the_popup_a_corner_resizes_it_and_the_next_open_remembers` |
| TP-POPUP-MEMORY-02 | The memory is in-session only and capped at thirty-two origins, oldest out; nothing about it reaches session.json | Persisting it would change the session format for a convenience; an uncapped ledger grows with every distinct command a long session ever opened | `the_oldest_origin_is_dropped_past_the_cap`, `the_release_remembers_the_frame_for_the_popups_origin` |
