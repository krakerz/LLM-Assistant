#!/usr/bin/env bash
# Bumps the version in src-tauri/Cargo.toml and src-tauri/tauri.conf.json,
# promotes the "Unreleased" changelog section to the new version, and commits.
# Does NOT push, and does NOT tag -- once this commit reaches main (via your
# usual PR flow), the release workflow reads the version out of Cargo.toml and
# creates the vX.Y.Z tag and draft release itself.
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "Usage: $0 <version>   e.g. $0 0.2.0" >&2
  exit 1
fi

version="$1"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Version must be plain semver (X.Y.Z), got: $version" >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_toml="$root/src-tauri/Cargo.toml"
tauri_conf="$root/src-tauri/tauri.conf.json"
changelog="$root/CHANGELOG.md"

if git -C "$root" rev-parse "v$version" >/dev/null 2>&1; then
  echo "A local tag v$version already exists" >&2
  exit 1
fi

if ! grep -q "^## \[Unreleased\]$" "$changelog"; then
  echo "CHANGELOG.md has no [Unreleased] heading to promote" >&2
  exit 1
fi

sed -i "0,/^version = \".*\"/s//version = \"$version\"/" "$cargo_toml"
sed -i "0,/\"version\": \".*\"/s//\"version\": \"$version\"/" "$tauri_conf"

date="$(date +%Y-%m-%d)"
python3 - "$changelog" "$version" "$date" <<'PYEOF'
import sys
path, version, date = sys.argv[1:4]
text = open(path).read()
marker = "## [Unreleased]\n"
idx = text.index(marker)
insert_at = idx + len(marker)
text = text[:insert_at] + f"\n## [{version}] - {date}\n" + text[insert_at:]
open(path, "w").write(text)
PYEOF

(cd "$root/src-tauri" && cargo update -p llm-assistant --precise "$version" >/dev/null 2>&1 || true)

git -C "$root" add "$cargo_toml" "$tauri_conf" "$changelog" "$root/src-tauri/Cargo.lock"
git -C "$root" commit -m "chore(release): v$version"

echo "Done. Review the commit, then merge/push it to main to trigger the v$version release."
