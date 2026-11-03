#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UPSTREAM="sevenaaaaaaaaa/V2HTML"
REF="${1:-960e8fdbdc16aaa5515e41efe494a3c56cdab2ba}"
DEST="$ROOT/third_party/conflow"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

SHA="$(gh api "repos/$UPSTREAM/commits/$REF" --jq .sha)"
SRC="$TMP/src"
mkdir -p "$SRC"
python3 - "$UPSTREAM" "$SHA" "$SRC" <<'PY'
import base64
import json
import pathlib
import subprocess
import sys

repo, sha, destination = sys.argv[1:]
destination = pathlib.Path(destination)
tree = json.loads(subprocess.check_output([
    "gh", "api", f"repos/{repo}/git/trees/{sha}?recursive=1"
]))["tree"]
exact = {"LICENSE", "requirements.txt", "bin/conflow.py", "bin/v2h.py"}
prefixes = ("engine/", "prompts/", "templates/")
for entry in tree:
    path = entry["path"]
    if entry["type"] != "blob" or not (path in exact or path.startswith(prefixes)):
        continue
    payload = json.loads(subprocess.check_output([
        "gh", "api", f"repos/{repo}/git/blobs/{entry['sha']}"
    ]))
    target = destination / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(base64.b64decode(payload["content"]))
PY

mkdir -p "$DEST/bin"
rm -rf "$DEST/engine" "$DEST/prompts" "$DEST/templates"
cp "$SRC/LICENSE" "$DEST/LICENSE"
cp "$SRC/requirements.txt" "$DEST/requirements.txt"
cp "$SRC/bin/conflow.py" "$DEST/bin/conflow.py"
cp "$SRC/bin/v2h.py" "$DEST/bin/v2h.py"
cp -R "$SRC/engine" "$DEST/engine"
cp -R "$SRC/prompts" "$DEST/prompts"
cp -R "$SRC/templates" "$DEST/templates"
printf '%s\n' \
  "upstream=https://github.com/$UPSTREAM" \
  "commit=$SHA" \
  "license=MIT" \
  > "$DEST/UPSTREAM"

echo "ConFlow synced: $SHA"
