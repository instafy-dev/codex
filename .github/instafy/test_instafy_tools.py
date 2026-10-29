"""Self-test for bump.sh, ci.sh and registry.py (run by `ci.sh self-test`).

Every test builds a throwaway "upstream" repository with stable tags, a fork clone of it and a
local bare `origin`, and drives the real scripts from this directory against them. Nothing
here reads the network or the repository the tests live in.
"""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import textwrap
import tomllib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
BUMP = HERE / "bump.sh"
CI_SH = HERE / "ci.sh"
REGISTRY_PY = HERE / "registry.py"
REPO_ROOT = HERE.parent.parent
OVERLAY_SOURCES = {
    ".github/instafy/bump.sh": BUMP,
    ".github/instafy/ci.sh": CI_SH,
    ".github/instafy/registry.py": REGISTRY_PY,
    ".github/instafy/test_instafy_tools.py": pathlib.Path(__file__).resolve(),
    ".github/workflows/instafy-ci.yml": REPO_ROOT / ".github/workflows/instafy-ci.yml",
    "INSTAFY.md": REPO_ROOT / "INSTAFY.md",
}

CORE_LIB = "".join(f"pub fn line_{n}() -> u32 {{ {n} }}\n" for n in range(1, 9))
# Beta's stage is an expression, like upstream's PreventIdleSleep: the report cannot parse it.
FEATURES_RS = """\
pub const FEATURES: &[FeatureSpec] = &[
    FeatureSpec {{
        id: Feature::Alpha,
        key: "alpha",
        stage: Stage::{alpha_stage},
        default_enabled: {alpha_default},
    }},
    FeatureSpec {{
        id: Feature::Beta,
        key: "beta",
        stage: if cfg!(target_os = "macos") {{
            Stage::Experimental
        }} else {{
            Stage::{beta_fallback}
        }},
        default_enabled: false,
    }},
];
"""
# Upstream's release commits bump codex-rs/Cargo.toml but leave the workspace members at 0.0.0.
CARGO_LOCK = """\
version = 4

[[package]]
name = "codex-core"
version = "0.0.0"
dependencies = [
 "codex-util",
 "serde",
]

[[package]]
name = "codex-util"
version = "0.0.0"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"
"""
# Stands in for cargo in the patch-tests self-test. It logs each call, rewrites the workspace
# member versions on `metadata` as cargo does (FAKE_CARGO_DRIFT also changes a dependency), and
# refuses --locked while the lock is stale, as cargo does at every stable tag.
FAKE_CARGO = """\
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$FAKE_CARGO_LOG"
version="$(sed -n 's/^version = "\\(.*\\)"$/\\1/p' Cargo.toml)"
case "$1" in
  metadata)
    sed -i.bak "s/^version = \\"0\\.0\\.0\\"$/version = \\"$version\\"/" Cargo.lock
    if [[ -n "${FAKE_CARGO_DRIFT:-}" ]]; then sed -i.bak 's/"aaaa"/"bbbb"/' Cargo.lock; fi
    rm -f Cargo.lock.bak
    ;;
  test)
    if grep -q '^version = "0.0.0"$' Cargo.lock; then
      echo "error: cannot update the lock file Cargo.lock because --locked was passed" >&2
      exit 101
    fi
    if [[ " $* " == *" --no-run "* ]]; then exit 0; fi
    echo "     Running unittests src/lib.rs (target/debug/deps/codex_core-0123abcd)"
    echo "test tests::line_five ... ok"
    ;;
esac
"""


def replace_line(text: str, number: int, new: str) -> str:
    lines = text.splitlines(keepends=True)
    lines[number - 1] = new + "\n"
    return "".join(lines)


class Fixture:
    """An upstream repository with tags, a fork clone and a bare origin, in a temp dir."""

    def __init__(self, root: pathlib.Path):
        self.root = root
        self.upstream = root / "upstream"
        self.fork = root / "fork"
        self.origin = root / "origin.git"
        self.clock = 1_700_000_000
        # No inherited git or GitHub Actions state: in CI, GITHUB_ACTIONS would switch ci.sh to
        # annotations and to the network check of base_commit against openai/codex.
        self.env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(("GIT_", "GITHUB_", "RUNNER_", "INSTAFY_"))
        }
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
            HOME=str(root),
            TMPDIR=str(root),
            GIT_AUTHOR_NAME="Upstream Dev",
            GIT_AUTHOR_EMAIL="dev@example.com",
            GIT_COMMITTER_NAME="Upstream Dev",
            GIT_COMMITTER_EMAIL="dev@example.com",
        )
        self.tags: dict[str, str] = {}

    # -- helpers ------------------------------------------------------------------------------

    def run(self, *args, cwd=None, check=True, env=None) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            [str(a) for a in args],
            cwd=cwd or self.fork,
            env={**self.env, **(env or {})},
            capture_output=True,
            text=True,
        )
        if check and proc.returncode != 0:
            raise AssertionError(
                f"{' '.join(map(str, args))} failed ({proc.returncode})\n"
                f"stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
            )
        return proc

    def git(self, *args, cwd=None, check=True) -> str:
        return self.run("git", *args, cwd=cwd, check=check).stdout.strip()

    def tick(self) -> dict:
        self.clock += 60
        stamp = f"{self.clock} +0000"
        return {"GIT_AUTHOR_DATE": stamp, "GIT_COMMITTER_DATE": stamp}

    def write(self, repo: pathlib.Path, files: dict[str, str | None]) -> None:
        for path, content in files.items():
            target = repo / path
            if content is None:
                self.git("rm", "-q", "-r", "--", path, cwd=repo)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
            self.git("add", "--", path, cwd=repo)

    def commit(self, repo: pathlib.Path, message: str, files: dict[str, str | None]) -> str:
        self.write(repo, files)
        self.run("git", "commit", "-q", "--allow-empty", "-m", message, cwd=repo, env=self.tick())
        return self.git("rev-parse", "HEAD", cwd=repo)

    def workspace_toml(self, version: str) -> str:
        return textwrap.dedent(
            f"""\
            [workspace]
            members = ["core", "util"]
            resolver = "2"

            [workspace.package]
            version = "{version}"

            [workspace.dependencies]
            codex-util = {{ path = "util" }}
            """
        )

    def upstream_release(self, tag: str, version: str, files: dict[str, str | None]) -> str:
        sha = self.commit(
            self.upstream,
            f"release {version}",
            {"codex-rs/Cargo.toml": self.workspace_toml(version), **files},
        )
        self.run("git", "tag", "-a", "-m", tag, tag, cwd=self.upstream, env=self.tick())
        self.tags[tag] = sha
        return sha

    # -- setup --------------------------------------------------------------------------------

    def create(self) -> None:
        self.upstream.mkdir()
        self.git("init", "-q", "-b", "main", cwd=self.upstream)
        self.upstream_release(
            "rust-v0.1.0",
            "0.1.0",
            {
                "README.md": "upstream\n",
                "codex-rs/core/Cargo.toml": '[package]\nname = "codex-core"\nversion.workspace = true\n\n'
                "[dependencies]\ncodex-util = { workspace = true }\n",
                "codex-rs/core/src/lib.rs": CORE_LIB,
                "codex-rs/util/Cargo.toml": '[package]\nname = "codex-util"\nversion.workspace = true\n',
                "codex-rs/util/src/lib.rs": "pub fn util() {}\n",
                "codex-rs/models-manager/models.json": '{"models": [{"slug": "gpt-test", "context_window": 1000}]}\n',
                "codex-rs/features/src/lib.rs": FEATURES_RS.format(
                    alpha_stage="Stable", alpha_default="true", beta_fallback="UnderDevelopment"
                ),
                "codex-rs/Cargo.lock": CARGO_LOCK,
                ".github/workflows/ci.yml": "on: push\n",
                ".github/workflows/release.yml": "on: push\n",
                ".github/workflows/zstd/helper.sh": "echo\n",
                ".github/dependabot.yaml": "version: 2\n",
                ".github/CODEOWNERS": "* @openai/team\n",
                ".github/scripts/keep.sh": "echo kept\n",
            },
        )
        # Upstream changes line 1 of core/src/lib.rs, the catalog and a feature flag.
        self.upstream_release(
            "rust-v0.1.1",
            "0.1.1",
            {
                "codex-rs/core/src/lib.rs": replace_line(CORE_LIB, 1, "pub fn line_1() -> u32 { 100 }"),
                "codex-rs/models-manager/models.json": '{"models": [{"slug": "gpt-test", "context_window": 2000}]}\n',
                "codex-rs/features/src/lib.rs": FEATURES_RS.format(
                    alpha_stage="Removed", alpha_default="false", beta_fallback="Removed"
                ),
            },
        )
        # Upstream rewrites line 5 differently from the fixture patch: a replay conflict.
        self.upstream_release(
            "rust-v0.1.2",
            "0.1.2",
            {"codex-rs/core/src/lib.rs": replace_line(
                replace_line(CORE_LIB, 1, "pub fn line_1() -> u32 { 100 }"), 5, "pub fn line_5() -> u32 { 555 }"
            )},
        )
        # Upstream ships exactly the fixture patch: the replay becomes empty.
        self.upstream_release(
            "rust-v0.1.3",
            "0.1.3",
            {"codex-rs/core/src/lib.rs": replace_line(
                replace_line(CORE_LIB, 1, "pub fn line_1() -> u32 { 100 }"), 5, "pub fn line_5() -> u32 { 50 }"
            )},
        )
        # A tag whose Cargo.toml version does not match, and an alpha tag.
        self.upstream_release("rust-v0.1.9", "0.1.1", {})
        self.upstream_release("rust-v0.2.0-alpha.1", "0.2.0-alpha.1", {})

        self.git("init", "-q", "--bare", "-b", "main", str(self.origin), cwd=self.root)
        self.git("clone", "-q", "--no-checkout", str(self.upstream), str(self.fork), cwd=self.root)
        self.git("remote", "rename", "origin", "upstream")
        self.git("remote", "add", "origin", str(self.origin))
        self.git("fetch", "-q", "upstream", "--tags")
        self.git("checkout", "-q", "--detach", "rust-v0.1.0")

        # The seed: the first Instafy files on top of rust-v0.1.0 (with upstream's workflows
        # still in place, as on the pre-bump integration branch).
        registry = textwrap.dedent(
            f"""\
            schema = 1
            base_tag = "rust-v0.1.0"
            base_commit = "{self.tags['rust-v0.1.0']}"
            previous_pin = "{self.tags['rust-v0.1.0']}"
            closure_roots = ["codex-rs/core"]
            closure_dirs = []
            report_models = ["gpt-test"]
            """
        )
        files = {path: source.read_text() for path, source in OVERLAY_SOURCES.items()}
        files["INSTAFY-PATCHES.toml"] = registry
        self.commit(self.fork, "seed Instafy files", files)
        self.git("branch", "seed")
        self.git("checkout", "-q", "--detach", "rust-v0.1.0")

    # -- driving the scripts ------------------------------------------------------------------

    def bump(self, *args, check=True) -> subprocess.CompletedProcess:
        return self.run("bash", BUMP, "--no-fetch", "--no-cargo", *args, check=check)

    def built(self, proc: subprocess.CompletedProcess) -> tuple[str, str]:
        base = re.search(r"^base: ([0-9a-f]{40})$", proc.stderr, re.M)
        tip = re.search(r"^tip:  ([0-9a-f]{40})$", proc.stderr, re.M)
        assert base and tip, proc.stderr
        return base.group(1), tip.group(1)

    def registry(self, rev: str) -> dict:
        return tomllib.loads(self.git("show", f"{rev}:INSTAFY-PATCHES.toml"))

    def files(self, rev: str) -> set[str]:
        return set(self.git("ls-tree", "-r", "--name-only", rev).splitlines())


class InstafyToolsTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="instafy-tools-")
        self.fx = Fixture(pathlib.Path(self._tmp.name).resolve())
        self.fx.create()

    def tearDown(self):
        self._tmp.cleanup()

    # -- tag validation -----------------------------------------------------------------------

    def test_refuses_tags_that_are_not_stable_releases(self):
        for tag in ("rust-v0.2.0-alpha.1", "v0.1.0", "rust-v0.1", "main"):
            proc = self.fx.bump("--dry-run", "--overlay-from", "seed", tag, check=False)
            self.assertNotEqual(proc.returncode, 0, tag)
            self.assertIn("is not a stable upstream tag", proc.stderr, tag)

    def test_refuses_a_tag_whose_cargo_version_differs(self):
        proc = self.fx.bump("--dry-run", "--overlay-from", "seed", "rust-v0.1.9", check=False)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn('says version "0.1.1"', proc.stderr)

    def test_refuses_conflicting_modes(self):
        proc = self.fx.bump("--open-pr", "rust-v0.1.0", check=False)
        self.assertIn("--open-pr needs --push", proc.stderr)
        proc = self.fx.bump("--dry-run", "--push", "rust-v0.1.0", check=False)
        self.assertIn("mutually exclusive", proc.stderr)

    # -- the generated base -------------------------------------------------------------------

    def test_generated_base_swaps_upstream_automation_for_the_instafy_files(self):
        fx = self.fx
        proc = fx.bump("--dry-run", "--overlay-from", "seed", "rust-v0.1.0")
        base, tip = fx.built(proc)
        tag = fx.tags["rust-v0.1.0"]
        self.assertEqual(base, tip)
        self.assertEqual(fx.git("rev-parse", f"{base}^"), tag)
        self.assertEqual(fx.git("rev-list", "--count", f"{tag}..{base}"), "1")

        files = fx.files(base)
        for gone in (".github/workflows/ci.yml", ".github/workflows/release.yml",
                     ".github/workflows/zstd/helper.sh", ".github/dependabot.yaml", ".github/CODEOWNERS"):
            self.assertNotIn(gone, files)
        for kept in (".github/scripts/keep.sh", "README.md", "codex-rs/core/src/lib.rs"):
            self.assertIn(kept, files)
        for added in OVERLAY_SOURCES:
            self.assertIn(added, files)
        self.assertEqual(fx.git("diff", "--name-only", tag, base, "--", "codex-rs"), "")

        registry = fx.registry(base)
        self.assertEqual(registry["base_tag"], "rust-v0.1.0")
        self.assertEqual(registry["base_commit"], tag)
        self.assertEqual(registry["closure_dirs"], ["codex-rs/core", "codex-rs/util"])
        self.assertEqual(registry.get("patch", []), [])

        message = fx.git("log", "-1", "--format=%B", base)
        self.assertIn("Instafy-Base-Tag: rust-v0.1.0", message)
        self.assertIn(f"Instafy-Base-Commit: {tag}", message)
        self.assertIn("  .github/workflows (3 files), .github/dependabot.yaml, .github/CODEOWNERS.\n", message)
        self.assertEqual(fx.git("log", "-1", "--format=%an <%ae> %cn", base),
                         "instafy-bot <306578215+instafy-bot@users.noreply.github.com> instafy-bot")

        # A dry run changes no refs.
        self.assertEqual(fx.git("for-each-ref", "refs/heads/instafy"), "")
        self.assertIn("No registered patches", proc.stdout)

        # Reproducible: again from the seed, and from the generated base itself.
        self.assertEqual(fx.built(fx.bump("--dry-run", "--overlay-from", "seed", "rust-v0.1.0"))[0], base)
        self.assertEqual(fx.built(fx.bump("--dry-run", "--overlay-from", base, "rust-v0.1.0"))[0], base)

        for check in (["workflow-allowlist", base], ["registry", base], ["tree-identity", base]):
            fx.run("bash", CI_SH, *check)

    def test_bump_to_a_new_tag_reports_upstream_changes(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        self.assertEqual(fx.git("rev-parse", "instafy/base/rust-v0.1.0"), base0)
        self.assertEqual(fx.git("rev-parse", "instafy/rust-v0.1.0"), base0)

        proc = fx.bump("--overlay-from", "instafy/base/rust-v0.1.0", "rust-v0.1.1")
        base1, tip1 = fx.built(proc)
        self.assertEqual(base1, tip1)
        registry = fx.registry(base1)
        self.assertEqual(registry["base_tag"], "rust-v0.1.1")
        self.assertEqual(registry["previous_pin"], base0)
        self.assertIn("1 upstream commits", proc.stdout)
        self.assertIn('context_window: 1000 -> 2000', proc.stdout)
        self.assertIn("Alpha: Stage::Stable default=true -> Stage::Removed default=false", proc.stdout)
        # Beta's stage is an expression: it is listed to compare by hand, not silently dropped.
        self.assertIn("  1 flags added, removed or changed stage/default\n", proc.stdout)
        self.assertIn("1 flags whose stage is not a plain Stage::X; compare by hand:\n    Beta: CHANGED",
                      proc.stdout)
        same = fx.run("python3", REGISTRY_PY, "report", "--old", fx.tags["rust-v0.1.1"], "--new",
                      fx.tags["rust-v0.1.2"], "--models-from", base1).stdout
        self.assertIn("  0 flags added, removed or changed stage/default\n", same)
        self.assertIn("    Beta: unchanged", same)

        # Downgrades need an explicit flag.
        proc = fx.bump("--dry-run", "--overlay-from", "instafy/base/rust-v0.1.1", "rust-v0.1.0", check=False)
        self.assertIn("older than the current base", proc.stderr)
        fx.bump("--dry-run", "--allow-downgrade", "--overlay-from", "instafy/base/rust-v0.1.1", "rust-v0.1.0")

        # Branches are immutable: a different commit for an existing branch is refused.
        fx.git("branch", "-f", "instafy/rust-v0.1.0", "seed")
        proc = fx.bump("--overlay-from", "seed", "rust-v0.1.0", check=False)
        self.assertIn("Fork branches are immutable", proc.stderr)

    def test_land_records_a_tree_identical_fast_forward_merge(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        _, tip1 = fx.built(fx.bump("--overlay-from", "instafy/base/rust-v0.1.0", "rust-v0.1.1"))

        proc = fx.bump("land", "--integration", "seed", "rust-v0.1.1", tip1)
        merge = re.search(r"^integration: ([0-9a-f]{40})$", proc.stdout, re.M).group(1)
        self.assertIn(f"pin: {tip1}", proc.stdout)
        self.assertEqual(fx.git("rev-parse", f"{merge}^1"), fx.git("rev-parse", "seed"))
        self.assertEqual(fx.git("rev-parse", f"{merge}^2"), tip1)
        self.assertEqual(fx.git("rev-parse", f"{merge}^{{tree}}"), fx.git("rev-parse", f"{tip1}^{{tree}}"))
        self.assertEqual(fx.git("rev-parse", "instafy/integration-next"), merge)
        self.assertIn(f"Instafy-Landed-Tip: {tip1}", fx.git("log", "-1", "--format=%B", merge))

        # Reproducible and idempotent.
        again = fx.bump("land", "--integration", "seed", "rust-v0.1.1", tip1)
        self.assertIn(f"integration: {merge}", again.stdout)
        # Already recorded on the integration head: nothing to do.
        done = fx.bump("land", "--integration", merge, "rust-v0.1.1", tip1)
        self.assertIn(f"pin: {tip1}", done.stdout)
        self.assertNotIn("integration:", done.stdout)

        # The next bump takes the Instafy files from the integration record, keeps the same tag
        # identical, and names the landed tip as previous_pin for a new tag.
        self.assertEqual(fx.built(fx.bump("--dry-run", "--overlay-from", merge, "rust-v0.1.1"))[0], tip1)
        base2, _ = fx.built(fx.bump("--dry-run", "--overlay-from", merge, "rust-v0.1.2"))
        self.assertEqual(fx.registry(base2)["previous_pin"], tip1)

        # land refuses a tip that is not the generated base plus registered patches, and an
        # approved SHA that is not the branch tip.
        fx.git("checkout", "-q", "--detach", base0)
        stray = fx.commit(fx.fork, "stray", {"codex-rs/util/src/lib.rs": "pub fn util() { }\n"})
        proc = fx.bump("land", "--integration", "seed", "--local-branch", "scratch", "rust-v0.1.0", stray, check=False)
        self.assertIn(f"instafy/rust-v0.1.0 is at {base0}, not the approved {stray}", proc.stderr)
        fx.git("update-ref", "refs/heads/instafy/rust-v0.1.0", stray)
        proc = fx.bump("land", "--integration", "seed", "--local-branch", "scratch", "rust-v0.1.0", check=False)
        self.assertIn(f"{stray} above instafy/base/rust-v0.1.0 has no Instafy-Patch trailer", proc.stderr)
        self.assertEqual(fx.git("for-each-ref", "refs/heads/scratch"), "")

    def test_land_refuses_tips_that_change_instafy_files_or_move_base_commit(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        workflow = fx.git("show", f"{base0}:.github/workflows/instafy-ci.yml")
        ci_sh = fx.git("show", f"{base0}:.github/instafy/ci.sh")
        stub = ci_sh.replace("tree_identity() {", "tree_identity() {\n  echo patches=0; return 0")
        patched = self.add_patch(base0, "scratch-patch")
        patch_files = {path: fx.git("show", f"{patched}:{path}") + "\n"
                       for path in ("codex-rs/core/src/lib.rs", "INSTAFY-PATCHES.toml")}
        fx.git("update-ref", "-d", "refs/heads/scratch-patch")

        def land(extra: dict[str, str], message="fix(instafy): return 50 from line 5\n\nInstafy-Patch: core-line-five"):
            """Land a patch commit on the base: the registered patch, plus extra."""
            fx.git("checkout", "-q", "--detach", base0)
            fx.write(fx.fork, {**patch_files, **extra})
            fx.run("git", "commit", "-q", "-m", message, env=fx.tick())
            tip = fx.git("rev-parse", "HEAD")
            fx.git("update-ref", "refs/heads/instafy/rust-v0.1.0", tip)
            proc = fx.bump("land", "--integration", "seed", "--local-branch", "scratch", "rust-v0.1.0", tip,
                           check=False)
            return tip, proc

        # The plain patch lands.
        _, proc = land({})
        self.assertEqual(proc.returncode, 0, proc.stderr)
        fx.git("update-ref", "-d", "refs/heads/scratch")

        # (a) The patch commit also moves CI to a billed macOS runner and stubs out tree identity.
        _, proc = land({".github/workflows/instafy-ci.yml": workflow.replace("ubuntu-24.04", "macos-15-xlarge"),
                        ".github/instafy/ci.sh": stub})
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("runs on 'macos-15-xlarge'", proc.stderr)
        self.assertIn("gate failed: workflow allowlist", proc.stderr)
        # Only the tooling: the allowlist passes, the land check does not.
        _, proc = land({".github/instafy/ci.sh": stub})
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("changes .github/instafy/ci.sh, which patch 'core-line-five' does not register", proc.stderr)
        self.assertIn("gate failed: generated base plus registered patches only", proc.stderr)

        # (b) The patch commit points the registry at another upstream commit and carries its code.
        moved = {
            path: fx.git("show", f"{fx.tags['rust-v0.1.1']}:{path}") + "\n"
            for path in ("codex-rs/Cargo.toml", "codex-rs/models-manager/models.json", "codex-rs/features/src/lib.rs")
        }
        _, proc = land({
            **moved,
            "INSTAFY-PATCHES.toml": patch_files["INSTAFY-PATCHES.toml"].replace(
                f'base_commit = "{fx.tags["rust-v0.1.0"]}"', f'base_commit = "{fx.tags["rust-v0.1.1"]}"'),
        })
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(f"the registry names base_commit {fx.tags['rust-v0.1.1']}, but the base's parents are", proc.stderr)
        self.assertIn("differs from the generated base's; only bump.sh writes it", proc.stderr)
        self.assertIn("changes codex-rs/Cargo.toml, codex-rs/features/src/lib.rs, codex-rs/models-manager/models.json",
                      proc.stderr)
        self.assertEqual(fx.git("for-each-ref", "refs/heads/scratch"), "")

        # A commit whose trailer names no registered patch.
        _, proc = land({}, message="fix(instafy): something else\n\nInstafy-Patch: unregistered")
        self.assertIn("carries Instafy-Patch: 'unregistered', which no [[patch]] at the tip registers", proc.stderr)
        self.assertIn("registered patches without a commit above the base: core-line-five", proc.stderr)

        # The tag must be available to check the base's parent against it.
        for tag in fx.git("tag", "--list", "rust-v0.1.0").split():
            fx.git("tag", "-d", tag)
        _, proc = land({})
        self.assertIn("tag rust-v0.1.0 is not available locally", proc.stderr)
        self.assertEqual(fx.git("for-each-ref", "refs/heads/scratch"), "")

    def test_bump_and_land_fetch_upstream_tags_outside_refs_tags(self):
        fx = self.fx
        # A clone with no local tags: whatever tag a push could carry, bump.sh put there.
        for tag in fx.git("tag", "--list").split():
            fx.git("tag", "-d", tag)
        proc = fx.run("bash", BUMP, "--no-cargo", "--overlay-from", "seed", "rust-v0.1.1")
        base, tip = fx.built(proc)
        self.assertEqual(fx.git("rev-parse", f"{base}^"), fx.tags["rust-v0.1.1"])
        self.assertEqual(fx.git("for-each-ref", "refs/tags"), "")
        self.assertEqual(fx.git("rev-parse", "refs/instafy/upstream-tags/rust-v0.1.1^{commit}"), fx.tags["rust-v0.1.1"])
        push = fx.run("git", "push", "--dry-run", "--porcelain", "--follow-tags", "origin",
                      f"{tip}:refs/heads/instafy/integration").stdout
        self.assertNotIn("refs/tags/", push)

        # land fetches the tag too, and with --no-fetch refuses to skip the parent check.
        fx.git("update-ref", "-d", "refs/instafy/upstream-tags/rust-v0.1.1")
        proc = fx.bump("land", "--integration", "seed", "rust-v0.1.1", check=False)
        self.assertIn("tag rust-v0.1.1 is not available locally", proc.stderr)
        proc = fx.run("bash", BUMP, "land", "--integration", "seed", "rust-v0.1.1")
        self.assertIn(f"pin: {tip}", proc.stdout)
        self.assertEqual(fx.git("for-each-ref", "refs/tags"), "")

    # -- registry and tree identity -----------------------------------------------------------

    def test_tree_identity_and_allowlist_reject_unregistered_changes(self):
        fx = self.fx
        base, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        fx.git("checkout", "-q", "--detach", base)
        changed = fx.commit(fx.fork, "unregistered", {"codex-rs/core/src/lib.rs": "changed\n"})
        proc = fx.run("bash", CI_SH, "tree-identity", changed, check=False)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("codex-rs/core/src/lib.rs (M) differs from rust-v0.1.0 but no [[patch]] registers it",
                      proc.stderr)

        fx.git("checkout", "-q", "--detach", base)
        readded = fx.commit(fx.fork, "re-add upstream CI", {".github/workflows/ci.yml": "on: pull_request\n",
                                                             ".github/dependabot.yml": "version: 2\n"})
        proc = fx.run("bash", CI_SH, "workflow-allowlist", readded, check=False)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(".github/workflows/ci.yml is not allowed", proc.stderr)
        self.assertIn(".github/dependabot.yml is not allowed", proc.stderr)
        proc = fx.run("bash", CI_SH, "tree-identity", readded, check=False)
        self.assertIn(".github/workflows/ci.yml (M) differs", proc.stderr)

        proc = fx.run("bash", CI_SH, "workflow-allowlist", "seed", check=False)
        self.assertIn(".github/workflows/release.yml is not allowed", proc.stderr)
        self.assertIn(".github/CODEOWNERS is not allowed", proc.stderr)

        # instafy-ci.yml itself: ubuntu-24.04 only, and no reusable workflows.
        workflow = fx.git("show", f"{base}:.github/workflows/instafy-ci.yml") + "\n"
        reusable = "  release:\n    uses: openai/codex/.github/workflows/rust-release.yml@main\n"
        cases = {
            "macOS": (workflow.replace("runs-on: ubuntu-24.04", "runs-on: macos-15-xlarge", 1),
                      "runs on 'macos-15-xlarge'"),
            "expression": (workflow.replace("runs-on: ubuntu-24.04", "runs-on: ${{ matrix.os }}", 1),
                           "runs on '${{ matrix.os }}'"),
            "list": (workflow.replace("runs-on: ubuntu-24.04", "runs-on:\n      - ubuntu-24.04", 1),
                     "runs on '(a block value)'"),
            "reusable": (workflow + reusable, "job 'release' calls a reusable workflow"),
            "quoted key": (workflow.replace("runs-on: ubuntu-24.04", '"runs-on": windows-latest', 1),
                           "runs on 'windows-latest'"),
            "flow jobs": ("on: push\njobs: {a: {runs-on: macos-latest}}\n", "jobs must be a block mapping"),
            "ok": (workflow.replace("runs-on: ubuntu-24.04", 'runs-on: "ubuntu-24.04"  # standard', 1), None),
        }
        for name, (text, expected) in cases.items():
            fx.git("checkout", "-q", "--detach", base)
            rev = fx.commit(fx.fork, name, {".github/workflows/instafy-ci.yml": text})
            proc = fx.run("bash", CI_SH, "workflow-allowlist", rev, check=False)
            if expected is None:
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertIn("only .github/workflows/instafy-ci.yml on ubuntu-24.04", proc.stdout)
            else:
                self.assertNotEqual(proc.returncode, 0, name)
                self.assertIn(expected, proc.stderr, name)
                self.assertIn("may run on ubuntu-24.04 only", proc.stderr, name)

        # No registered patches: patch-tests never reaches cargo.
        fx.git("checkout", "-q", "--detach", base)
        path = f"{os.path.dirname(shutil.which('python3'))}:{os.path.dirname(shutil.which('git'))}:/usr/bin:/bin"
        self.assertIsNone(shutil.which("cargo", path=path), "cargo must not be reachable here")
        proc = fx.run("bash", CI_SH, "patch-tests", env={"PATH": path})
        self.assertIn("No registered patches", proc.stdout)

    def test_ci_tree_identity_counts_only_the_patches_a_tree_carries(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        fx.git("update-ref", "-d", "refs/heads/instafy/rust-v0.1.0")
        patched = self.add_patch(base0, "instafy/rust-v0.1.0")
        base1, tip1 = fx.built(fx.bump("--overlay-from", patched, "rust-v0.1.1"))

        def identity(rev):
            out = fx.root / "github-output"
            out.write_text("")
            proc = fx.run("bash", CI_SH, "tree-identity", rev, check=False, env={"GITHUB_OUTPUT": str(out)})
            return proc, out.read_text().strip()

        # The generated base lists the patch but carries none of its code: no patch tests.
        self.assertEqual([p["id"] for p in fx.registry(base1)["patch"]], ["core-line-five"])
        self.assertIn("line_5() -> u32 { 5 }", fx.git("show", f"{base1}:codex-rs/core/src/lib.rs"))
        proc, output = identity(base1)
        self.assertEqual((proc.returncode, output), (0, "patches=0"), proc.stderr)
        proc, output = identity(tip1)
        self.assertEqual((proc.returncode, output), (0, "patches=1"), proc.stderr)

        # Above the base, every registered patch must be there: a tree that registers one
        # without its code fails, instead of sending patch-tests after a missing test.
        fx.git("checkout", "-q", "--detach", base1)
        unpatched = fx.commit(fx.fork, "docs only", {"INSTAFY.md": "changed\n"})
        proc, output = identity(unpatched)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("patch 'core-line-five' registers files the tree does not change", proc.stderr)
        self.assertEqual(output, "")

        # A commit straight on the tag is a base, and a base carries no patch code.
        forged = fx.git("commit-tree", f"{tip1}^{{tree}}", "-p", fx.tags["rust-v0.1.1"], "-m", "base with patch code")
        proc, output = identity(forged)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("patch 'core-line-five' registers it, but a generated base carries no patch code", proc.stderr)

    def test_patch_tests_resolve_the_lock_before_building_locked(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        fx.git("update-ref", "-d", "refs/heads/instafy/rust-v0.1.0")
        patched = self.add_patch(base0, "instafy/rust-v0.1.0")
        _, tip1 = fx.built(fx.bump("--overlay-from", patched, "rust-v0.1.1"))
        # The committed lock is upstream's: members at 0.0.0, which --locked alone refuses.
        self.assertIn('name = "codex-core"\nversion = "0.0.0"', fx.git("show", f"{tip1}:codex-rs/Cargo.lock"))

        bin_dir = fx.root / "fake-bin"
        bin_dir.mkdir()
        (bin_dir / "cargo").write_text(FAKE_CARGO)
        (bin_dir / "cargo").chmod(0o755)
        log = fx.root / "cargo.log"
        env = {"PATH": f"{bin_dir}:{os.environ['PATH']}", "FAKE_CARGO_LOG": str(log)}

        fx.git("checkout", "-q", "--detach", tip1)
        proc = fx.run("bash", CI_SH, "patch-tests", env=env)
        self.assertIn("2 workspace members moved to 0.1.1; external dependencies unchanged", proc.stdout)
        self.assertIn("1  codex-core|lib|tests::line_five", proc.stdout)
        calls = log.read_text().splitlines()
        self.assertEqual([c.split()[0] for c in calls], ["metadata", "test", "test"])
        self.assertTrue(all("--locked" in c for c in calls[1:]), calls)

        # Resolution that changes anything beyond the member versions stops before cargo test.
        fx.git("checkout", "-q", "--force", "--detach", tip1)
        log.unlink()
        proc = fx.run("bash", CI_SH, "patch-tests", env={**env, "FAKE_CARGO_DRIFT": "1"}, check=False)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("external dependencies changed: serde", proc.stderr)
        self.assertIn("changed Cargo.lock beyond the workspace member versions", proc.stderr)
        self.assertEqual([c.split()[0] for c in log.read_text().splitlines()], ["metadata"])

    def test_registry_check_verifies_base_commit_against_the_peeled_upstream_tag(self):
        fx = self.fx
        base, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        upstream = {"INSTAFY_UPSTREAM_URL": str(fx.upstream)}
        # The fixture's tags are annotated, like openai/codex's: the check must peel them.
        self.assertNotEqual(fx.git("rev-parse", "rust-v0.1.0"), fx.tags["rust-v0.1.0"])
        proc = fx.run("python3", REGISTRY_PY, "check", "--rev", base, "--verify-upstream-tag", env=upstream)
        self.assertIn("base_commit matches upstream", proc.stdout)

        fx.git("checkout", "-q", "--detach", base)
        text = fx.git("show", f"{base}:INSTAFY-PATCHES.toml").replace(
            f'base_commit = "{fx.tags["rust-v0.1.0"]}"', f'base_commit = "{fx.tags["rust-v0.1.1"]}"'
        )
        moved = fx.commit(fx.fork, "wrong base_commit", {"INSTAFY-PATCHES.toml": text + "\n"})
        proc = fx.run("python3", REGISTRY_PY, "check", "--rev", moved, "--verify-upstream-tag", env=upstream,
                      check=False)
        self.assertIn(f"upstream rust-v0.1.0 is {fx.tags['rust-v0.1.0']}, but base_commit is", proc.stderr)

    def test_registry_check_rejects_malformed_entries(self):
        fx = self.fx
        base, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        good = fx.git("show", f"{base}:INSTAFY-PATCHES.toml")
        patch = textwrap.dedent(
            """
            [[patch]]
            id = "Bad_Id"
            why = ""
            billing = "yes"
            security = false
            files = [".github/workflows/ci.yml", "INSTAFY.md", "codex-rs/../x"]
            public_tests = []
            fork_tests = ["codex-core client::tests"]
            upstream = { issue = "https://example.com/1", status = "maybe" }
            instafy_side_alternative = "none"
            drop_when = "later"
            last_validated_tag = "main"
            surprise = 1
            """
        )
        cases = {
            "unknown top-level": (good + "\nextra = 1\n", "unknown top-level key 'extra'"),
            "closure drift": (good.replace('  "codex-rs/util",\n', ""), "closure_dirs is not the path-dependency closure"),
            "bad base_tag": (good.replace('base_tag = "rust-v0.1.0"', 'base_tag = "rust-v0.2.0-alpha.1"'), "is not a stable upstream tag"),
            "patch entry": (good + patch, None),
        }
        for name, (text, expected) in cases.items():
            fx.git("checkout", "-q", "--detach", base)
            rev = fx.commit(fx.fork, name, {"INSTAFY-PATCHES.toml": text})
            proc = fx.run("python3", REGISTRY_PY, "check", "--rev", rev, check=False)
            self.assertNotEqual(proc.returncode, 0, name)
            if expected:
                self.assertIn(expected, proc.stderr, name)
            else:
                for message in ("unknown key 'surprise'", "id must be kebab-case", "why must be a non-empty string",
                                "billing must be true or false", "public_tests must be a non-empty list",
                                "belongs to the generated base commit", "must be a normalised repository path",
                                "fork_tests entry 'codex-core client::tests' must be",
                                "upstream.issue must be an openai/codex issue URL", "upstream.status must be one of",
                                "last_validated_tag must be a stable upstream tag"):
                    self.assertIn(message, proc.stderr, message)

    def test_lock_drift_allows_only_workspace_member_versions_to_move(self):
        # Upstream's release commit bumps codex-rs/Cargo.toml, not Cargo.lock, so resolving a
        # stable tag moves the workspace members from 0.0.0 to the tag version, and nothing else.
        committed = textwrap.dedent(
            """\
            version = 4

            [[package]]
            name = "codex-core"
            version = "0.0.0"
            dependencies = ["serde"]

            [[package]]
            name = "serde"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aaaa"
            """
        )
        resolved = committed.replace('version = "0.0.0"', 'version = "0.1.0"')
        cases = {
            "members moved": (resolved, "0.1.0", 0, "1 workspace members moved to 0.1.0"),
            "wrong version": (resolved, "0.2.0", 1, "moved to a version other than 0.2.0: codex-core 0.0.0 -> 0.1.0"),
            "external drift": (resolved.replace('"aaaa"', '"bbbb"'), "0.1.0", 1, "external dependencies changed: serde"),
            "rewired member": (resolved.replace('dependencies = ["serde"]', "dependencies = []"), "0.1.0", 1,
                               "workspace members changed dependencies: codex-core"),
        }
        before = self.fx.root / "before.lock"
        before.write_text(committed)
        for name, (text, version, code, expected) in cases.items():
            after = self.fx.root / "after.lock"
            after.write_text(text)
            proc = self.fx.run("python3", REGISTRY_PY, "lock-drift", "--before", before, "--after", after,
                               "--version", version, check=False)
            self.assertEqual(proc.returncode, code, name)
            self.assertIn(expected, proc.stdout + proc.stderr, name)

    # -- patches ------------------------------------------------------------------------------

    def add_patch(self, base: str, branch: str) -> str:
        """Commit a registered patch (code plus its [[patch]] entry) on top of base."""
        fx = self.fx
        fx.git("checkout", "-q", "--detach", base)
        entry = textwrap.dedent(
            """
            [[patch]]
            id = "core-line-five"
            why = "Instafy needs line 5 to return 50."
            billing = false
            security = false
            files = ["codex-rs/core/src/lib.rs"]
            public_tests = ["packages/runtime-agent/tests/example.rs::line_five"]
            fork_tests = ["codex-core|lib|tests::line_five"]
            upstream = { issue = "https://github.com/openai/codex/issues/1", status = "open" }
            instafy_side_alternative = "None: the value is internal to codex-core."
            drop_when = "Upstream returns 50."
            last_validated_tag = "rust-v0.1.0"
            """
        )
        registry = fx.git("show", f"{base}:INSTAFY-PATCHES.toml") + "\n" + entry
        fx.write(fx.fork, {
            "codex-rs/core/src/lib.rs": replace_line(CORE_LIB, 5, "pub fn line_5() -> u32 { 50 }"),
            "INSTAFY-PATCHES.toml": registry,
        })
        fx.run("git", "commit", "-q", "-m", "fix(instafy): return 50 from line 5\n\nInstafy-Patch: core-line-five",
               env={**fx.tick(), "GIT_AUTHOR_NAME": "Patch Author", "GIT_AUTHOR_EMAIL": "author@example.com"})
        sha = fx.git("rev-parse", "HEAD")
        fx.git("update-ref", f"refs/heads/{branch}", sha)
        return sha

    def test_registered_patches_are_replayed_and_checked(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        # The generated tip branch equals the base until a patch lands on it.
        fx.git("update-ref", "-d", "refs/heads/instafy/rust-v0.1.0")
        patched = self.add_patch(base0, "instafy/rust-v0.1.0")
        fx.run("python3", REGISTRY_PY, "check", "--rev", patched)
        fx.run("python3", REGISTRY_PY, "tree-identity", "--rev", patched, "--exact")

        proc = fx.bump("--overlay-from", patched, "rust-v0.1.1")
        base1, tip1 = fx.built(proc)
        self.assertEqual(fx.git("rev-parse", f"{tip1}^"), base1)
        self.assertEqual(fx.git("diff-tree", "--no-commit-id", "--name-only", "-r", tip1),
                         "codex-rs/core/src/lib.rs")
        content = fx.git("show", f"{tip1}:codex-rs/core/src/lib.rs")
        self.assertIn("line_1() -> u32 { 100 }", content)
        self.assertIn("line_5() -> u32 { 50 }", content)
        self.assertEqual(fx.git("log", "-1", "--format=%an|%cn|%(trailers:key=Instafy-Patch,valueonly)", tip1),
                         "Patch Author|instafy-bot|core-line-five")
        self.assertEqual([p["id"] for p in fx.registry(base1)["patch"]], ["core-line-five"])
        # The report's range-diff names the replayed patch.
        self.assertIn("fix(instafy): return 50 from line 5", proc.stdout.split("## Patches")[1])
        # Reproducible with patches too.
        self.assertEqual(fx.built(fx.bump("--dry-run", "--overlay-from", patched, "rust-v0.1.1")), (base1, tip1))

        # An unregistered commit on the old stack stops the bump.
        fx.git("checkout", "-q", "--detach", patched)
        stray = fx.commit(fx.fork, "stray", {"codex-rs/util/src/lib.rs": "pub fn util() { }\n"})
        fx.git("update-ref", "refs/heads/instafy/rust-v0.1.0", stray)
        proc = fx.bump("--dry-run", "--overlay-from", patched, "rust-v0.1.1", check=False)
        self.assertIn("has no Instafy-Patch trailer", proc.stderr)
        fx.git("update-ref", "refs/heads/instafy/rust-v0.1.0", patched)

        # Upstream shipped the same change: the empty replay stops the bump.
        proc = fx.bump("--dry-run", "--overlay-from", patched, "rust-v0.1.3", check=False)
        self.assertIn("patch core-line-five is empty on rust-v0.1.3", proc.stderr)

    def test_a_conflicting_replay_stops_and_rerere_replays_the_resolution(self):
        fx = self.fx
        base0, _ = fx.built(fx.bump("--overlay-from", "seed", "rust-v0.1.0"))
        fx.git("update-ref", "-d", "refs/heads/instafy/rust-v0.1.0")
        patched = self.add_patch(base0, "instafy/rust-v0.1.0")

        proc = fx.bump("--dry-run", "--overlay-from", patched, "rust-v0.1.2", check=False)
        self.assertEqual(proc.returncode, 3, proc.stderr)
        self.assertIn("Patch core-line-five does not apply cleanly on rust-v0.1.2", proc.stderr)
        self.assertIn("why: Instafy needs line 5 to return 50.", proc.stderr)
        self.assertIn("release 0.1.2", proc.stderr)  # the upstream commit touching its files
        worktree = pathlib.Path(re.search(r"^Resolve it in (\S+) ", proc.stderr, re.M).group(1))

        resolved = replace_line(replace_line(CORE_LIB, 1, "pub fn line_1() -> u32 { 100 }"), 5,
                                "pub fn line_5() -> u32 { 50 } // Instafy, over upstream's 555")
        (worktree / "codex-rs/core/src/lib.rs").write_text(resolved)
        fx.git("add", "--", "codex-rs/core/src/lib.rs", cwd=worktree)
        fx.run("git", "-c", "rerere.enabled=true", "commit", "-q", "--no-verify", "-C", patched, cwd=worktree)
        fx.git("worktree", "remove", "--force", str(worktree))

        proc = fx.bump("--dry-run", "--overlay-from", patched, "rust-v0.1.2")
        _, tip = fx.built(proc)
        self.assertEqual(fx.git("show", f"{tip}:codex-rs/core/src/lib.rs") + "\n", resolved)

    # -- pushing ------------------------------------------------------------------------------

    def test_push_creates_branches_only_and_never_overwrites(self):
        fx = self.fx
        fx.git("push", "-q", "origin", "seed:refs/heads/instafy/integration")
        base, tip = fx.built(fx.bump("--push", "--overlay-from", "seed", "rust-v0.1.0"))
        remote = fx.git("ls-remote", "origin")
        self.assertIn(f"{base}\trefs/heads/instafy/base/rust-v0.1.0", remote)
        self.assertIn(f"{tip}\trefs/heads/instafy/rust-v0.1.0", remote)
        self.assertNotIn("refs/tags/", remote)

        # Pushing again is a no-op; a remote branch at another commit is never overwritten.
        fx.bump("--push", "--overlay-from", "seed", "rust-v0.1.0")
        fx.git("push", "-q", "--force", "origin", "seed:refs/heads/instafy/base/rust-v0.1.1")
        fx.git("fetch", "-q", "origin", "+refs/heads/instafy/*:refs/remotes/origin/instafy/*")
        proc = fx.bump("--push", "--overlay-from", "instafy/base/rust-v0.1.0", "rust-v0.1.1", check=False)
        self.assertIn("origin/instafy/base/rust-v0.1.1 is at", proc.stderr)
        self.assertIn("Fork branches are immutable", proc.stderr)

        # land --push fast-forwards instafy/integration and pushes nothing else new.
        fx.git("fetch", "-q", "origin", "+refs/heads/instafy/*:refs/remotes/origin/instafy/*")
        proc = fx.bump("land", "--push", "rust-v0.1.0", tip)
        merge = re.search(r"^integration: ([0-9a-f]{40})$", proc.stdout, re.M).group(1)
        self.assertEqual(fx.git("ls-remote", "origin", "refs/heads/instafy/integration").split("\t")[0], merge)
        self.assertNotIn("refs/tags/", fx.git("ls-remote", "origin"))


if __name__ == "__main__":
    unittest.main()
