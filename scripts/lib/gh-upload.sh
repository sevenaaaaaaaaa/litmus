# shellcheck shell=bash
# Shared GitHub Release upload helper (sourced by scripts/release.sh and scripts/daily-release.sh).
#
# This Mac's DNS returns NXDOMAIN for uploads.github.com, so when it does not resolve we
# resolve it via DoH (1.1.1.1) and upload with `curl --resolve`.

GH_REPO_SLUG="${GH_REPO_SLUG:-sevenaaaaaaaaa/litmus}"

# gh_release_has_asset <tag> <name>
gh_release_has_asset() {
  gh release view "$1" -R "$GH_REPO_SLUG" --json assets --jq '.assets[].name' 2>/dev/null \
    | grep -Fxq "$2"
}

# gh_upload_asset <tag> <file> [--clobber]
gh_upload_asset() {
  local tag="$1" f="$2" clobber="${3:-}" name ip rel_id token
  name="$(basename "$f")"
  if python3 -c 'import socket;socket.gethostbyname("uploads.github.com")' 2>/dev/null; then
    gh release upload "$tag" "$f" -R "$GH_REPO_SLUG" ${clobber:+--clobber}
  else
    echo "→ DNS 拦截 uploads.github.com，DoH 解析..."
    if [[ -n "$clobber" ]] && gh_release_has_asset "$tag" "$name"; then
      gh release delete-asset "$tag" "$name" -R "$GH_REPO_SLUG" --yes
    fi
    ip=$(curl -s "https://1.1.1.1/dns-query?name=uploads.github.com&type=A" \
      -H "accept: application/dns-json" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["Answer"][-1]["data"])')
    rel_id=$(gh api "repos/$GH_REPO_SLUG/releases/tags/$tag" --jq .id)
    token=$(gh auth token)
    curl --fail --silent --show-error -X POST \
      -H "Authorization: Bearer $token" -H "Content-Type: application/octet-stream" \
      --resolve "uploads.github.com:443:$ip" \
      "https://uploads.github.com/repos/$GH_REPO_SLUG/releases/$rel_id/assets?name=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1]))' "$name")" \
      --data-binary "@$f" >/dev/null
  fi
  echo "✅ 上传 $name"
}
