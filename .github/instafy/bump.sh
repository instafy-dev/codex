#!/usr/bin/env bash
# Rebuild the Instafy fork on an upstream openai/codex stable tag, and record approved bumps
# on instafy/integration. See INSTAFY.md ("Bumping to a new upstream tag").
#
#   bash .github/instafy/bump.sh [options] <tag>
#   bash .github/instafy/bump.sh land [options] <tag> [<approved-sha>]
#
# Run it from a clone of instafy-dev/codex whose `upstream` remote is openai/codex. It never
# touches the current checkout: commits are built with plumbing in a private index, patches are
# replayed in a temporary worktree, and refs change only as described below.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: bump.sh [options] <tag>
       bump.sh land [options] <tag> [<approved-sha>]

bump.sh <tag> builds two commits for a stable upstream tag (rust-vX.Y.Z):
  instafy/base/<tag>  the tag commit plus one generated commit that deletes upstream's
                      workflows, Dependabot config and CODEOWNERS and adds the Instafy files
                      (.github/workflows/instafy-ci.yml, .github/instafy/, INSTAFY.md and
                      INSTAFY-PATCHES.toml with a regenerated header);
  instafy/<tag>       the base plus every patch INSTAFY-PATCHES.toml registers, replayed from
                      instafy/<previous tag> with rerere.
It then runs the static gates and prints a report (the review PR body): upstream changes,
catalog and feature-flag differences, range-diff of the patches and gate results.

bump.sh land <tag> [<approved-sha>] records an approved instafy/<tag> tip on
instafy/integration with a tree-identical merge (first parent: the previous integration
head, second parent: the tip), so integration only ever fast-forwards. It prints the pin,
which is the tip itself.

options:
  --dry-run            build and check everything, but update no refs and push nothing
  --push               push the branches (new branches only, never a tag, never a force push)
  --open-pr            with --push: open the review PR (instafy/<tag> into instafy/base/<tag>)
                       when the registry lists patches
  --remote NAME        the instafy-dev/codex remote (default: origin)
  --upstream NAME      the openai/codex remote tags are fetched from (default: upstream)
  --no-fetch           use local refs only
  --overlay-from REF   take the Instafy files and the registry from REF
                       (default: <remote>/instafy/integration)
  --previous-pin SHA   record SHA as previous_pin (default for a new tag: the tip the overlay's
                       land merge records, else the overlay commit; when rebuilding the same
                       tag: the overlay registry's previous_pin)
  --allow-downgrade    allow a tag older than the overlay registry's base_tag
  --no-cargo           skip the cargo metadata gate (resolution only, no build)
  --report FILE        write the report to FILE instead of stdout
  --integration REF    land: the integration head to build on
                       (default: <remote>/instafy/integration)
  --local-branch NAME  land: the local branch that receives the merge
                       (default: instafy/integration-next)
EOF
}

if (( BASH_VERSINFO[0] < 4 )); then
  echo "bump.sh: needs bash 4 or newer (on macOS: brew install bash)" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
registry_py="$script_dir/registry.py"
ci_sh="$script_dir/ci.sh"

TAG_RE='^rust-v[0-9]+\.[0-9]+\.[0-9]+$'
BOT_NAME="instafy-bot"
BOT_EMAIL="306578215+instafy-bot@users.noreply.github.com"
# Paths the generated base commit owns. bump.sh copies them from the overlay commit (the
# registry is regenerated) and restores them after every patch replay.
OVERLAY_PATHS=(.github/workflows/instafy-ci.yml .github/instafy INSTAFY.md)
OWNED_PATHS=(.github/workflows .github/instafy INSTAFY.md INSTAFY-PATCHES.toml)
REMOVED_FILES=(.github/dependabot.yml .github/dependabot.yaml .github/CODEOWNERS CODEOWNERS docs/CODEOWNERS)
# Upstream tags are fetched here, never into refs/tags: a `git push --tags` or `--follow-tags`
# would otherwise carry them to the fork, and a rust-v* tag push runs openai's release workflow
# on paid macOS runners.
UPSTREAM_TAGS=refs/instafy/upstream-tags

dry_run=0 push=0 open_pr=0 fetch=1 cargo_gate=1 allow_downgrade=0
remote=origin upstream=upstream overlay_from="" previous_pin="" report_file=""
integration="" local_branch="instafy/integration-next"
command=bump
positional=()

die() {
  echo "bump.sh: error: $*" >&2
  exit 1
}

note() {
  echo "bump.sh: $*" >&2
}

while (( $# )); do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --push) push=1 ;;
    --open-pr) open_pr=1 ;;
    --remote) remote="${2:?--remote needs a value}"; shift ;;
    --upstream) upstream="${2:?--upstream needs a value}"; shift ;;
    --no-fetch) fetch=0 ;;
    --overlay-from) overlay_from="${2:?--overlay-from needs a value}"; shift ;;
    --previous-pin) previous_pin="${2:?--previous-pin needs a value}"; shift ;;
    --allow-downgrade) allow_downgrade=1 ;;
    --no-cargo) cargo_gate=0 ;;
    --report) report_file="${2:?--report needs a value}"; shift ;;
    --integration) integration="${2:?--integration needs a value}"; shift ;;
    --local-branch) local_branch="${2:?--local-branch needs a value}"; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) usage >&2; die "unknown option $1" ;;
    *) positional+=("$1") ;;
  esac
  shift
done

if (( ${#positional[@]} )) && [[ "${positional[0]}" == land ]]; then
  command=land
  positional=("${positional[@]:1}")
fi
case "$command:${#positional[@]}" in
  bump:1 | land:1 | land:2) ;;
  *) usage >&2; exit 2 ;;
esac
tag="${positional[0]}"
approved="${positional[1]:-}"

if (( open_pr && !push )); then
  die "--open-pr needs --push: the PR's branches must exist on $remote"
fi
if (( dry_run && push )); then
  die "--dry-run and --push are mutually exclusive"
fi
if ! [[ "$tag" =~ $TAG_RE ]]; then
  die "$tag is not a stable upstream tag (rust-vX.Y.Z); alpha and other tags are never a base"
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "run bump.sh inside a clone of instafy-dev/codex"
cd "$repo_root"

work="$(mktemp -d "${TMPDIR:-/tmp}/instafy-bump.XXXXXX")"
keep_work=0
worktrees=()
cleanup() {
  local wt
  for wt in ${worktrees[@]+"${worktrees[@]}"}; do
    git worktree remove --force "$wt" >/dev/null 2>&1 || true
  done
  if (( keep_work )); then
    note "kept $work"
  else
    rm -rf "$work"
  fi
}
trap cleanup EXIT

# rerere for every git command this script runs, without touching the repository's config.
export GIT_CONFIG_COUNT=2
export GIT_CONFIG_KEY_0=rerere.enabled GIT_CONFIG_VALUE_0=true
export GIT_CONFIG_KEY_1=rerere.autoUpdate GIT_CONFIG_VALUE_1=true

gates=()
gate() {
  local name="$1"
  shift
  local out
  if out="$("$@" 2>&1)"; then
    # The command's last line is its one-line result.
    local detail
    detail="$(tail -n 1 <<<"$out")"
    gates+=("- [x] $name${detail:+: $detail}")
    note "gate ok: $name${detail:+: $detail}"
  else
    printf '%s\n' "$out" >&2
    die "gate failed: $name"
  fi
}

rev_or_empty() {
  git rev-parse --verify --quiet "$1^{commit}" 2>/dev/null || true
}

# A fork branch, preferring the local branch over the remote-tracking one.
resolve_branch() {
  local name="$1" sha
  sha="$(rev_or_empty "refs/heads/$name")"
  [[ -n "$sha" ]] || sha="$(rev_or_empty "refs/remotes/$remote/$name")"
  echo "$sha"
}

# version_lt rust-vA.B.C rust-vX.Y.Z: true when the first tag is the older release.
version_lt() {
  local a b i
  IFS=. read -r -a a <<<"${1#rust-v}"
  IFS=. read -r -a b <<<"${2#rust-v}"
  for i in 0 1 2; do
    (( 10#${a[i]} < 10#${b[i]} )) && return 0
    (( 10#${a[i]} > 10#${b[i]} )) && return 1
  done
  return 1
}

raw_date() {
  git log -1 --format=%cd --date=raw "$1"
}

as_bot() {
  local date="$1"
  shift
  env GIT_AUTHOR_NAME="$BOT_NAME" GIT_AUTHOR_EMAIL="$BOT_EMAIL" GIT_AUTHOR_DATE="$date" \
    GIT_COMMITTER_NAME="$BOT_NAME" GIT_COMMITTER_EMAIL="$BOT_EMAIL" GIT_COMMITTER_DATE="$date" \
    "$@"
}

trailer() {
  git log -1 --format="%(trailers:key=$2,valueonly,separator=%x2C)" "$1" | sed -e 's/[[:space:]]*$//'
}

fetch_remote_branches() {
  (( fetch )) || return 0
  note "fetching instafy/* branches from $remote"
  git fetch --quiet --no-tags "$remote" "+refs/heads/instafy/*:refs/remotes/$remote/instafy/*"
}

# Not forced: if upstream ever moves a stable tag, the fetch fails instead of following it.
fetch_upstream_tag() {
  (( fetch )) || return 0
  note "fetching $tag from $upstream into $UPSTREAM_TAGS/$tag"
  git fetch --quiet --no-tags "$upstream" "refs/tags/$tag:$UPSTREAM_TAGS/$tag"
}

# The commit the upstream tag peels to. With --no-fetch, a local refs/tags/<tag> also counts.
upstream_tag_commit() {
  local sha
  sha="$(rev_or_empty "$UPSTREAM_TAGS/$tag")"
  [[ -n "$sha" ]] || sha="$(rev_or_empty "refs/tags/$tag")"
  [[ -n "$sha" ]] || die "tag $tag is not available locally; fetch it from $upstream (drop --no-fetch)"
  echo "$sha"
}

# Create or confirm a local branch. Fork branches are immutable: never move one.
set_branch() {
  local name="$1" sha="$2" existing
  existing="$(rev_or_empty "refs/heads/$name")"
  if [[ -z "$existing" ]]; then
    git update-ref "refs/heads/$name" "$sha" ""
    note "created local branch $name at $sha"
  elif [[ "$existing" == "$sha" ]]; then
    note "local branch $name is already at $sha"
  else
    die "local branch $name is at $existing, not $sha. Fork branches are immutable; delete the local branch only if it was never pushed."
  fi
}

# Push new branches only. Never a tag, never a force push, never over a different commit.
push_branches() {
  local refspecs=() name sha remote_sha pair
  for pair in "$@"; do
    name="${pair%%=*}"
    sha="${pair#*=}"
    [[ "$name" == instafy/* ]] || die "refusing to push $name: bump.sh pushes only instafy/* branches"
    remote_sha="$(git ls-remote "$remote" "refs/heads/$name" | cut -f1)"
    if [[ "$remote_sha" == "$sha" ]]; then
      note "$remote/$name is already at $sha"
    elif [[ -n "$remote_sha" ]]; then
      die "$remote/$name is at $remote_sha, not $sha. Fork branches are immutable; bump a new tag instead."
    else
      refspecs+=("$sha:refs/heads/$name")
    fi
  done
  if (( ${#refspecs[@]} )); then
    git push --no-follow-tags "$remote" "${refspecs[@]}"
  fi
}

allowlist_gate() {
  gate "workflow allowlist, $1" bash "$ci_sh" workflow-allowlist "$2"
}

registry_gate() {
  gate "registry format and closure, $1" python3 "$registry_py" check --rev "$2"
}

# ---------------------------------------------------------------------------------------------
# bump <tag>

build_base() {
  local index="$work/index"
  export GIT_INDEX_FILE="$index"
  git read-tree "$tag_commit"

  # Upstream automation, plus anything upstream ever puts at the paths the base owns.
  local removed=() path
  while IFS= read -r -d '' path; do
    removed+=("$path")
  done < <(git ls-tree -r -z --name-only "$tag_commit" -- .github/workflows .github/instafy INSTAFY.md INSTAFY-PATCHES.toml)
  local workflow_count removed_other=()
  workflow_count="$(printf '%s\n' "${removed[@]+"${removed[@]}"}" | grep -c '^\.github/workflows/' || true)"
  for path in "${REMOVED_FILES[@]}"; do
    if git cat-file -e "$tag_commit:$path" 2>/dev/null; then
      removed+=("$path")
      removed_other+=("$path")
    fi
  done
  if (( ${#removed[@]} )); then
    printf '%s\0' "${removed[@]}" | git update-index -z --force-remove --stdin
  fi

  git ls-tree -r -z "$overlay_sha" -- "${OVERLAY_PATHS[@]}" | git update-index -z --index-info

  python3 "$registry_py" render --from "$overlay_sha" --base-tag "$tag" \
    --base-commit "$tag_commit" --previous-pin "$previous_pin" > "$work/registry.toml"
  local blob
  blob="$(git hash-object -w "$work/registry.toml")"
  printf '100644 blob %s\tINSTAFY-PATCHES.toml\n' "$blob" | git update-index --index-info

  local tree
  tree="$(git write-tree)"
  unset GIT_INDEX_FILE

  local deleted=".github/workflows ($workflow_count files)" path_
  for path_ in ${removed_other[@]+"${removed_other[@]}"}; do
    deleted+=", $path_"
  done
  cat > "$work/base-message" <<EOF
chore(instafy): generate base for $tag

Generated by .github/instafy/bump.sh on top of the upstream tag
$tag ($tag_commit).

- Deletes upstream's automation, which never runs in this fork:
  $deleted.
- Adds .github/workflows/instafy-ci.yml, .github/instafy/, INSTAFY.md and
  INSTAFY-PATCHES.toml (base_tag $tag, previous_pin ${previous_pin:0:12}).

codex-rs is byte-identical to the tag. The commit is reproducible: bump.sh
builds the same SHA again from the same tag and Instafy files.

Instafy-Base-Tag: $tag
Instafy-Base-Commit: $tag_commit
EOF
  base_sha="$(as_bot "$tag_date" git commit-tree "$tree" -p "$tag_commit" -F "$work/base-message")"
  note "base commit $base_sha (tree $tree)"
}

replay_patches() {
  local ids=() id
  while IFS= read -r id; do
    ids+=("$id")
  done < <(python3 "$registry_py" patch-ids --rev "$base_sha")
  patch_count="${#ids[@]}"
  if (( patch_count == 0 )); then
    tip_sha="$base_sha"
    note "no registered patches: instafy/$tag is the base commit"
    return 0
  fi

  prev_base_sha="$(resolve_branch "instafy/base/$prev_tag")"
  prev_tip_sha="$(resolve_branch "instafy/$prev_tag")"
  [[ -n "$prev_base_sha" && -n "$prev_tip_sha" ]] \
    || die "the registry lists $patch_count patches, but instafy/base/$prev_tag or instafy/$prev_tag is missing locally and on $remote"
  git merge-base --is-ancestor "$prev_base_sha" "$prev_tip_sha" \
    || die "instafy/base/$prev_tag is not an ancestor of instafy/$prev_tag"
  [[ -z "$(git rev-list --merges "$prev_base_sha..$prev_tip_sha")" ]] \
    || die "instafy/$prev_tag has merge commits above its base; patches must be single commits"

  # Map every commit above the previous base to its Instafy-Patch trailer.
  declare -A commit_of=()
  local commit pid
  while IFS= read -r commit; do
    pid="$(trailer "$commit" Instafy-Patch)"
    [[ -n "$pid" ]] || die "commit $commit on instafy/$prev_tag has no Instafy-Patch trailer; every commit above the base must be one registered patch"
    [[ -z "${commit_of[$pid]:-}" ]] || die "two commits on instafy/$prev_tag carry Instafy-Patch: $pid; squash them"
    commit_of[$pid]="$commit"
  done < <(git rev-list --reverse "$prev_base_sha..$prev_tip_sha")
  for pid in "${!commit_of[@]}"; do
    if ! printf '%s\n' "${ids[@]}" | grep -qxF "$pid"; then
      note "dropping patch $pid (${commit_of[$pid]}): no longer registered"
    fi
  done

  local wt="$work/replay"
  git worktree add --quiet --detach "$wt" "$base_sha"
  worktrees+=("$wt")
  local file registered pick_log="$work/cherry-pick.log" picked
  for id in "${ids[@]}"; do
    commit="${commit_of[$id]:-}"
    [[ -n "$commit" ]] || die "patch $id is registered, but no commit on instafy/$prev_tag carries Instafy-Patch: $id"
    note "replaying $id ($commit)"
    picked=1
    git -C "$wt" cherry-pick --no-commit "$commit" >"$pick_log" 2>&1 || picked=0
    if grep -q "using previous resolution" "$pick_log"; then
      note "rerere reused a recorded resolution for $id"
    fi
    # The base owns the Instafy files and the registry; a patch commit may have registered
    # itself, but its replay carries only its code.
    git -C "$wt" restore --source="$base_sha" --staged --worktree -- "${OWNED_PATHS[@]}" 2>/dev/null || true
    local unresolved
    unresolved="$(git -C "$wt" diff --name-only --diff-filter=U)"
    if [[ -n "$unresolved" ]]; then
      keep_work=1
      worktrees=()
      {
        echo
        echo "Patch $id does not apply cleanly on $tag. Unresolved: $(tr '\n' ' ' <<<"$unresolved")"
        sed 's/^/  | /' "$pick_log"
        python3 "$registry_py" patch-info --rev "$base_sha" "$id" | sed 's/^/  /'
        echo "Upstream commits touching its files since $prev_tag:"
        mapfile -t registered < <(python3 "$registry_py" patch-files --rev "$base_sha" "$id")
        git log --oneline --no-decorate "$prev_base_commit..$tag_commit" -- "${registered[@]}" | sed 's/^/  /'
        echo
        echo "Resolve it in $wt and record the resolution with rerere:"
        echo "  cd $wt"
        echo "  # edit the conflicted files, then"
        echo "  git add -- <files>"
        echo "  git -c rerere.enabled=true commit --no-verify -C $commit"
        echo "Then re-run bump.sh with the same arguments: rerere replays the resolution."
        echo "Afterwards: git worktree remove --force $wt"
      } >&2
      exit 3
    fi
    if git -C "$wt" diff --cached --quiet HEAD; then
      (( picked )) || { cat "$pick_log" >&2; die "cherry-pick of patch $id ($commit) failed"; }
      die "patch $id is empty on $tag: upstream already contains it. Remove it from INSTAFY-PATCHES.toml (commit the edit on a branch off instafy/integration and pass --overlay-from)."
    fi
    env GIT_COMMITTER_NAME="$BOT_NAME" GIT_COMMITTER_EMAIL="$BOT_EMAIL" GIT_COMMITTER_DATE="$tag_date" \
      git -C "$wt" commit --quiet --no-verify -C "$commit"
    mapfile -t registered < <(python3 "$registry_py" patch-files --rev "$base_sha" "$id")
    while IFS= read -r file; do
      printf '%s\n' "${registered[@]}" | grep -qxF "$file" \
        || die "patch $id changes $file, which its [[patch]] entry does not register"
    done < <(git -C "$wt" diff-tree --no-commit-id --name-only -r --no-renames HEAD)
  done
  tip_sha="$(git -C "$wt" rev-parse HEAD)"
  note "patched tip $tip_sha ($patch_count patches)"
}

# Resolution only: no build, no target dir. Needs the crates.io index and git dependencies.
# Upstream's release commit bumps codex-rs/Cargo.toml's version but not Cargo.lock, where the
# workspace members stay at 0.0.0, so `--locked` fails at every stable tag. Resolve without it
# and fail unless the lock changes in exactly those member versions.
cargo_metadata_gate() {
  local wt="$work/metadata"
  git worktree add --quiet --detach "$wt" "$tip_sha"
  worktrees+=("$wt")
  cp "$wt/codex-rs/Cargo.lock" "$work/Cargo.lock.committed"
  gate "cargo metadata resolves codex-rs" \
    bash -c 'cargo metadata --quiet --format-version 1 --manifest-path "$1" >/dev/null' _ "$wt/codex-rs/Cargo.toml"
  gate "Cargo.lock drift" \
    python3 "$registry_py" lock-drift --before "$work/Cargo.lock.committed" \
    --after "$wt/codex-rs/Cargo.lock" --version "${tag#rust-v}"
}

write_report() {
  local closure_dirs=() ahead
  mapfile -t closure_dirs < <(git show "$base_sha:INSTAFY-PATCHES.toml" | python3 -c '
import sys, tomllib
for d in tomllib.loads(sys.stdin.read())["closure_dirs"]:
    print(d)')
  echo "# instafy/$tag"
  echo
  echo "- Upstream tag: \`$tag\` -> \`$tag_commit\`"
  echo "- \`instafy/base/$tag\`: \`$base_sha\` (the tag plus one generated commit)"
  echo "- \`instafy/$tag\`: \`$tip_sha\` ($patch_count registered patches)"
  echo "- Instafy files from: \`$overlay_ref\` (\`$overlay_sha\`)"
  echo "- Previous base: \`$prev_tag\` (\`$prev_base_commit\`); previous pin \`$previous_pin\`"
  echo
  echo "## Upstream changes since $prev_tag"
  echo
  if [[ "$prev_base_commit" == "$tag_commit" ]]; then
    echo "Same tag: no upstream changes."
  elif git merge-base --is-ancestor "$prev_base_commit" "$tag_commit" 2>/dev/null; then
    echo "- $(git rev-list --count "$prev_base_commit..$tag_commit") upstream commits"
  else
    ahead="$(git rev-list --count "$prev_base_commit...$tag_commit" 2>/dev/null || echo '?')"
    echo "- $tag does not descend from $prev_tag ($ahead commits differ on either side)"
  fi
  local stat
  stat="$(git diff --shortstat "$prev_base_commit" "$tag_commit" -- codex-rs)"
  echo "- codex-rs:${stat:- no changes}"
  stat="$(git diff --shortstat "$prev_base_commit" "$tag_commit" -- "${closure_dirs[@]}")"
  echo "- the ${#closure_dirs[@]} crates Instafy builds (closure_dirs):${stat:- no changes}"
  echo
  echo "## Catalog and feature flags"
  echo
  echo '```'
  python3 "$registry_py" report --old "$prev_base_commit" --new "$tag_commit" --models-from "$base_sha"
  echo '```'
  echo
  echo "## Patches"
  echo
  if (( patch_count == 0 )); then
    echo "No registered patches: \`instafy/$tag\` is \`instafy/base/$tag\`, and codex-rs is identical to $tag."
  else
    echo '```'
    if [[ -n "${prev_base_sha:-}" ]]; then
      git range-diff --no-color "$prev_base_sha..$prev_tip_sha" "$base_sha..$tip_sha"
    fi
    echo '```'
  fi
  echo
  echo "## Gates"
  echo
  printf '%s\n' "${gates[@]}"
}

bump() {
  fetch_upstream_tag
  fetch_remote_branches
  tag_commit="$(upstream_tag_commit)"
  tag_date="$(raw_date "$tag_commit")"
  local version
  version="$(python3 "$registry_py" tag-version --rev "$tag_commit")"
  [[ "rust-v$version" == "$tag" ]] \
    || die "$tag's codex-rs/Cargo.toml says version \"$version\"; refusing a tag whose version does not match"
  gates+=("- [x] tag and version: $tag is a stable tag and codex-rs/Cargo.toml says $version")

  overlay_ref="${overlay_from:-refs/remotes/$remote/instafy/integration}"
  overlay_sha="$(rev_or_empty "$overlay_ref")"
  [[ -n "$overlay_sha" ]] || die "overlay source $overlay_ref does not exist; pass --overlay-from"
  local f
  for f in .github/workflows/instafy-ci.yml .github/instafy/bump.sh .github/instafy/ci.sh \
    .github/instafy/registry.py INSTAFY.md INSTAFY-PATCHES.toml; do
    git cat-file -e "$overlay_sha:$f" 2>/dev/null || die "overlay source $overlay_ref has no $f"
  done
  for f in bump.sh ci.sh registry.py; do
    if [[ "$(git hash-object "$script_dir/$f")" != "$(git rev-parse "$overlay_sha:.github/instafy/$f")" ]]; then
      note "warning: $script_dir/$f differs from $overlay_ref; the gates run with your local copy"
    fi
  done

  prev_tag="$(python3 "$registry_py" field --rev "$overlay_sha" base_tag)"
  prev_base_commit="$(python3 "$registry_py" field --rev "$overlay_sha" base_commit)"
  [[ "$prev_tag" =~ $TAG_RE ]] || die "the registry at $overlay_ref has base_tag \"$prev_tag\""
  git cat-file -e "$prev_base_commit^{commit}" 2>/dev/null \
    || die "the registry at $overlay_ref names base_commit $prev_base_commit, which this clone lacks"
  if version_lt "$tag" "$prev_tag" && (( !allow_downgrade )); then
    die "$tag is older than the current base $prev_tag; pass --allow-downgrade to build it anyway"
  fi
  if [[ -z "$previous_pin" ]]; then
    if [[ "$tag" == "$prev_tag" ]]; then
      # Rebuilding the same tag keeps its record, so the base comes out identical.
      previous_pin="$(python3 "$registry_py" field --rev "$overlay_sha" previous_pin)"
    else
      # The pin is the landed instafy/<tag> tip, which a land merge names in its trailer.
      previous_pin="$(trailer "$overlay_sha" Instafy-Landed-Tip)"
      previous_pin="${previous_pin:-$overlay_sha}"
    fi
  fi
  previous_pin="$(git rev-parse --verify "$previous_pin^{commit}" 2>/dev/null)" \
    || die "previous pin $previous_pin is not a commit in this clone"

  build_base
  allowlist_gate "instafy/base/$tag" "$base_sha"
  registry_gate "instafy/base/$tag" "$base_sha"
  gate "tree identity, instafy/base/$tag" \
    python3 "$registry_py" tree-identity --rev "$base_sha" --base
  [[ -z "$(git diff --no-renames --name-only "$tag_commit" "$base_sha" -- codex-rs)" ]] \
    || die "codex-rs differs between $tag and the generated base"

  replay_patches
  if [[ "$tip_sha" != "$base_sha" ]]; then
    allowlist_gate "instafy/$tag" "$tip_sha"
    registry_gate "instafy/$tag" "$tip_sha"
  fi
  gate "exact tree identity, instafy/$tag" \
    python3 "$registry_py" tree-identity --rev "$tip_sha" --exact
  if (( cargo_gate )); then
    cargo_metadata_gate
  else
    gates+=("- [ ] cargo metadata: skipped (--no-cargo)")
  fi

  if [[ -n "$report_file" ]]; then
    write_report > "$report_file"
    note "report written to $report_file"
  else
    write_report
  fi

  if (( dry_run )); then
    note "dry run: no refs changed, nothing pushed"
  else
    set_branch "instafy/base/$tag" "$base_sha"
    set_branch "instafy/$tag" "$tip_sha"
    if (( push )); then
      push_branches "instafy/base/$tag=$base_sha" "instafy/$tag=$tip_sha"
      if (( open_pr )); then
        open_review_pr
      fi
    fi
  fi
  echo "base: $base_sha" >&2
  echo "tip:  $tip_sha" >&2
}

open_review_pr() {
  if (( patch_count == 0 )); then
    note "no registered patches, so there is no replay to review; the approval point is the gitlink bump in instafy-dev/instafy"
    return 0
  fi
  local slug body="$report_file"
  slug="$(git remote get-url "$remote" | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')"
  if [[ -z "$body" ]]; then
    body="$work/report.md"
    write_report > "$body"
  fi
  gh pr create --repo "$slug" --base "instafy/base/$tag" --head "instafy/$tag" \
    --title "instafy/$tag: replay $patch_count patches on $tag" --body-file "$body"
}

# ---------------------------------------------------------------------------------------------
# land <tag> [<approved-sha>]

land() {
  fetch_remote_branches
  local base tip old
  base="$(resolve_branch "instafy/base/$tag")"
  [[ -n "$base" ]] || die "instafy/base/$tag does not exist locally or on $remote"
  if [[ -n "$approved" ]]; then
    tip="$(rev_or_empty "$approved")"
    [[ -n "$tip" ]] || die "$approved is not a commit"
  else
    tip="$(resolve_branch "instafy/$tag")"
    [[ -n "$tip" ]] || die "instafy/$tag does not exist locally or on $remote; pass the approved SHA"
  fi
  local branch_tip
  branch_tip="$(resolve_branch "instafy/$tag")"
  if [[ -n "$branch_tip" && "$branch_tip" != "$tip" ]]; then
    die "instafy/$tag is at $branch_tip, not the approved $tip"
  fi

  [[ "$(trailer "$base" Instafy-Base-Tag)" == "$tag" ]] \
    || die "instafy/base/$tag ($base) is not a base generated by bump.sh for $tag"
  fetch_upstream_tag
  local tag_commit
  tag_commit="$(upstream_tag_commit)"
  [[ "$(git rev-parse "$base^")" == "$tag_commit" ]] \
    || die "instafy/base/$tag's parent is not the $tag commit ($tag_commit)"
  git merge-base --is-ancestor "$base" "$tip" || die "$tip does not descend from instafy/base/$tag"
  [[ -z "$(git rev-list --merges "$base..$tip")" ]] || die "$tip has merge commits above its base"
  local commit
  while IFS= read -r commit; do
    [[ -n "$(trailer "$commit" Instafy-Patch)" ]] \
      || die "$commit above instafy/base/$tag has no Instafy-Patch trailer"
  done < <(git rev-list "$base..$tip")
  [[ "$(python3 "$registry_py" field --rev "$tip" base_tag)" == "$tag" ]] \
    || die "the registry at $tip is not for $tag"

  allowlist_gate "instafy/$tag" "$tip"
  registry_gate "instafy/$tag" "$tip"
  # The commits above the base: registered patches changing only their own files, the registry
  # header (base_tag, base_commit) untouched, and base_commit the base's parent, the tag commit.
  gate "generated base plus registered patches only, instafy/$tag" \
    python3 "$registry_py" land-check --base "$base" --tip "$tip"
  gate "exact tree identity, instafy/$tag" python3 "$registry_py" tree-identity --rev "$tip" --exact

  old="$(rev_or_empty "${integration:-refs/remotes/$remote/instafy/integration}")"
  [[ -n "$old" ]] || die "no integration head; pass --integration"
  if git merge-base --is-ancestor "$tip" "$old"; then
    note "instafy/$tag ($tip) is already recorded on the integration head $old"
    echo "pin: $tip"
    return 0
  fi

  # Reproducible: the newer of the two parents' committer dates, so first-parent dates never
  # go backwards and re-running land builds the same SHA.
  local old_date tip_date date
  old_date="$(raw_date "$old")"
  tip_date="$(raw_date "$tip")"
  if (( ${old_date%% *} >= ${tip_date%% *} )); then date="$old_date"; else date="$tip_date"; fi
  cat > "$work/land-message" <<EOF
chore(instafy): land $tag

Records the instafy/$tag tip on instafy/integration. The tree is
identical to that tip, and the first parent is the previous integration
head, so instafy/integration only fast-forwards and
\`git log --first-parent instafy/integration\` lists every bump.

Instafy-Landed-Tag: $tag
Instafy-Landed-Tip: $tip
Instafy-Previous-Integration: $old
EOF
  local merge
  merge="$(as_bot "$date" git commit-tree "$tip^{tree}" -p "$old" -p "$tip" -F "$work/land-message")"
  [[ "$(git rev-parse "$merge^{tree}")" == "$(git rev-parse "$tip^{tree}")" ]] \
    || die "the integration merge's tree differs from $tip"
  git merge-base --is-ancestor "$old" "$merge" || die "the integration merge does not fast-forward $old"
  note "integration merge $merge (tree $(git rev-parse "$merge^{tree}") = $tip^{tree}; parents $old $tip)"

  if (( dry_run )); then
    note "dry run: no refs changed, nothing pushed"
  else
    local existing
    existing="$(rev_or_empty "refs/heads/$local_branch")"
    if [[ -n "$existing" && "$existing" != "$merge" ]] && ! git merge-base --is-ancestor "$existing" "$merge"; then
      die "local branch $local_branch is at $existing, which $merge does not fast-forward"
    fi
    # The old value makes this a compare-and-swap; an empty one means "create only".
    git update-ref "refs/heads/$local_branch" "$merge" "$existing"
    note "local branch $local_branch is at $merge"
    if (( push )); then
      push_branches "instafy/base/$tag=$base" "instafy/$tag=$tip"
      # A plain push: the remote rejects anything but a fast-forward of instafy/integration.
      git push --no-follow-tags "$remote" "$merge:refs/heads/instafy/integration"
    fi
  fi
  echo "integration: $merge"
  echo "pin: $tip"
  note "the pin is the instafy/$tag tip: instafy-dev/instafy's codex gitlink and approvedGitlinks.codex" \
    "in scripts/public-boundary-policy.json point at it"
}

case "$command" in
  bump) bump ;;
  land) land ;;
esac
