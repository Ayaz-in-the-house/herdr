"""The web-pane benchmark must not touch the machine it measures.

A benchmark that writes into `~/.config/herdr` measures a session it has
already changed, and the change outlives the run. A benchmark that leaves a
process behind adds to the very cost it was built to report. Both failures are
silent: the numbers still print, and they are wrong in a direction nobody
checks.

The third test here is the one that matters most and is easiest to forget. The
first two are negative — they say the run touched nothing. A benchmark that
measures *nothing at all* passes both of them. So there is a positive twin:
rendering a report from an empty run must fail loudly rather than print an
empty table under a green heading.
"""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from scripts.bench_web_pane import (
    EmptyRunError,
    ProcessSample,
    SpawnLedger,
    cpu_percent,
    isolated_config,
    process_role,
    render_report,
)


class IsolationTests(unittest.TestCase):
    """BH-1 / BH-3 — the run writes only inside its own directory."""

    def test_isolated_config_writes_the_provider_under_its_own_root(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = isolated_config(root, ["chromium-shim", "open"])
            self.assertTrue(path.is_file())
            self.assertTrue(
                path.is_relative_to(root),
                "the config escaped the run's own root",
            )
            text = path.read_text()
            self.assertIn("[web]", text)
            self.assertIn('"chromium-shim"', text)
            self.assertIn('"open"', text)

    def test_isolated_config_refuses_a_root_inside_the_live_config(self) -> None:
        # The guard is on the *shape* of the path, not on the machine's state,
        # so it holds on a machine where ~/.config/herdr does not exist yet.
        live = Path.home() / ".config" / "herdr"
        with self.assertRaises(ValueError):
            isolated_config(live, ["terminal-browser", "open"])
        with self.assertRaises(ValueError):
            isolated_config(live / "nested", ["terminal-browser", "open"])

    def test_isolated_config_refuses_an_empty_provider(self) -> None:
        with TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                isolated_config(Path(tmp), [])


class LedgerTests(unittest.TestCase):
    """BH-2 — everything the run spawned is named, so it can be reclaimed."""

    def test_the_ledger_reports_what_it_still_owns(self) -> None:
        ledger = SpawnLedger()
        ledger.record(111)
        ledger.record(222)
        self.assertEqual(ledger.outstanding(), [111, 222])
        ledger.release(111)
        self.assertEqual(ledger.outstanding(), [222])

    def test_the_ledger_never_owns_a_pid_it_did_not_spawn(self) -> None:
        ledger = SpawnLedger()
        with self.assertRaises(KeyError):
            ledger.release(999)

    def test_the_ledger_refuses_pid_one(self) -> None:
        # A ledger that can hold pid 1 is one bad substitution away from
        # terminating init. The cost of the guard is one comparison.
        ledger = SpawnLedger()
        with self.assertRaises(ValueError):
            ledger.record(1)


class AttributionTests(unittest.TestCase):
    """Roles come from `--type=` argv, never from the process name.

    Every chromium child is called `electron` in `ps`. Counting by name is how
    one main plus four children reads as five runtimes.
    """

    def test_chromium_children_are_told_apart_by_their_type_flag(self) -> None:
        self.assertEqual(process_role("/x/electron --type=renderer --y"), "renderer")
        self.assertEqual(process_role("/x/electron --type=gpu-process"), "gpu")
        self.assertEqual(process_role("/x/electron --type=utility --z"), "utility")
        self.assertEqual(process_role("/x/electron --type=zygote"), "zygote")

    def test_the_node_mode_cli_is_not_a_browser(self) -> None:
        self.assertEqual(
            process_role("/x/electron /x/cli/dist/main.js open about:blank"), "cli"
        )
        self.assertEqual(
            process_role("/x/electron /x/browser/dist/main.js"), "browser-main"
        )

    def test_an_unrecognised_argv_is_named_unknown_not_guessed(self) -> None:
        self.assertEqual(process_role("/usr/bin/something-else"), "unknown")


class CpuTests(unittest.TestCase):
    def test_cpu_percent_is_a_share_of_one_core_over_the_window(self) -> None:
        # 100 jiffies at 100 Hz over 1 s is one core fully busy.
        self.assertAlmostEqual(cpu_percent(jiffies=100, hz=100, window_s=1.0), 100.0)
        self.assertAlmostEqual(cpu_percent(jiffies=50, hz=100, window_s=1.0), 50.0)
        self.assertAlmostEqual(cpu_percent(jiffies=200, hz=100, window_s=2.0), 100.0)

    def test_a_zero_window_is_refused_rather_than_dividing(self) -> None:
        with self.assertRaises(ValueError):
            cpu_percent(jiffies=10, hz=100, window_s=0.0)


class ReportTests(unittest.TestCase):
    """BH-4 — the positive twin: a run that measured nothing must say so."""

    def test_an_empty_run_raises_instead_of_rendering_a_green_empty_table(self) -> None:
        with self.assertRaises(EmptyRunError):
            render_report(provider=["terminal-browser", "open"], samples=[], window_s=25.0)

    def test_a_report_names_the_provider_and_every_sample(self) -> None:
        report = render_report(
            provider=["terminal-browser", "open"],
            samples=[
                ProcessSample(pid=10, role="browser-main", rss_kb=230000, jiffies=1990),
                ProcessSample(pid=11, role="renderer", rss_kb=90000, jiffies=52),
            ],
            window_s=25.0,
        )
        self.assertIn("terminal-browser open", report)
        self.assertIn("browser-main", report)
        self.assertIn("renderer", report)
        # Totals are the line a reader acts on, so they must be present.
        self.assertIn("total", report.lower())

    def test_a_report_counts_renderers_separately_from_runtimes(self) -> None:
        # This is the number the whole exercise turns on: one runtime with
        # three renderers must never read as three runtimes.
        report = render_report(
            provider=["terminal-browser", "open"],
            samples=[
                ProcessSample(pid=10, role="browser-main", rss_kb=1, jiffies=0),
                ProcessSample(pid=11, role="renderer", rss_kb=1, jiffies=0),
                ProcessSample(pid=12, role="renderer", rss_kb=1, jiffies=0),
                ProcessSample(pid=13, role="renderer", rss_kb=1, jiffies=0),
            ],
            window_s=1.0,
        )
        self.assertIn("runtimes: 1", report)
        self.assertIn("renderers: 3", report)


if __name__ == "__main__":
    unittest.main()


class SelfExclusionTests(unittest.TestCase):
    """The instrument must not appear in its own measurement.

    A census that greps for the provider's install path matches the shell
    running the census — that shell's command line contains the very string it
    searches for. This has now been observed twice on this project, in two
    different tools, which makes it a class rather than an accident.
    """

    def test_the_measuring_process_is_excluded(self) -> None:
        from scripts.bench_web_pane import is_self_or_ancestor

        self.assertTrue(is_self_or_ancestor(42, own=42, ancestors=frozenset()))

    def test_an_ancestor_of_the_measuring_process_is_excluded(self) -> None:
        from scripts.bench_web_pane import is_self_or_ancestor

        self.assertTrue(is_self_or_ancestor(7, own=42, ancestors=frozenset({7, 9})))

    def test_an_unrelated_process_is_kept(self) -> None:
        from scripts.bench_web_pane import is_self_or_ancestor

        # The positive twin: an exclusion that excluded everything would pass
        # the two tests above and measure nothing at all.
        self.assertFalse(is_self_or_ancestor(1234, own=42, ancestors=frozenset({7, 9})))
