"""Headful tests: the popup window, moved and resized with a real drag.

The Rust suite proves the gesture arithmetic, the state a release leaves and
the cells the renderer paints. It cannot prove that a terminal's SGR drag
sequences reach that code at all: the mouse path has several owners before
the popup, and TP-POPUP-MOVE-01 could be green in Rust and dead on screen. So
this drives the binary the way a person does — opens a popup from a bar
section, drags its title bar, drags its corner, closes it and opens it again
to find it where it was left.

Skipped, loudly, when there is no binary to drive. `HERDR_HEADFUL_BINARY`
points the harness at a candidate build before it is installed.
"""

from __future__ import annotations

import unittest

from .headful_harness import HeadfulSession, binary_available

#: `sleep` rather than `true`: a command that exits at once closes the popup
#: before the screen is read (measured 2026-08-18). Cells rather than percent
#: so the opened size is a number the assertions can name.
BAR_WITH_POPUP = """onboarding = false

[shell.bars.top]
enabled = true
size = 3
border = true

[[shell.bars.top.sections]]
kind = "content"
min = 9
max = 10
widget = { kind = "resource", metric = "cpu" }
action = { kind = "popup", argv = ["sleep", "60"], width = 40, height = 12 }
"""

TOP_LEFT = "┌╭"
TOP_RIGHT = "┐╮"
BOTTOM_LEFT = "└╰"


def frame_of(session: HeadfulSession) -> tuple[int, int, int, int] | None:
    """The popup frame as (top, left, bottom, right), found from its title.

    The title sits on the top border right after the corner, so the corner
    glyph followed by the title is the one place on screen that is the frame
    and nothing else — a toast that mentions the popup does not match.
    """
    found = session.find_regex(r"[┌╭] popup ")
    if found is None:
        return None
    top, left = found
    row = session.row(top)
    right = next((i for i in range(left + 1, session.cols) if row[i] in TOP_RIGHT), None)
    bottom = next(
        (r for r in range(top + 1, session.rows) if session.row(r)[left] in BOTTOM_LEFT),
        None,
    )
    if right is None or bottom is None:
        return None
    return top, left, bottom, right


@unittest.skipUnless(binary_available(), "no installed herdr to drive")
class HeadfulPopupWindowTests(unittest.TestCase):
    """The popup as a window the person can take hold of."""

    def test_the_title_bar_drags_the_popup_a_corner_resizes_it_and_the_next_open_remembers(
        self,
    ) -> None:
        """TP-POPUP-MOVE-01, TP-POPUP-RESIZE-01, TP-POPUP-MEMORY-01 on screen.

        One session rather than three because the third claim — the popup
        opens where it was left — is only true of a popup that was moved and
        resized in this same session by these same gestures.
        """
        with HeadfulSession(BAR_WITH_POPUP) as session:
            session.settle(18)
            found = session.find_regex(r"CPU", within_rows=5)
            self.assertIsNotNone(found, "no section to press")
            section_row, section_col = found
            session.click(section_row, section_col + 2)
            frame = frame_of(session)
            self.assertIsNotNone(
                frame,
                f"pressing the section drew no popup frame:\n{session.text()[:1500]}",
            )
            top, left, bottom, right = frame
            self.assertEqual(
                (right - left + 1, bottom - top + 1),
                (40, 12),
                "control: the popup opened at the size the section asked for",
            )

            # TP-POPUP-MOVE-01 · the title bar, dragged 3 rows down and 5 right.
            session.press(top, left + 10)
            session.drag_to(top + 1, left + 12)
            session.drag_to(top + 3, left + 15)
            session.release(top + 3, left + 15)
            moved = frame_of(session)
            self.assertEqual(
                moved,
                (top + 3, left + 5, bottom + 3, right + 5),
                f"the frame did not follow the drag:\n{session.text()[:1500]}",
            )
            top, left, bottom, right = moved

            # TP-POPUP-RESIZE-01 · the bottom-right corner, dragged 2 down and 4 right.
            session.press(bottom, right)
            session.drag_to(bottom + 1, right + 2)
            session.drag_to(bottom + 2, right + 4)
            session.release(bottom + 2, right + 4)
            resized = frame_of(session)
            self.assertEqual(
                resized,
                (top, left, bottom + 2, right + 4),
                f"the corner did not resize the frame:\n{session.text()[:1500]}",
            )

            # `sleep` ignores the Esc the first press outside delivers, so the
            # second press closes the popup (TP-CHROME-49/50).
            session.click(session.rows - 2, 1)
            session.click(session.rows - 2, 1)
            self.assertIsNone(
                frame_of(session), f"the popup did not close:\n{session.text()[:1500]}"
            )

            # TP-POPUP-MEMORY-01 · the same section opens the popup where it was left.
            session.click(section_row, section_col + 2)
            self.assertEqual(
                frame_of(session),
                resized,
                f"the popup did not reopen where it was left:\n{session.text()[:1500]}",
            )


if __name__ == "__main__":
    unittest.main()
