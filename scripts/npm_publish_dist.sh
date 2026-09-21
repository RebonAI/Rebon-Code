#!/usr/bin/env bash
# Publish every @rebon/cli tarball sitting in dist/npm to the public npm
# registry. Used by the per-platform jobs in .github/workflows/release.yml:
# there is no cross-job artifact staging (upload-artifact hangs on the
# self-hosted Gitea), so each runner builds AND publishes only its own
# tarball(s), and this script just publishes whatever rebon-cli-*.tgz are
# present in dist/npm.
#
# Requires NPM_ORG_TOKEN in the environment. Includes a watchdog for two npm
# failure modes: (1) npm prints the `+ @rebon/...` success line but the
# process lingers because of a keep-alive HTTP agent that never gets
# destroyed; (2) the publish stalls before any success line. The success
# line is npm's authoritative "registry accepted" marker, trusted even if we
# had to SIGKILL the lingering process.
set -eu

test -n "${NPM_ORG_TOKEN:-}" || { echo "NPM_ORG_TOKEN secret required" >&2; exit 1; }

printf '@rebon:registry=https://registry.npmjs.org/\n//registry.npmjs.org/:_authToken=%s\n' \
  "$NPM_ORG_TOKEN" > .npmrc
export NPM_CONFIG_USERCONFIG="$PWD/.npmrc"
trap 'rm -f "$PWD/.npmrc"' EXIT

npm whoami --registry=https://registry.npmjs.org/

# A 36 MB payload over a slow residential uplink can legitimately take
# minutes; 180s killed a mid-flight win32 upload on v0.1.4. Keep the watchdog
# (npm's lingering keep-alive bug is real) but give the upload real room, and
# retry once. If npm times out after the registry accepted the upload, a
# registry lookup resolves the attempt as successful.
publish_one() {
  tarball="$1"
  if publish_attempt "$tarball"; then return 0; fi
  echo "[retry] $tarball: first publish attempt failed; retrying once" >&2
  sleep 5
  publish_attempt "$tarball"
}

package_json_for() {
  tar -xOf "$1" package/package.json 2>/dev/null || tar -xOf "$1" package.json 2>/dev/null
}

package_field() {
  package_json_for "$1" | node -e '
const field = process.argv[1];
let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", chunk => input += chunk);
process.stdin.on("end", () => {
  const pkg = JSON.parse(input);
  const value = pkg && pkg[field];
  if (!value) process.exit(1);
  process.stdout.write(String(value));
});
' "$2"
}

published_version_exists() {
  tarball="$1"
  package_name="$(package_field "$tarball" name 2>/dev/null || true)"
  package_version="$(package_field "$tarball" version 2>/dev/null || true)"
  if [ -z "$package_name" ] || [ -z "$package_version" ]; then
    echo "[verify] $tarball: could not read package name/version" >&2
    return 1
  fi

  spec="${package_name}@${package_version}"
  published_version="$(npm view "$spec" version --registry=https://registry.npmjs.org/ 2>/dev/null || true)"
  if [ "$published_version" = "$package_version" ]; then
    echo "skip: $spec already published"
    return 0
  fi
  return 1
}

publish_attempt() {
  tarball="$1"
  logfile="$(mktemp)"
  hard_timeout=480
  lingering_grace=5

  npm publish "$tarball" --tag latest --access public \
      --registry=https://registry.npmjs.org/ \
      --fetch-timeout=90000 --fetch-retries=2 >"$logfile" 2>&1 &
  npm_pid=$!
  tail -f "$logfile" 2>/dev/null &
  tail_pid=$!

  (
    start=$(date +%s)
    saw_success_at=0
    while kill -0 "$npm_pid" 2>/dev/null; do
      now=$(date +%s)
      if [ "$saw_success_at" -eq 0 ] && grep -qE '^\+ (@rebon/|rebon@)' "$logfile" 2>/dev/null; then
        saw_success_at=$now
      fi
      if [ "$saw_success_at" -gt 0 ] && [ $((now - saw_success_at)) -ge "$lingering_grace" ]; then
        echo "[watchdog] $tarball: success printed but npm did not exit; killing" >&2
        kill -KILL "$npm_pid" 2>/dev/null || true
        break
      fi
      if [ $((now - start)) -ge "$hard_timeout" ]; then
        echo "[watchdog] $tarball: ${hard_timeout}s without success; killing" >&2
        kill -KILL "$npm_pid" 2>/dev/null || true
        break
      fi
      sleep 1
    done
  ) &
  watchdog_pid=$!

  set +e
  wait "$npm_pid"
  rc=$?
  set -e

  kill "$watchdog_pid" 2>/dev/null || true
  kill "$tail_pid" 2>/dev/null || true
  wait 2>/dev/null || true

  if grep -qE '^\+ (@rebon/|rebon@)' "$logfile"; then
    rm -f "$logfile"
    return 0
  fi
  if grep -q 'cannot publish over' "$logfile"; then
    echo "skip: $tarball already published"
    rm -f "$logfile"
    return 0
  fi
  if published_version_exists "$tarball"; then
    rm -f "$logfile"
    return 0
  fi
  echo "publish failed for $tarball (rc=$rc)" >&2
  rm -f "$logfile"
  return "${rc:-1}"
}

# rebon-cli-*.tgz covers @rebon/cli + platform packages; rebon-<x.y.z>.tgz is
# the bare `rebon` alias built by --kind bare. dist/npm is wiped before every
# build, so the broad glob only ever sees tarballs this job just produced.
tarballs="$(find dist/npm -maxdepth 1 -type f -name 'rebon-*.tgz' | sort)"
test -n "$tarballs" || { echo "no rebon-*.tgz in dist/npm" >&2; exit 1; }
for t in $tarballs; do
  publish_one "$t"
done
