"""Inspect provider/model + raw usage for recent rebon sessions."""

import json
import os
import sys
from pathlib import Path
from collections import Counter, defaultdict


def iter_lines(jsonl_path: Path):
    with jsonl_path.open("r", encoding="utf-8", errors="replace") as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                yield lineno, json.loads(line)
            except json.JSONDecodeError:
                continue


def main():
    project_dir = Path.home() / ".rebon" / "projects" / os.environ.get("REBON_PROJECT_SLUG", "")
    files = sorted(
        project_dir.glob("sess-*.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )[:8]

    for path in files:
        models = Counter()
        first_line_keys = None
        meta_snip = None
        max_input = 0
        max_input_sample = None
        nonzero_cache = 0
        nonzero_examples = []
        usage_keys = Counter()

        for lineno, obj in iter_lines(path):
            # collect top-level fields once
            if first_line_keys is None:
                first_line_keys = sorted(obj.keys())
            # session metadata line typically has type=summary or has model field
            t = obj.get("type")
            if t and t != "user" and t != "assistant" and meta_snip is None:
                meta_snip = (t, sorted(obj.keys()))
            msg = obj.get("message")
            if not isinstance(msg, dict):
                continue
            if msg.get("role") != "assistant":
                continue
            # model fields can live on the message
            for k in ("model", "model_id", "provider"):
                v = msg.get(k)
                if v:
                    models[(k, v)] += 1
            usage = msg.get("usage") or {}
            for k in usage:
                usage_keys[k] += 1
            inp = int(usage.get("input_tokens") or 0)
            if inp > max_input:
                max_input = inp
                max_input_sample = (lineno, usage)
            hit = int(usage.get("prompt_cache_hit_tokens") or 0)
            miss = int(usage.get("prompt_cache_miss_tokens") or 0)
            cr = int(usage.get("cache_read_input_tokens") or 0)
            cc = int(usage.get("cache_creation_input_tokens") or 0)
            if hit or miss or cr or cc:
                nonzero_cache += 1
                if len(nonzero_examples) < 3:
                    nonzero_examples.append((lineno, usage))

        print(f"=== {path.name} ===")
        print(f"  first-line top-keys: {first_line_keys}")
        if meta_snip:
            print(f"  non-user/assistant entry: type={meta_snip[0]}, keys={meta_snip[1]}")
        print(f"  models / providers: {dict(models)}")
        print(f"  assistant turns with cache nonzero: {nonzero_cache}")
        print(f"  usage field freq: {dict(usage_keys)}")
        if max_input_sample:
            ln, u = max_input_sample
            print(f"  largest-input turn line={ln} usage={u}")
        if nonzero_examples:
            for ln, u in nonzero_examples:
                print(f"  nonzero example line={ln}: {u}")
        print()


if __name__ == "__main__":
    main()
