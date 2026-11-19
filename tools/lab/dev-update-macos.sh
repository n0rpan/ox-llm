#!/usr/bin/env bash
# macOS arm64 twin of dev-update.sh: public, attested dev assets only; no sign-in, no PATH,
# shell-profile, service or Gatekeeper changes. Needs gh (brew install gh); no checksum-only mode.
# Written for bash 3.2 and BSD tools; only POSIX find/ln/mv/diff/tar features beyond that are probed.
set -euo pipefail
umask 077
repo=oxstack/ox-llm
test "$#" -le 1 || { echo 'Usage: dev-update-macos.sh [separate-dev-prefix]' >&2; exit 2; }
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || { echo 'This installer is for macOS on Apple silicon (arm64) only' >&2; exit 1; }
[[ $UID != 0 && $EUID != 0 && -z ${SUDO_USER-} ]] || { echo 'Refusing to run as root or under sudo' >&2; exit 1; }
command -v gh >/dev/null || { echo 'gh is required to verify build provenance: brew install gh (there is no checksum-only fallback)' >&2; exit 1; }
for tool in curl jq shasum tar dscl find mv ln diff mktemp awk id; do command -v "$tool" >/dev/null || { echo "Missing tool: $tool" >&2; exit 1; }; done
# Plain absolute path with no relative or doubled components (no realpath on macOS).
plain_abs() { [[ $1 == /* && $1 != *$'\n'* && $1 != *$'\r'* && $1 != *//* && /$1/ != */../* && /$1/ != */./* ]]; }
# The real account home, not $HOME: $HOME can be redirected into a workspace.
name=$(id -un)
account_home=$(dscl . -read "/Users/$name" NFSHomeDirectory | awk '$1=="NFSHomeDirectory:" {sub(/^[^ ]+ +/, ""); print; exit}')
account_uid=$(dscl . -read "/Users/$name" UniqueID | awk '$1=="UniqueID:" {print $2; exit}')
[[ $account_uid == "$UID" && $account_home != / ]] && plain_abs "$account_home" || { echo 'Cannot determine the account home' >&2; exit 1; }
root="${account_home%/}/.local/state/oxc/dev-builds"
prefix=${1:-"$root/default"}
prefix=${prefix%/}
plain_abs "$prefix" || { echo 'Refusing a dev prefix that is not a plain absolute path' >&2; exit 1; }
if [[ ${prefix%/*} != "$root" ]]; then
  echo 'Refusing a dev prefix outside the protected dev-builds directory' >&2
  exit 1
fi
# find -maxdepth 0 replaces stat -c: same behaviour with GNU and BSD find.
owned_by_me_or_root() { [[ -n $(find "$1" -maxdepth 0 \( -user 0 -o -user "$EUID" \) -print) ]]; }
not_shared_or_sticky() { [[ -n $(find "$1" -maxdepth 0 \( -perm -1000 -o \( ! -perm -020 -a ! -perm -002 \) \) -print) ]]; }
ensure_ancestor() {
  local path=$1 parent
  if [[ $path != / ]]; then
    parent=${path%/*}
    ensure_ancestor "${parent:-/}" || return 1
  fi
  if [[ ! -e $path && ! -L $path ]]; then
    mkdir -m 700 -- "$path" || return 1
  fi
  [[ -d $path && ! -L $path ]] || return 1
  owned_by_me_or_root "$path" && not_shared_or_sticky "$path"
}
parent=${prefix%/*}
ensure_ancestor "${parent:-/}" || {
  echo 'Refusing a dev prefix with an unsafe ancestor directory' >&2
  exit 1
}
if [[ ! -e $prefix && ! -L $prefix ]]; then
  mkdir -m 700 -- "$prefix"
fi
if ! [[ -d $prefix && ! -L $prefix && -n $(find "$prefix" -maxdepth 0 -user "$EUID" -perm 700 -print) ]]; then
  echo 'Refusing a dev prefix that is not an owned mode-0700 directory' >&2
  exit 1
fi
if test -e "$prefix/.dev-channel" || test -L "$prefix/.dev-channel"; then
  { test -f "$prefix/.dev-channel" && test ! -L "$prefix/.dev-channel" && [[ -n $(find "$prefix/.dev-channel" -maxdepth 0 -user "$EUID" -links 1 -print) ]]; } || exit 1
else
  test -z "$(find "$prefix" -mindepth 1 -maxdepth 1 -print -quit)" || {
    echo 'Refusing a nonempty prefix without a dev-channel marker' >&2; exit 1;
  }
fi
# mkdir is the atomic lock (no flock on macOS); a stale lock is removed by hand.
lock="$prefix/.update.lock.d"
mkdir -m 700 -- "$lock" 2>/dev/null || { echo "Another update is running, or remove a stale lock: $lock" >&2; exit 1; }
work=
cleanup() { rm -rf -- "$work" "$lock"; rm -f -- "$prefix/.current.$$" "$prefix/.previous.$$"; }
trap cleanup EXIT
test ! -L "$prefix/.dev-channel"
if test -f "$prefix/.dev-channel"; then
  test "$(cat "$prefix/.dev-channel")" = 'ox-llm dev v1'
else
  printf 'ox-llm dev v1\n' > "$prefix/.dev-channel"
fi
work=$(mktemp -d "$prefix/.download.XXXXXXXX")
fetch() {
  curl -q --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --connect-timeout 15 --max-time 120 --max-filesize "$3" "$1" -o "$2"
}
fetch "https://api.github.com/repos/$repo/releases/tags/dev" "$work/release.json" 5242880
sha=$(jq -er 'select(.tag_name=="dev" and .prerelease and (.draft|not)) | .target_commitish | select(test("^[0-9a-f]{40}$"))' "$work/release.json")
suffix="-dev.${sha:0:7}-macos-arm64.tar.gz"
archive=$(jq -er --arg suffix "$suffix" \
  '[.assets[]|select(.state=="uploaded" and (.name|startswith("ox-llm-v")) and (.name|endswith($suffix)))|.name] | select(length>0) | sort_by(sub("^ox-llm-v";"")|split("-")[0]|split(".")|map(tonumber)) | .[-1]' "$work/release.json") || {
  echo "The dev release for commit $sha has no macos-arm64 asset; keeping the current install unchanged" >&2
  exit 1
}
version=${archive#ox-llm-}; version=${version%-macos-arm64.tar.gz}
[[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+[0-9A-Za-z.+-]*-dev\.[0-9a-f]{7}$ ]]
download() {
  local url
  url=$(jq -er --arg name "$1" '[.assets[]|select(.state=="uploaded" and .name==$name)|.browser_download_url] | select(length==1) | .[0]' "$work/release.json")
  case "$url" in "https://github.com/$repo/releases/download/dev/"*) ;; *) return 1;; esac
  fetch "$url" "$work/$1" "$2"
}
download "$archive" 1073741824
download "SHA256SUMS-$version" 1048576
bundle="ox-llm-$version.provenance.jsonl"
download "$bundle" 16777216
digest=$(awk -v name="$archive" '$2==name && $1 ~ /^[0-9a-f]+$/ && length($1)==64 {print $1}' "$work/SHA256SUMS-$version")
[[ $digest =~ ^[0-9a-f]{64}$ ]]
printf '%s  %s\n' "$digest" "$work/$archive" | shasum -a 256 -c - >/dev/null
# --bundle disables gh's authentication requirement; never consult a saved account.
env -u GH_TOKEN -u GITHUB_TOKEN -u GH_ENTERPRISE_TOKEN -u GITHUB_ENTERPRISE_TOKEN -u GH_DEBUG \
  GH_CONFIG_DIR="$work/gh" GH_PROMPT_DISABLED=1 gh attestation verify "$work/$archive" --bundle "$work/$bundle" \
  --repo "$repo" --hostname github.com --signer-workflow "$repo/.github/workflows/dev-build.yml" \
  --source-ref refs/heads/main --source-digest "$sha" --deny-self-hosted-runners >/dev/null
id="$sha-${digest:0:12}"
mkdir -p "$prefix/releases" "$work/payload"
tar -tvzf "$work/$archive" > "$work/listing"
tar -tzf "$work/$archive" > "$work/entries"
# A negated command never trips errexit, so each refusal is an explicit exit.
if grep -Ev '^usr(/|$)' "$work/entries"; then exit 1; fi
if grep -E '(^|/)\.\.(/|$)' "$work/entries"; then exit 1; fi
# Only regular files and directories: no links, devices or fifos.
if grep -Ev '^[-d]' "$work/listing"; then exit 1; fi
tar -xzf "$work/$archive" -C "$work/payload" --no-same-owner --no-same-permissions
test -z "$(find "$work/payload" ! -type f ! -type d -print -quit)"
{ test -x "$work/payload/usr/bin/oxa" && test ! -L "$work/payload/usr/bin/oxa"; } || exit 1
# Verified and extracted: strip quarantine from these new files only. spctl and Gatekeeper
# settings are never touched, and nothing outside this payload is modified.
if command -v xattr >/dev/null; then
  find "$work/payload" -type f -print | while IFS= read -r file; do
    if xattr -p com.apple.quarantine "$file" >/dev/null 2>&1; then
      xattr -d com.apple.quarantine "$file"
    fi
  done
fi
if command -v codesign >/dev/null && ! codesign -v "$work/payload/usr/bin/oxa" 2>/dev/null; then
  echo 'Warning: codesign -v rejected the new oxa binary; it may not start on this Mac' >&2
fi
jq -n --arg sha "$sha" --arg version "$version" --arg digest "$digest" \
  --arg time "$(date -u +%FT%TZ)" --arg archive "$archive" \
  '{channel:"dev",platform:"macos-arm64",source_sha:$sha,version:$version,archive:$archive,sha256:$digest,installed_at:$time}' \
  > "$work/payload/receipt.json"
cp "$work/$bundle" "$work/payload/provenance.jsonl"
if test -e "$prefix/releases/$id"; then
  diff -qr "$work/payload/usr" "$prefix/releases/$id/usr"
else
  mv "$work/payload" "$prefix/releases/$id"
fi
old=$(readlink "$prefix/current" || true)
test ! -e "$prefix/current" || test -L "$prefix/current"
if test -e "$prefix/bin" || test -L "$prefix/bin"; then
  { test -L "$prefix/bin" && test "$(readlink "$prefix/bin")" = current/usr/bin; } || exit 1
else
  ln -s current/usr/bin "$prefix/bin"
fi
# Replace a symlink atomically: GNU mv needs -T, BSD mv needs -h; probe which one works.
mkdir "$work/probe"
ln -s probe "$work/l1"; ln -s probe "$work/l2"
if mv -Tf "$work/l1" "$work/l2" 2>/dev/null; then flag=-Tf; else flag=-hf; fi
# Keep the previous build reachable for rollback (prefix/previous).
if test -n "$old" && test "$old" != "releases/$id"; then
  ln -s "$old" "$prefix/.previous.$$"
  mv $flag "$prefix/.previous.$$" "$prefix/previous"
fi
ln -s "releases/$id" "$prefix/.current.$$"
mv $flag "$prefix/.current.$$" "$prefix/current"
# Prune only after switching to a different build so reinstalls retain rollback.
if test "$old" != "releases/$id"; then
  for installed in "$prefix"/releases/*; do
    test -d "$installed" && test ! -L "$installed" || continue
    [[ ${installed##*/} =~ ^[0-9a-f]{40}-[0-9a-f]{12}$ ]] || continue
    test "$installed" = "$prefix/releases/$id" || test "$installed" = "$prefix/$old" || {
      jq -e '.channel=="dev"' "$installed/receipt.json" >/dev/null && rm -rf -- "$installed"
    }
  done
fi
printf 'Installed dev %s (%s); receipt: %s/current/receipt.json\nRollback: previous build at %s/previous\n' "$version" "$sha" "$prefix" "$prefix"
