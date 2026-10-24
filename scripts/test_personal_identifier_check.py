"""Tests for the personal-identifier guard.

The guard's whole value is that it fails, so the cases below are written in
pairs: an identifier that must be refused and the neutral spelling of the same
thing that must not be. A guard that only ever passes and a guard that cries at
ordinary English are both useless, and the second is worse — people switch it
off and it stops protecting anything.
"""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.personal_identifier_check import (
    check,
    hash_word,
    load_denylist,
    scan_text,
    tracked_files,
)

# The identifiers below are assembled at run time rather than written out.
#
# Spelled literally they would be real matches in a tracked file, and this file
# is tracked, so the guard would flag its own tests — which it did, in the gate,
# on the first attempt. The obvious fix is to make the guard skip this path, but
# a file the guard cannot read is a blind spot, and a blind spot is a worse
# trade than the small indirection here. Each name still reads as what it is.
TAILNET = "100." + "70.0.1"
HOME_PATH = "/home/" + "someone"
HOME_SLUG = "-home-" + "someone"
MAIL = "someone" + "@gmail.com"

# A MagicDNS domain of the shape a real tailnet has: the identifier after
# `tail` carries digits. Documentation names ("tailnet.ts.net") do not, and the
# guard uses exactly that to tell one from the other — see the pair below.
MAGICDNS = "box." + "tail" + "9f2c1" + ".ts.net"
DOC_MAGICDNS = "box.tailnet.ts.net"

# A word that stands for a client's name. Never write a real one here: the
# denylist exists precisely so the repository does not have to carry them.
DENIED_WORD = "zephyrworks"


class ScanRefusesPrivateIdentifiers(unittest.TestCase):
    def test_a_tailnet_address_is_refused(self) -> None:
        found = scan_text(f"reach it at http://{TAILNET}:8770/")
        self.assertEqual(found, [f"tailnet address {TAILNET}"])

    def test_the_documentation_address_is_allowed(self) -> None:
        self.assertEqual(scan_text("http://192.0.2.1:8770 is an example"), [])

    def test_an_address_below_the_reserved_range_is_not_a_tailnet_address(self) -> None:
        # 100.63.x.x sits outside 192.0.2.2/10 and is ordinary public space.
        self.assertEqual(scan_text("100.63.1.1 is not shared space"), [])

    def test_a_named_home_directory_is_refused(self) -> None:
        self.assertEqual(
            scan_text(f"it lives in {HOME_PATH}/projects"),
            [f"home directory {HOME_PATH}"],
        )

    def test_a_neutral_home_directory_is_allowed(self) -> None:
        self.assertEqual(scan_text("it lives in /home/user/projects"), [])

    def test_the_encoded_slug_form_is_refused(self) -> None:
        self.assertEqual(
            scan_text(f"~/.claude/projects/{HOME_SLUG}/memory"),
            # The report names the slug without the dash the rule anchors on.
            [f"encoded home directory {HOME_SLUG[1:]}"],
        )

    def test_ordinary_prose_about_a_home_directory_is_allowed(self) -> None:
        # The slug rule is anchored on the leading dash for exactly this: an
        # unanchored `home-` matches English and the guard becomes noise.
        self.assertEqual(scan_text("the home directory and the home origin"), [])

    def test_a_personal_mail_address_is_refused(self) -> None:
        self.assertEqual(
            scan_text(f"reported by {MAIL} in the log"),
            [f"mail address {MAIL}"],
        )

    def test_upstream_and_project_addresses_are_allowed(self) -> None:
        text = "ogulcancelik@gmail.com, hey@herdr.dev, a@users.noreply.github.com"
        self.assertEqual(scan_text(text), [])

    def test_an_address_under_a_reserved_top_level_domain_is_allowed(self) -> None:
        # RFC 2606 reserves .example, .test and .invalid whole, not only their
        # second-level names: upstream's endpoint fixtures write
        # dev@build.example. A real domain that only resembles one is refused.
        text = "dev@build.example, ci@runner.test, x@nowhere.invalid"
        self.assertEqual(scan_text(text), [])
        lookalike = "me" + "@example.co"
        self.assertEqual(scan_text(lookalike), [f"mail address {lookalike}"])

    def test_one_offender_is_named_once_however_often_it_repeats(self) -> None:
        found = scan_text(f"{HOME_PATH} {HOME_PATH} {HOME_PATH}")
        self.assertEqual(found, [f"home directory {HOME_PATH}"])


class CheckWalksTheTrackedTree(unittest.TestCase):
    def _repo(self, root: Path) -> None:
        subprocess.run(["git", "-C", str(root), "init", "-q"], check=True)
        subprocess.run(
            ["git", "-C", str(root), "add", "-A"],
            check=True,
        )

    def test_a_tracked_file_with_an_identifier_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "notes.md").write_text(f"see {HOME_PATH}/x\n", encoding="utf-8")
            self._repo(root)
            errors = check(root)
            self.assertEqual(errors, [f"notes.md: home directory {HOME_PATH}"])

    def test_an_untracked_file_is_not_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kept.md").write_text("clean\n", encoding="utf-8")
            self._repo(root)
            # Written after `git add`, so git does not track it.
            (root / "scratch.md").write_text(f"{HOME_PATH}\n", encoding="utf-8")
            self.assertEqual(check(root), [])

    def test_the_vendor_subtree_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            vendored = root / "vendor" / "upstream"
            vendored.mkdir(parents=True)
            (vendored / "HACKING.md").write_text(f"{HOME_PATH}\n", encoding="utf-8")
            self._repo(root)
            self.assertEqual(check(root), [])


class ScanRefusesMagicDnsDomains(unittest.TestCase):
    """TP-1.5 / TP-1.6.

    The address rule already refuses tailnet *addresses*. It never saw the
    *domain*, and the domain is the more telling of the two: an address names
    one machine, the domain names the whole network and every machine on it.
    Nine days of a public repository proved the gap.
    """

    def test_a_magicdns_domain_is_refused(self) -> None:
        found = scan_text(f"ssh into {MAGICDNS} to read the log")
        self.assertEqual(found, [f"tailnet domain {MAGICDNS}"])

    def test_a_documentation_domain_is_allowed(self) -> None:
        # Tailscale's own docs spell the placeholder without digits. Requiring a
        # digit is what separates a real tailnet id from a stand-in, and it
        # costs nothing: a guard that refuses the documentation spelling makes
        # the documentation unwritable.
        self.assertEqual(scan_text(f"e.g. {DOC_MAGICDNS}"), [])

    def test_a_bare_ts_net_host_is_allowed(self) -> None:
        self.assertEqual(scan_text("login.ts.net is the coordination server"), [])

    def test_the_domain_is_refused_whatever_its_case(self) -> None:
        self.assertEqual(
            scan_text(MAGICDNS.upper()),
            [f"tailnet domain {MAGICDNS.upper()}"],
        )


class ScanRefusesDeniedWords(unittest.TestCase):
    """TP-1.1 / TP-1.3 / TP-1.4 / TP-1.7.

    Client and machine names have no shape a regular expression can find —
    `northwind` looks like any other word. The only way to refuse them is a
    list, and a list of them written into a public repository *is* the leak it
    is supposed to prevent. So the repository stores SHA-256 of each word and
    never the word: the check can recognise what it must not carry.
    """

    def setUp(self) -> None:
        self.denylist = frozenset({hash_word(DENIED_WORD)})

    def test_a_denied_word_is_refused(self) -> None:
        found = scan_text(f"the {DENIED_WORD} branch", denylist=self.denylist)
        self.assertEqual(found, [f'denied word "{DENIED_WORD}"'])

    def test_the_match_ignores_case(self) -> None:
        found = scan_text(DENIED_WORD.upper(), denylist=self.denylist)
        self.assertEqual(found, [f'denied word "{DENIED_WORD.upper()}"'])

    def test_a_denied_word_inside_a_branch_name_is_refused(self) -> None:
        # `feat/<client>-alpha` is exactly how the names reached the fixtures.
        found = scan_text(f'worktree_on_branch("a", "feat/{DENIED_WORD}-alpha")',
                          denylist=self.denylist)
        self.assertEqual(found, [f'denied word "{DENIED_WORD}"'])

    def test_a_denied_word_inside_a_path_is_refused(self) -> None:
        found = scan_text(f"~/projects/{DENIED_WORD}-api/task.py",
                          denylist=self.denylist)
        self.assertEqual(found, [f'denied word "{DENIED_WORD}"'])

    def test_a_word_that_merely_contains_the_denied_one_is_allowed(self) -> None:
        # Substring matching would refuse "acme" inside "acmeism" and turn the
        # guard into noise. Tokens are matched whole.
        found = scan_text(f"{DENIED_WORD}ism", denylist=self.denylist)
        self.assertEqual(found, [])

    def test_an_unlisted_word_is_allowed(self) -> None:
        self.assertEqual(scan_text("northwind ltd", denylist=self.denylist), [])

    def test_no_denylist_refuses_no_words(self) -> None:
        # A fresh clone, and every build box, has no local denylist. The guard
        # must still run — with the structural rules only — instead of failing
        # or silently passing everything.
        self.assertEqual(scan_text(f"the {DENIED_WORD} branch"), [])


class DenylistFileTests(unittest.TestCase):
    """TP-1.7 / TP-1.8."""

    def test_hash_word_is_lowercased_sha256(self) -> None:
        import hashlib

        self.assertEqual(
            hash_word("AcmeCorp"),
            hashlib.sha256(b"acmecorp").hexdigest(),
        )

    def test_a_missing_file_loads_an_empty_denylist(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(load_denylist(Path(tmp) / "absent.txt"), frozenset())

    def test_comments_and_blank_lines_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "denylist.txt"
            path.write_text(
                f"# a comment\n\n{hash_word('one')}\n  {hash_word('two')}  \n",
                encoding="utf-8",
            )
            self.assertEqual(
                load_denylist(path),
                frozenset({hash_word("one"), hash_word("two")}),
            )

    def test_a_corrupt_line_is_an_error(self) -> None:
        # A denylist that quietly drops the line it cannot parse is a guard
        # with a hole in it, and the hole is invisible. Refusing loudly is the
        # only honest option.
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "denylist.txt"
            path.write_text("not-a-sha256\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                load_denylist(path)

    def test_the_shipped_denylist_holds_only_hashes(self) -> None:
        # The whole point: reading this file must teach a reader nothing.
        shipped = Path(__file__).resolve().parents[1] / "scripts" / "identity_denylist.txt"
        if not shipped.exists():
            self.skipTest("no denylist is shipped in this checkout")
        for line in shipped.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            self.assertRegex(stripped, r"^[0-9a-f]{64}$", line)


class DeniedWordsInTheTree(unittest.TestCase):
    """TP-1.2 / TP-1.3 — the subtree contract, restated for the word rule.

    `vendor/` and the lockfiles were already skipped for the address rule. The
    word rule needs the same exemption for a sharper reason: a lockfile's
    base64 digests contain every letter sequence eventually, so scanning them
    for words produces matches that mean nothing at all.
    """

    def _repo(self, root: Path) -> None:
        subprocess.run(["git", "-C", str(root), "init", "-q"], check=True)
        subprocess.run(["git", "-C", str(root), "add", "-A"], check=True)

    def test_a_denied_word_in_a_tracked_file_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "notes.md").write_text(f"{DENIED_WORD}\n", encoding="utf-8")
            self._repo(root)
            errors = check(root, denylist=frozenset({hash_word(DENIED_WORD)}))
            self.assertEqual(errors, [f'notes.md: denied word "{DENIED_WORD}"'])

    def test_a_denied_word_under_vendor_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            vendored = root / "vendor" / "upstream"
            vendored.mkdir(parents=True)
            (vendored / "NOTES.md").write_text(f"{DENIED_WORD}\n", encoding="utf-8")
            self._repo(root)
            self.assertEqual(
                check(root, denylist=frozenset({hash_word(DENIED_WORD)})), []
            )

    def test_a_denied_word_in_a_lockfile_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            website = root / "website"
            website.mkdir()
            (website / "bun.lock").write_text(f"sha512-{DENIED_WORD}\n", encoding="utf-8")
            self._repo(root)
            self.assertEqual(
                check(root, denylist=frozenset({hash_word(DENIED_WORD)})), []
            )


class RealTreeTests(unittest.TestCase):
    def test_this_repository_names_nobody(self) -> None:
        # The one assertion that does the actual work. Everything above proves
        # the guard can tell a private identifier from a neutral one; this is
        # what makes a branch that reintroduces one fail the gate instead of
        # reaching a public remote.
        errors = check(Path(__file__).resolve().parents[1])
        self.assertEqual(errors, [], "\n".join(errors))

    def test_the_scan_actually_reads_the_tree(self) -> None:
        # TP-1.10 · negative control. "No violations" and "read nothing" produce
        # the same empty list, and this session watched a scan report a clean
        # tree twice while measuring an empty one. Proving the walk reaches real
        # files is the only thing that makes the assertion above mean anything.
        root = Path(__file__).resolve().parents[1]
        names = tracked_files(root)
        self.assertGreater(len(names), 500, "the tracked-file walk found almost nothing")
        self.assertIn("justfile", names)

    def test_the_scan_fits_in_its_time_budget(self) -> None:
        # TP-1.9 · `just check` already costs ~143 s. A guard that adds a minute
        # to it is a guard people start skipping, so the budget is part of the
        # contract rather than a hope.
        import time

        root = Path(__file__).resolve().parents[1]
        started = time.monotonic()
        check(root, denylist=frozenset({hash_word("zzzzzzzz")}))
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 15.0, f"the guard took {elapsed:.1f}s")


if __name__ == "__main__":
    unittest.main()
