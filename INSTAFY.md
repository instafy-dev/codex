# Instafy fork

`instafy-dev/codex` is a fork of `openai/codex` that Instafy consumes only as the `codex` git
submodule of [`instafy-dev/instafy`](https://github.com/instafy-dev/instafy).
`packages/runtime-agent` there depends on the `codex-rs` crates by path, and the runtime image
and Desktop app compile them. Nothing is built, signed or released from this fork.

The fork carries **no code patches**. Each branch is an upstream stable tag plus one generated
commit that swaps upstream's automation for the Instafy files. Everything Instafy needs from
Codex lives in code Instafy owns: runtime-agent, the OpenAI proxy and the Desktop runtime agent.
[`INSTAFY-PATCHES.toml`](INSTAFY-PATCHES.toml) is the registry of patches. It is empty, and CI
fails if `codex-rs` differs from the tag at all.

## Branch model

| Branch | Contents | Moves |
|---|---|---|
| `instafy/base/<tag>` | the upstream tag commit plus one generated commit | never |
| `instafy/<tag>` | the base plus one commit per registered patch | fast-forward only (a new patch) |
| `instafy/integration` | a record of every landed bump | fast-forward only |

- **The generated commit** deletes `.github/workflows`, `.github/dependabot.y*ml` and
  `CODEOWNERS`. It adds `.github/workflows/instafy-ci.yml`, `.github/instafy/`, this file and
  `INSTAFY-PATCHES.toml`, whose header it regenerates. `bump.sh` builds it and nothing else
  edits it. It is reproducible: the same tag and Instafy files give the same SHA. Its
  `Instafy-Base-Tag` and `Instafy-Base-Commit` trailers name the tag.
- **A tag is `rust-vX.Y.Z` only.** Stable tags are commits off upstream `main` that no upstream
  branch contains. Always build on `refs/tags/<tag>^{commit}`, never on `main`, a release
  branch or an `-alpha` tag.
- **`instafy/integration` records each bump as a tree-identical merge.**
  `git commit-tree <tip>^{tree} -p <previous integration> -p <tip>` keeps its tree equal to the
  newest tip. `git log --first-parent instafy/integration` lists the bumps, and
  `branch = instafy/integration` in instafy-dev/instafy's `.gitmodules` stays true.
- **The pin is the `instafy/<tag>` tip.** instafy-dev/instafy's `codex` gitlink and
  `approvedGitlinks.codex` point at it. Every tip stays reachable from `instafy/integration`, so
  every old pin still fetches.
- **Branches are immutable.** Never force-push or delete an `instafy/*` branch. A bad bump is
  fixed with a new branch (the next patch release), not a rewrite.
- **`main` is stale.** It still carries upstream's files. Never sync it with the GitHub "Sync
  fork" button and never push to it.
- **Never push a tag.** `rust-v*`, `rusty-v8-v*`, `codex-zsh-v*` and `python-v*` are all
  excluded. For the same reason, never push an upstream commit as a branch tip. A push runs the
  workflows in the pushed commit, and upstream commits carry openai's paid macOS release builds.
  `bump.sh` pushes only `instafy/*` branches whose tips pass the workflow allowlist, and it
  fetches upstream tags into `refs/instafy/upstream-tags/`, never `refs/tags/`, so a
  `git push --tags` or `--follow-tags` has no `rust-v*` tag to carry. The owner can back this
  up with a repository ruleset that restricts tag creation.

## CI

The only workflow is [`.github/workflows/instafy-ci.yml`](.github/workflows/instafy-ci.yml).
It runs on pull requests, on pushes to `instafy/integration`, `instafy/base/**` and
`instafy/rust-v*`, and on manual dispatch. It uses standard `ubuntu-24.04` runners only, never
larger runners, macOS or Windows. While the registry is empty it runs no cargo at all.

- **Workflow allowlist.** Fails if `.github/workflows` holds anything besides `instafy-ci.yml`,
  if a job in it runs on anything but `ubuntu-24.04` or calls a reusable workflow, or if a
  Dependabot config or `CODEOWNERS` file exists.
- **Tree identity.** Has three steps:
  - `ci.sh registry` validates `INSTAFY-PATCHES.toml`. It checks the schema below, checks that
    `closure_dirs` is the path-dependency closure of `closure_roots`, and checks that
    `base_commit` is the commit openai/codex's `base_tag` peels to. That last check is a
    read-only `git ls-remote`.
  - `ci.sh tree-identity` fetches `base_commit` (trees only). It then fails unless
    `git diff <base_commit> HEAD` changes only the Instafy files, deletes only upstream
    automation, and otherwise touches only files that a `[[patch]]` registers. With the
    registry empty, `codex-rs` must be identical to the tag. On a generated base (a commit
    whose only parent is `base_commit`) no registered file may differ: the base's registry
    lists every patch, but its code is the bare tag. Everywhere else the check is exact: every
    registered file must differ. It outputs the number of patches the tree carries, which is
    0 on a generated base.
  - `ci.sh self-test` runs the unit tests of `bump.sh` and `registry.py` against throwaway
    repositories.
- **Registered patch tests.** Runs only when the tree carries a registered patch. It first
  resolves `codex-rs/Cargo.lock` without `--locked` and fails unless only the workspace member
  versions moved (see the cargo gate below). It then builds and runs each patch's `fork_tests`
  with `--locked` in one cargo selection. An entry that matches no passing test fails the job
  instead of passing silently.

instafy-dev/instafy's CI builds and tests runtime-agent against the pinned submodule. That is
the check that matters for Instafy.

GitHub takes each run's workflow from a specific commit:

- a `pull_request` run: the PR's merge commit;
- a push or tag push: the pushed commit;
- a `pull_request_target` run: the base branch;
- scheduled and issue runs: the default branch.

So the allowlist only guards branches that carry it. Upstream workflows that have already run
here (`blocking-ci.yml`, `v8-canary.yml`, `cla.yml`) are disabled in the Actions settings, which
also covers any later re-add under the same path. A workflow that has never run here cannot be
disabled in advance. If an upstream workflow ever starts a run, disable it at once with
`gh workflow disable <file> -R instafy-dev/codex`.

## Bumping to a new upstream tag

**When:**

- Every two weeks, and never more than 30 days behind.
- At once for a security fix or a model Instafy needs.
- Pick the newest stable patch release that is at least 48 hours old.

Run everything as `instafy-bot` from a clone of this fork. `origin` must be
`instafy-dev/codex` and `upstream` must be `openai/codex`. Needs bash 4+ and Python 3.11+.
`bump.sh` never touches your checkout.

```bash
# 1. Build and check everything. Changes no refs and pushes nothing.
bash .github/instafy/bump.sh --dry-run rust-vX.Y.Z

# 2. Create the local branches and push them (new branches only, never tags or force pushes).
bash .github/instafy/bump.sh --push --report /tmp/bump-rust-vX.Y.Z.md rust-vX.Y.Z
```

`bump.sh <tag>` does the following:

1. Refuses anything that is not `rust-vX.Y.Z` or whose `codex-rs/Cargo.toml` version differs
   from the tag. Also refuses a tag older than the current base unless you pass
   `--allow-downgrade`.
2. Fetches only that tag from `upstream`, into `refs/instafy/upstream-tags/<tag>`, and the
   `instafy/*` branches from `origin`.
3. Builds the generated base commit. The Instafy files come from `origin/instafy/integration`
   (change the source with `--overlay-from <ref>`). The registry header is regenerated:
   `base_tag`, `base_commit`, `previous_pin` (the tip that the integration head's land merge
   records, which is the pin being replaced) and `closure_dirs` (recomputed from
   `closure_roots` at the tag).
4. Replays each registered patch from `instafy/<previous tag>` in registry order, with rerere.
   The base keeps its own Instafy files and registry, so a replay carries only the patch's
   code. A patch that has become empty or touches unregistered files stops the bump.
5. Runs the static gates:
   - workflow allowlist on both tips;
   - registry format and closure;
   - tree identity (no patch file on the base, exact at the tip);
   - `cargo metadata` resolution, with no build (skip with `--no-cargo`). Upstream's release
     commit bumps `codex-rs/Cargo.toml` but leaves the workspace crates at `0.0.0` in
     `Cargo.lock`, so `--locked` fails at every stable tag. The gate resolves without it and
     fails unless the lock changes in exactly those member versions.
6. Prints the report, which is also the PR body:
   - upstream commit and diff counts, overall and for the crates Instafy builds;
   - `models.json` changes for `report_models`;
   - feature-flag stage and default changes, plus a "compare by hand" list of flags whose stage
     is an expression rather than a plain `Stage::X`;
   - the patch `range-diff`;
   - the gate results.
7. Creates `instafy/base/<tag>` and `instafy/<tag>` locally. With `--push`, pushes them. With
   `--open-pr`, opens the replay review PR when there are patches.

**On a conflict,** `bump.sh` stops with exit code 3. It prints the patch's purpose and tests,
`git log <previous tag>..<tag> -- <its files>`, and a kept worktree. Resolve the conflict there
and commit with `git -c rerere.enabled=true commit -C <patch commit>`, then re-run the same
`bump.sh` command. rerere replays your resolution.

**Then pin it in instafy-dev/instafy:**

1. Bump the `codex` gitlink to the `instafy/<tag>` tip and `approvedGitlinks.codex` in
   `scripts/public-boundary-policy.json`.
2. Re-seed `packages/runtime-agent/Cargo.lock` from `codex-rs/Cargo.lock`. The copied lock
   still lists the codex crates at `0.0.0` (see the cargo gate above), so let cargo move them
   to the tag version before any `--locked` check.
3. Adapt runtime-agent to upstream's API changes.

The "Public boundary (trusted base)" check fails on every gitlink change **by design**. That
failure is the owner's approval point, so do not "fix" it. Once the public PR is approved, record
the bump:

```bash
bash .github/instafy/bump.sh land --push rust-vX.Y.Z <approved instafy/rust-vX.Y.Z SHA>
```

`land` does the following:

- fetches the tag and checks that the base's parent is the tag commit;
- checks that the tip is the generated base plus one commit per registered patch, each
  changing only its own registered files and the registry's patch section, with the registry
  header (`base_tag`, `base_commit`, ...) byte-identical to the base's;
- re-runs the gates, including the workflow allowlist;
- builds the tree-identical integration merge;
- fast-forwards `instafy/integration` with a plain push;
- prints the pin.

Without `--push`, it writes the merge to the local branch `instafy/integration-next`. With
`--dry-run`, it writes no ref at all.

## The registry and adding a patch

`INSTAFY-PATCHES.toml` documents its own schema:

- **Header:** `base_tag`, `base_commit`, `previous_pin`, `closure_roots`, `closure_dirs` and
  `report_models`.
- **One `[[patch]]` per patch:** `id`, `why`, `billing`, `security`, `files`, `public_tests`,
  `fork_tests`, `upstream`, `instafy_side_alternative`, `drop_when` and `last_validated_tag`.

`bump.sh` regenerates everything above the "Registered patches" line and carries the rest
forward verbatim. Keep `closure_roots` in step with runtime-agent's path dependencies.

A patch may be added only if all of these hold:

- a public black-box test in instafy-dev/instafy fails without it;
- runtime-agent calls no API the patch adds;
- no Instafy-side fix exists;
- an upstream issue is filed and linked (openai/codex takes no external pull requests);
- its fork tests live in files of their own. Shared `mod.rs` test lists caused past replay
  conflicts.

To add one:

1. Branch from `instafy/<tag>`.
2. Make one commit that changes the code, adds the tests and adds the `[[patch]]` entry. End
   the message with the trailer `Instafy-Patch: <id>`.
3. Open a PR into `instafy/<tag>`. CI checks tree identity against the new entry and runs its
   `fork_tests`.
4. Merge with "Rebase and merge" so the branch stays linear.
5. Record it with `bump.sh land <tag>` and pin the new tip.

To drop or edit an entry at the next bump, commit the registry change on a branch off
`instafy/integration` and pass that branch as `--overlay-from`. A commit on the old stack
without a registry entry is dropped.

## Running the checks locally

```bash
bash .github/instafy/ci.sh workflow-allowlist        # or: ... workflow-allowlist <rev>
bash .github/instafy/ci.sh registry                  # INSTAFY_VERIFY_UPSTREAM_TAG=1 adds the ls-remote check
bash .github/instafy/ci.sh tree-identity
bash .github/instafy/ci.sh self-test
bash .github/instafy/ci.sh patch-tests               # builds only when patches are registered
```

Every check reads the committed tree, not your working tree. `patch-tests` builds your
checkout and rewrites the workspace member versions in `codex-rs/Cargo.lock` first, as CI does.
It applies the same settings as CI unless you override them: `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `RUST_MIN_STACK=16777216`. Point `CARGO_TARGET_DIR` at a
scratch directory with about 10 GiB free, and delete it afterwards.
