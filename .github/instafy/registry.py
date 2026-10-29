#!/usr/bin/env python3
"""INSTAFY-PATCHES.toml tooling for the Instafy fork of openai/codex.

.github/instafy/ci.sh (the fork's CI) and .github/instafy/bump.sh (the bump script) call
this; see INSTAFY.md. Standard library only; needs Python 3.11+ for tomllib.

Every command reads files from a git revision (default HEAD), never from the working tree,
so CI, bump.sh and a reviewer all check exactly what is committed.

  registry.py check [--rev REV] [--verify-upstream-tag]
      Validate the registry format, and that closure_dirs is what closure_roots reaches.
  registry.py tree-identity [--rev REV] [--exact | --base]
      Fail unless `git diff <base_commit> REV` changes only the Instafy files, deletes only
      upstream automation, and otherwise touches only files registered by a [[patch]].
      --exact also fails when a registered file is unchanged (REV carries every patch);
      --base fails unless REV is a generated base: base_commit is its only parent and no
      registered file differs (the base lists the patches but carries none of their code).
  registry.py land-check --base REV --tip REV
      Fail unless TIP is BASE plus one commit per registered patch that changes only that
      patch's files and the registry's patch section, BASE's parent is base_commit, and the
      registry header above the "Registered patches" line is byte-identical.
  registry.py workflow-runners [--rev REV]
      Fail unless every job in .github/workflows/instafy-ci.yml runs on `ubuntu-24.04` and none
      calls a reusable workflow.
  registry.py count [--rev REV]            number of registered patches
  registry.py field [--rev REV] KEY        a top-level scalar (base_tag, base_commit, ...)
  registry.py patch-ids [--rev REV]        registered patch ids, in replay order
  registry.py patch-files [--rev REV] ID   the files patch ID may change
  registry.py patch-info [--rev REV] ID    a readable summary of patch ID
  registry.py fork-tests [--rev REV]       every fork_tests entry, one per line
  registry.py tag-version --rev REV        codex-rs/Cargo.toml [workspace.package] version
  registry.py render --from REV --base-tag TAG --base-commit SHA --previous-pin SHA
      Print the registry for a new base: the generated header with new values, then the
      hand-maintained patch section of REV's registry, verbatim.
  registry.py lock-drift --before FILE --after FILE --version VERSION
      Fail unless the second Cargo.lock differs from the first only in workspace member
      versions moving to VERSION. Upstream's release commits bump codex-rs/Cargo.toml but
      not Cargo.lock, so resolving a stable tag always rewrites those, and only those.
  registry.py report --old REV --new REV [--models-from REV]
      Catalog (models.json) and feature-flag differences between two upstream commits, for
      the report_models of REV's registry (default HEAD).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import posixpath
import re
import subprocess
import sys
import tomllib

REGISTRY = "INSTAFY-PATCHES.toml"
SCHEMA = 1
# INSTAFY_UPSTREAM_URL exists for the self-test; CI and bump.sh use openai/codex.
UPSTREAM_URL = os.environ.get("INSTAFY_UPSTREAM_URL", "https://github.com/openai/codex.git")
TAG_RE = re.compile(r"^rust-v[0-9]+\.[0-9]+\.[0-9]+$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
ID_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
ISSUE_RE = re.compile(r"^https://github\.com/openai/codex/issues/[0-9]+$")
FORK_TEST_RE = re.compile(r"^([A-Za-z0-9_-]+)\|(lib|[A-Za-z0-9_-]+)\|(.*)$")
UPSTREAM_STATUSES = ("open", "closed", "fixed")

# Paths the generated base commit owns. They may differ from the upstream tag freely.
OVERLAY_FILES = (".github/workflows/instafy-ci.yml", "INSTAFY.md", REGISTRY)
OVERLAY_DIR = ".github/instafy/"
# Upstream automation the generated base commit deletes: CI, release and bot workflows,
# Dependabot and CODEOWNERS in every location GitHub reads it from.
REMOVED_FILES = (
    ".github/dependabot.yml",
    ".github/dependabot.yaml",
    ".github/CODEOWNERS",
    "CODEOWNERS",
    "docs/CODEOWNERS",
)
REMOVED_DIR = ".github/workflows/"
WORKFLOW = ".github/workflows/instafy-ci.yml"
# The only runner this fork's jobs may use: standard hosted Linux, never a paid larger runner.
ALLOWED_RUNNER = "ubuntu-24.04"

CATALOG = "codex-rs/models-manager/models.json"
FEATURES = "codex-rs/features/src/lib.rs"

HEADER_KEYS = (
    "schema",
    "base_tag",
    "base_commit",
    "previous_pin",
    "closure_roots",
    "closure_dirs",
    "report_models",
)
PATCH_KEYS = (
    "id",
    "why",
    "billing",
    "security",
    "files",
    "public_tests",
    "fork_tests",
    "upstream",
    "instafy_side_alternative",
    "drop_when",
    "last_validated_tag",
)
PATCH_MARKER = "# ---- Registered patches"

HEADER_TEMPLATE = """\
# INSTAFY-PATCHES.toml: every change this fork carries on top of an upstream openai/codex tag.
#
# Everything above the "Registered patches" line is generated by .github/instafy/bump.sh from
# the previous registry; edit closure_roots and report_models here and the next bump keeps
# them, but never edit base_tag, base_commit, previous_pin or closure_dirs by hand.
#
# The fork's CI (.github/instafy/ci.sh tree-identity) fails unless `git diff <base_commit> HEAD`
# changes only the Instafy files (.github/instafy/, .github/workflows/instafy-ci.yml, INSTAFY.md
# and this file), deletes only upstream workflows, Dependabot config and CODEOWNERS, and
# otherwise touches only files a [[patch]] below registers. See INSTAFY.md.

schema = {schema}

# The upstream stable tag this branch is built from, and the commit that tag peels to.
base_tag = "{base_tag}"
base_commit = "{base_commit}"

# The codex commit instafy-dev/instafy pinned before this base.
previous_pin = "{previous_pin}"

# The codex-rs crates instafy-dev/instafy builds directly: packages/runtime-agent's path
# dependencies plus the helper binaries its runtime image and Desktop app ship.
closure_roots = {closure_roots}

# Every codex-rs crate those roots reach through path dependencies at base_tag. Generated from
# closure_roots; bump.sh summarises upstream changes under these directories.
closure_dirs = {closure_dirs}

# Model slugs whose catalog entries bump.sh compares between the previous base and the new tag.
report_models = {report_models}

"""

DEFAULT_PATCH_SECTION = """\
# ---- Registered patches (hand-maintained below this line; bump.sh carries it forward) ----
#
# None. codex-rs on this branch is byte-identical to base_tag.
#
# A patch may be registered only when all of these hold (INSTAFY.md, "Adding a patch"):
#   - a public black-box test in instafy-dev/instafy fails without it (public_tests);
#   - runtime-agent calls no API that the patch adds;
#   - no Instafy-side fix exists (in runtime-agent, the proxy or Desktop);
#   - an upstream issue is filed and linked (openai/codex takes no external pull requests);
#   - its fork tests live in files of their own, not in shared mod.rs test lists.
#
# Each patch is exactly one commit on instafy/<tag>, carrying the trailer "Instafy-Patch: <id>".
# Entries are replayed in the order listed. Every key is required:
#
# [[patch]]
# id = "incomplete-not-resent"          # kebab-case; matches the commit's Instafy-Patch trailer
# why = "..."                           # what Instafy needs and what breaks without it
# billing = true                        # whether it changes metering or billed tokens
# security = false                      # whether it enforces a security boundary
# files = ["codex-rs/core/src/client.rs"]
#                                       # every path the patch changes; nothing else may differ
# public_tests = ["packages/runtime-agent/tests/proxy_retry_budget.rs::incomplete_max_output_tokens_is_not_resent"]
#                                       # instafy-dev/instafy tests that fail without the patch
# fork_tests = ["codex-core|lib|client::tests::incomplete_"]
#                                       # "<cargo package>|<lib or test target>|<test name filter>";
#                                       # ci.sh patch-tests builds and runs these, and fails if
#                                       # an entry matches no passing test
# upstream = { issue = "https://github.com/openai/codex/issues/12345", status = "open" }
#                                       # status: open, closed or fixed
# instafy_side_alternative = "..."      # why runtime-agent, the proxy or Desktop cannot do it
# drop_when = "..."                     # the change that makes the patch unnecessary
# last_validated_tag = "rust-v0.159.2"  # newest tag whose replay passed the gates and public_tests
"""


class RegistryError(Exception):
    pass


def git(*args: str, input: bytes | None = None, check: bool = True) -> bytes:
    proc = subprocess.run(["git", *args], input=input, capture_output=True)
    if check and proc.returncode != 0:
        raise RegistryError(
            f"git {' '.join(args)} failed: {proc.stderr.decode(errors='replace').strip()}"
        )
    return proc.stdout


def show(rev: str, path: str) -> bytes | None:
    proc = subprocess.run(["git", "show", f"{rev}:{path}"], capture_output=True)
    if proc.returncode != 0:
        return None
    return proc.stdout


def resolve(rev: str) -> str:
    return git("rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}").decode().strip()


def load(rev: str) -> tuple[dict, str]:
    raw = show(rev, REGISTRY)
    if raw is None:
        raise RegistryError(f"{REGISTRY} is missing at {rev}")
    text = raw.decode()
    try:
        return tomllib.loads(text), text
    except tomllib.TOMLDecodeError as err:
        raise RegistryError(f"{REGISTRY} at {rev} is not valid TOML: {err}") from err


def patches(data: dict) -> list[dict]:
    value = data.get("patch", [])
    return value if isinstance(value, list) else []


def parents(rev: str) -> list[str]:
    """The parent SHAs recorded in rev's commit object. Unlike rev-parse REV^, this also works
    on the shallow single-commit checkout CI makes."""
    header = git("cat-file", "commit", rev).decode().split("\n\n", 1)[0]
    return [line.split()[1] for line in header.splitlines() if line.startswith("parent ")]


def registry_header(text: str) -> str | None:
    """The generated part of a registry: everything above the "Registered patches" line."""
    index = text.find(PATCH_MARKER)
    return text[:index] if index >= 0 else None


def trailer(rev: str, key: str) -> str:
    return git(
        "log", "-1", f"--format=%(trailers:key={key},valueonly,separator=%x2C)", rev
    ).decode().strip()


# ---------------------------------------------------------------------------------------------
# Path-dependency closure


def closure(rev: str, roots: list[str]) -> list[str]:
    """Every crate directory the roots reach through path dependencies at rev."""
    workspace_raw = show(rev, "codex-rs/Cargo.toml")
    if workspace_raw is None:
        raise RegistryError(f"codex-rs/Cargo.toml is missing at {rev}")
    workspace = tomllib.loads(workspace_raw.decode())
    workspace_deps = workspace.get("workspace", {}).get("dependencies", {})

    def dependency_dirs(crate_dir: str) -> list[str]:
        raw = show(rev, f"{crate_dir}/Cargo.toml")
        if raw is None:
            raise RegistryError(f"{crate_dir}/Cargo.toml is missing at {rev}")
        manifest = tomllib.loads(raw.decode())
        tables = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
        for target in manifest.get("target", {}).values():
            tables += [target.get("dependencies", {}), target.get("build-dependencies", {})]
        found = []
        for table in tables:
            for name, spec in table.items():
                if not isinstance(spec, dict):
                    continue
                if spec.get("workspace"):
                    inherited = workspace_deps.get(name)
                    if isinstance(inherited, dict) and "path" in inherited:
                        found.append(posixpath.normpath(posixpath.join("codex-rs", inherited["path"])))
                elif "path" in spec:
                    found.append(posixpath.normpath(posixpath.join(crate_dir, spec["path"])))
        return found

    seen: set[str] = set()
    stack = list(roots)
    while stack:
        crate_dir = stack.pop()
        if crate_dir in seen:
            continue
        seen.add(crate_dir)
        stack.extend(dependency_dirs(crate_dir))
    return sorted(seen)


# ---------------------------------------------------------------------------------------------
# Format check


def validate(data: dict, rev: str | None, verify_upstream_tag: bool) -> list[str]:
    errors: list[str] = []

    for key in data:
        if key not in HEADER_KEYS and key != "patch":
            errors.append(f"unknown top-level key {key!r}")
    for key in HEADER_KEYS:
        if key not in data:
            errors.append(f"missing top-level key {key!r}")
    if errors:
        return errors

    if data["schema"] != SCHEMA:
        errors.append(f"schema must be {SCHEMA}, not {data['schema']!r}")
    if not isinstance(data["base_tag"], str) or not TAG_RE.match(data["base_tag"]):
        errors.append(f"base_tag {data['base_tag']!r} is not a stable upstream tag (rust-vX.Y.Z)")
    for key in ("base_commit", "previous_pin"):
        if not isinstance(data[key], str) or not SHA_RE.match(data[key]):
            errors.append(f"{key} must be a full 40-character commit SHA")

    for key in ("closure_roots", "closure_dirs"):
        value = data[key]
        if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
            errors.append(f"{key} must be a list of strings")
            continue
        if not value:
            errors.append(f"{key} must not be empty")
        if value != sorted(set(value)):
            errors.append(f"{key} must be sorted and free of duplicates")
        for entry in value:
            if not entry.startswith("codex-rs/") or posixpath.normpath(entry) != entry:
                errors.append(f"{key} entry {entry!r} must be a normalised path under codex-rs/")
    models = data["report_models"]
    if not isinstance(models, list) or not all(isinstance(m, str) and m for m in models):
        errors.append("report_models must be a list of model slugs")

    seen_ids: set[str] = set()
    seen_files: dict[str, str] = {}
    raw_patches = data.get("patch", [])
    if not isinstance(raw_patches, list):
        errors.append("patch must be an array of [[patch]] tables")
        raw_patches = []
    for index, entry in enumerate(raw_patches):
        where = f"[[patch]] #{index + 1}"
        if not isinstance(entry, dict):
            errors.append(f"{where} must be a table")
            continue
        pid = entry.get("id")
        if isinstance(pid, str):
            where = f"[[patch]] {pid!r}"
        for key in entry:
            if key not in PATCH_KEYS:
                errors.append(f"{where}: unknown key {key!r}")
        missing = [key for key in PATCH_KEYS if key not in entry]
        if missing:
            errors.append(f"{where}: missing {', '.join(missing)}")
            continue
        if not isinstance(pid, str) or not ID_RE.match(pid):
            errors.append(f"{where}: id must be kebab-case")
        elif pid in seen_ids:
            errors.append(f"{where}: duplicate id")
        else:
            seen_ids.add(pid)
        for key in ("why", "instafy_side_alternative", "drop_when"):
            if not isinstance(entry[key], str) or not entry[key].strip():
                errors.append(f"{where}: {key} must be a non-empty string")
        for key in ("billing", "security"):
            if not isinstance(entry[key], bool):
                errors.append(f"{where}: {key} must be true or false")
        for key in ("files", "public_tests", "fork_tests"):
            value = entry[key]
            if not isinstance(value, list) or not value or not all(
                isinstance(v, str) and v for v in value
            ):
                errors.append(f"{where}: {key} must be a non-empty list of strings")
        if isinstance(entry["files"], list):
            for path in entry["files"]:
                if not isinstance(path, str):
                    continue
                if path.startswith("/") or posixpath.normpath(path) != path or path.startswith(".."):
                    errors.append(f"{where}: file {path!r} must be a normalised repository path")
                elif path.startswith(".github/") or path in OVERLAY_FILES:
                    errors.append(
                        f"{where}: file {path!r} belongs to the generated base commit, not a patch"
                    )
                elif path in seen_files:
                    errors.append(
                        f"{where}: file {path!r} is already registered by {seen_files[path]!r};"
                        " a file belongs to one patch"
                    )
                else:
                    seen_files[path] = str(pid)
        if isinstance(entry["fork_tests"], list):
            for test in entry["fork_tests"]:
                if isinstance(test, str) and not FORK_TEST_RE.match(test):
                    errors.append(
                        f"{where}: fork_tests entry {test!r} must be"
                        " '<package>|<lib or test target>|<filter>'"
                    )
        upstream = entry["upstream"]
        if not isinstance(upstream, dict) or set(upstream) != {"issue", "status"}:
            errors.append(f"{where}: upstream must be {{ issue = \"...\", status = \"...\" }}")
        else:
            if not isinstance(upstream["issue"], str) or not ISSUE_RE.match(upstream["issue"]):
                errors.append(f"{where}: upstream.issue must be an openai/codex issue URL")
            if upstream["status"] not in UPSTREAM_STATUSES:
                errors.append(f"{where}: upstream.status must be one of {', '.join(UPSTREAM_STATUSES)}")
        if not isinstance(entry["last_validated_tag"], str) or not TAG_RE.match(
            entry["last_validated_tag"]
        ):
            errors.append(f"{where}: last_validated_tag must be a stable upstream tag")

    if errors or rev is None:
        return errors

    missing_roots = [r for r in data["closure_roots"] if show(rev, f"{r}/Cargo.toml") is None]
    if missing_roots:
        errors.append(
            "closure_roots without a Cargo.toml at this revision (moved upstream?): "
            + ", ".join(missing_roots)
        )
    else:
        expected = closure(rev, data["closure_roots"])
        if data["closure_dirs"] != expected:
            added = sorted(set(expected) - set(data["closure_dirs"]))
            removed = sorted(set(data["closure_dirs"]) - set(expected))
            errors.append(
                "closure_dirs is not the path-dependency closure of closure_roots"
                f" (missing: {added or 'none'}; extra: {removed or 'none'});"
                " regenerate it with bump.sh"
            )

    if verify_upstream_tag:
        tag = data["base_tag"]
        # Ask for the peeled entry explicitly: an exact pattern alone lists only the tag object
        # of an annotated tag, not the commit it points to.
        out = git("ls-remote", UPSTREAM_URL, f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}").decode()
        refs = dict(reversed(line.split("\t", 1)) for line in out.splitlines() if "\t" in line)
        peeled = refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}")
        if peeled is None:
            errors.append(f"{UPSTREAM_URL} has no tag {tag}")
        elif peeled != data["base_commit"]:
            errors.append(f"upstream {tag} is {peeled}, but base_commit is {data['base_commit']}")
    return errors


# ---------------------------------------------------------------------------------------------
# Tree identity


def tree_identity(rev: str, mode: str = "any") -> tuple[list[str], list[str]]:
    """Return (errors, notes) for `git diff <base_commit> rev` against the registry.

    mode "exact" also requires every registered file to differ (rev carries every patch);
    mode "base" requires rev to be a generated base: base_commit is its only parent and no
    registered file differs, because a base lists the patches but carries none of their code.
    """
    data, _ = load(rev)
    errors = validate(data, None, False)
    if errors:
        return [f"{REGISTRY}: {e}" for e in errors], []
    base = data["base_commit"]
    if git("cat-file", "-t", base, check=False).decode().strip() != "commit":
        return [f"base_commit {base} is not available locally; fetch it first"], []
    if mode == "base" and parents(rev) != [base]:
        return [
            f"{rev} is not a generated base: its parents are {parents(rev) or 'none'},"
            f" not base_commit {base}"
        ], []

    registered: dict[str, str] = {}
    for entry in patches(data):
        for path in entry["files"]:
            registered[path] = entry["id"]

    out = git("diff", "--name-status", "--no-renames", "-z", base, rev, "--")
    fields = out.decode().split("\0")
    changes = list(zip(fields[0::2], fields[1::2]))

    errors, notes = [], []
    touched: dict[str, set[str]] = {}
    overlay, removed = 0, 0
    codex_rs = []
    for status, path in changes:
        if path.startswith("codex-rs/"):
            codex_rs.append(path)
        if path in OVERLAY_FILES or path.startswith(OVERLAY_DIR):
            overlay += 1
        elif status == "D" and (path in REMOVED_FILES or path.startswith(REMOVED_DIR)):
            removed += 1
        elif path in registered and mode == "base":
            errors.append(
                f"{path} ({status}) differs from {data['base_tag']}: patch"
                f" {registered[path]!r} registers it, but a generated base carries no patch code"
            )
        elif path in registered:
            touched.setdefault(registered[path], set()).add(path)
        else:
            errors.append(
                f"{path} ({status}) differs from {data['base_tag']} but no [[patch]] registers it"
            )

    if mode == "exact":
        for entry in patches(data):
            untouched = sorted(set(entry["files"]) - touched.get(entry["id"], set()))
            if untouched:
                errors.append(
                    f"patch {entry['id']!r} registers files the tree does not change: "
                    + ", ".join(untouched)
                )

    if codex_rs:
        notes.append(f"codex-rs differs from the tag in {len(codex_rs)} files: " + ", ".join(codex_rs))
    notes.append(
        f"{len(changes)} paths differ from {data['base_tag']} ({base[:12]}): {overlay} Instafy"
        f" files, {removed} deleted upstream automation files,"
        f" {sum(len(v) for v in touched.values())} registered patch files"
        + ("" if codex_rs else f"; codex-rs is identical to {data['base_tag']}")
    )
    return errors, notes


# ---------------------------------------------------------------------------------------------
# Land shape


def land_check(base: str, tip: str) -> tuple[list[str], str]:
    """Return (errors, summary): is tip the generated base plus registered patch commits only?

    tree-identity compares tip with the base_commit tip's own registry names, and lets the
    Instafy files differ freely, so a patch commit that rewrites CI or re-points base_commit
    passes it. This checks the commits between base and tip instead: each is a registered patch
    changing only its own files and the registry's patch section, and the header naming the tag
    stays the generated base's.
    """
    errors: list[str] = []
    _, base_text = load(base)
    tip_data, tip_text = load(tip)
    base_header, tip_header = registry_header(base_text), registry_header(tip_text)
    if base_header is None or tip_header is None:
        where = "base" if base_header is None else "tip"
        errors.append(f"{REGISTRY} has no {PATCH_MARKER!r} line at the {where}")
    elif base_header != tip_header:
        errors.append(
            f"{REGISTRY} above the {PATCH_MARKER!r} line differs from the generated base's;"
            " only bump.sh writes it"
        )
    base_commit = tip_data.get("base_commit")
    if parents(base) != [base_commit]:
        errors.append(
            f"the registry names base_commit {base_commit}, but the base's parents are"
            f" {parents(base) or 'none'}"
        )

    files_of = {
        entry.get("id"): set(entry.get("files", []))
        for entry in patches(tip_data)
        if isinstance(entry, dict)
    }
    commit_of: dict[str, str] = {}
    commits = git("rev-list", "--reverse", f"{base}..{tip}").decode().split()
    for commit in commits:
        pid = trailer(commit, "Instafy-Patch")
        if pid not in files_of:
            errors.append(
                f"{commit} carries Instafy-Patch: {pid!r}, which no [[patch]] at the tip registers"
            )
            continue
        if pid in commit_of:
            errors.append(
                f"{commit_of[pid]} and {commit} both carry Instafy-Patch: {pid}; squash them"
            )
        commit_of[pid] = commit
        changed = git(
            "diff-tree", "--no-commit-id", "--name-only", "-r", "--no-renames", "-z", commit
        ).decode().split("\0")
        extra = sorted(
            path for path in changed if path and path != REGISTRY and path not in files_of[pid]
        )
        if extra:
            errors.append(
                f"{commit} (Instafy-Patch: {pid}) changes {', '.join(extra)}, which patch"
                f" {pid!r} does not register"
            )
    missing = [str(pid) for pid in files_of if pid not in commit_of]
    if missing:
        errors.append("registered patches without a commit above the base: " + ", ".join(missing))
    return errors, (
        f"{len(commits)} registered patch commits above the base; registry header unchanged;"
        f" base parent is base_commit {str(base_commit)[:12]}"
    )


# ---------------------------------------------------------------------------------------------
# Workflow runners


WORKFLOW_KEY_RE = re.compile(r"""^(?:"([^"]*)"|'([^']*)'|([A-Za-z0-9_.-]+))\s*:(?:\s+(.*))?$""")


def workflow_runner_errors(text: str) -> tuple[list[str], int]:
    """Return (errors, job count) for a workflow's jobs.

    Every job must set `runs-on: ubuntu-24.04` and must not call a reusable workflow (`uses:`),
    whose jobs pick their own runners. The parser reads block-style YAML by indentation and
    fails closed on anything else (flow style, anchors, merge keys, tabs): the file is ours,
    and it only needs to be simple.
    """
    errors: list[str] = []
    jobs: dict[str, dict[str, list[tuple[int, str]]]] = {}
    in_jobs, job, job_indent, prop_indent = False, None, None, None
    for number, raw in enumerate(text.splitlines(), 1):
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        if raw[indent] == "\t":
            errors.append(f"line {number}: tab indentation")
            continue
        match = WORKFLOW_KEY_RE.match(stripped)
        key = next((g for g in match.groups()[:3] if g is not None), None) if match else None
        value = re.sub(r"(?:^|\s+)#.*$", "", match.group(4) or "").strip() if match else ""
        if indent == 0:
            in_jobs, job, job_indent = key == "jobs", None, None
            if in_jobs and value:
                errors.append(f"line {number}: jobs must be a block mapping")
            continue
        if not in_jobs:
            continue
        job_indent = job_indent or indent
        if indent < job_indent:
            errors.append(f"line {number}: unexpected indentation under jobs")
        elif indent == job_indent:
            if key is None or value:
                errors.append(f"line {number}: expected a job id with a block mapping")
                job = None
                continue
            job, prop_indent = key, None
            jobs[job] = {}
        elif job is not None:
            prop_indent = prop_indent or indent
            if indent == prop_indent:
                if key is None:
                    errors.append(f"line {number}: expected a key in job {job!r}")
                elif key in ("runs-on", "uses"):
                    jobs[job].setdefault(key, []).append((number, value))
    if not jobs and not errors:
        errors.append("no jobs found")
    for name, props in jobs.items():
        for number, _ in props.get("uses", []):
            errors.append(
                f"line {number}: job {name!r} calls a reusable workflow, which picks its own runners"
            )
        runners = props.get("runs-on", [])
        if len(runners) != 1:
            errors.append(f"job {name!r} must set runs-on exactly once, not {len(runners)} times")
            continue
        number, value = runners[0]
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        if value != ALLOWED_RUNNER:
            errors.append(
                f"line {number}: job {name!r} runs on {value or '(a block value)'!r};"
                f" this fork uses {ALLOWED_RUNNER} only (larger, macOS and Windows runners are billed)"
            )
    return errors, len(jobs)


# ---------------------------------------------------------------------------------------------
# Rendering


def toml_list(values: list[str]) -> str:
    if not values:
        return "[]"
    return "[\n" + "".join(f"  {json.dumps(v)},\n" for v in values) + "]"


def render(source_rev: str, base_tag: str, base_commit: str, previous_pin: str) -> str:
    data, text = load(source_rev)
    roots = data.get("closure_roots")
    if not isinstance(roots, list) or not roots:
        raise RegistryError(f"{REGISTRY} at {source_rev} has no closure_roots")
    kept_roots = []
    for root in roots:
        if show(base_commit, f"{root}/Cargo.toml") is None:
            print(
                f"warning: closure root {root} has no Cargo.toml at {base_tag}; dropped it."
                " runtime-agent must follow the upstream move.",
                file=sys.stderr,
            )
        else:
            kept_roots.append(root)
    dirs = closure(base_commit, kept_roots)
    header = HEADER_TEMPLATE.format(
        schema=SCHEMA,
        base_tag=base_tag,
        base_commit=base_commit,
        previous_pin=previous_pin,
        closure_roots=toml_list(sorted(set(kept_roots))),
        closure_dirs=toml_list(dirs),
        report_models=toml_list(list(data.get("report_models", []))),
    )
    index = text.find(PATCH_MARKER)
    section = text[index:] if index >= 0 else DEFAULT_PATCH_SECTION
    rendered = header + section
    new_data = tomllib.loads(rendered)
    errors = validate(new_data, None, False)
    if errors:
        raise RegistryError("rendered registry is invalid: " + "; ".join(errors))
    return rendered


# ---------------------------------------------------------------------------------------------
# Cargo.lock drift


def lock_drift(before_text: str, after_text: str, version: str) -> tuple[list[str], str]:
    before, after = tomllib.loads(before_text), tomllib.loads(after_text)
    errors = []
    if before.get("version") != after.get("version"):
        errors.append(f"lock file format changed: {before.get('version')} -> {after.get('version')}")

    def split(lock: dict) -> tuple[dict[str, dict], list[str]]:
        members, external = {}, []
        for package in lock.get("package", []):
            if "source" in package:
                external.append(json.dumps(package, sort_keys=True))
            else:
                members[package["name"]] = package
        return members, sorted(external)

    before_members, before_external = split(before)
    after_members, after_external = split(after)
    if before_external != after_external:
        added = sorted(set(after_external) - set(before_external))
        removed = sorted(set(before_external) - set(after_external))
        names = sorted({json.loads(p)["name"] for p in added + removed})
        errors.append(f"external dependencies changed: {', '.join(names[:20])}")
    if set(before_members) != set(after_members):
        errors.append(
            "workspace members changed: "
            + ", ".join(sorted(set(before_members) ^ set(after_members)))
        )
    moved, wrong, rewired = 0, [], []
    for name in sorted(set(before_members) & set(after_members)):
        old, new = dict(before_members[name]), dict(after_members[name])
        if old.get("version") != new.get("version"):
            moved += 1
            if new.get("version") != version:
                wrong.append(f"{name} {old.get('version')} -> {new.get('version')}")
        old.pop("version", None)
        new.pop("version", None)
        if old != new:
            rewired.append(name)
    if wrong:
        errors.append(
            f"{len(wrong)} workspace members moved to a version other than {version}: "
            + ", ".join(wrong[:5])
            + (", ..." if len(wrong) > 5 else "")
        )
    if rewired:
        errors.append("workspace members changed dependencies: " + ", ".join(rewired[:10]))
    return errors, f"{moved} workspace members moved to {version}; external dependencies unchanged"


# ---------------------------------------------------------------------------------------------
# Upstream report


FEATURE_RE = re.compile(
    r'FeatureSpec \{\s*id: Feature::(\w+),\s*key: "([^"]+)",\s*stage: (Stage::\w+)(.*?)'
    r"default_enabled: (true|false|[^,\n]+),",
    re.S,
)


FEATURE_ID_RE = re.compile(r"id: Feature::(\w+),")


def features(rev: str) -> tuple[dict[str, tuple[str, str, str]], dict[str, str]] | None:
    """(parsed, unparsed) FEATURES entries at rev.

    parsed maps a flag to (key, stage, default). An entry whose stage is not a plain
    `Stage::X` (an `if cfg!(...)` expression, say) lands in unparsed instead, as its
    whitespace-normalised source text, so the report can still say whether it changed.
    """
    raw = show(rev, FEATURES)
    if raw is None:
        return None
    text = raw.decode()
    start = text.find("pub const FEATURES: &[FeatureSpec] = &[")
    if start < 0:
        return None
    end = text.find("\n];", start)
    table = text[start : end if end >= 0 else len(text)]
    parsed = {
        m.group(1): (m.group(2), m.group(3), m.group(5).strip())
        for m in FEATURE_RE.finditer(table)
    }
    unparsed = {}
    for chunk in table.split("FeatureSpec {")[1:]:
        found = FEATURE_ID_RE.search(chunk)
        if found and found.group(1) not in parsed:
            unparsed[found.group(1)] = " ".join(chunk.split())
    return parsed, unparsed


def catalog(rev: str) -> dict[str, dict] | None:
    raw = show(rev, CATALOG)
    if raw is None:
        return None
    return {m.get("slug"): m for m in json.loads(raw).get("models", [])}


def short(value) -> str:
    text = json.dumps(value, sort_keys=True)
    if len(text) <= 120:
        return text
    digest = hashlib.sha256(text.encode()).hexdigest()[:12]
    return f"<{len(text)} chars, sha256 {digest}>"


def describe_feature(value: tuple[str, str, str] | None) -> str:
    return "absent" if value is None else f"{value[1]} default={value[2]}"


def report(old: str, new: str, models: list[str]) -> str:
    lines = []
    old_catalog, new_catalog = catalog(old), catalog(new)
    lines.append(f"Catalog ({CATALOG}):")
    if old_catalog is None or new_catalog is None:
        lines.append(f"  {CATALOG} is missing at {'old' if old_catalog is None else 'new'}; compare by hand")
    else:
        added = sorted(set(new_catalog) - set(old_catalog), key=str)
        removed = sorted(set(old_catalog) - set(new_catalog), key=str)
        lines.append(f"  models added: {', '.join(map(str, added)) or 'none'}")
        lines.append(f"  models removed: {', '.join(map(str, removed)) or 'none'}")
        for slug in models:
            before, after = old_catalog.get(slug), new_catalog.get(slug)
            if before is None and after is None:
                lines.append(f"  {slug}: in neither catalog")
                continue
            if before is None or after is None:
                lines.append(f"  {slug}: {'added' if before is None else 'REMOVED'}")
                continue
            changed = sorted(k for k in set(before) | set(after) if before.get(k) != after.get(k))
            if not changed:
                lines.append(f"  {slug}: unchanged")
                continue
            lines.append(f"  {slug}: {len(changed)} changed keys")
            for key in changed:
                lines.append(f"    {key}: {short(before.get(key))} -> {short(after.get(key))}")
    old_table, new_table = features(old), features(new)
    lines.append(f"Feature flags ({FEATURES}):")
    if old_table is None or new_table is None:
        lines.append("  feature table not found; compare by hand")
    else:
        (old_features, old_unparsed), (new_features, new_unparsed) = old_table, new_table
        unparsed = set(old_unparsed) | set(new_unparsed)
        changes = []
        for name in sorted((set(old_features) | set(new_features)) - unparsed):
            before, after = old_features.get(name), new_features.get(name)
            if before != after:
                changes.append(
                    f"    {name}: {describe_feature(before)} -> {describe_feature(after)}"
                )
        lines.append(f"  {len(changes)} flags added, removed or changed stage/default")
        lines.extend(changes)
        if unparsed:
            lines.append(
                f"  {len(unparsed)} flags whose stage is not a plain Stage::X; compare by hand:"
            )
            for name in sorted(unparsed):
                before, after = old_unparsed.get(name), new_unparsed.get(name)
                if before is not None and after is not None:
                    state = "unchanged" if before == after else "CHANGED"
                elif before is None:
                    was = old_features.get(name)
                    state = "added" if was is None else f"was {describe_feature(was)}; now an expression"
                else:
                    now = new_features.get(name)
                    state = "REMOVED" if now is None else f"was an expression; now {describe_feature(now)}"
                lines.append(f"    {name}: {state}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------------------------
# CLI


def cmd_check(args) -> int:
    rev = resolve(args.rev)
    data, _ = load(rev)
    errors = validate(data, rev, args.verify_upstream_tag)
    if errors:
        for error in errors:
            print(f"error: {REGISTRY}: {error}", file=sys.stderr)
        return 1
    count = len(patches(data))
    print(
        f"{REGISTRY} ok: base_tag {data['base_tag']} ({data['base_commit'][:12]}),"
        f" {count} registered patch{'es' if count != 1 else ''},"
        f" {len(data['closure_dirs'])} closure dirs"
        + (", base_commit matches upstream" if args.verify_upstream_tag else "")
    )
    return 0


def cmd_tree_identity(args) -> int:
    errors, notes = tree_identity(resolve(args.rev), args.mode)
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    for note in notes:
        print(note)
    return 1 if errors else 0


def report_errors(errors: list[str], summary: str, prefix: str) -> int:
    for error in errors:
        print(f"error: {prefix}{error}", file=sys.stderr)
    if errors:
        return 1
    print(summary)
    return 0


def patch_by_id(rev: str, pid: str) -> dict:
    data, _ = load(rev)
    for entry in patches(data):
        if entry.get("id") == pid:
            return entry
    raise RegistryError(f"no [[patch]] with id {pid!r} at {rev}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    def with_rev(p, default="HEAD"):
        p.add_argument("--rev", default=default, required=default is None)
        return p

    p = with_rev(sub.add_parser("check"))
    p.add_argument("--verify-upstream-tag", action="store_true")
    p = with_rev(sub.add_parser("tree-identity"))
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--exact", dest="mode", action="store_const", const="exact", default="any")
    mode.add_argument("--base", dest="mode", action="store_const", const="base")
    p = sub.add_parser("land-check")
    p.add_argument("--base", required=True)
    p.add_argument("--tip", required=True)
    with_rev(sub.add_parser("workflow-runners"))
    with_rev(sub.add_parser("count"))
    p = with_rev(sub.add_parser("field"))
    p.add_argument("key", choices=["schema", "base_tag", "base_commit", "previous_pin"])
    with_rev(sub.add_parser("patch-ids"))
    p = with_rev(sub.add_parser("patch-files"))
    p.add_argument("id")
    p = with_rev(sub.add_parser("patch-info"))
    p.add_argument("id")
    with_rev(sub.add_parser("fork-tests"))
    with_rev(sub.add_parser("tag-version"), default=None)
    p = sub.add_parser("render")
    p.add_argument("--from", dest="source", required=True)
    p.add_argument("--base-tag", required=True)
    p.add_argument("--base-commit", required=True)
    p.add_argument("--previous-pin", required=True)
    p = sub.add_parser("lock-drift")
    p.add_argument("--before", required=True)
    p.add_argument("--after", required=True)
    p.add_argument("--version", required=True)
    p = sub.add_parser("report")
    p.add_argument("--old", required=True)
    p.add_argument("--new", required=True)
    p.add_argument("--models-from", default="HEAD")
    args = parser.parse_args(argv)

    try:
        if args.command == "check":
            return cmd_check(args)
        if args.command == "tree-identity":
            return cmd_tree_identity(args)
        if args.command == "land-check":
            errors, summary = land_check(resolve(args.base), resolve(args.tip))
            return report_errors(errors, summary, "")
        if args.command == "workflow-runners":
            raw = show(args.rev, WORKFLOW)
            if raw is None:
                raise RegistryError(f"{WORKFLOW} is missing at {args.rev}")
            errors, jobs = workflow_runner_errors(raw.decode())
            return report_errors(errors, f"{jobs} jobs, all on {ALLOWED_RUNNER}", f"{WORKFLOW}: ")
        if args.command == "count":
            data, _ = load(args.rev)
            print(len(patches(data)))
        elif args.command == "field":
            data, _ = load(args.rev)
            print(data.get(args.key, ""))
        elif args.command == "patch-ids":
            data, _ = load(args.rev)
            for entry in patches(data):
                print(entry["id"])
        elif args.command == "patch-files":
            for path in patch_by_id(args.rev, args.id)["files"]:
                print(path)
        elif args.command == "patch-info":
            entry = patch_by_id(args.rev, args.id)
            print(f"patch {entry['id']}")
            print(f"  why: {entry['why']}")
            print(f"  billing: {entry['billing']}, security: {entry['security']}")
            print(f"  files: {', '.join(entry['files'])}")
            print(f"  public tests: {', '.join(entry['public_tests'])}")
            print(f"  fork tests: {', '.join(entry['fork_tests'])}")
            print(f"  upstream: {entry['upstream']['issue']} ({entry['upstream']['status']})")
            print(f"  drop when: {entry['drop_when']}")
        elif args.command == "fork-tests":
            data, _ = load(args.rev)
            for entry in patches(data):
                for test in entry["fork_tests"]:
                    print(test)
        elif args.command == "tag-version":
            raw = show(args.rev, "codex-rs/Cargo.toml")
            if raw is None:
                raise RegistryError(f"codex-rs/Cargo.toml is missing at {args.rev}")
            print(tomllib.loads(raw.decode()).get("workspace", {}).get("package", {}).get("version", ""))
        elif args.command == "render":
            if not TAG_RE.match(args.base_tag):
                raise RegistryError(f"{args.base_tag!r} is not a stable upstream tag")
            sys.stdout.write(
                render(args.source, args.base_tag, args.base_commit, args.previous_pin)
            )
        elif args.command == "lock-drift":
            with open(args.before) as before, open(args.after) as after:
                errors, summary = lock_drift(before.read(), after.read(), args.version)
            return report_errors(errors, summary, "Cargo.lock: ")
        elif args.command == "report":
            data, _ = load(args.models_from)
            print(report(args.old, args.new, list(data.get("report_models", []))))
    except RegistryError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
