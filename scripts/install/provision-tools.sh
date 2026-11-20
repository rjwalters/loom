#!/usr/bin/env bash
# scripts/install/provision-tools.sh — tools-only install of `loom-tools`
#
# Issue #12940 (epic #12708, Phase 2d). A session-only machine needs the agent
# tools and the repo payload but no daemon, so every install path that starts
# from `loom-daemon` (install.sh, provision-daemon.sh, `loom-daemon
# release-fetch`) is unusable there. This is the daemon-free entry:
#
#   scripts/install/provision-tools.sh [--tag TAG] [--repo OWNER/NAME]
#                                      [--dest-dir DIR] [--workspace DIR]
#   ./install.sh --tools-only [same flags]
#
# It adds no download, verification or install logic of its own. The
# `loom-tools` release asset carries the same `release-fetch`, `install-binary`
# and `resync-payload` commands as the daemon (generalised by `--bin`, not
# copied), so the steps are:
#
#   1. Bootstrap: `gh release download` of loom-tools-<triple> + .sha256 and a
#      sha256 check. This is the ONLY step that runs before the full
#      verification, and it is checksum-only: it exists to get a runnable
#      `loom-tools` onto the machine without a daemon.
#   2. `<bootstrap> release-fetch --bin loom-tools ...`: the real fetch, with
#      the daemon's checksum / signature / glibc policy (and its
#      LOOM_DAEMON_UPDATE_REQUIRE_SIGNATURE required mode). Exit != 0 refuses.
#      Set LOOM_DAEMON_UPDATE_REQUIRE_SIGNATURE=1 where the checksum-only
#      bootstrap trust above is not acceptable.
#   3. `install-binary stage` beside the destination, a `--version` check of
#      the STAGED file (must run and carry the tag's version), then
#      `install-binary publish`, which retains the previous binary as
#      <dest>.previous. A failing check leaves <dest> untouched; a failing
#      post-publish check restores <dest>.previous.
#   4. With --workspace: `loom-tools resync-payload` materialises the payload
#      embedded in the binary (the build.rs source the daemon embeds too).
#
# Source this file to call `provision_machine_tools`; run it to use the CLI.

_pmt_say()  { echo "  [loom-tools] $*"; }
_pmt_warn() { echo "  [loom-tools] WARNING: $*" >&2; }

# The release target triple for this host, or empty when unsupported.
_pmt_triple() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64) echo "aarch64-apple-darwin" ;;
    Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
  esac
}

# _pmt_sha256_ok <dir> <asset>: the .sha256 beside <asset> matches it.
_pmt_sha256_ok() {
  local dir="$1" asset="$2" want got
  [[ -f "$dir/$asset" && -f "$dir/$asset.sha256" ]] || return 1
  want="$(awk '{print $1; exit}' "$dir/$asset.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$dir/$asset" | awk '{print $1}')"
  else
    got="$(shasum -a 256 "$dir/$asset" | awk '{print $1}')"
  fi
  [[ -n "$want" && "$want" == "$got" ]]
}

# _pmt_fetch <repo> <tag> <triple>: steps 1-2. Prints the verified binary's
# path on stdout; everything else goes to stderr. Fails closed.
_pmt_fetch() {
  local repo="$1" tag="$2" triple="$3" tmp asset out line bin=""
  asset="loom-tools-$triple"
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/loom-tools-bootstrap.XXXXXX")" || return 1
  if ! gh release download "$tag" -R "$repo" -p "$asset" -p "$asset.sha256" -D "$tmp" --clobber >&2; then
    _pmt_warn "could not download $asset + $asset.sha256 from $repo@$tag"
    rm -rf "$tmp"; return 1
  fi
  if ! _pmt_sha256_ok "$tmp" "$asset"; then
    _pmt_warn "checksum mismatch (or missing) for $asset from $repo@$tag; refusing it"
    rm -rf "$tmp"; return 1
  fi
  chmod +x "$tmp/$asset"
  # exit 0 = verified; 1 = verification failed; 2 = could not download.
  if ! out="$("$tmp/$asset" release-fetch --bin loom-tools --target "$triple" \
        --repo "$repo" --tag "$tag" 2>&2)"; then
    _pmt_warn "release-fetch refused $asset from $repo@$tag; nothing was installed"
    rm -rf "$tmp"; return 1
  fi
  while IFS= read -r line; do
    case "$line" in BIN_PATH=*) bin="${line#BIN_PATH=}" ;; esac
  done <<<"$out"
  rm -rf "$tmp"
  if [[ -z "$bin" || ! -x "$bin" ]]; then
    _pmt_warn "release-fetch reported success but no usable BIN_PATH; refusing"
    return 1
  fi
  echo "$bin"
}

# provision_machine_tools <verified_bin> <dest_dir> [<expected_version>]
#
# Step 3. <verified_bin> must already be verified AND carry `install-binary`
# (it is the helper). Sets PROVISIONED_TOOLS_BIN to <dest_dir>/loom-tools.
PROVISIONED_TOOLS_BIN=""
provision_machine_tools() {
  local src="${1:-}" dest_dir="${2:-${LOOM_TOOLS_BIN_DIR:-$HOME/.local/bin}}" want="${3:-}"
  local dest="$dest_dir/loom-tools" staged="" out ver nl=$'\n'
  PROVISIONED_TOOLS_BIN="$dest"
  if [[ -z "$src" || ! -x "$src" ]]; then
    _pmt_warn "no executable loom-tools to install ('${src:-<unset>}')"; return 1
  fi
  mkdir -p "$dest_dir" || { _pmt_warn "could not create $dest_dir"; return 1; }

  if ! out="$("$src" install-binary stage "$src" "$dest" 2>&1)"; then
    _pmt_warn "could not stage loom-tools beside $dest; it was left untouched (${out##*"$nl"})"
    return 1
  fi
  staged="${out##*"$nl"}"
  if [[ "$staged" != "$dest_dir"/.loom-tools.loom-install.* || ! -f "$staged" ]]; then
    _pmt_warn "install-binary stage printed no staged path; $dest was left untouched"
    return 1
  fi

  # The post-stage check runs on the STAGED file, so a candidate that does not
  # load (or is not the release asked for) never reaches $dest.
  ver="$("$staged" --version 2>/dev/null)" || ver=""
  if [[ -z "$ver" || ( -n "$want" && "$ver" != *"$want"* ) ]]; then
    rm -f "$staged"
    _pmt_warn "staged loom-tools failed its check (version '${ver:-<did not run>}', wanted '${want:-any}'); the previous binary at $dest was left untouched"
    return 1
  fi

  if ! "$src" install-binary publish "$staged" "$dest"; then
    _pmt_warn "failed to publish loom-tools to $dest (it was left untouched)"
    return 1
  fi
  # Post-publish check; on failure put the retained previous binary back.
  if ! "$dest" --version >/dev/null 2>&1; then
    if [[ -f "$dest.previous" ]] && cp -p "$dest.previous" "$dest.restore.$$" && mv -f "$dest.restore.$$" "$dest"; then
      _pmt_warn "the published loom-tools did not run; restored the previous binary from $dest.previous"
    else
      _pmt_warn "the published loom-tools did not run and there is no previous binary to restore"
    fi
    return 1
  fi
  _pmt_say "installed loom-tools -> $dest ($ver)"
  case ":${PATH:-}:" in *":$dest_dir:"*) ;; *) _pmt_warn "$dest_dir is not on your PATH" ;; esac
  return 0
}

provision_tools_main() {
  local repo="${LOOM_TOOLS_REPO:-rjwalters/loom}" tag="" dest_dir="" workspace="" triple bin
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo) repo="${2:?--repo needs OWNER/NAME}"; shift 2 ;;
      --tag) tag="${2:?--tag needs a release tag}"; shift 2 ;;
      --dest-dir) dest_dir="${2:?--dest-dir needs a directory}"; shift 2 ;;
      --workspace) workspace="${2:?--workspace needs a directory}"; shift 2 ;;
      -h|--help) sed -n '2,/^# Source this file/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; return 0 ;;
      *) _pmt_warn "unknown argument: $1"; return 2 ;;
    esac
  done
  command -v gh >/dev/null 2>&1 || { _pmt_warn "gh (GitHub CLI) is required to fetch the release"; return 1; }
  triple="$(_pmt_triple)"
  [[ -n "$triple" ]] || { _pmt_warn "unsupported platform $(uname -sm)"; return 1; }
  if [[ -z "$tag" ]]; then
    tag="$(gh release view -R "$repo" --json tagName --jq .tagName)" || { _pmt_warn "could not resolve the latest release of $repo"; return 1; }
  fi
  bin="$(_pmt_fetch "$repo" "$tag" "$triple")" || return 1
  provision_machine_tools "$bin" "$dest_dir" "${tag#v}" || { rm -rf "$(dirname "$bin")"; return 1; }
  rm -rf "$(dirname "$bin")"
  if [[ -n "$workspace" ]]; then
    "$PROVISIONED_TOOLS_BIN" resync-payload --workspace "$workspace" || { _pmt_warn "payload install into $workspace failed"; return 1; }
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
  provision_tools_main "$@"
fi
