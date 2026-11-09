#!/usr/bin/env bash
# Installs Deltos on x86-64 Linux from its GitHub releases: the latest release, or the version
# given as an argument. Debian and Ubuntu get the .deb through apt; Fedora, RHEL and openSUSE get
# the .rpm through dnf, zypper or yum; other distributions, or any with --user, get the binary in
# ~/.local/bin with a menu entry, which needs no root. Every download is checked against the
# release's SHA256SUMS.txt before anything is installed.
#
#   curl -fsSL https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.sh | bash -s -- 0.1.0 --user

set -euo pipefail

readonly releases="https://github.com/khaodius/deltos/releases"

workdir=""

usage() {
  cat << 'EOF_USAGE'
Usage: install.sh [VERSION] [--user]

Installs the latest Deltos release, or VERSION (such as 0.1.0).

  --user  install in ~/.local for this user only, instead of the .deb or .rpm package for
          the whole system
EOF_USAGE
}

say() {
  printf '%s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [ -n "$workdir" ]; then
    rm -rf "$workdir"
  fi
}

download() {
  local url="$1" destination="$2" output=(--silent --show-error)
  if [ "${3:-}" = progress ] && [ -t 2 ]; then
    output=(--progress-bar)
  fi
  curl --fail --location --proto '=https' --tlsv1.2 --retry 3 "${output[@]}" --output "$destination" "$url"
}

sha256_of() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

run_as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  else
    sudo "$@"
  fi
}

install_deb() {
  # apt reads local packages as its unprivileged _apt user.
  chmod 755 "$workdir"
  chmod 644 "$1"
  say "Installing with apt"
  run_as_root apt-get update -qq || say "warning: apt-get update failed, installing with the package lists as they are"
  run_as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -q --allow-downgrades "$1"
}

install_rpm() {
  if command -v dnf > /dev/null; then
    say "Installing with dnf"
    run_as_root dnf install -y "$1"
  elif command -v zypper > /dev/null; then
    say "Installing with zypper"
    run_as_root zypper --non-interactive install --allow-unsigned-rpm "$1"
  else
    say "Installing with yum"
    run_as_root yum install -y "$1"
  fi
}

install_user() {
  local archive="$1"
  local bin_directory="${XDG_BIN_HOME:-$HOME/.local/bin}"
  local data_directory="${XDG_DATA_HOME:-$HOME/.local/share}"
  local target="$bin_directory/deltos"

  say "Installing in $target"
  tar -xzf "$archive" -C "$workdir" || fail "could not unpack $(basename "$archive")"
  local unpacked
  unpacked=$(find "$workdir" -mindepth 1 -maxdepth 1 -type d -name 'deltos-*' | head -n 1)
  if [ -z "$unpacked" ] || [ ! -x "$unpacked/deltos" ]; then
    fail "the archive has no deltos binary"
  fi

  mkdir -p "$bin_directory" "$data_directory/applications" "$data_directory/icons"
  # A rename replaces the file even while Deltos runs from it.
  cp "$unpacked/deltos" "$target.partial"
  chmod 755 "$target.partial"
  mv -f "$target.partial" "$target"

  cp -R "$unpacked/share/icons/hicolor" "$data_directory/icons/"
  sed "s|^Exec=.*|Exec=\"$target\" %U|" "$unpacked/share/applications/deltos.desktop" > "$data_directory/applications/deltos.desktop"
  if command -v update-desktop-database > /dev/null; then
    update-desktop-database "$data_directory/applications" > /dev/null 2>&1 || true
  fi

  case ":$PATH:" in
    *":$bin_directory:"*) ;;
    *) say "note: $bin_directory is not on your PATH, so start Deltos from the applications menu or as $target" ;;
  esac
}

main() {
  local requested_version="" format="" argument
  for argument in "$@"; do
    case "$argument" in
      --user) format=user ;;
      -h | --help)
        usage
        return
        ;;
      v[0-9]* | [0-9]*) requested_version="${argument#v}" ;;
      *)
        usage >&2
        fail "unknown argument: $argument"
        ;;
    esac
  done

  [ "$(uname -s)" = Linux ] || fail "this script installs Deltos on Linux; on Windows use install.ps1 or install.cmd"
  local machine
  machine=$(uname -m)
  [ "$machine" = x86_64 ] || fail "Deltos has Linux builds for x86-64 only, and this machine is $machine"
  command -v curl > /dev/null || fail "curl is needed to download Deltos"
  command -v sha256sum > /dev/null || command -v shasum > /dev/null ||
    fail "sha256sum or shasum is needed to check the download"

  if [ -z "$format" ]; then
    if command -v apt-get > /dev/null && command -v dpkg > /dev/null; then
      format=deb
    elif command -v dnf > /dev/null || command -v zypper > /dev/null || command -v yum > /dev/null; then
      format=rpm
    else
      format=user
    fi
  fi
  if [ "$format" != user ] && [ "$(id -u)" -ne 0 ] && ! command -v sudo > /dev/null; then
    fail "installing the .$format package needs root: run this as root, or pass --user to install for your user only"
  fi

  # Release file names carry the version between this prefix and suffix.
  local prefix suffix
  case "$format" in
    deb) prefix=deltos_ suffix=_amd64.deb ;;
    rpm) prefix=deltos- suffix=-1.x86_64.rpm ;;
    user) prefix=deltos- suffix=-x86_64-linux.tar.gz ;;
  esac

  workdir=$(mktemp -d)
  trap cleanup EXIT

  local checksums_url
  if [ -n "$requested_version" ]; then
    checksums_url="$releases/download/v$requested_version/SHA256SUMS.txt"
  else
    checksums_url="$releases/latest/download/SHA256SUMS.txt"
  fi
  download "$checksums_url" "$workdir/SHA256SUMS.txt" ||
    fail "could not download $checksums_url${requested_version:+; there may be no $requested_version release}"

  local expected_hash="" file_name="" listed_hash listed_name
  while read -r listed_hash listed_name; do
    listed_name="${listed_name#\*}"
    case "$listed_name" in
      "$prefix"*"$suffix")
        expected_hash="$listed_hash"
        file_name="$listed_name"
        ;;
    esac
  done < "$workdir/SHA256SUMS.txt"
  [ -n "$file_name" ] || fail "the release has no $prefix*$suffix file"
  local version="${file_name#"$prefix"}"
  version="${version%"$suffix"}"

  say "Downloading Deltos $version ($file_name)"
  local package="$workdir/$file_name"
  download "$releases/download/v$version/$file_name" "$package" progress ||
    fail "could not download $file_name"
  [ "$(sha256_of "$package")" = "$expected_hash" ] ||
    fail "$file_name does not match its SHA-256 checksum in SHA256SUMS.txt, so nothing was installed"

  case "$format" in
    deb) install_deb "$package" ;;
    rpm) install_rpm "$package" ;;
    user) install_user "$package" ;;
  esac

  say "Deltos $version is installed. Open it from your applications menu, or run deltos."
}

# Nothing runs until bash has read this whole file, so a download cut short installs nothing and
# no command the script starts can read the rest of the script from the pipe.
main "$@"
