# Session Manager Codex Fork Operations

This repository is a thin fork of `openai/codex` used by Session Manager.

## Branch Strategy

- `main`: protected default branch; no direct pushes.
- `sm/<ticket>-<slug>`: implementation branches for Session Manager patch stack.
- `sync/upstream-<yyyymmdd>`: automated/manual upstream sync branches.

All changes merge through pull requests targeting `main`.

## CI Baseline

Fork CI is `.github/workflows/sm-fork-baseline.yml` and nothing else. It builds
the bridge-critical crates, tests `codex-protocol`, and checks that no other
workflow is enabled.

Every upstream workflow stays disabled in the fork's Actions settings. They
target OpenAI's private `codex-runners` runner group and paid large macOS/ARM
runners, so on this fork they fail before running any code. Disabling them in
settings, rather than editing the files, keeps upstream syncs conflict-free.

When an upstream sync adds a workflow, the `workflow-allowlist` job fails and
prints the path. Disable it with `gh workflow disable -R rajeshgoli/codex <path>`.

## Upstream Sync Workflow

Cadence: weekly (or immediately for high-priority upstream fixes).

1. Fetch upstream `openai/codex` into local `upstream` remote.
2. Create `sync/upstream-<yyyymmdd>` from current fork `main`.
3. Merge `upstream/main` into the sync branch.
4. Resolve conflicts with Session Manager bridge patches favored only where required.
5. Run fork CI (`sm-fork-baseline`) and Session Manager bridge conformance checks.
6. Open PR into `main` and merge after review.

If sync conflicts are large, split into:

- PR 1: pure upstream merge conflict resolution.
- PR 2: re-apply/adjust Session Manager bridge patches.

## Release Tagging For SM Pinning

Use annotated tags with this format:

- `sm-fork-v<YYYY.MM.DD>-schema-v<N>`

Tag notes must include:

- upstream base commit SHA
- fork commit SHA
- event schema version
- compatibility notes for Session Manager

Session Manager pins explicit fork tags and schema version in provider metadata.

Bootstrap verification commit: 2026-03-01
