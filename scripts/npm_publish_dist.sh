#!/usr/bin/env bash
# Publish every @rebon/cli tarball sitting in dist/npm to the public npm
# registry. Used by the per-platform jobs in .github/workflows/release.yml:
# nothing is handed between jobs as an artifact, so each runner builds AND
# publishes only its own tarball(s), and this script publishes whatever
# rebon-*.tgz are present in dist/npm.
#
# Authentication is npm trusted publishing (OIDC), so there is no token here
# and there must not be one: npm reads an `_authToken` as an instruction to
# authenticate as that token's owner, and having done so it will not attempt a
# trusted publish. What the job supplies instead is `permissions: id-token:
# write`, which npm exchanges for a short-lived credential scoped to this
# workflow. The registry checks the claim against the trusted publisher
# configured on each package — owner, repository, workflow filename and
# environment — so a publish from another workflow, or from a fork, has
# nothing to present.
#
# Provenance comes with it: npm attests the build from the same credential,
# which is why nothing here passes --provenance.
#
# Requires npm >= 11.5.1, which the publish jobs install. A runner's bundled
# npm is generally older and fails with a plain authentication error rather
# than anything that names the version.
#
# The watchdog covers two npm failure modes: (1) npm prints the `+ @rebon/...`
# success line but the process lingers because of a keep-alive HTTP agent that
# never gets destroyed; (2) the publish stalls before any success line. The
# success line is npm's authoritative "registry accepted" marker, trusted even
# if we had to SIGKILL the lingering process.
set -eu

# No .npmrc, and no `npm whoami`: there is no user to be. A trusted publish
# authenticates per publish, so the first thing that proves the credential
# works is the publish itself.

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
  # Wide enough to cover what still happens after the success line: a trusted
  # publish signs the provenance attestation and uploads it once the tarball
  # is accepted, so killing at the old five seconds could land between the two
  # and leave a published version with no attestation.
  lingering_grace=30

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
