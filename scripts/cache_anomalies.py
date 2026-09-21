"""Identify turns where DeepSeek prompt-cache hit rate drops sharply.

Reads a rebon transcript and flags every assistant turn whose miss tokens
indicate the prefix cache was invalidated. Three categories:

  PREFIX_BREAK   — miss jumped by >= 10K tokens vs the previous turn
                   AND hit_rate dropped by >= 30 percentage points.
                   These are the strongest signal that something upstream
                   (system prompt / runtime_context / tool list / transient
                   context) changed shape between turns.

  RECOVERED      — a PREFIX_BREAK whose NEXT turn returns to >= 80% hit.
                   Most diagnostic: whatever broke the prefix was itself
                   transient (e.g. a one-off git status flip).

  SUSTAINED_LOW  — three or more consecutive turns under 50% hit. Suggests
                   the prefix never settled (or the entire base_system /
                   tool definitions are churning every turn).

Usage:
    python scripts/cache_anomalies.py <path-to-session.jsonl>

If no path is given, the newest session across all rebon projects is used.
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Turn:
    idx: int          # 1-based turn ordinal within the session
    line: int         # 1-based JSONL line number for grep-ability
    input_tokens: int
    hit: int
    miss: int

    @property
    def hit_rate(self) -> float:
        denom = self.hit + self.miss
        return (self.hit / denom * 100.0) if denom else 0.0


def load_turns(path: Path) -> list[Turn]:
    turns: list[Turn] = []
    with path.open("r", encoding="utf-8") as fh:
        ordinal = 0
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg = obj.get("message")
            if not isinstance(msg, dict) or msg.get("role") != "assistant":
                continue
            usage = msg.get("usage") or {}
            ordinal += 1
            turns.append(
                Turn(
                    idx=ordinal,
                    line=lineno,
                    input_tokens=int(usage.get("input_tokens") or 0),
                    hit=int(usage.get("prompt_cache_hit_tokens") or 0),
                    miss=int(usage.get("prompt_cache_miss_tokens") or 0),
                )
            )
    return turns


def find_newest_session() -> Path | None:
    root = Path.home() / ".rebon" / "projects"
    if not root.exists():
        return None
    candidates = []
    for proj in root.iterdir():
        if proj.is_dir():
            candidates.extend(proj.glob("sess-*.jsonl"))
    if not candidates:
        return None
    return max(candidates, key=lambda p: p.stat().st_mtime)


def classify(turns: list[Turn]) -> dict[str, list[tuple[Turn, Turn | None, Turn | None]]]:
    """Return {category: [(turn, prev, next)]} for each anomaly turn."""
    out = {"PREFIX_BREAK": [], "RECOVERED": [], "SUSTAINED_LOW": []}

    for i, t in enumerate(turns):
        prev = turns[i - 1] if i > 0 else None
        nxt = turns[i + 1] if i + 1 < len(turns) else None
        if prev is None:
            continue
        miss_delta = t.miss - prev.miss
        rate_drop = prev.hit_rate - t.hit_rate
        if miss_delta >= 10_000 and rate_drop >= 30.0:
            out["PREFIX_BREAK"].append((t, prev, nxt))
            if nxt is not None and nxt.hit_rate >= 80.0:
                out["RECOVERED"].append((t, prev, nxt))

    # Sustained low: three or more consecutive turns under 50%
    run_start = None
    for i, t in enumerate(turns):
        if t.hit_rate < 50.0 and (t.hit + t.miss) > 0:
            if run_start is None:
                run_start = i
        else:
            if run_start is not None and i - run_start >= 3:
                # report the run's first turn
                first = turns[run_start]
                prev = turns[run_start - 1] if run_start > 0 else None
                last = turns[i - 1]
                out["SUSTAINED_LOW"].append((first, prev, last))
            run_start = None
    if run_start is not None and len(turns) - run_start >= 3:
        first = turns[run_start]
        prev = turns[run_start - 1] if run_start > 0 else None
        last = turns[-1]
        out["SUSTAINED_LOW"].append((first, prev, last))

    return out


def fmt(t: Turn | None) -> str:
    if t is None:
        return "                      (none)"
    return (
        f"turn={t.idx:>4} line={t.line:>6}  "
        f"in={t.input_tokens:>7,}  hit={t.hit:>7,}  miss={t.miss:>7,}  "
        f"rate={t.hit_rate:>5.1f}%"
    )


def main() -> None:
    if len(sys.argv) > 1:
        path = Path(sys.argv[1])
    else:
        found = find_newest_session()
        if found is None:
            print("no session files found under ~/.rebon/projects", file=sys.stderr)
            sys.exit(2)
        path = found

    print(f"session: {path}")
    turns = load_turns(path)
    if not turns:
        print("  (no assistant turns)")
        return
    total_in = sum(t.input_tokens for t in turns)
    total_hit = sum(t.hit for t in turns)
    total_miss = sum(t.miss for t in turns)
    overall = (total_hit / (total_hit + total_miss) * 100.0) if (total_hit + total_miss) else 0.0
    print(
        f"  turns={len(turns)}  input={total_in:,}  hit={total_hit:,}  "
        f"miss={total_miss:,}  overall_hit_rate={overall:.1f}%"
    )

    cats = classify(turns)

    print()
    print(f"=== PREFIX_BREAK ({len(cats['PREFIX_BREAK'])} turns) ===")
    print("   miss jumped >= 10K AND hit_rate dropped >= 30pp vs prev turn")
    for t, prev, nxt in cats["PREFIX_BREAK"]:
        print(f"  prev: {fmt(prev)}")
        print(f"  >>>:  {fmt(t)}")
        print(f"  next: {fmt(nxt)}")
        print()

    print(f"=== RECOVERED ({len(cats['RECOVERED'])} turns) ===")
    print("   PREFIX_BREAK whose next turn returns to >= 80% hit — strongest")
    print("   signal that the disruption was transient")
    for t, prev, nxt in cats["RECOVERED"]:
        print(f"  break at turn {t.idx} (line {t.line}): "
              f"miss {prev.miss:,} -> {t.miss:,} -> {nxt.miss:,}")

    print()
    print(f"=== SUSTAINED_LOW ({len(cats['SUSTAINED_LOW'])} runs) ===")
    print("   3+ consecutive turns below 50% hit")
    for first, prev, last in cats["SUSTAINED_LOW"]:
        span = last.idx - first.idx + 1
        print(f"  turns {first.idx}..{last.idx}  ({span} consecutive)  "
              f"prev_rate={prev.hit_rate if prev else 0:.1f}%  "
              f"first_rate={first.hit_rate:.1f}%  last_rate={last.hit_rate:.1f}%")


if __name__ == "__main__":
    main()
