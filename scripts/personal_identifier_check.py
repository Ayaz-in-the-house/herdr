"""Fail the build if a tracked file names a private machine or account.

Why this exists
---------------
This fork is public. Its docs, cartography maps and test fixtures are written
on one developer's machine, and that machine's home directory, tailnet address
and mail account kept ending up in committed text — 240 occurrences across 29
files by the time anyone looked. None of them were needed: the docs are about
layout, the maps about structure, and the fixtures about string handling.

Cleaning them once is not enough. Every branch cut before the cleanup still
carries the old spellings, and a three-way merge takes a side silently when
only one side changed a region. That is exactly how `license_guard_check`
came to exist, and this guard is the same shape for the same reason: a
decision that has to survive merges needs a test that fails, not a habit.

What it refuses
---------------
- **Tailnet addresses.** Anything in the shared CGNAT range (RFC 6598,
  `192.0.2.2/10`) except the two documentation addresses below. Those
  addresses name real machines on a private network.
- **Home directories with a person's name in them.** `/home/<name>` and the
  dash-encoded project-slug form `home-<name>`, unless the name is on the
  neutral list.
- **Mail addresses**, unless they belong to upstream, to a code host's noreply
  domain, or to a reserved example domain.
- **Tailnet (MagicDNS) domains.** ``<host>.tail<id>.ts.net`` where the id
  carries a digit: an address names one machine, the domain names the whole
  network. Tailscale's documentation placeholder (``tailnet.ts.net``, no
  digit) and the bare coordination hosts (``login.ts.net``) stay allowed.
- **Denied words.** Client and machine names have no shape a pattern can
  find, so they come from a denylist — and a list of them in a public tree
  would be the leak itself. The tree carries only ``sha256(lowercased word)``
  (``scripts/identity_denylist.txt``), plus an optional local file at
  ``$XDG_CONFIG_HOME/herdr-fork/identity_denylist.txt`` that never leaves
  the machine (kept apart from the product's own config directory). Tokens are matched whole, case-insensitively; a fresh clone with
  no denylist still runs the structural rules above.

What it deliberately allows
---------------------------
Upstream's own published address stays: it appears in files upstream owns
(`website/src/components/AuthorCard.astro`, the vendored `.mailmap`) and
removing another project's attribution would be wrong as well as noisy.
`vendor/` is skipped entirely for the same reason — it is someone else's
tree, and rewriting it would break the patch index that `just check` verifies.

Run directly::

    python3 -m scripts.personal_identifier_check
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys
from pathlib import Path

# Subtrees this guard does not read. `vendor/` is upstream's own source and is
# verified by the patch index instead; lockfiles are machine-generated and
# their base64 digests produce matches that mean nothing.
SKIPPED_SUBTREES = (
    "vendor/",
    # Build output is never a place an identifier matters, and a build box whose
    # scratch index happens to hold it (measured 2026-09-16: cross-target `.d`
    # files naming the box's home) must not fail the gate for a tree that is
    # clean.
    "target/",
    "website/bun.lock",
    "tests/visual/package-lock.json",
)

# Home directory names that name a role rather than a person. Most are the
# throwaway names the docs and path-encoding tests already use; `linuxbrew` is
# a package manager's own account.
NEUTRAL_HOME_NAMES = frozenset(
    {
        "user",
        "runner",
        "herdr",
        "can",
        "me",
        "test",
        "tester",
        "phone-tester",
        "probe",
        "end",
        "tmp",
        "linuxbrew",
        "a",
        "b",
        "x",
        # `origin` reaches the slug rule through a visual fixture named
        # `...-files-locations-home-origin`, where "home origin" is the
        # scenario, not somebody's account.
        "origin",
    }
)

# Addresses that may appear because they are not ours to remove: the project's
# own contact addresses, upstream's maintainer, the FSF (quoted by the licence
# analysis), a code host's noreply domain and SSH user, reserved example
# domains, and the assistant-vendor noreply sink the commit gate refuses by
# name.
#
# That last one is the address this check exists to keep out of the tree, in
# the one place it has to appear: `scripts/conventional_commits.py` refuses a
# commit trailer that credits an assistant as an author, and a rule that names
# an address cannot be tested without writing it down. It belongs to nobody —
# it is a no-reply sink, the same class as the code host's above — so it is
# listed rather than smuggled past this check by splitting the string, which
# would defeat the check for everything else too.
ALLOWED_MAIL_PATTERNS = (
    re.compile(r"@users\.noreply\.github\.com$"),
    re.compile(r"^noreply@anthropic\.com$"),
    re.compile(r"@(example|invalid|test)\.(com|org|net|invalid)$"),
    # RFC 2606 reserves these top-level domains whole (upstream's endpoint
    # fixtures write `dev@build.example`).
    re.compile(r"@[A-Za-z0-9.-]+\.(example|invalid|test)$"),
    re.compile(r"@herdr\.dev$"),
    re.compile(r"@fsf\.org$"),
    re.compile(r"^git@github\.com$"),
    re.compile(r"^ogulcancelik@gmail\.com$"),
    re.compile(r"^m@mitchellh\.com$"),
)

# RFC 6598 reserves 192.0.2.2/10 for shared address space; tailnets live
# there. These two are the documentation addresses this repository may use.
ALLOWED_TAILNET_ADDRESSES = frozenset({"192.0.2.2", "192.0.2.1"})

TAILNET_RE = re.compile(r"\b100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.\d{1,3}\.\d{1,3}\b")
HOME_PATH_RE = re.compile(r"/home/([A-Za-z][A-Za-z0-9._-]*)")
# The slug form comes from encoding a path: `/home/x/projects/y` becomes
# `-home-x-projects-y`, so the segment is always preceded by its own dash. An
# unanchored `home-` would match ordinary prose — "home directory", "home
# origin" — and a guard that cries at English is a guard people switch off.
HOME_SLUG_RE = re.compile(r"-home-([A-Za-z][A-Za-z0-9._]*)")
MAIL_RE = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
# A MagicDNS name of the shape a real tailnet has: the label after `tail`
# carries a digit. The documentation placeholder (`tailnet.ts.net`) does not,
# which is what lets the docs stay writable while real networks are refused.
# Anchored on the literal `.tail` so the scan of a large file is a fast
# substring search; the host label in front is read back by hand.
MAGICDNS_RE = re.compile(r"\.tail[A-Za-z0-9]*[0-9][A-Za-z0-9]*\.ts\.net\b", re.IGNORECASE)
HOST_LABEL_CHARS = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-")
# The word rule matches whole tokens: `acme` inside `acmeism` is not a hit, a
# denied word inside a path or a branch name is.
WORD_RE = re.compile(r"[A-Za-z0-9]+")
SHA256_HEX_RE = re.compile(r"^[0-9a-f]{64}$")
SHIPPED_DENYLIST = Path("scripts") / "identity_denylist.txt"


def hash_word(word: str) -> str:
    """The denylist form of ``word``: SHA-256 of its lowercase spelling."""
    return hashlib.sha256(word.lower().encode("utf-8")).hexdigest()


def load_denylist(path: Path) -> frozenset[str]:
    """Hashes from a denylist file: one per line, comments and blanks ignored.

    A missing file is an empty denylist — a fresh clone and every build box
    have none. A line that is not a SHA-256 hex digest is an error, not a
    skipped line: a denylist that quietly drops what it cannot parse is a guard
    with an invisible hole in it.
    """
    if not path.exists():
        return frozenset()
    hashes: set[str] = set()
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if not SHA256_HEX_RE.match(stripped):
            raise ValueError(f"{path}:{number}: not a sha256 digest: {stripped!r}")
        hashes.add(stripped)
    return frozenset(hashes)


def local_denylist_path() -> Path:
    """The per-machine denylist, outside the tree so it never ships."""
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "herdr-fork" / "identity_denylist.txt"



def tracked_files(root: Path) -> list[str]:
    """Every path git tracks under ``root``, minus the skipped subtrees."""
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return [
        name
        for name in out.split("\0")
        if name and not name.startswith(SKIPPED_SUBTREES)
    ]


def scan_text(text: str, denylist: frozenset[str] = frozenset()) -> list[str]:
    """Return every identifier in ``text`` this guard refuses (empty == clean)."""
    found: list[str] = []

    for match in TAILNET_RE.finditer(text):
        if match.group(0) not in ALLOWED_TAILNET_ADDRESSES:
            found.append(f"tailnet address {match.group(0)}")

    for match in HOME_PATH_RE.finditer(text):
        if match.group(1) not in NEUTRAL_HOME_NAMES:
            found.append(f"home directory /home/{match.group(1)}")

    for match in HOME_SLUG_RE.finditer(text):
        if match.group(1) not in NEUTRAL_HOME_NAMES:
            found.append(f"encoded home directory home-{match.group(1)}")

    for match in MAIL_RE.finditer(text):
        address = match.group(0)
        if not any(pattern.search(address) for pattern in ALLOWED_MAIL_PATTERNS):
            found.append(f"mail address {address}")

    for match in MAGICDNS_RE.finditer(text):
        start = match.start()
        while start > 0 and text[start - 1] in HOST_LABEL_CHARS:
            start -= 1
        if start == match.start():
            continue  # `.tail…` with no host label in front is not a machine name
        found.append(f"tailnet domain {text[start:match.end()]}")

    if denylist:
        # Hash each distinct spelling once: a large file repeats its tokens
        # far more than it varies them, and the digest is the expensive part.
        judged: dict[str, bool] = {}
        for match in WORD_RE.finditer(text):
            token = match.group(0)
            key = token.lower()
            verdict = judged.get(key)
            if verdict is None:
                verdict = hash_word(key) in denylist
                judged[key] = verdict
            if verdict:
                found.append(f'denied word "{token}"')

    # One offender named once is enough; repeats in the same file add noise
    # without adding information.
    seen: list[str] = []
    for item in found:
        if item not in seen:
            seen.append(item)
    return seen


def check(root: Path, denylist: frozenset[str] = frozenset()) -> list[str]:
    """Return every violation across the tracked tree (empty == healthy)."""
    errors: list[str] = []
    for name in tracked_files(root):
        path = root / name
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue  # binary or unreadable: nothing to read an identifier out of
        for offender in scan_text(text, denylist):
            errors.append(f"{name}: {offender}")
    return errors


def main(argv: list[str] | None = None) -> int:
    root = Path(argv[0]) if argv else Path(__file__).resolve().parent.parent
    denylist = load_denylist(root / SHIPPED_DENYLIST) | load_denylist(local_denylist_path())
    errors = check(root, denylist)
    if errors:
        print(
            "error: a tracked file names a private machine or account",
            file=sys.stderr,
        )
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        print(
            "\nThis repository is public. Replace the identifier with a neutral "
            "one (/home/user, an RFC 6598 documentation address), or add it to "
            "the allow lists in scripts/personal_identifier_check.py if it "
            "belongs to someone else and must stay.",
            file=sys.stderr,
        )
        return 1
    print("personal identifiers: OK — no private machine or account named")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
