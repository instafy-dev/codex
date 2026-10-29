#!/usr/bin/env bash
# Instafy fork CI checks. .github/workflows/instafy-ci.yml runs exactly these commands; run
# them locally the same way (see INSTAFY.md):
#
#   bash .github/instafy/ci.sh workflow-allowlist [REV]
#   bash .github/instafy/ci.sh registry [REV]
#   bash .github/instafy/ci.sh tree-identity [REV]
#   bash .github/instafy/ci.sh self-test
#   bash .github/instafy/ci.sh patch-tests
#
# Every check reads the committed tree of REV (default HEAD), not the working tree; only
# patch-tests builds the checkout.
#
# This fork carries no build of its own while INSTAFY-PATCHES.toml registers no patches:
# codex-rs is then byte-identical to the upstream tag, and instafy-dev/instafy's CI builds and
# tests runtime-agent against it. patch-tests is the only command that runs cargo, and the
# workflow runs it only when the registry lists at least one patch.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
registry_py="$script_dir/registry.py"

# The one workflow file this fork may carry. Upstream releases add openai's workflows (paid
# larger runners, release and bot automation); the generated base commit drops them, and this
# check fails any branch that brings one back.
allowed_workflow=".github/workflows/instafy-ci.yml"
# Upstream files the generated base commit deletes and no branch may bring back.
forbidden_files=(
  .github/dependabot.yml
  .github/dependabot.yaml
  .github/CODEOWNERS
  CODEOWNERS
  docs/CODEOWNERS
)

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
  local rev="${1:-HEAD}" status=0 entry
  cd "$(git rev-parse --show-toplevel)"
  git rev-parse --verify --quiet "$rev^{commit}" >/dev/null || {
    annotate_error "$rev is not a commit"
    return 1
  }

  while IFS= read -r -d '' entry; do
    if [[ "$entry" != "$allowed_workflow" ]]; then
      annotate_error "$entry is not allowed: this fork runs only $allowed_workflow. bump.sh drops upstream workflows from every base; never merge them back (see INSTAFY.md)."
      status=1
    fi
  done < <(git ls-tree -r -z --name-only "$rev" -- .github/workflows)

  if ! git cat-file -e "$rev:$allowed_workflow" 2>/dev/null; then
    annotate_error "$allowed_workflow is missing."
    status=1
  elif ! python3 "$registry_py" workflow-runners --rev "$rev" >/dev/null; then
    # Standard ubuntu-24.04 runners only: larger, macOS and Windows runners are billed.
    annotate_error "$allowed_workflow may run on ubuntu-24.04 only and call no reusable workflow."
    status=1
  fi

  for entry in "${forbidden_files[@]}"; do
    if git cat-file -e "$rev:$entry" 2>/dev/null; then
      annotate_error "$entry is not allowed: upstream's Dependabot config and CODEOWNERS do not apply to this fork. Delete it (see INSTAFY.md)."
      status=1
    fi
  done

  if (( status == 0 )); then
    echo "workflow allowlist ok at $(git rev-parse --short "$rev"): only $allowed_workflow on ubuntu-24.04, no Dependabot config or CODEOWNERS"
  fi
  return "$status"
}

registry() {
  local rev="${1:-HEAD}" verify=()
  # In CI, also prove base_commit is the commit openai/codex's tag peels to (a network read).
  if [[ -n "${GITHUB_ACTIONS:-}" || "${INSTAFY_VERIFY_UPSTREAM_TAG:-0}" == 1 ]]; then
    verify=(--verify-upstream-tag)
  fi
  python3 "$registry_py" check --rev "$rev" ${verify[@]+"${verify[@]}"} || {
    annotate_error "INSTAFY-PATCHES.toml failed its format check"
    return 1
  }
}

# Writes patches=<n> to GITHUB_OUTPUT: the number of registered patches this tree carries.
tree_identity() {
  local rev="${1:-HEAD}" base count
  base="$(python3 "$registry_py" field --rev "$rev" base_commit)"
  if ! git cat-file -e "$base^{commit}" 2>/dev/null; then
    # CI checks out one commit; fetch the base's trees only (no blobs, no history).
    group "fetch base_commit $base"
    git fetch --no-tags --depth=1 --filter=blob:none origin "$base"
    endgroup
  fi
  # A commit whose only parent is base_commit is a generated base (instafy/base/<tag>). Its
  # registry lists every patch, but its code is the bare tag, so no registered file may differ
  # and there are no patch tests to run. Everything else (a patched tip, a pull request's merge
  # commit, an integration merge) must carry every registered patch: the exact check.
  # cat-file reads the parents from the commit object, which a shallow checkout still has.
  if [[ "$(git cat-file commit "$rev" | sed -n '/^$/q; s/^parent //p')" == "$base" ]]; then
    python3 "$registry_py" tree-identity --rev "$rev" --base || {
      annotate_error "a generated base may differ from the upstream tag only in the Instafy files and deleted upstream automation (see INSTAFY.md)"
      return 1
    }
    count=0
  else
    python3 "$registry_py" tree-identity --rev "$rev" --exact || {
      annotate_error "the tree must differ from the upstream tag in the Instafy files, deleted upstream automation and every registered patch file, and nothing else (see INSTAFY.md)"
      return 1
    }
    count="$(python3 "$registry_py" count --rev "$rev")"
  fi
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    echo "patches=$count" >> "$GITHUB_OUTPUT"
  fi
}

self_test() {
  python3 -m unittest discover -s "$script_dir" -p 'test_*.py' -v
}

# "<cargo package>|<lib or test target>|<name filter>" for every registered patch.
fork_tests() {
  python3 "$registry_py" fork-tests --rev HEAD
}

# The cargo test binary a fork_tests entry runs: the lib target is named after the package
# (dashes become underscores); an integration test target keeps its own name.
fork_test_binary() {
  local package="$1" target="$2"
  if [[ "$target" == lib ]]; then
    echo "${package//-/_}"
  else
    echo "$target"
  fi
}

patch_tests() {
  local entries=() entry package target filter
  while IFS= read -r entry; do
    entries+=("$entry")
  done < <(fork_tests)
  if (( ${#entries[@]} == 0 )); then
    echo "No registered patches: codex-rs is identical to the upstream tag, nothing to build."
    return 0
  fi

  # One package set and one target selection for the whole build and run. Cargo resolves
  # features per selection, so testing each crate on its own rebuilds shared dependencies
  # (tokio, rustls, aws-lc, ...) once per feature set. Cargo runs every test binary through
  # `ci.sh run-test-binary`, which applies that binary's filters.
  local packages=() targets=() seen=" "
  for entry in "${entries[@]}"; do
    IFS='|' read -r package target filter <<<"$entry"
    if [[ "$seen" != *" -p $package "* ]]; then
      packages+=(-p "$package")
      seen+="-p $package "
    fi
    if [[ "$target" == lib ]]; then
      [[ "$seen" == *" --lib "* ]] || { targets+=(--lib); seen+="--lib "; }
    elif [[ "$seen" != *" --test $target "* ]]; then
      targets+=(--test "$target")
      seen+="--test $target "
    fi
  done

  local committed_lock base_tag
  committed_lock="$(mktemp)"
  git show HEAD:codex-rs/Cargo.lock > "$committed_lock"
  base_tag="$(python3 "$registry_py" field --rev HEAD base_tag)"
  cd "$(git rev-parse --show-toplevel)/codex-rs"
  export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
  export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
  export RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}"
  export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

  # Upstream's release commit bumps codex-rs/Cargo.toml's version but leaves the workspace
  # members at 0.0.0 in Cargo.lock, so `--locked` fails at every stable tag before it builds
  # anything. Resolve without it first, then fail unless the lock moved in exactly those member
  # versions (bump.sh's cargo metadata gate does the same), and only then build --locked.
  group "resolve Cargo.lock: workspace member versions only"
  cargo metadata --quiet --format-version 1 >/dev/null
  if ! python3 "$registry_py" lock-drift --before "$committed_lock" --after Cargo.lock --version "${base_tag#rust-v}"; then
    rm -f "$committed_lock"
    annotate_error "resolving codex-rs changed Cargo.lock beyond the workspace member versions"
    return 1
  fi
  rm -f "$committed_lock"
  endgroup

  group "cargo test --no-run: build the patch test binaries"
  cargo test --locked "${packages[@]}" "${targets[@]}" --no-run
  endgroup

  local runner log rc
  runner="target.'cfg(all())'.runner = [\"bash\", \"$script_dir/ci.sh\", \"run-test-binary\"]"
  log="$(mktemp)"
  group "cargo test: registered patch tests"
  set +e
  cargo test --locked "${packages[@]}" "${targets[@]}" --no-fail-fast --config "$runner" 2>&1 | tee "$log"
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

  local status=0 binary count
  echo "Registered patch test coverage (passing tests per fork_tests entry):"
  for entry in "${entries[@]}"; do
    IFS='|' read -r package target filter <<<"$entry"
    binary="$(fork_test_binary "$package" "$target")"
    count="$(awk -v b="$binary" -v f="$filter" '$1 == b && (f == "" || index($2, f)) { n++ } END { print n + 0 }' <<<"$passed")"
    printf '  %4d  %s\n' "$count" "$entry"
    if (( count == 0 )); then
      annotate_error "no passing test matches fork_tests entry '$entry'; a test or module was renamed, update INSTAFY-PATCHES.toml"
      status=1
    fi
  done
  if (( rc != 0 )); then
    annotate_error "registered patch tests failed"
    return "$rc"
  fi
  return "$status"
}

# Cargo's test runner hook: `ci.sh run-test-binary <test executable> [args]`. Cargo has
# already set the working directory and CARGO_* environment the test expects; this only
# appends the binary's filters from the registry, one process per filter (some upstream
# tests depend on process-global state, and upstream runs each test in its own process).
run_test_binary() {
  local exe="$1"
  shift
  local name entry package target filter filters=() found=0
  name="$(basename "$exe")"
  name="${name%-*}"
  while IFS= read -r entry; do
    IFS='|' read -r package target filter <<<"$entry"
    if [[ "$(fork_test_binary "$package" "$target")" == "$name" ]]; then
      filters+=("$filter")
      found=1
    fi
  done < <(fork_tests)
  if (( found == 0 )); then
    echo "$name has no registered patch tests; not running it"
    return 0
  fi
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
  echo "usage: $0 workflow-allowlist [REV] | registry [REV] | tree-identity [REV] | self-test | patch-tests" >&2
  exit 2
}

case "${1:-}" in
  workflow-allowlist) shift; workflow_allowlist "$@" ;;
  registry) shift; registry "$@" ;;
  tree-identity) shift; tree_identity "$@" ;;
  self-test) self_test ;;
  patch-tests) patch_tests ;;
  run-test-binary) shift; run_test_binary "$@" ;;
  *) usage ;;
esac
