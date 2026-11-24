#!/usr/bin/env bash
# Steps of the colgrep index workflows (index-update and index-rebuild).
#
#   colgrep-index.sh install    colgrep release tarball and ONNX Runtime (CPU) from pip
#   colgrep-index.sh checkout   guard 2: index the current origin/main, not the commit that triggered the run
#   colgrep-index.sh restore    download the newest published index, if any (index-update only)
#   colgrep-index.sh build      colgrep init on the checkout, then a query to prove the index answers
#   colgrep-index.sh publish    guards 3 and 4: upload the new asset only if it is not older than the published
#                               one, then delete every asset older than the previous one
#
# Environment: TAG (the release tag that holds the index), REPO (owner/name), GH_TOKEN. The release is a
# prerelease under a fixed tag, so it never shows as the latest app release and does not match release.yml's
# v* tag trigger. Asset name: colgrep-index-<commit sha>-<run id>.tar.zst, holding ~/.local/share/colgrep (the
# index, keyed by project path, and the runner's checkout path is constant) and ~/.cache/huggingface (the model).
set -euo pipefail

: "${TAG:?TAG is not set}"
: "${REPO:?REPO is not set}"
: "${COLGREP_VERSION:=latest}"
: "${ORT_VERSION:=1.23.2}"
: "${KEEP_ASSETS:=2}"

asset_re='^colgrep-index-[0-9a-f]{40}-[0-9]+\.tar\.zst$'
scratch="${RUNNER_TEMP:?RUNNER_TEMP is not set}/colgrep-index"
mkdir -p "$scratch"

# A test tag must never match release.yml's v* trigger or collide with an app release.
check_tag() {
  case "$TAG" in
    *[!A-Za-z0-9._-]* | "" | v[0-9]*) echo "refusing tag '$TAG': use letters, digits, . _ - and not a v<digit> prefix" >&2; exit 1 ;;
  esac
}

# Prints "<name>\t<sha>" of the newest asset by upload time, or nothing when the release or its assets are
# missing. Any other failure of the API call is an error.
newest_asset() {
  local err out
  err="$scratch/api.err"
  if ! out=$(gh api "repos/$REPO/releases/tags/$TAG" 2>"$err"); then
    if grep -q 'HTTP 404' "$err"; then return 0; fi
    cat "$err" >&2
    return 1
  fi
  printf '%s' "$out" | jq -r --arg re "$asset_re" \
    '[.assets[] | select(.name | test($re))] | sort_by(.created_at) | last // empty | [.name, (.name | capture("^colgrep-index-(?<sha>[0-9a-f]{40})-").sha)] | @tsv'
}

# Prints every matching asset name, newest upload first.
assets_newest_first() {
  gh api "repos/$REPO/releases/tags/$TAG" |
    jq -r --arg re "$asset_re" '[.assets[] | select(.name | test($re))] | sort_by(.created_at) | reverse | .[].name'
}

cmd_install() {
  local base d lib
  if [ "$COLGREP_VERSION" = latest ]; then
    base=https://github.com/lightonai/next-plaid/releases/latest/download
  else
    base="https://github.com/lightonai/next-plaid/releases/download/$COLGREP_VERSION"
  fi
  curl -fsSL "$base/colgrep-x86_64-unknown-linux-gnu.tar.xz" | tar xJ -C "$RUNNER_TEMP"
  d=$(ls -d "$RUNNER_TEMP"/colgrep-*/ | head -1)
  echo "$d" >> "$GITHUB_PATH"
  "$d/colgrep" --version
  python3 -m venv "$RUNNER_TEMP/ort"
  "$RUNNER_TEMP/ort/bin/pip" install -q "onnxruntime==$ORT_VERSION"
  lib=$(ls "$RUNNER_TEMP"/ort/lib/python*/site-packages/onnxruntime/capi/libonnxruntime.so* | head -1)
  echo "ORT_DYLIB_PATH=$lib" >> "$GITHUB_ENV"
}

cmd_checkout() {
  check_tag
  git fetch --no-tags origin main
  git checkout --detach origin/main
  echo "indexing $(git rev-parse HEAD) (the run was triggered by ${TRIGGER_SHA:-unknown})"
}

cmd_restore() {
  local row name
  row=$(newest_asset)
  if [ -z "$row" ]; then
    echo "no published index; building from nothing"
    return 0
  fi
  name=${row%%$'\t'*}
  echo "restoring $name"
  gh release download "$TAG" --repo "$REPO" --pattern "$name" --dir "$scratch" --clobber
  tar -C "$HOME" --zstd -xf "$scratch/$name"
  rm -f "$scratch/$name"
  du -sm "$HOME/.local/share/colgrep" "$HOME/.cache/huggingface" 2>/dev/null || true
}

cmd_build() {
  colgrep init -y .
  # Prove the index answers before anything is uploaded.
  colgrep -k 3 "where is the main entry point of the app" > "$scratch/query.txt"
  test -s "$scratch/query.txt"
  du -sm "$HOME/.local/share/colgrep" "$HOME/.cache/huggingface" 2>/dev/null || true
}

cmd_publish() {
  check_tag
  local own row pub_name pub_sha file
  own=$(git rev-parse HEAD)

  # Guard 3: re-read the newest published asset now, and upload only when this run's commit is that asset's
  # commit or a descendant of it. Otherwise exit successfully with the published assets untouched.
  row=$(newest_asset)
  if [ -n "$row" ]; then
    pub_name=${row%%$'\t'*}
    pub_sha=${row##*$'\t'}
    if [ "$pub_sha" != "$own" ]; then
      if ! git cat-file -e "$pub_sha^{commit}" 2>/dev/null; then
        git fetch --no-tags origin "$pub_sha" 2>/dev/null || true
      fi
      if ! git cat-file -e "$pub_sha^{commit}" 2>/dev/null; then
        echo "the newest published index ($pub_name) is built from $pub_sha, which this checkout does not have; not uploading"
        return 0
      fi
      if ! git merge-base --is-ancestor "$pub_sha" "$own"; then
        echo "the newest published index ($pub_name) is not an ancestor of $own; not uploading"
        return 0
      fi
    fi
  fi

  # The release is created here, once, when it does not exist (one run at a time, so no race).
  if ! gh release view "$TAG" --repo "$REPO" > /dev/null 2>&1; then
    gh release create "$TAG" --repo "$REPO" --prerelease --latest=false --target "$own" \
      --title "colgrep index" \
      --notes "Search index for the code search tool, rebuilt by the index workflows. Not an app release."
  fi

  file="colgrep-index-${own}-${GITHUB_RUN_ID:?GITHUB_RUN_ID is not set}.tar.zst"
  test -d "$HOME/.local/share/colgrep"
  tar -C "$HOME" -cf - .local/share/colgrep .cache/huggingface | zstd -T0 -3 -q -o "$scratch/$file"
  ls -l "$scratch/$file"

  # Guard 4, first half: upload the new asset under its own name before anything is deleted.
  gh release upload "$TAG" "$scratch/$file" --repo "$REPO"
  if ! assets_newest_first | grep -Fxq "$file"; then
    echo "the uploaded asset $file is not listed on the release" >&2
    exit 1
  fi

  # Guard 4, second half: delete only assets older than the previous one (keep the new one and the one before).
  assets_newest_first | tail -n "+$((KEEP_ASSETS + 1))" | while IFS= read -r old; do
    echo "deleting $old"
    gh release delete-asset "$TAG" "$old" --repo "$REPO" --yes
  done
  rm -f "$scratch/$file"
}

case "${1:-}" in
  install) cmd_install ;;
  checkout) cmd_checkout ;;
  restore) check_tag; cmd_restore ;;
  build) cmd_build ;;
  publish) cmd_publish ;;
  *) echo "usage: colgrep-index.sh install|checkout|restore|build|publish" >&2; exit 2 ;;
esac
