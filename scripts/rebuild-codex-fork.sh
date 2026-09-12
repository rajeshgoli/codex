#!/usr/bin/env bash
# Install both runtime binaries outside Cargo's disposable build directory.
set -euo pipefail

keep_build=false
case "${1:-}" in
  --keep-build) keep_build=true ;;
  --help|-h)
    echo "Usage: scripts/rebuild-codex-fork.sh [--keep-build]"
    echo "Build and install codex-fork and its code-mode host, then clean release artifacts."
    echo "Use --keep-build to retain the release cache for faster subsequent builds."
    exit 0
    ;;
  "") ;;
  *) echo "Unknown option: $1" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
  echo "Expected at most one option" >&2
  exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="$repo_root/codex-rs/target"
runtime_dir="$repo_root/.codex-fork-runtime"
cd "$repo_root/codex-rs"

export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=symbols
# Reuse the package builder's exact-version, checksum-verified V8 artifacts.
python3 - "$repo_root" "$target_dir" <<'PY'
import os
import subprocess
import sys
from pathlib import Path

repo_root, target_dir = map(Path, sys.argv[1:])
os.environ["CODEX_REPO_ROOT"] = str(repo_root)
sys.path.insert(0, str(repo_root / "scripts"))
from codex_package.targets import TARGET_SPECS
from codex_package.v8 import resolve_codex_v8_cargo_env

rustc_version = subprocess.check_output(["rustc", "-vV"], text=True)
host = next(line.removeprefix("host: ") for line in rustc_version.splitlines() if line.startswith("host: "))
v8_env = resolve_codex_v8_cargo_env(
    TARGET_SPECS[host], cache_root=target_dir / "release" / "fork-v8"
)
subprocess.run(
    ["cargo", "build", "--locked", "--release", "--target-dir", str(target_dir),
     "-p", "codex-cli", "--bin", "codex-fork",
     "-p", "codex-code-mode-host", "--bin", "codex-code-mode-host"],
    env={**os.environ, **v8_env},
    check=True,
)
PY

mkdir -p "$runtime_dir"
stage_dir="$(mktemp -d "$runtime_dir/.install.XXXXXX")"
trap 'rm -rf -- "$stage_dir"' EXIT
for binary in codex-fork codex-code-mode-host; do
  install -m 755 "$target_dir/release/$binary" "$stage_dir/$binary"
done
"$stage_dir/codex-fork" --version
"$stage_dir/codex-code-mode-host" --help >/dev/null

# Rename each file so running processes can retain their existing executable.
mv -f "$stage_dir/codex-code-mode-host" "$runtime_dir/codex-code-mode-host"
mv -f "$stage_dir/codex-fork" "$runtime_dir/codex-fork"
"$repo_root/bin/codex-fork" --version
"$repo_root/bin/codex-code-mode-host" --help >/dev/null

if [[ "$keep_build" == false ]]; then
  cargo clean --release --target-dir "$target_dir"
fi
du -sh "$runtime_dir"
