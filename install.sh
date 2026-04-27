#!/usr/bin/env bash
set -euo pipefail

[ -n "${BASH_VERSION:-}" ] || { echo "error: install.sh requires bash, not sh"; exit 1; }

REPO="DoctorMozg/orno"
BINARY="orno"
# Archive naming contract: ${BINARY}-${VERSION}-${TARGET}.tar.gz (or .zip on Windows)
# This must match .github/workflows/release.yml archive: $bin-$tag-$target
DEFAULT_INSTALL_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"

detect_target() {
  local os arch
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        *)
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-pc-windows-msvc" >&2
          exit 1
          ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64) echo "aarch64-apple-darwin" ;;
        *)
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-pc-windows-msvc" >&2
          exit 1
          ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      case "$arch" in
        x86_64)
          echo "note: detected Windows shell ($os); installation works under Git Bash" >&2
          echo "x86_64-pc-windows-msvc"
          ;;
        *)
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-pc-windows-msvc" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-pc-windows-msvc" >&2
      exit 1
      ;;
  esac
}

latest_version() {
  local url="https://api.github.com/repos/${REPO}/releases/latest"
  local response=""

  if command -v curl >/dev/null 2>&1; then
    response=$(curl --proto '=https' --tlsv1.2 -fsSL "$url" 2>/dev/null || true)
  elif command -v wget >/dev/null 2>&1; then
    response=$(wget --secure-protocol=TLSv1_2 -qO- "$url" 2>/dev/null || true)
  else
    echo "error: neither curl nor wget found; one is required" >&2
    exit 1
  fi

  local tag
  tag=$(echo "$response" | grep '"tag_name"' | sed 's/.*"tag_name": *"\(.*\)".*/\1/' | head -1)

  if [ -z "$tag" ]; then
    echo "error: no release found for ${REPO}. orno is pre-v0.1.0 -- build from source: https://github.com/${REPO}#build-from-source" >&2
    exit 1
  fi

  # Validate tag format to prevent injection via a poisoned API response
  # (e.g. a man-in-the-middle could otherwise inject shell metacharacters
  # that flow into the download URL).
  if ! echo "$tag" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$'; then
    echo "error: unexpected tag format from GitHub API: $tag" >&2
    exit 1
  fi

  echo "$tag"
}

download_archive() {
  local dest_file="$1"
  local url="$2"

  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fSL -o "$dest_file" "$url" || {
      echo "error: could not download archive from $url" >&2
      exit 1
    }
  elif command -v wget >/dev/null 2>&1; then
    wget --secure-protocol=TLSv1_2 -O "$dest_file" "$url" || {
      echo "error: could not download archive from $url" >&2
      exit 1
    }
  else
    echo "error: neither curl nor wget found; one is required" >&2
    exit 1
  fi
}

verify_archive() {
  local archive="$1"
  local url="$2"

  # Respect opt-out for environments where the sidecar isn't reachable
  # (e.g. an offline mirror). Default-on is the secure behavior.
  if [ "${ORNO_SKIP_SHA256:-0}" = "1" ]; then
    echo "warning: sha256 verification skipped (ORNO_SKIP_SHA256=1)" >&2
    return 0
  fi

  local sha_url="${url}.sha256"
  local sha_file="${archive}.sha256"

  # Download the sidecar checksum file published by the release workflow.
  if command -v curl >/dev/null 2>&1; then
    if ! curl --proto '=https' --tlsv1.2 -fsSL -o "$sha_file" "$sha_url"; then
      echo "error: failed to download checksum from $sha_url" >&2
      exit 1
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget --secure-protocol=TLSv1_2 -qO "$sha_file" "$sha_url"; then
      echo "error: failed to download checksum from $sha_url" >&2
      exit 1
    fi
  else
    echo "error: neither curl nor wget found; one is required" >&2
    exit 1
  fi

  # The sidecar from taiki-e/upload-rust-binary-action contains just the hex digest
  # (no filename column), so compare digests directly rather than feeding sha256sum -c.
  local expected_hash
  expected_hash=$(awk '{print $1}' "$sha_file")
  if [ -z "$expected_hash" ]; then
    echo "error: checksum file is empty: $sha_file" >&2
    exit 1
  fi

  local actual_hash
  if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "$archive" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual_hash=$(shasum -a 256 "$archive" | awk '{print $1}')
  else
    echo "error: neither sha256sum nor shasum found; cannot verify archive" >&2
    exit 1
  fi

  if [ "$actual_hash" != "$expected_hash" ]; then
    echo "error: sha256 mismatch for $archive" >&2
    echo "  expected: $expected_hash" >&2
    echo "  actual:   $actual_hash" >&2
    exit 1
  fi
  echo "sha256 verified: $(basename "$archive")"
}

extract_archive() {
  local archive="$1"
  local scratch_dir="$2"

  case "$archive" in
    *.tar.gz)
      tar -xzf "$archive" -C "$scratch_dir"
      ;;
    *.zip)
      command -v unzip >/dev/null 2>&1 || { echo "error: unzip required for Windows archives" >&2; exit 1; }
      unzip -q "$archive" -d "$scratch_dir"
      ;;
    *)
      echo "error: unknown archive format: $archive" >&2
      exit 1
      ;;
  esac

  # Defense-in-depth: a malicious archive could plant a symlink that
  # subsequent `find` calls follow into an attacker-chosen path. Strip them
  # before any post-extract step inspects the tree.
  find "$scratch_dir" -type l -delete
}

install_binary() {
  local scratch_dir="$1"
  local install_dir="$2"
  local bin_path

  # `-P` keeps find from following symlinks; `-type f` rejects them outright.
  # Together with the `find -type l -delete` in extract_archive, this means an
  # archive that planted a symlink-to-/etc/passwd cannot get installed.
  bin_path=$(find -P "$scratch_dir" -type f \( -name "${BINARY}" -o -name "${BINARY}.exe" \) | head -1)

  if [ -z "$bin_path" ]; then
    echo "error: binary not found in extracted archive" >&2
    exit 1
  fi

  if [ ! -f "$bin_path" ] || [ -L "$bin_path" ]; then
    echo "error: binary not found or is a symlink: $bin_path" >&2
    exit 1
  fi

  mkdir -p "$install_dir"
  mv "$bin_path" "$install_dir/"

  # chmod is a no-op on Windows filesystems and unnecessary for .exe
  case "$bin_path" in
    *.exe) ;;
    *) chmod +x "$install_dir/$BINARY" ;;
  esac
}

main() {
  local target version scratch_dir ext archive_name archive_path archive_url

  INSTALL_DIR="${ORNO_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
  VERSION="${ORNO_VERSION:-}"

  target=$(detect_target)
  version="${VERSION:-$(latest_version)}"

  scratch_dir=$(mktemp -d)
  # Use double quotes so the path is expanded at trap-define-time; the local
  # `scratch_dir` is out of scope by the time EXIT fires under `set -u`.
  trap "rm -rf '$scratch_dir'" EXIT

  ext="tar.gz"
  case "$target" in
    *windows-msvc) ext="zip" ;;
  esac
  archive_name="${BINARY}-${version}-${target}.${ext}"
  archive_path="$scratch_dir/$archive_name"
  archive_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"

  echo "Installing ${BINARY} ${version} (${target})..."
  download_archive "$archive_path" "$archive_url"
  verify_archive "$archive_path" "$archive_url"
  extract_archive "$archive_path" "$scratch_dir"
  install_binary "$scratch_dir" "$INSTALL_DIR"

  echo "Installed ${BINARY} ${version} to ${INSTALL_DIR}/${BINARY}"

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "note: add '${INSTALL_DIR}' to your PATH to use ${BINARY} from any shell" ;;
  esac
}

main "$@"
