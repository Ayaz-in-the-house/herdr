"""The player's node tests are a gate only because a recipe runs them.

`just check` names its python modules by hand and a guard makes sure none is
forgotten. The node suite under `src/web/player/test/` has the same failure
shape with one runner fewer: a test file added without the `node --test` line
is green because it never ran. This is the guard for that line, written on the
addition side, and it also pins that every test file really uses `node:test`
— a file that imports nothing is a file the runner reports as passing with
zero tests.

TP-PLAYER-SUITE-01
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
JUSTFILE = REPO_ROOT / "justfile"
PLAYER_TESTS = REPO_ROOT / "src" / "web" / "player" / "test"
NODE_LINE = "node --test src/web/player/test/*.test.mjs"


def _recipe_body(name: str) -> str:
    text = JUSTFILE.read_text()
    start = text.index(f"\n{name}")
    rest = text[start + 1 :]
    end = len(rest)
    for match in re.finditer(r"\n(?=\S)", rest):
        if match.start() > 0:
            end = match.start()
            break
    return rest[:end]


class NodeSuiteWiringTests(unittest.TestCase):
    def test_check_and_test_recipes_run_the_node_suite(self) -> None:
        for recipe in ("check: ci windows-lint", "test:"):
            body = _recipe_body(recipe)
            self.assertIn(NODE_LINE, body, f"`{recipe}` does not run the node suite")

    def test_every_player_test_file_uses_the_node_runner(self) -> None:
        files = sorted(PLAYER_TESTS.glob("*.test.mjs"))
        self.assertGreaterEqual(len(files), 1, "no player test files; the recipe line guards nothing")
        for path in files:
            text = path.read_text(encoding="utf-8")
            self.assertIn("node:test", text, f"{path.name} does not use the node:test runner")
            self.assertRegex(text, r"\btest\(", f"{path.name} declares no test")


if __name__ == "__main__":
    unittest.main()
