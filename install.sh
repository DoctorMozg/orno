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
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc" >&2
          exit 1
          ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64) echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *)
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc" >&2
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
          echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "error: unsupported platform: $os/$arch. Supported targets: x86_64-unknown-linux-gnu, aarch64-apple-darwin, x86_64-apple-darwin, x86_64-pc-windows-msvc" >&2
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

  echo "$tag"
}

download_archive() {
  local version="$1"
  local dest_file="$2"
  local url="https://github.com/${REPO}/releases/download/${version}/$(basename "$dest_file")"

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
}

install_binary() {
  local scratch_dir="$1"
  local install_dir="$2"
  local bin_path

  bin_path=$(find "$scratch_dir" \( -name "${BINARY}" -o -name "${BINARY}.exe" \) | head -1)

  if [ -z "$bin_path" ]; then
    echo "error: binary not found in extracted archive" >&2
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
  local target version scratch_dir ext archive_name

  INSTALL_DIR="${ORNO_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
  VERSION="${ORNO_VERSION:-}"

  target=$(detect_target)
  version="${VERSION:-$(latest_version)}"

  scratch_dir=$(mktemp -d)
  trap 'rm -rf "$scratch_dir"' EXIT

  ext="tar.gz"
  case "$target" in
    *windows-msvc) ext="zip" ;;
  esac
  archive_name="${BINARY}-${version}-${target}.${ext}"

  echo "Installing ${BINARY} ${version} (${target})..."
  download_archive "$version" "$scratch_dir/$archive_name"
  extract_archive "$scratch_dir/$archive_name" "$scratch_dir"
  install_binary "$scratch_dir" "$INSTALL_DIR"

  echo "Installed ${BINARY} ${version} to ${INSTALL_DIR}/${BINARY}"

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) echo "note: add '${INSTALL_DIR}' to your PATH to use ${BINARY} from any shell" ;;
  esac
}

main "$@"
