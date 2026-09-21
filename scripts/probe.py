"""Check session metadata, ALL line types, and look for model/provider."""
import json
import os
from pathlib import Path
from collections import Counter

project_dir = Path.home() / ".rebon" / "projects" / os.environ.get("REBON_PROJECT_SLUG", "")

# Check sibling files near transcripts
print("=== Files in project dir ===")
for p in sorted(project_dir.iterdir(), key=lambda x: x.stat().st_mtime, reverse=True)[:15]:
    print(f"  {p.name}  ({p.stat().st_size} bytes)")
print()

# Check ~/.rebon/ for config / session metadata
home = Path.home() / ".rebon"
print("=== Top-level .rebon files ===")
for p in sorted(home.iterdir()):
    print(f"  {p.name}  ({'DIR' if p.is_dir() else p.stat().st_size})")
print()

# Now examine line types in newest transcript
p = project_dir / "sess-18b1ec09d716b538-1.jsonl"
types_seen = Counter()
all_top_keys = Counter()
sample_by_type = {}
with p.open("r", encoding="utf-8") as fh:
    for i, line in enumerate(fh):
        obj = json.loads(line)
        t = obj.get("type", "??")
        types_seen[t] += 1
        for k in obj.keys():
            all_top_keys[k] += 1
        if t not in sample_by_type:
            sample_by_type[t] = (i + 1, obj)

print(f"=== Line types in {p.name} ===")
for t, c in types_seen.most_common():
    print(f"  type={t!r:20} count={c}")
print()
print("=== Top-level keys seen ===")
for k, c in all_top_keys.most_common():
    print(f"  {k:25} {c}")
print()
print("=== One sample per type (truncated content) ===")
for t, (ln, obj) in sample_by_type.items():
    print(f"--- line {ln} type={t} ---")
    o = dict(obj)
    if "message" in o and isinstance(o["message"], dict):
        m = dict(o["message"])
        if "content" in m:
            c = m["content"]
            if isinstance(c, str):
                m["content"] = c[:120] + "..." if len(c) > 120 else c
            else:
                m["content"] = f"<{type(c).__name__}, {len(c) if hasattr(c, '__len__') else '?'} elts>"
        o["message"] = m
    print(json.dumps(o, ensure_ascii=False)[:600])
    print()
