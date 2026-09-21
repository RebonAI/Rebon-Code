#!/bin/sh
# Cut a rebon-cli (npm) release with the tag and the published version in
# lockstep. Bumps the workspace version -> commit -> tag v<version>, in the
# only order that keeps them consistent.
#
#   Usage:  scripts/release-cli.sh 0.2.0
#
# Then push (tag + its commit, so CI builds/publishes the bumped version):
#   git push origin main v0.2.0
#
# Why this matters: .github/workflows/release.yml derives the npm version from
# [workspace.package] version (NOT the tag). Tag v0.2.0 with the workspace
# still at 0.1.2 would publish 0.1.2 — or silently "skip: already published"
# and go green having shipped nothing. The .git/hooks/pre-push guard also
# re-checks this, so a hand-typed `git tag` can't slip past.
set -eu

ver=${1:?usage: scripts/release-cli.sh <x.y.z>}
manifest=Cargo.toml
tag="v$ver"

[ -f "$manifest" ] || { echo "run from the repo root ($manifest not found)" >&2; exit 1; }

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null 2>&1; then
    echo "tag $tag already exists — delete it first (git tag -d $tag) or pick a new version" >&2
    exit 1
fi

# Rewrite the version inside the [workspace.package] table only.
awk -v v="$ver" '
    /^\[workspace\.package\]/ {p=1}
    p && /^[[:space:]]*version[[:space:]]*=/ && !done {
        sub(/"[^"]+"/, "\"" v "\""); done=1
    }
    /^\[/ && !/^\[workspace\.package\]/ {p=0}
    {print}
' "$manifest" > "$manifest.tmp" && mv "$manifest.tmp" "$manifest"

# Keep the bare `rebon` npm alias in lockstep. CI regenerates its version and
# @rebon/cli range from the workspace version at publish time (--kind bare),
# so this bump is about keeping the checked-in file truthful — a stale ^0.x
# range here pinned `npm update -g rebon` to the old minor (0.2.1 -> 0.3.0).
bare=npm/bare/package.json
sed -e "s|\"version\": \"[^\"]*\"|\"version\": \"$ver\"|" \
    -e "s|\"@rebon/cli\": \"[^\"]*\"|\"@rebon/cli\": \"^$ver\"|" \
    "$bare" > "$bare.tmp" && mv "$bare.tmp" "$bare"

# Sync ALL workspace member versions in the lockfile to the bumped version.
# `version.workspace = true` means every crate moves together, so the old
# `-p rebon-cli` (which touched one crate and swallowed errors) left the rest
# stale and broke `cargo ... --locked` in CI. `--offline`: a pure version bump
# never needs the network, and no `|| true` so a real failure is loud.
cargo update --workspace --offline

message=$(mktemp)
trap 'rm -f "$message"' EXIT HUP INT TERM
printf 'release(cli): Release v%s

Keep the shipped version and release tag in sync.
' "$ver" > "$message"
locks="Cargo.lock"
git add -- "$manifest" "$bare" $locks
git commit -F "$message" -- "$manifest" "$bare" $locks
git tag "$tag"

echo ""
echo "==> committed + tagged $tag"
echo "    push with:  git push origin main $tag"
