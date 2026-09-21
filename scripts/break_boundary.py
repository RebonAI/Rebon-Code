"""For each RECOVERED prefix-break, locate the surrounding transcript lines
and classify the boundary it sits on:

  SUBMIT      — a non-isMeta user message appears between prev-assistant and
                this assistant. The break-turn is iteration 0 of a brand-new
                submit; params.runtime_context_message was just rebuilt.

  META_INJECT — the only thing between prev-assistant and this assistant is
                one or more isMeta=True system-reminder messages (attachments
                producer). These get APPENDED to history, growing the prefix
                but not invalidating position 0.

  TOOL_RESULT — between prev-assistant and this assistant there are only
                user tool_result messages (continuous iteration chain, no
                new submit). No rebuild of params should occur.

  MIXED       — some combination of META_INJECT + TOOL_RESULT (or anything
                else weird).

For each, print:
  - what's between prev and break (in order)
  - what tool the prev-assistant called
  - the user-prompt text if SUBMIT (truncated)
  - sizes of likely-changing fields (git_status, memory, REBON.md) as we
    can read them NOW (just for reference; they may have changed since).
"""

from __future__ import annotations
import json
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_SESSION = (
    Path.home()
    / ".rebon"
    / "projects"
    / "E--dev-spine-reverse"
    / "sess-18b1ebf1eefedb0c-0.jsonl"
)


@dataclass
class Line:
    lineno: int
    obj: dict


def load(path: Path) -> list[Line]:
    out: list[Line] = []
    with path.open("r", encoding="utf-8") as fh:
        for i, raw in enumerate(fh, 1):
            raw = raw.strip()
            if not raw:
                continue
            try:
                out.append(Line(i, json.loads(raw)))
            except json.JSONDecodeError:
                continue
    return out


def is_assistant(line: Line) -> bool:
    msg = line.obj.get("message")
    return isinstance(msg, dict) and msg.get("role") == "assistant"


def usage_of(line: Line) -> dict:
    msg = line.obj.get("message") or {}
    return msg.get("usage") or {}


def tool_uses(line: Line) -> list[str]:
    msg = line.obj.get("message") or {}
    content = msg.get("content") or []
    if not isinstance(content, list):
        return []
    return [
        c.get("name", "?")
        for c in content
        if isinstance(c, dict) and c.get("type") == "tool_use"
    ]


def text_blocks(line: Line) -> str:
    msg = line.obj.get("message") or {}
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for c in content:
            if isinstance(c, dict) and c.get("type") == "text":
                t = c.get("text", "")
                if t:
                    parts.append(t)
        return " | ".join(parts)
    return ""


def user_line_kind(line: Line) -> str:
    """Return one of: 'meta', 'tool_result', 'plain', 'other'."""
    msg = line.obj.get("message") or {}
    if msg.get("role") != "user":
        return "other"
    if line.obj.get("isMeta"):
        return "meta"
    content = msg.get("content") or []
    if isinstance(content, list):
        for c in content:
            if isinstance(c, dict) and c.get("type") == "tool_result":
                return "tool_result"
        return "plain"
    if isinstance(content, str):
        return "plain"
    return "other"


def first_text_of_user(line: Line) -> str:
    msg = line.obj.get("message") or {}
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        for c in content:
            if isinstance(c, dict) and c.get("type") == "text":
                t = c.get("text", "")
                if t:
                    return t
    return ""


@dataclass
class Boundary:
    kind: str  # SUBMIT / META_INJECT / TOOL_RESULT / MIXED / FIRST_TURN
    between: list[tuple[int, str]] = field(default_factory=list)  # (lineno, classification)
    user_prompt_excerpt: str = ""


def classify_boundary(lines: list[Line], assistants: list[Line], i: int) -> Boundary:
    """For assistants[i], look at what's between assistants[i-1] and assistants[i]."""
    if i == 0:
        return Boundary(kind="FIRST_TURN")
    prev_lineno = assistants[i - 1].lineno
    cur_lineno = assistants[i].lineno
    between = []
    kinds = set()
    user_prompt = ""
    for line in lines:
        if line.lineno <= prev_lineno or line.lineno >= cur_lineno:
            continue
        msg = line.obj.get("message") or {}
        if msg.get("role") == "assistant":
            between.append((line.lineno, "assistant?"))
            kinds.add("other")
            continue
        kind = user_line_kind(line)
        between.append((line.lineno, kind))
        kinds.add(kind)
        if kind == "plain" and not user_prompt:
            user_prompt = first_text_of_user(line)[:200]

    if "plain" in kinds:
        return Boundary(kind="SUBMIT", between=between, user_prompt_excerpt=user_prompt)
    if kinds == {"tool_result"}:
        return Boundary(kind="TOOL_RESULT", between=between)
    if kinds == {"meta"}:
        return Boundary(kind="META_INJECT", between=between)
    if "meta" in kinds and "tool_result" in kinds:
        return Boundary(kind="MIXED", between=between)
    return Boundary(kind=f"WEIRD({sorted(kinds)})", between=between)


def main():
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_SESSION
    if not path.exists():
        print(f"missing session: {path}", file=sys.stderr)
        sys.exit(2)

    lines = load(path)
    assistants = [ln for ln in lines if is_assistant(ln)]

    # Build per-assistant turn metrics + classify breaks
    @dataclass
    class TurnRow:
        idx: int
        line: Line
        hit: int
        miss: int
        input_t: int

        @property
        def rate(self) -> float:
            d = self.hit + self.miss
            return self.hit / d * 100.0 if d else 0.0

    turns = []
    for i, a in enumerate(assistants):
        u = usage_of(a)
        turns.append(
            TurnRow(
                idx=i + 1,
                line=a,
                hit=int(u.get("prompt_cache_hit_tokens") or 0),
                miss=int(u.get("prompt_cache_miss_tokens") or 0),
                input_t=int(u.get("input_tokens") or 0),
            )
        )

    recovered_indices = []
    for i in range(1, len(turns) - 1):
        prev, cur, nxt = turns[i - 1], turns[i], turns[i + 1]
        miss_delta = cur.miss - prev.miss
        rate_drop = prev.rate - cur.rate
        if miss_delta >= 10_000 and rate_drop >= 30.0 and nxt.rate >= 80.0:
            recovered_indices.append(i)

    print(f"session: {path}")
    print(f"assistant turns: {len(turns)}")
    print(f"RECOVERED breaks: {len(recovered_indices)}")
    print()

    # Tally boundary kinds across all RECOVERED breaks
    tally: dict[str, int] = {}
    for i in recovered_indices:
        b = classify_boundary(lines, assistants, i)
        tally[b.kind] = tally.get(b.kind, 0) + 1
    print("Boundary kind tally for RECOVERED breaks:")
    for kind, count in sorted(tally.items(), key=lambda kv: -kv[1]):
        print(f"  {kind:20} {count}")
    print()

    # Detailed dump
    for i in recovered_indices:
        cur = turns[i]
        prev = turns[i - 1]
        b = classify_boundary(lines, assistants, i)
        prev_tools = tool_uses(prev.line)
        prev_text = text_blocks(prev.line)[:120].replace("\n", " ")
        print(f"--- turn {cur.idx} (line {cur.line.lineno}) ---")
        print(
            f"  prev: turn={prev.idx} line={prev.line.lineno} "
            f"hit={prev.hit:,} miss={prev.miss:,} rate={prev.rate:.1f}% "
            f"tools={prev_tools}"
        )
        print(f"  prev_text: {prev_text!r}")
        print(
            f"  break: hit={cur.hit:,} miss={cur.miss:,} rate={cur.rate:.1f}% "
            f"input={cur.input_t:,}"
        )
        print(f"  boundary kind: {b.kind}")
        if b.between:
            print(f"  between (lineno, kind):")
            for ln, kind in b.between:
                print(f"    L{ln}  {kind}")
        if b.user_prompt_excerpt:
            print(f"  user prompt excerpt: {b.user_prompt_excerpt!r}")
        print()


if __name__ == "__main__":
    main()
