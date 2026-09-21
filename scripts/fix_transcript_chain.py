"""Fix parentUuid chain in rebon transcript JSONL files.

The engine previously wrote each user prompt as a root (parentUuid=null),
breaking the chain for reconstruct_chain. This script patches each
non-first root user entry to chain to the previous entry.

Usage:
    python scripts/fix_transcript_chain.py                  # dry-run all sessions
    python scripts/fix_transcript_chain.py --apply          # apply fixes
    python scripts/fix_transcript_chain.py --file path.jsonl --apply  # fix one file
"""

import json
import os
import sys
from pathlib import Path


def fix_jsonl(path: Path, apply: bool) -> int:
    """Fix a single JSONL file. Returns number of entries patched."""
    with open(path, encoding="utf-8") as f:
        lines = f.readlines()

    entries = []
    for line in lines:
        stripped = line.strip()
        if not stripped:
            entries.append(None)
            continue
        try:
            entries.append(json.loads(stripped))
        except json.JSONDecodeError:
            entries.append(None)

    patched = 0
    prev_uuid = None

    for i, entry in enumerate(entries):
        if entry is None:
            continue

        uuid = entry.get("uuid")
        parent = entry.get("parentUuid")

        if parent is None and entry.get("type") == "user" and prev_uuid is not None:
            # This is a root user entry that should chain to the previous entry
            entry["parentUuid"] = prev_uuid
            patched += 1

        # Track the last uuid we saw
        if uuid:
            prev_uuid = uuid

    if patched == 0:
        return 0

    print(f"  {path.name}: {patched} entries to patch", end="")

    if apply:
        with open(path, "w", encoding="utf-8") as f:
            for entry in entries:
                if entry is None:
                    f.write("\n")
                else:
                    f.write(json.dumps(entry, ensure_ascii=False) + "\n")
        print(" [APPLIED]")
    else:
        print(" [DRY-RUN]")

    return patched


def main():
    apply = "--apply" in sys.argv
    single_file = None

    for i, arg in enumerate(sys.argv):
        if arg == "--file" and i + 1 < len(sys.argv):
            single_file = Path(sys.argv[i + 1])

    if single_file:
        files = [single_file]
    else:
        # Find all JSONL files in the boncli projects directory
        config_dir = os.environ.get("REBON_CONFIG_DIR") or os.environ.get("USERPROFILE", "")
        if not config_dir:
            config_dir = os.path.expanduser("~")
        projects_root = Path(config_dir) / ".boncli" / "projects"

        if not projects_root.exists():
            print(f"Projects root not found: {projects_root}")
            sys.exit(1)

        files = sorted(projects_root.rglob("*.jsonl"))

    total_patched = 0
    total_files = 0

    for f in files:
        n = fix_jsonl(f, apply)
        if n > 0:
            total_patched += n
            total_files += 1

    print(f"\nTotal: {total_patched} entries in {total_files} files", end="")
    if not apply and total_patched > 0:
        print(" (re-run with --apply to write changes)")
    else:
        print()


if __name__ == "__main__":
    main()
