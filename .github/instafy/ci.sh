#!/usr/bin/env bash
# Instafy fork CI checks. .github/workflows/instafy-ci.yml runs exactly these
# commands; run them locally the same way (see INSTAFY.md):
#
#   bash .github/instafy/ci.sh workflow-allowlist
#   bash .github/instafy/ci.sh rust
#
# Instafy consumes this fork only as the `codex` submodule of instafy-dev/instafy,
# whose own CI builds and tests runtime-agent against it. This script therefore
# checks only the crates runtime-agent path-depends on and the tests that cover
# Instafy's patches, not the whole upstream workspace.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# The one workflow file this fork may carry. Upstream syncs re-add openai's
# workflows (paid larger runners, release and bot automation); they must be
# dropped again, and this check fails the sync PR until they are.
allowed_workflow=".github/workflows/instafy-ci.yml"

# No separate `cargo check` of the crates runtime-agent path-depends on: the
# test build below already compiles all of them, and instafy-dev/instafy's
# build.yml runs `cargo check --locked --tests` on runtime-agent against this
# submodule with runtime-agent's real feature set.

# The tests that cover Instafy's patches, as "<test binary>|<name filter>|<what
# it covers>". An empty filter runs every test in that binary. Each entry must
# match at least one passing test, so a renamed test or module fails the job
# instead of passing silently.
patch_tests=(
  "codex_api||99f24c873 retryable proxy 429 mapping"
  "codex_models_manager||f3104759e GPT-6 Luna in the bundled models.json"
  "codex_exec_server|reqwest_http_client::tests::|11be4c61f e834d276e loopback HTTP client"
  "codex_rmcp_client|stdio_server_launcher::tests::|11be4c61f e834d276e stdio launcher"
  "process_group_cleanup||11be4c61f e834d276e stdio server process groups"
  "codex_mcp|runtime::tests::|11be4c61f MCP runtime generations"
  "codex_core|client::tests::|3745197c1 bounded execution before a final response"
  "codex_core|session::tests::|11be4c61f turn abort and shutdown ordering"
  "codex_mcp|connection_manager::tests::|11be4c61f MCP connection manager"
  "codex_mcp|rmcp_client::tests::|11be4c61f MCP rmcp client"
  "codex_rmcp_client|rmcp_client::tests::|11be4c61f rmcp client"
  "codex_core|tasks::tests::|11be4c61f task lifecycle"
  "codex_core|session::mcp::tests::|11be4c61f session MCP lifecycle"
  "codex_core|session::turn::tests::|11be4c61f turn lifecycle"
  "all|retryable_proxy_rate_limit::|99f24c873 retryable proxy 429 end to end"
  "all|responses_lite::|3745197c1 responses-lite required tool choice"
  "all|incomplete|upstream incomplete-response coverage; narrow to incomplete_response_not_retried:: once #3 lands"
)

# One package set and one target selection for the whole test build and run.
# Cargo resolves features per selection, so testing each crate on its own
# rebuilds shared dependencies (tokio, rustls, aws-lc, ...) once per feature
# set. Instead, cargo runs every test binary through `ci.sh run-test-binary`,
# which applies that binary's filters from patch_tests.
test_packages=(-p codex-api -p codex-models-manager -p codex-exec-server -p codex-rmcp-client -p codex-mcp -p codex-core)
test_targets=(--lib --test all --test process_group_cleanup)

annotate_error() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    echo "::error::$*"
  else
    echo "error: $*" >&2
  fi
}

group() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::group::$*"; else echo "==> $*"; fi
}

endgroup() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::endgroup::"; fi
}

workflow_allowlist() {
  cd "$repo_root"
  local status=0 entry

  while IFS= read -r -d '' entry; do
    if [[ "$entry" != "$allowed_workflow" ]]; then
      annotate_error "$entry is not allowed: this fork runs only $allowed_workflow. Drop upstream workflow files after every sync (see INSTAFY.md)."
      status=1
    fi
  done < <(find .github/workflows -mindepth 1 \( -type f -o -type l \) -print0 2>/dev/null | sort -z)

  if [[ ! -f "$allowed_workflow" ]]; then
    annotate_error "$allowed_workflow is missing."
    status=1
  fi

  for entry in .github/dependabot.yml .github/dependabot.yaml; do
    if [[ -e "$entry" || -L "$entry" ]]; then
      annotate_error "$entry is not allowed: upstream's Dependabot config opens update PRs this fork does not take. Delete it (see INSTAFY.md)."
      status=1
    fi
  done

  if (( status == 0 )); then
    echo "workflow allowlist ok: only $allowed_workflow, no Dependabot config"
  fi
  return "$status"
}

rust() {
  cd "$repo_root/codex-rs"
  export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
  export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
  export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
  export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

  group "cargo test --no-run: build the patch test binaries"
  cargo test --locked "${test_packages[@]}" "${test_targets[@]}" --no-run
  endgroup

  local runner log rc
  runner="target.'cfg(all())'.runner = [\"bash\", \"$repo_root/.github/instafy/ci.sh\", \"run-test-binary\"]"
  log="$(mktemp)"
  group "cargo test: Instafy patch tests"
  set +e
  cargo test --locked "${test_packages[@]}" "${test_targets[@]}" --no-fail-fast --config "$runner" 2>&1 | tee "$log"
  rc="${PIPESTATUS[0]}"
  set -e
  endgroup

  # "<binary> <test>" for every passing test, from cargo's "Running ... (…/deps/<binary>-<hash>)" headers.
  local passed
  passed="$(awk '
    { gsub(/\033\[[0-9;]*m/, "") }
    /^ *Running / { bin = $NF; sub(/\)$/, "", bin); sub(/.*\//, "", bin); sub(/-[0-9a-f]+$/, "", bin); next }
    /^test .* \.\.\. ok$/ { print bin, $2 }
  ' "$log")"
  rm -f "$log"

  local status=0 entry binary filter covers count
  echo "Instafy patch test coverage (passing tests per entry):"
  for entry in "${patch_tests[@]}"; do
    IFS='|' read -r binary filter covers <<<"$entry"
    count="$(awk -v b="$binary" -v f="$filter" '$1 == b && (f == "" || index($2, f)) { n++ } END { print n + 0 }' <<<"$passed")"
    printf '  %4d  %-22s %-31s %s\n' "$count" "$binary" "${filter:-(all)}" "$covers"
    if (( count == 0 )); then
      annotate_error "no passing test in $binary matches '${filter:-(all)}' ($covers); a test or module was renamed, update .github/instafy/ci.sh"
      status=1
    fi
  done
  if (( rc != 0 )); then
    annotate_error "Instafy patch tests failed"
    return "$rc"
  fi
  return "$status"
}

# Cargo's test runner hook: `ci.sh run-test-binary <test executable> [args]`.
# Cargo has already set the working directory and CARGO_* environment the
# test expects; this only appends the binary's filters from patch_tests.
run_test_binary() {
  local exe="$1"
  shift
  local name entry binary filter covers filters=()
  name="$(basename "$exe")"
  name="${name%-*}"
  for entry in "${patch_tests[@]}"; do
    IFS='|' read -r binary filter covers <<<"$entry"
    if [[ "$binary" == "$name" ]]; then
      filters+=("$filter")
    fi
  done
  if (( ${#filters[@]} == 0 )); then
    echo "$name has no Instafy patch tests; not running it"
    return 0
  fi
  # One process per filter. Some upstream tests depend on process-global state
  # (tracing's callsite interest cache, for one) and upstream runs every test
  # in its own process with nextest; sharing one process across filter groups
  # makes e.g. session::turn::tests::post_sampling_token_estimate_is_disabled_by_always_on_sinks
  # fail whenever session::tests:: ran first.
  local rc=0
  for filter in "${filters[@]}"; do
    if [[ -z "$filter" ]]; then
      "$exe" "$@" || rc=$?
    else
      "$exe" "$@" "$filter" || rc=$?
    fi
  done
  return "$rc"
}

usage() {
  echo "usage: $0 workflow-allowlist|rust" >&2
  exit 2
}

case "${1:-}" in
  workflow-allowlist) workflow_allowlist ;;
  rust) rust ;;
  run-test-binary) shift; run_test_binary "$@" ;;
  *) usage ;;
esac
