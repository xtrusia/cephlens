#!/bin/sh
set -eu

installer_url="${CEPHLENS_INSTALLER_URL:-https://github.com/xtrusia/cephlens/releases/latest/download/cephlens-installer.sh}"

if command -v mktemp >/dev/null 2>&1; then
    tmp_file="$(mktemp "${TMPDIR:-/tmp}/cephlens-installer.XXXXXX")"
else
    tmp_file="${TMPDIR:-/tmp}/cephlens-installer.$$"
fi

cleanup() {
    rm -f "$tmp_file"
}
trap cleanup EXIT HUP INT TERM

if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -LsSf "$installer_url" -o "$tmp_file"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$tmp_file" "$installer_url"
else
    echo "error: neither curl nor wget is available" >&2
    exit 1
fi

sh "$tmp_file" "$@"
