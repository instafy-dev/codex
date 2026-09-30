# Instafy fork

`instafy-dev/codex` is a fork of `openai/codex` that Instafy consumes only as the
`codex` git submodule of [`instafy-dev/instafy`](https://github.com/instafy-dev/instafy).
`packages/runtime-agent` there depends on the `codex-rs` crates by path, and the runtime
image compiles them for Linux. Nothing is built, signed or released from this fork.

- `main` is the default branch and still carries upstream's files. Never sync it with the
  GitHub "Sync fork" button, and never push upstream tags (`rust-v*`, `rusty-v8-v*`,
  `codex-zsh-v*`, `python-v*`) here: a push runs the workflows in the pushed commit, so
  either would start upstream's paid macOS release builds.
- Instafy work lands on `instafy/integration` through pull requests. Instafy's own commits
  use the `fix(instafy)` / `feat(instafy)` prefix.

## CI

The only workflow is [`.github/workflows/instafy-ci.yml`](.github/workflows/instafy-ci.yml).
It runs on every pull request whose merge commit carries it (stacked PRs included), on pushes
to `instafy/integration` and on manual dispatch, on standard `ubuntu-24.04` runners only. It never uses larger runners, macOS or
Windows.

- **workflow-allowlist** fails if `.github/workflows` contains anything besides
  `instafy-ci.yml`, or if `.github/dependabot.yml`/`.yaml` exists.
- **rust** builds and runs only the tests that cover Instafy's patches, listed per test binary in
  [`.github/instafy/ci.sh`](.github/instafy/ci.sh). An entry that matches no passing test
  fails the job instead of passing silently. The job is skipped when nothing under
  `codex-rs/`, `.github/instafy/` or the workflow changed.

There is no clippy matrix, nextest platform matrix, Bazel, release build or macOS/Windows
coverage here. `instafy-dev/instafy`'s CI builds and tests runtime-agent against the pinned
submodule (including `cargo check --tests` on runtime-agent), which is the check that
matters for Instafy.

## Syncing upstream

Every upstream merge re-adds openai's workflows: paid `macos-15-xlarge` runners, release
jobs, issue bots and schedules. Drop them again in the same merge, before pushing:

```bash
git rm -r -q -f .github/workflows
git checkout HEAD -- .github/workflows/instafy-ci.yml
git rm -q -f --ignore-unmatch .github/dependabot.yml .github/dependabot.yaml .github/CODEOWNERS
bash .github/instafy/ci.sh workflow-allowlist
```

If a sync renames a module or test that `ci.sh` filters on, the rust job names the entry
that matched nothing; update it in `ci.sh`. When a new Instafy patch lands, add its tests
there too.

The allowlist only guards branches that carry this file. GitHub takes a `pull_request` run's
workflows from the PR's merge commit, a push's or tag push's from the pushed commit, a
`pull_request_target` run's from the base branch, and scheduled and issue runs from the
default branch. Upstream workflows that have already run in this repository
(`blocking-ci.yml`, `v8-canary.yml`, `cla.yml`) are disabled in the Actions settings, which
covers any later re-add under the same path. A workflow that has never run here cannot be
disabled in advance, so if an upstream workflow ever starts a run, disable it at once
(`gh workflow disable <file> -R instafy-dev/codex`).

## Running the checks locally

```bash
bash .github/instafy/ci.sh workflow-allowlist
bash .github/instafy/ci.sh rust
```

With rustup installed, `rust` uses the toolchain pinned in `codex-rs/rust-toolchain.toml`,
as CI does. It applies the same settings as CI unless you override them:
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and `RUST_MIN_STACK=8388608`. Point
`CARGO_TARGET_DIR` at a scratch directory with about 10 GiB free, and delete it
afterwards.
