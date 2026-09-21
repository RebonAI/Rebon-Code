#!/bin/sh
# What CI would say, before the push instead of after it.
#
# This mirrors the `verify`, `repo-health` and `test` jobs of
# .github/workflows/release.yml. It is not a second opinion: where CI runs a
# command, this runs the same command with the same flags, so that a green
# run here means the same thing. When the two drift apart, CI is right and
# this file is the bug.
#
# The one flag added on top of CI's is `-q` on the test suites (and `dot` on
# node --test). It changes the reporting, never the verdict: CI writes to a
# log that is read after the fact, while this writes to the terminal of
# whoever is pushing. Narrating all ~2600 passing tests by name buried the
# step headers, the SKIPPED lines and the flake retries — the parts of this
# output that are actually read — under four thousand lines. Failures still
# print in full; the dots are only what passing looks like.
#
#   scripts/preflight.sh            everything runnable on this machine
#   scripts/preflight.sh --quick    stop after the cheap gates
#
# Run from the pre-push hook (scripts/hooks/pre-push). Skip a push past it
# with `git push --no-verify`, which is the honest way to skip — there is no
# environment variable that turns it off quietly.
#
# Two CI steps have no local equivalent and are named as skipped rather
# than dropped:
#   - the containment rlimit probe (needs os.fork; POSIX only)
#   - the Boa runner's six cross-compilation targets (needs the toolchains)
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"

quick=0
[ "${1:-}" = "--quick" ] && quick=1

# The Node the plugin host is built and tested against. CI provisions exactly
# this; the runtime's own resolution refuses anything outside >=24.19.0 <25.
PINNED_NODE=24.19.0

started=$(date +%s)
step=0
skipped=''
flaky=''

# CI is macOS, where `python3` is the interpreter. On Windows `python3` is
# usually the Microsoft Store's execution-alias stub: it sits on PATH,
# `command -v` finds it, and running it exits 49 without printing a word — so
# a plain `command -v` test picks a program that cannot run anything. Ask each
# candidate to answer before believing in it.
python_bin=''
for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version >/dev/null 2>&1; then
        python_bin=$candidate
        break
    fi
done
if [ -z "$python_bin" ]; then
    echo "preflight: no working python on PATH (tried python3, python)" >&2
    exit 1
fi

say() {
    step=$((step + 1))
    echo ""
    echo "preflight [$step] $1"
}

skip() {
    skipped="$skipped
  - $1"
    echo "preflight -- SKIPPED: $1"
}

# Two suites are known to fail on this host for reasons that are not in the
# code: rebon-session-runtime meets a loopback reset (os error 10054) traced
# to the host's security software rather than to either end of the socket, and
# computer-use occasionally does not see an endpoint record immediately after
# writing it. Both pass on their own, every time.
#
# A gate a flake can veto is a gate people turn off, so a failed suite gets
# one more attempt. It is announced on both the failure and the retry, and
# listed again at the end: a retry that quietly buries a real failure would be
# worse than a slow hook. Two failures in a row still stop the push.
retry() {
    label=$1
    shift
    if "$@"; then
        return 0
    fi
    echo ""
    echo "preflight -- $label FAILED; running it once more before giving up" >&2
    if "$@"; then
        flaky="$flaky
  - $label (failed, then passed on a second run)"
        echo "preflight -- $label passed on the second run" >&2
        return 0
    fi
    # Both runs failed, which is enough to stop the push — but not enough to
    # say they failed for the same reason. This runs a whole suite, so the two
    # lists can name different tests: a deterministic failure in one crate and
    # an unrelated flake in another read as one verdict here. Say what was
    # actually observed and let the reader compare the two lists.
    echo "preflight -- $label failed on both runs (not necessarily the same test — compare the two failure lists above)" >&2
    return 1
}

finish() {
    elapsed=$(( $(date +%s) - started ))
    echo ""
    echo "preflight: $step steps in ${elapsed}s"
    if [ -n "$skipped" ]; then
        echo "preflight: not run on this machine:$skipped"
    fi
    if [ -n "$flaky" ]; then
        echo "preflight: needed a second run:$flaky"
    fi
}

# ---------------------------------------------------------------- cheap gates

say "release scripts still parse"
"$python_bin" -m py_compile scripts/build_npm_package.py scripts/publish_npm_package.py
bash -n scripts/npm_publish_dist.sh

say "cargo metadata"
cargo metadata --format-version 1 --no-deps >/dev/null

say "cargo check -p rebon-cli --locked"
cargo check -p rebon-cli --locked

say "no JS engine in the build graph"
cargo check -p rebon-harness -p rebon-kernel-seats -p rebon-provider -p rebon-plugin-host --locked
if cargo tree -p rebon-cli -i deno_core >/dev/null 2>&1; then
    echo "deno_core is back in rebon-cli's dependency graph" >&2
    exit 1
fi

if [ "$quick" = 1 ]; then
    echo ""
    echo "preflight: --quick, stopping before the test suites"
    finish
    exit 0
fi

# ------------------------------------------------------------- resolving node

# Resolved before any suite runs, not just before the plugin-plane ones. CI
# can leave this to the `verify` job because `test` is a separate job with its
# own steps; here everything shares one process, and suites outside the plugin
# plane want it too — `rebon-plugin-mcp` has a test that `expect`s it and
# panics with `NotPresent` rather than skipping.
#
# The plugin-plane tests skip themselves without an absolute REBON_TEST_NODE,
# and a skipped test reads as a pass, so REBON_REQUIRE_TEST_NODE turns any
# later skip back into a failure.
node_bin=${REBON_TEST_NODE:-}
if [ -z "$node_bin" ] && command -v node >/dev/null 2>&1; then
    node_bin=$(command -v node)
fi

# Three suites open a session through the runtime's own resolution ladder,
# whose version gate is fail-closed: on anything but the pinned Node they
# refuse rather than run weakly. Rather than asking whoever pushes to switch
# their global runtime for the length of a hook, look where a pinned Node is
# normally installed and use it just for this run. Only when PATH does not
# already have it, and never over an explicit REBON_TEST_NODE.
if [ -z "${REBON_TEST_NODE:-}" ]; then
    have_pinned=0
    if [ -n "$node_bin" ] && [ "$("$node_bin" -p "process.versions.node" 2>/dev/null)" = "$PINNED_NODE" ]; then
        have_pinned=1
    fi
    if [ "$have_pinned" = 0 ]; then
        # `git config`-style Windows paths arrive with backslashes.
        nvm_home=$(printf '%s' "${NVM_HOME:-}" | tr '\\' '/')
        for candidate in \
            "$nvm_home/v$PINNED_NODE/node.exe" \
            "$nvm_home/v$PINNED_NODE/bin/node" \
            "$HOME/.nvm/versions/node/v$PINNED_NODE/bin/node" \
            "$HOME/.cache/rebon-ci/node-v$PINNED_NODE-darwin-arm64/bin/node"
        do
            case "$candidate" in /v"$PINNED_NODE"/*) continue ;; esac
            if [ -x "$candidate" ]; then
                echo "preflight -- PATH has no Node $PINNED_NODE; using $candidate for this run"
                node_bin=$candidate
                break
            fi
        done
    fi
fi

if [ -z "$node_bin" ]; then
    skip "every test that needs a real Node (none on PATH, no REBON_TEST_NODE)"
else
    # Absolute: the tests assert it, and a bare `node` from PATH is relative
    # on some shells.
    case "$node_bin" in
        /*|[A-Za-z]:[/\\]*) : ;;
        *) node_bin=$(cd "$(dirname "$node_bin")" && pwd)/$(basename "$node_bin") ;;
    esac
    # Windows: `command -v node` hands back an extensionless path, and
    # `kernel_loop_backend_plane` passes this value straight to
    # REBON_PLUGIN_NODE, whose resolution is fail-closed and checks the path
    # exactly as given. Rust sees no such file and refuses the session with
    # "no usable Node runtime" on a machine with a working Node.
    #
    # Testing `[ -e "$node_bin" ]` first cannot catch this: msys resolves the
    # extensionless name to node.exe and reports it as existing, so the shell
    # and Rust disagree about the same string. Prefer the .exe whenever one is
    # there — on macOS and Linux there never is, so nothing changes.
    if [ -e "$node_bin.exe" ]; then
        node_bin="$node_bin.exe"
    fi
    REBON_TEST_NODE=$node_bin
    REBON_REQUIRE_TEST_NODE=1
    export REBON_TEST_NODE REBON_REQUIRE_TEST_NODE
    node_version=$("$node_bin" -p "process.versions.node")
    echo "preflight -- node $node_version at $node_bin"
fi

# ------------------------------------------------------------------ the tests

say "cargo test -p rebon-cli --locked"
retry "rebon-cli" cargo test -q -p rebon-cli --locked

say "the crates the plugin-first refactor touches"
retry "the plugin-first crates" cargo test -q --locked -p rebon-core -p rebon-harness -p rebon-tool -p rebon-acp \
    -p rebon-agent-core -p rebon-slash-commands -p rebon-acp-client -p rebon-config \
    -p rebon-proto -p rebon-schema-gen -p rebon-i18n-gen -p rebon-session-state \
    -p rebon-kernel -p rebon-kernel-seats -p rebon-provider -p rebon-plugin-host \
    -p rebon-instructions -p rebon-session -p rebon-session-host \
    -p rebon-session-runtime -p rebon-mcp-channel -p rebon-rc-runner -p rebon-command-seat -p rebon-dialog -p rebon-tui \
    -p rebon-render -p rebon-message-tui -p rebon-plugin-cron -p rebon-plugin-web \
    -p rebon-plugin-monitor -p rebon-plugin-memory -p rebon-plugin-notebook \
    -p rebon-plugin-structured-output -p rebon-plugin-profile \
    -p rebon-plugin-skill -p rebon-plugin-image-gen -p rebon-plugin-escalation \
    -p rebon-plugin-mcp -p rebon-plugin-tasks -p rebon-plugin-plan-mode \
    -p rebon-plugin-updater -p rebon-plugin-agents -p rebon-plugin-remote \
    -p rebon-plugin-lsp-mcp -p rebon-plugin-browser -p rebon-plugin-onboarding \
    -p sandbox-win -p rebon-plugin-sandbox -p rebon-plugin-workflow

# Serial, and out of the batch above: computer-use's endpoint tests fail about
# one run in four when that crate's own suite runs in parallel, and pass every
# time on their own. Measured rather than guessed — four runs of the crate
# alone reproduced it with no other crate involved. The six tests that touch
# the environment already share one lock and drop it last, so the cause is
# deeper than a missing guard; running them serially keeps a real failure
# visible instead of letting a retry paper over it.
#
# This is the one place preflight is stricter than CI, which runs the crate in
# the batch. CI has not lost this draw yet; that is luck, not immunity.
say "computer-use, serially"
retry "rebon-plugin-computer-use" cargo test -q -p rebon-plugin-computer-use --locked -- --test-threads=1

# -------------------------------------------------------------- the Node gate

if [ -n "$node_bin" ]; then
    say "plugin protocol on Node $node_version"
    if [ "$node_version" = "$PINNED_NODE" ]; then
        # CI pins this version and runs the host's own suites against it.
        # They assert behaviour of the runtime rebon ships, so a different
        # Node makes them a test of the wrong thing rather than a weaker one.
        # `dot` for the same reason `cargo test` runs with `-q`: the default
        # TAP reporter narrates every passing assertion. Failures still print
        # in full underneath the dots.
        "$node_bin" --test --test-reporter=dot runtimes/node/plugin-host/test/*.test.mjs
        "$node_bin" --test --test-reporter=dot runtimes/node/compose-runtime/test/*.test.mjs
    else
        skip "node --test suites (CI pins Node $PINNED_NODE, this run has $node_version)"
    fi

    # These start a host directly and run on whatever Node is here — a real
    # child process is all they need.
    #
    # Serial where marked: those share one process kernel and process-wide
    # registration slots, so running them at once is several tests writing one
    # seat table.
    retry "rebon-plugin-protocol" cargo test -q -p rebon-plugin-protocol --locked
    retry "rebon-node-runtime" cargo test -q -p rebon-node-runtime --locked
    retry "rebon-plugin-supervisor" cargo test -q -p rebon-plugin-supervisor --locked
    retry "rebon-harness plugin_plane_e2e" cargo test -q -p rebon-harness --test plugin_plane_e2e --locked -- --test-threads=1
    retry "rebon-code-runner" cargo test -q -p rebon-code-runner --locked -- --test-threads=1
    retry "rebon-kernel-seats kernel_code_mode_js" cargo test -q -p rebon-kernel-seats --test kernel_code_mode_js --locked
    retry "rebon-plugin-host kernel_seats_plane" cargo test -q -p rebon-plugin-host --test kernel_seats_plane --locked -- --test-threads=1

    # And these three open a session through the runtime's own resolution
    # ladder, which enforces the pinned range (>=24.19.0 <25.0.0) and is
    # fail-closed: on an older Node they do not run weakly, they refuse with
    # "no usable Node runtime".
    #
    # The split is measured rather than guessed: on 22.21.1 everything above
    # passes and exactly these three refuse.
    if [ "$node_version" = "$PINNED_NODE" ]; then
        retry "rebon-harness plugin_plane_loop_e2e" cargo test -q -p rebon-harness --test plugin_plane_loop_e2e --locked -- --test-threads=1
        retry "rebon-harness kernel_loop_backend_plane" cargo test -q -p rebon-harness --test kernel_loop_backend_plane --locked -- --test-threads=1
        retry "rebon-harness kernel_mainline_deepseek_js" cargo test -q -p rebon-harness --test kernel_mainline_deepseek_js --locked -- --test-threads=1
    else
        skip "3 harness suites that open a session (the runtime's version gate needs Node $PINNED_NODE; this run has $node_version)"
    fi
fi

# ------------------------------------------------------------- the other tree

skip "containment rlimit probe (needs os.fork)"
skip "Boa runner cross-compilation targets (needs six toolchains installed)"

finish
