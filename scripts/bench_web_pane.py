"""Measure what one web pane costs, without changing the machine measuring it.

The numbers this produces decide whether herdr keeps `terminal-browser` as its
web-pane provider, so the harness has two obligations beyond arithmetic.

It must not write into the live session. A benchmark that edits
`~/.config/herdr` has already changed the thing it reports on, and the change
outlives the run. Every path it writes is therefore rooted under a directory
the caller owns, and roots that live under the real config are refused by shape
rather than by checking whether the file happens to exist today.

It must attribute processes by `--type=` argv and never by name. Every chromium
child — renderer, gpu, utility, zygote — is called `electron` in `ps`, which is
exactly how one runtime with four children reads as five runtimes. That
misreading is the reason this file exists (M-BP-4).
"""

from __future__ import annotations

import json
import shlex
from dataclasses import dataclass
from pathlib import Path

#: Roles that are one browser engine each. Everything else is a child of one.
RUNTIME_ROLES = frozenset({"browser-main"})
#: One per page under site isolation — unavoidable in chromium, and in any
#: provider built on it. Counting these as runtimes is the error M-BP-4 corrects.
RENDERER_ROLES = frozenset({"renderer"})


class EmptyRunError(RuntimeError):
    """A run that sampled nothing must not render a table.

    An empty table under a green heading reads as "measured, and it was cheap".
    """


@dataclass(frozen=True)
class ProcessSample:
    pid: int
    role: str
    rss_kb: int
    jiffies: int


class SpawnLedger:
    """The pids this run created, so that all of them can be reclaimed.

    A benchmark that leaves a process behind adds to the cost it reports. The
    ledger holds only what the run spawned: it cannot be handed a pid it does
    not own, which keeps cleanup from ever reaching somebody else's process.
    """

    def __init__(self) -> None:
        self._pids: list[int] = []

    def record(self, pid: int) -> None:
        if pid <= 1:
            raise ValueError(f"refusing to take ownership of pid {pid}")
        if pid not in self._pids:
            self._pids.append(pid)

    def release(self, pid: int) -> None:
        if pid not in self._pids:
            raise KeyError(f"pid {pid} was never spawned by this run")
        self._pids.remove(pid)

    def outstanding(self) -> list[int]:
        return list(self._pids)


def isolated_config(root: Path, provider: list[str]) -> Path:
    """Write a herdr config naming `provider` as the web-pane command.

    `root` becomes `XDG_CONFIG_HOME`, so the file lands at `<root>/herdr/config.toml`
    and the live config is never opened, let alone written.
    """
    if not provider:
        raise ValueError("a web pane born running nothing is a blank seat, not a browser")

    live = Path.home() / ".config" / "herdr"
    resolved = Path(root).expanduser()
    # By shape, not by existence: the guard must hold on a machine where the
    # live config has not been created yet. `is_relative_to` covers the
    # directory itself and everything under it, so one check is the whole guard
    # — an earlier draft had a second, narrower one beside it, and mutation
    # testing showed the narrow one could be deleted with every test still green.
    if resolved.is_relative_to(live):
        raise ValueError(f"{resolved} is inside the live config; pick a scratch root")

    target = resolved / "herdr"
    target.mkdir(parents=True, exist_ok=True)
    path = target / "config.toml"
    rendered = ", ".join(json.dumps(part) for part in provider)
    path.write_text(f"[web]\ncommand = [{rendered}]\n")
    return path


def process_role(argv: str) -> str:
    """Name a process by what it is, from its argv.

    The `--type=` flags are checked before the bundle paths. Measured 2026-08-31
    on tb v0.6.0: a real renderer's argv carries `--app-path=` but *not*
    `browser/dist/main.js`, so today the order is not load-bearing and a
    mutation that reverses it survives the suite. It is kept because the
    guarantee is cheap and the argv shape belongs to a third party that can
    change it without telling us — but the ordering is defensive, not required,
    and this comment says so rather than asserting a dependency that does not
    exist.
    """
    if "agent-browser" in argv:
        return "daemon"
    if "--type=renderer" in argv:
        return "renderer"
    if "--type=gpu-process" in argv:
        return "gpu"
    if "--type=utility" in argv:
        return "utility"
    if "--type=zygote" in argv:
        return "zygote"
    if "browser/dist/main.js" in argv:
        return "browser-main"
    if "cli/dist/main.js" in argv:
        return "cli"
    return "unknown"


def is_self_or_ancestor(pid: int, *, own: int, ancestors: frozenset[int]) -> bool:
    """True when `pid` is the measuring process itself, or one of its parents.

    A census that greps for the provider's install path matches the shell
    running the census, because that shell's own command line contains the
    path it is grepping for. Measured twice now: once here on 2026-08-31, and
    once before when a `pgrep -f "just check"` reported a gate as running that
    had already finished. The measurement instrument must not appear in the
    measurement.
    """
    return pid == own or pid in ancestors


def cpu_percent(*, jiffies: int, hz: int, window_s: float) -> float:
    """CPU as a share of one core over the window, not of the whole machine."""
    if window_s <= 0:
        raise ValueError("a window of zero seconds measures nothing")
    if hz <= 0:
        raise ValueError("clock ticks per second must be positive")
    return jiffies * 100.0 / hz / window_s


def render_report(
    *, provider: list[str], samples: list[ProcessSample], window_s: float, hz: int = 100
) -> str:
    if not samples:
        raise EmptyRunError(
            "the run produced no process samples; report nothing rather than nothing-shaped"
        )

    runtimes = sum(1 for s in samples if s.role in RUNTIME_ROLES)
    renderers = sum(1 for s in samples if s.role in RENDERER_ROLES)
    total_rss = sum(s.rss_kb for s in samples)
    total_cpu = cpu_percent(
        jiffies=sum(s.jiffies for s in samples), hz=hz, window_s=window_s
    )

    lines = [
        f"provider: {' '.join(shlex.quote(p) for p in provider)}",
        f"window: {window_s:.0f}s · processes: {len(samples)} · "
        f"runtimes: {runtimes} · renderers: {renderers}",
        "",
        "| pid | role | RSS MB | CPU % |",
        "|---|---|---|---|",
    ]
    for s in sorted(samples, key=lambda s: (-s.jiffies, s.pid)):
        pct = cpu_percent(jiffies=s.jiffies, hz=hz, window_s=window_s)
        lines.append(f"| {s.pid} | {s.role} | {s.rss_kb / 1024:.0f} | {pct:.1f} |")
    lines.append(
        f"| | **total** | **{total_rss / 1024:.0f}** | **{total_cpu:.1f}** |"
    )
    return "\n".join(lines)
