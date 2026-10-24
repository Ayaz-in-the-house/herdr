"""Scheduled work that reads a display's state must run in that display's view.

Per-display surfaces are registers: outside a display's window they hold the
session's own view, which once a second display attaches is nobody's browser.
A scheduled task that reads one from outside the loop therefore takes a
request, matches nothing, and drops the action without a word — on every
display, not just the extra ones.

That failure is invisible in the suite because each half is individually
correct: the field is per-display, the consumer compiles, every unit test
passes. Only the pairing is wrong. This test pins the pairing.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"

# Consumers that are deliberately outside the loop, with the reason. Adding a
# name here is a claim that the function reads no per-display surface.
OUTSIDE_LOOP_ALLOWED = {
    # One channel, one operation in flight: draining per display would let the
    # first display served eat progress meant for the session.
    "sync_file_operation_worker",
}


def rust_sources():
    return {p: p.read_text(encoding="utf-8") for p in SRC.rglob("*.rs")}


def bundled_fields(state_src):
    """Field names declared inside client_surfaces! — the per-display registers."""
    start = state_src.index("client_surfaces! {")
    end = state_src.index("\n}\n", state_src.index("    ephemeral {", start))
    return set(re.findall(r"^\s{4}([a-z_][a-z0-9_]*)\s*:", state_src[start:end], re.M))


def function_body(sources, name):
    """Brace-matched body of the first `fn <name>` found."""
    for text in sources.values():
        match = re.search(r"fn " + re.escape(name) + r"\s*\([^)]*\)[^{]*\{", text)
        if not match:
            continue
        depth = 0
        start = match.end() - 1
        for index in range(start, len(text)):
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
                if depth == 0:
                    return text[start : index + 1]
        return text[start:]
    return None


def scheduled_calls(sources):
    """(outside, inside) scheduled task names around the per-display pass.

    The server's scheduler is the only one since U2b removed the in-process
    loop. Outside: its `self.app.sync_*` calls before the pass (the scope this
    test always had). Inside: what the pass itself drives.
    """
    body = function_body(sources, "handle_scheduled_tasks_headless")
    before = body.split("for_each_display(", 1)[0]
    outside = re.findall(r"self\.app\.(sync_\w+)\(", before)
    inside = re.findall(r"self\.(sync_\w+)\(", function_body(sources, "sync_per_display_surfaces"))
    return outside, inside


def reads_per_display_state(sources, name, fields):
    """Fields the function reads, following one level of its own helpers."""
    body = function_body(sources, name)
    if body is None:
        return set()
    helpers = set(re.findall(r"self\.(consume_\w+|start_\w+|open_\w+|apply_\w+)\(", body))
    text = body + "".join(function_body(sources, h) or "" for h in helpers)
    return {f for f in re.findall(r"state\.([a-z_][a-z0-9_]*)", text) if f in fields}


class PerDisplaySchedulingTest(unittest.TestCase):
    def test_outside_the_loop_reads_no_per_display_state(self):
        sources = rust_sources()
        fields = bundled_fields(sources[SRC / "app" / "state.rs"])
        self.assertTrue(fields, "no per-display fields found; the parser is stale")

        outside, _ = scheduled_calls(sources)
        self.assertTrue(outside, "no scheduled calls found before the pass; the parser is stale")
        offenders = {}
        for name in dict.fromkeys(outside):
            if name in OUTSIDE_LOOP_ALLOWED:
                continue
            hits = reads_per_display_state(sources, name, fields)
            if hits:
                offenders[name] = sorted(hits)

        self.assertEqual(
            offenders,
            {},
            "these scheduled tasks read a display's own state but run outside "
            "every display's view, so once a second display attaches they act "
            "on nobody's state and silently do nothing: "
            + "; ".join(f"{k} reads {', '.join(v)}" for k, v in offenders.items())
            + ". Move the call inside for_each_display, or split the part that "
            "reads per-display state out of it.",
        )

    def test_the_per_display_pass_runs_only_inside_the_loop(self):
        """One scheduler is left; what can drift is the pass leaving the loop."""
        sources = rust_sources()
        _, inside = scheduled_calls(sources)
        self.assertTrue(
            {"sync_file_manager_requests", "sync_file_manager_io_results"} <= set(inside),
            "the per-display pass no longer drives the file-manager consumers",
        )
        scheduler = "".join(function_body(sources, "handle_scheduled_tasks_headless").split())
        self.assertIn(
            "for_each_display(|app|app.sync_per_display_surfaces(now))",
            scheduler,
            "the server must run the per-display pass inside for_each_display",
        )


if __name__ == "__main__":
    unittest.main()
