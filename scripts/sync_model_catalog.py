#!/usr/bin/env python3
"""Refresh the embedded model catalogue from models.dev.

models.dev is the upstream: one JSON document describing every provider's
models, their limits, their prices and — the part rebon could not answer on
its own — which request-body knobs each model actually supports. The
`experimental.modes` block is what makes the fast-mode gate possible:

    "modes": {"fast": {"provider": {"body": {"service_tier": "priority"}}}}

A model with no `fast` mode does not take `service_tier`, which is exactly
the question `apply_openai_service_tier` used to answer with a guess.

The upstream document is 4 MB across 213 providers. rebon can only talk to
the vendors in `PROVIDERS` below, so the snapshot keeps those and drops the
fields no caller reads. Run this when a vendor ships a model; the snapshot
is committed, so a fresh clone works offline and a build never reaches the
network.

    python scripts/sync_model_catalog.py [--check]

`--check` re-fetches and exits non-zero when the snapshot is stale, which is
what CI runs.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import sys
import urllib.request
from pathlib import Path

UPSTREAM = "https://models.dev/api.json"

# The score axis. LMArena publishes its leaderboards as a CC-BY-4.0 dataset
# on Hugging Face, and the datasets server serves them as plain JSON with no
# key, so a snapshot can carry the scores as long as it credits the source.
SCORES_BOARD = "lmarena/agent"
SCORES_HOME = "https://lmarena.ai"
SCORES_ROWS = (
    "https://datasets-server.huggingface.co/rows"
    "?dataset=lmarena-ai%2Fleaderboard-dataset&config=agent&split=latest&offset=0"
)

REPO_ROOT = Path(__file__).resolve().parent.parent
SNAPSHOT = REPO_ROOT / "crates" / "rebon-api" / "model-table" / "models.json"

# models.dev provider ids for the vendors `ProviderVendor` knows how to
# reach. The `-cn` and `-coding-plan` siblings are the same catalogue
# behind a different endpoint or plan, so they are folded into the parent
# rather than carried twice.
PROVIDERS = [
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "zhipuai",
    "moonshotai",
    "minimax",
    "siliconflow",
    "alibaba",
    "volcengine",
    "opencode",
    "openrouter",
    "ollama-cloud",
]

# Per-model fields kept verbatim.
FLAGS = (
    "name",
    "family",
    "knowledge",
    "release_date",
    "last_updated",
    "reasoning",
    "tool_call",
    "attachment",
    "temperature",
    "structured_output",
    "open_weights",
)


def fetch(url: str) -> dict:
    request = urllib.request.Request(url, headers={"User-Agent": "rebon-catalog-sync"})
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.loads(response.read().decode("utf-8"))


def reasoning_efforts(model: dict) -> list[str]:
    for option in model.get("reasoning_options") or []:
        if option.get("type") == "effort":
            return list(option.get("values") or [])
    return []


def modes(model: dict) -> dict:
    """Flatten `experimental.modes` down to what a request builder needs.

    Each mode keeps the body the provider expects plus the price it bills
    at, and `service_tier` is lifted out of the body because that is the
    one knob rebon gates on today.
    """
    out: dict[str, dict] = {}
    raw = ((model.get("experimental") or {}).get("modes")) or {}
    for name, mode in raw.items():
        body = ((mode.get("provider") or {}).get("body")) or {}
        entry: dict = {}
        if body:
            entry["body"] = body
        service_tier = body.get("service_tier")
        if isinstance(service_tier, str):
            entry["service_tier"] = service_tier
        if mode.get("cost"):
            entry["cost"] = mode["cost"]
        out[name] = entry
    return out


def trim_model(model_id: str, model: dict) -> dict:
    trimmed: dict = {"id": model_id}
    for key in FLAGS:
        if key in model:
            trimmed[key] = model[key]
    modalities = model.get("modalities") or {}
    if modalities:
        trimmed["modalities"] = {
            "input": modalities.get("input") or [],
            "output": modalities.get("output") or [],
        }
    efforts = reasoning_efforts(model)
    if efforts:
        trimmed["reasoning_efforts"] = efforts
    if model.get("limit"):
        trimmed["limit"] = model["limit"]
    if model.get("cost"):
        trimmed["cost"] = model["cost"]
    mode_map = modes(model)
    if mode_map:
        trimmed["modes"] = mode_map
    return trimmed


def leaderboard_candidates(model_name: str) -> list[str]:
    """The ids a leaderboard row could be talking about.

    The board writes models the way a person says them — `GPT 6 Astra
    (Max)`, `Claude Fable 5.1 (Max)`, `DeepSeek V4 Pro (High) (0813)` —
    while the catalogue uses wire ids. The parenthesised groups are the
    reasoning effort and the snapshot date, neither of which is part of the
    id, and the rest joins on after lowercasing and dashing. Vendors
    disagree about `.` versus `-` inside a version, so both spellings are
    tried.
    """
    base = re.sub(r"\s*\([^)]*\)", "", model_name).strip().lower()
    base = re.sub(r"\s+", "-", base)
    return [
        base,
        base.replace(".", "-"),
        base.replace("-", ""),
        re.sub(r"(\d)-(\d)", r"\1.\2", base),
    ]


def leaderboard_variant(model_name: str) -> str | None:
    """The effort a scored row was measured at (`Max`, `High`, `xHigh`)."""
    for group in re.findall(r"\(([^)]*)\)", model_name):
        if not group.strip().isdigit():
            return group.strip()
    return None


def fetch_scores() -> dict:
    """The LMArena agent board, keyed by the ids it can be joined to.

    Why this board: it scores models on agent sessions, which is the work
    rebon does, and it is published under CC-BY-4.0 — so it can travel with
    this snapshot as long as it is credited. The text board has two orders
    of magnitude more coverage and measures chat preference, which is not
    the same question and would rank a talkative model above a capable one.

    A model appears once per effort. The best row wins: the tier a model
    earns is what it can do, not what it does when asked to think less.
    """
    payload = fetch(f"{SCORES_ROWS}&length=100")
    rows = [entry["row"] for entry in payload.get("rows", [])]
    total = payload.get("num_rows_total") or len(rows)
    best: dict[str, dict] = {}
    for row in rows:
        name = row.get("model_name") or ""
        score = row.get("score")
        rank = row.get("rank")
        if score is None or rank is None:
            continue
        entry = {
            "value": round(float(score), 6),
            "rank": int(rank),
            "of": int(total),
            "measured_as": name,
            "variant": leaderboard_variant(name),
        }
        for candidate in leaderboard_candidates(name):
            kept = best.get(candidate)
            if kept is None or entry["rank"] < kept["rank"]:
                best[candidate] = entry
    published = next(
        (row.get("leaderboard_publish_date") for row in rows if row.get("leaderboard_publish_date")),
        None,
    )
    return {"by_id": best, "published": published, "count": len(rows)}


def build(upstream: dict, scores: dict) -> dict:
    providers: dict[str, dict] = {}
    matched = 0
    for provider_id in PROVIDERS:
        provider = upstream.get(provider_id)
        if provider is None:
            print(f"warning: models.dev has no provider {provider_id!r}", file=sys.stderr)
            continue
        models = {}
        for model_id, model in sorted((provider.get("models") or {}).items()):
            trimmed = trim_model(model_id, model)
            score = scores["by_id"].get(model_id.lower())
            if score is not None:
                trimmed["score"] = score
                matched += 1
            models[model_id] = trimmed
        providers[provider_id] = {
            "id": provider_id,
            "name": provider.get("name") or provider_id,
            "doc": provider.get("doc"),
            "api": provider.get("api"),
            "models": models,
        }
    print(
        f"scored {matched} catalogue entries from {scores['count']} board rows "
        f"(one row can score the same model under several providers)"
    )
    return {
        "source": UPSTREAM,
        "generated_at": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "scores": {
            "board": SCORES_BOARD,
            "source": SCORES_HOME,
            "license": "CC-BY-4.0",
            "attribution": "Model scores from LMArena (lmarena.ai), CC-BY-4.0",
            "published": scores["published"],
            "count": scores["count"],
        },
        "providers": providers,
    }


def serialize(catalogue: dict) -> str:
    return json.dumps(catalogue, ensure_ascii=False, indent=1, sort_keys=False) + "\n"


def strip_timestamp(text: str) -> str:
    """Everything but `generated_at`, so `--check` reports real drift."""
    parsed = json.loads(text)
    parsed.pop("generated_at", None)
    return json.dumps(parsed, ensure_ascii=False, sort_keys=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when the committed snapshot differs from upstream",
    )
    args = parser.parse_args()

    catalogue = build(fetch(UPSTREAM), fetch_scores())
    rendered = serialize(catalogue)

    if args.check:
        if not SNAPSHOT.exists():
            print(f"missing snapshot: {SNAPSHOT}", file=sys.stderr)
            return 1
        current = SNAPSHOT.read_text(encoding="utf-8")
        if strip_timestamp(current) != strip_timestamp(rendered):
            print(
                "model catalogue is stale; run python scripts/sync_model_catalog.py",
                file=sys.stderr,
            )
            return 1
        print("model catalogue is current")
        return 0

    SNAPSHOT.parent.mkdir(parents=True, exist_ok=True)
    # Bytes, not text: a text-mode write would translate `\n` to the
    # platform's separator, and the tree is pinned to LF (`.gitattributes`).
    SNAPSHOT.write_bytes(rendered.encode("utf-8"))
    models = sum(len(p["models"]) for p in catalogue["providers"].values())
    print(
        f"wrote {SNAPSHOT.relative_to(REPO_ROOT)}: "
        f"{len(catalogue['providers'])} providers, {models} models, "
        f"{len(rendered) / 1024:.0f} KiB"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
