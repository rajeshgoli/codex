# Session Manager Codex Fork Operations

This repository is a thin fork of `openai/codex` used by Session Manager.

## Branch Strategy

- `main`: protected default branch; no direct pushes.
- `sm/<ticket>-<slug>`: implementation branches for Session Manager patch stack.
- `sync/upstream-<yyyymmdd>`: automated/manual upstream sync branches.

All changes merge through pull requests targeting `main`.

## CI Baseline

Fork CI baseline uses upstream workflows already in this repository:

- `.github/workflows/ci.yml`
- `.github/workflows/rust-ci.yml`

A bootstrap commit is used to verify GitHub Actions runs successfully on this fork's `main` branch.

## Upstream Sync Workflow

Cadence: weekly (or immediately for high-priority upstream fixes).

1. Fetch upstream `openai/codex` into local `upstream` remote.
2. Create `sync/upstream-<yyyymmdd>` from current fork `main`.
3. Merge `upstream/main` into the sync branch.
4. Resolve conflicts with Session Manager bridge patches favored only where required.
5. Run fork CI and Session Manager bridge conformance checks.
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
