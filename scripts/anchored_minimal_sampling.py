#!/usr/bin/env python3
"""Sample the first-turn trajectory of Anchored Minimal sessions.

Anchoring changes a prior, not a switch: upstream's own evidence for the
`bash` + `str_replace_editor` schema pair is frequentist (5/5 anchored, the
standard-family schemas 11/11 did not), so a single session that reads "wrong"
proves nothing. This runs the same prompt N times through `rebon exec`, each
run a fresh session, and tallies what actually varies — the opening voice of
the first thinking block and the first tool called.

Each run reports:

  * `anchored`  — whether the Anchored Minimal bootstrap really engaged.
                  Asking for `--capability minimal` is not enough; the provider
                  has to opt in, and otherwise Minimal silently degrades to a
                  plain reduced-tool request that must not be counted as an
                  anchored sample.
  * `tool`      — the first tool name. Under the anchored bootstrap this can
                  only be `bash` or `str_replace_editor`; anything else means
                  the request was not the anchored one.
  * `voice`     — the first person-marker in the first sentence of the first
                  thinking block (`we` / `i` / `user` / `none`).

`--capability both` runs a matched Normal control so the Minimal numbers have
something to sit against.

WARNING: `rebon exec` auto-approves every permission request, so tools really
run in `--cwd`. Keep the prompt read-only (the default inspects a diff) or
point `--cwd` at a scratch checkout.

Usage:
  python scripts/anchored_minimal_sampling.py -n 10 \
      --provider deepseek-responses --model v4-pro --effort max \
      --prompt "look my diff" --out runs/anchor-2026-08-16

  # A/B against Normal, 5 each:
  python scripts/anchored_minimal_sampling.py -n 5 --capability both ...
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
EXE = ".exe" if os.name == "nt" else ""

# The two schemas the anchored bootstrap advertises (see
# crates/rebon-core/src/anchored_minimal.rs). A first call to anything else
# means the model did not see the anchored request.
ANCHORED_TOOLS = {"bash", "str_replace_editor"}

# The first pattern that matches inside the first sentence wins, so a sentence
# is labelled by how it *opens*. Word boundaries keep "we" out of "were" and
# "i" out of "inspect".
#
# Measured caveat: this opening label barely separates the two families —
# both like to open "The user says ...". The `body` label below is what
# actually discriminates, so read `open` as colour, not as the result.
VOICE_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("we", re.compile(r"\b(?:we|we're|we've|us|our|let's)\b")),
    ("i", re.compile(r"\b(?:i|i'm|i've|my|me)\b")),
    ("user", re.compile(r"\bthe user\b")),
]

# First-person families, counted over the *whole* first thinking block. The
# majority family is the run's `body` label — the trajectory's own voice, as
# opposed to how it happened to address the user in sentence one.
WE_FAMILY = re.compile(r"\b(?:we|we're|we've|we'll|us|our|let's)\b")
I_FAMILY = re.compile(r"\b(?:i|i'm|i've|i'll|my|me|let me)\b")

# Counted over the whole first thinking block, the way upstream tallied theirs.
MARKERS = ["we", "let's", "let me", "i need", "we need", "the user", "i'll", "we'll"]


def default_bin() -> Path | None:
    """Prefer an explicitly built binary over whatever `rebon` is on PATH.

    A stale PATH install is the usual reason a run "fails" here: it predates
    `--capability` and rejects the flag outright.
    """
    override = os.environ.get("REBON_BIN")
    if override:
        return Path(override)
    for profile in ("release", "debug"):
        candidate = REPO / "target" / profile / f"rebon{EXE}"
        if candidate.is_file():
            return candidate
    found = shutil.which("rebon")
    return Path(found) if found else None


def ensure_capability_flag(binary: Path) -> None:
    """Fail loudly, before burning tokens, if the binary predates --capability."""
    help_text = subprocess.run(
        [str(binary), "exec", "--help"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    ).stdout
    if "--capability" not in help_text:
        sys.exit(
            f"{binary} has no `exec --capability` flag — it is older than this "
            "script.\nBuild the current CLI and point at it:\n"
            "  cargo build -p rebon-cli --release\n"
            f"  REBON_BIN={REPO / 'target' / 'release' / ('rebon' + EXE)}"
        )


@dataclass
class Run:
    """One `rebon exec` invocation, parsed down to what varies between runs."""

    index: int
    capability: str
    ok: bool = False
    session_id: str = ""
    anchored: bool = False
    reported_capability: str = ""
    first_tool: str = ""
    first_thinking: str = ""
    first_sentence: str = ""
    voice: str = "none"
    body: str = "none"
    we_hits: int = 0
    i_hits: int = 0
    marker_counts: Counter = field(default_factory=Counter)
    stop_reason: str = ""
    input_tokens: int = 0
    output_tokens: int = 0
    seconds: float = 0.0
    error: str = ""


def first_sentence(text: str) -> str:
    """First sentence of a thinking block, CJK punctuation included."""
    stripped = text.strip()
    match = re.search(r"[.!?。！？\n]", stripped)
    return (stripped[: match.end()] if match else stripped).strip()


def classify_voice(sentence: str) -> str:
    lowered = sentence.lower()
    hits = [
        (match.start(), label)
        for label, pattern in VOICE_PATTERNS
        if (match := pattern.search(lowered))
    ]
    return min(hits)[1] if hits else "none"


def classify_body(text: str) -> tuple[str, int, int]:
    """Majority first-person family across the whole thinking block."""
    lowered = text.lower()
    we_hits = len(WE_FAMILY.findall(lowered))
    i_hits = len(I_FAMILY.findall(lowered))
    if we_hits == i_hits:
        return ("tie" if we_hits else "none", we_hits, i_hits)
    return ("we" if we_hits > i_hits else "i", we_hits, i_hits)


def count_markers(text: str) -> Counter:
    lowered = text.lower()
    return Counter(
        {
            marker: len(re.findall(rf"\b{re.escape(marker)}\b", lowered))
            for marker in MARKERS
            if re.search(rf"\b{re.escape(marker)}\b", lowered)
        }
    )


def build_argv(args: argparse.Namespace, capability: str) -> list[str]:
    argv = [str(args.bin)]
    if args.cwd:
        argv += ["--cwd", str(args.cwd)]
    if args.provider:
        argv += ["--provider", args.provider]
    if args.model:
        argv += ["--model", args.model]
    if args.effort:
        argv += ["--effort", args.effort]
    argv += ["exec", "--json", "--capability", capability]
    if args.max_iterations:
        argv += ["--max-iterations", str(args.max_iterations)]
    # The prompt is a trailing var-arg, so it goes last and is taken verbatim.
    argv.append(args.prompt)
    return argv


def parse_events(run: Run, stdout: str) -> None:
    """Fold the JSONL event feed into the run record.

    Only the *first* thinking block and the *first* tool call are recorded:
    the anchored bootstrap governs the first request, and everything after it
    inherits that trajectory rather than re-deciding it.
    """
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        if kind == "session":
            run.session_id = event.get("sessionId", "")
            run.anchored = bool(event.get("anchoredMinimal"))
            run.reported_capability = event.get("capabilityMode", "")
        elif kind == "thinking" and not run.first_thinking:
            run.first_thinking = event.get("text", "")
            run.first_sentence = first_sentence(run.first_thinking)
            run.voice = classify_voice(run.first_sentence)
            run.body, run.we_hits, run.i_hits = classify_body(run.first_thinking)
            run.marker_counts = count_markers(run.first_thinking)
        elif kind == "action.called" and not run.first_tool:
            run.first_tool = event.get("name", "")
        elif kind == "result":
            run.stop_reason = event.get("stopReason", "")
            usage = event.get("usage") or {}
            run.input_tokens = usage.get("input_tokens", 0)
            run.output_tokens = usage.get("output_tokens", 0)
        elif kind == "error" and not run.error:
            run.error = event.get("message", "")


def execute(args: argparse.Namespace, index: int, capability: str) -> Run:
    run = Run(index=index, capability=capability)
    argv = build_argv(args, capability)
    started = time.monotonic()
    try:
        done = subprocess.run(
            argv,
            cwd=str(args.cwd or REPO),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=args.timeout,
        )
    except subprocess.TimeoutExpired:
        run.seconds = time.monotonic() - started
        run.error = f"timeout after {args.timeout}s"
        return run
    run.seconds = time.monotonic() - started
    parse_events(run, done.stdout)
    # A turn that ends on the iteration cap still produced the first request,
    # which is the whole measurement — so `ok` tracks having a first thinking
    # block, not the process exit code.
    run.ok = bool(run.first_thinking or run.first_tool)
    if not run.ok and not run.error:
        tail = (done.stderr or "").strip().splitlines()
        run.error = tail[-1] if tail else f"exit {done.returncode}, no events"

    if args.out:
        stem = args.out / f"{capability}-{index:02d}"
        stem.with_suffix(".jsonl").write_text(done.stdout, encoding="utf-8")
        stem.with_suffix(".stderr.log").write_text(done.stderr, encoding="utf-8")
    return run


def replay(directory: Path) -> dict[str, list[Run]]:
    """Re-aggregate saved runs without calling the model.

    The classifier is the part of this that keeps changing; the raw feed is
    not. Sharpening the analysis should cost nothing, so `--replay` re-reads
    an earlier `--out` directory instead of resampling.
    """
    results: dict[str, list[Run]] = {}
    for path in sorted(directory.glob("*.jsonl")):
        capability, _, index = path.stem.rpartition("-")
        run = Run(index=int(index) if index.isdigit() else 0, capability=capability)
        parse_events(run, path.read_text(encoding="utf-8", errors="replace"))
        run.ok = bool(run.first_thinking or run.first_tool)
        results.setdefault(capability, []).append(run)
    return results


def sample(args: argparse.Namespace, capability: str) -> list[Run]:
    if args.jobs == 1:
        return [execute(args, i, capability) for i in range(1, args.n + 1)]
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [
            pool.submit(execute, args, i, capability) for i in range(1, args.n + 1)
        ]
        return [future.result() for future in futures]


def truncate(text: str, width: int) -> str:
    """Collapse to one line. ASCII-only output: a cp936 console mangles the rest."""
    collapsed = " ".join(text.split())
    return collapsed if len(collapsed) <= width else collapsed[: width - 3] + "..."


def report(capability: str, runs: list[Run]) -> None:
    print(f"\n=== capability={capability} | {len(runs)} runs ===")
    print(
        f"{'#':>3}  {'ok':<4} {'anch':<5} {'tool':<19} {'open':<6} "
        f"{'body':<12} {'s':>4}  first sentence"
    )
    for run in runs:
        body = f"{run.body} {run.we_hits}w/{run.i_hits}i" if run.ok else ""
        print(
            f"{run.index:>3}  "
            f"{'yes' if run.ok else 'NO':<4} "
            f"{('yes' if run.anchored else 'no'):<5} "
            f"{truncate(run.first_tool or '-', 19):<19} "
            f"{run.voice:<6} "
            f"{body:<12} "
            f"{run.seconds:>4.0f}  "
            f"{truncate(run.first_sentence or run.error, 76)}"
        )

    good = [run for run in runs if run.ok]
    if not good:
        print("no usable runs")
        return

    def share(counter: Counter, total: int) -> str:
        return " | ".join(
            f"{name} {count}/{total} ({count / total:.0%})"
            for name, count in counter.most_common()
        )

    total = len(good)
    print(f"\nopen    : {share(Counter(run.voice for run in good), total)}  (first sentence)")
    print(f"body    : {share(Counter(run.body for run in good), total)}  (whole first thinking block)")
    print(f"tool    : {share(Counter(run.first_tool or '-' for run in good), total)}")
    anchored = [run for run in good if run.anchored]
    # Off-schema only means something for runs that claimed to be anchored;
    # a Normal control calling `Bash` is just Normal behaving normally.
    off_schema = sorted(
        {run.first_tool for run in anchored if run.first_tool and run.first_tool not in ANCHORED_TOOLS}
    )
    print(
        f"anchored: {len(anchored)}/{total}"
        + (f" | off-schema first tool: {', '.join(off_schema)}" if off_schema else "")
    )
    markers = Counter()
    for run in good:
        markers.update(run.marker_counts)
    if markers:
        print(
            "markers : "
            + " | ".join(f"{name} {count}" for name, count in markers.most_common())
            + "  (occurrences in the whole first thinking block)"
        )
    tokens = (
        f"tokens  : in {sum(run.input_tokens for run in good)} "
        f"out {sum(run.output_tokens for run in good)}"
    )
    # Replayed runs have no wall clock — it lives in the process, not the feed.
    if any(run.seconds for run in good):
        tokens += f" | median {statistics.median(run.seconds for run in good):.0f}s/run"
    print(tokens)
    failed = [run for run in runs if not run.ok]
    if failed:
        print(f"failed  : {len(failed)} - " + "; ".join(f"#{r.index} {r.error}" for r in failed))


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("-n", type=int, default=5, help="runs per capability mode (default 5)")
    parser.add_argument("--prompt", default="look my diff", help="prompt sent verbatim each run")
    parser.add_argument(
        "--capability",
        choices=["minimal", "normal", "both"],
        default="minimal",
        help="sample Minimal, Normal, or both as a matched control",
    )
    parser.add_argument("--provider", help="custom provider id, e.g. deepseek-responses")
    parser.add_argument("--model", help="model name, e.g. v4-pro")
    parser.add_argument(
        "--effort", choices=["low", "medium", "high", "xhigh", "max"], help="reasoning effort"
    )
    parser.add_argument(
        "--max-iterations",
        type=int,
        default=1,
        help="cap the agentic loop (default 1 — the first request is the measurement; "
        "pass 0 to leave it uncapped)",
    )
    parser.add_argument("--cwd", type=Path, help="working directory for the session (default: repo root)")
    parser.add_argument("--out", type=Path, help="directory to write raw JSONL + stderr per run")
    parser.add_argument("--bin", type=Path, help="rebon binary (default: $REBON_BIN, target/, PATH)")
    parser.add_argument("--jobs", type=int, default=1, help="concurrent runs (default 1)")
    parser.add_argument("--timeout", type=int, default=600, help="per-run timeout in seconds")
    parser.add_argument(
        "--replay",
        type=Path,
        metavar="DIR",
        help="re-report an earlier --out directory instead of sampling (no model calls)",
    )
    args = parser.parse_args()
    # Thinking text is echoed to the console; one character outside the console
    # codepage would otherwise take the whole report down with it.
    sys.stdout.reconfigure(errors="replace")

    if args.replay:
        replayed = replay(args.replay)
        if not replayed:
            sys.exit(f"no *.jsonl runs under {args.replay}")
        print(f"replaying {args.replay}")
        for mode in sorted(replayed):
            report(mode, replayed[mode])
        return 0

    args.bin = args.bin or default_bin()
    if not args.bin:
        sys.exit("no rebon binary found; build one or set --bin / REBON_BIN")
    ensure_capability_flag(args.bin)
    if args.cwd:
        args.cwd = args.cwd.resolve()
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)

    modes = ["minimal", "normal"] if args.capability == "both" else [args.capability]
    print(
        f"binary {args.bin}\nprompt {args.prompt!r} | provider {args.provider or '(config)'} | "
        f"model {args.model or '(config)'} | effort {args.effort or '(config)'} | "
        f"{args.n} runs x {', '.join(modes)}"
    )
    print("tools execute for real (permissions auto-approved) in " + str(args.cwd or REPO))

    results = {mode: sample(args, mode) for mode in modes}
    for mode in modes:
        report(mode, results[mode])
    if args.out:
        summary = {
            mode: [vars(run) | {"marker_counts": dict(run.marker_counts)} for run in runs]
            for mode, runs in results.items()
        }
        (args.out / "summary.json").write_text(
            json.dumps(summary, indent=2, ensure_ascii=False), encoding="utf-8"
        )
        print(f"\nraw runs written to {args.out}")

    return 0 if any(run.ok for runs in results.values() for run in runs) else 1


if __name__ == "__main__":
    raise SystemExit(main())
