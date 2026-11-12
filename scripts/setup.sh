#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
toolchain="$repo_root/target/toolchain"
sdk_name=MacOSX11.3.sdk
sdk_archive="$toolchain/$sdk_name.tar.xz"
sdk_directory="$toolchain/$sdk_name"
# Same SDK release used by cargo-zigbuild's own Linux build image.
sdk_url="https://github.com/phracker/MacOSX-SDKs/releases/download/11.3/$sdk_name.tar.xz"
sdk_sha256=cd4f08a75577145b8f05245a2975f7c81401d75e9535dcffbb879ee1deefcbf4
zigbuild_version=0.24.1
zig_version=0.17.1

for prerequisite in python3 rustup curl tar; do
    if ! command -v "$prerequisite" >/dev/null 2>&1; then
        printf 'setup: %s is required; install it first.\n' "$prerequisite" >&2
        exit 1
    fi
done

mkdir -p "$toolchain"
if [ ! -x "$toolchain/bin/python" ]; then
    printf 'Creating the local cross-compilation tool environment...\n'
    python3 -m venv "$toolchain"
fi
python="$toolchain/bin/python"

if ! "$python" - "$zigbuild_version" "$zig_version" <<'PY'
import importlib.metadata
import sys

try:
    expected = {"cargo-zigbuild": sys.argv[1], "ziglang": sys.argv[2]}
    sys.exit(not all(importlib.metadata.version(package) == version
                     for package, version in expected.items()))
except importlib.metadata.PackageNotFoundError:
    sys.exit(1)
PY
then
    printf 'Installing cargo-zigbuild %s and Zig %s...\n' "$zigbuild_version" "$zig_version"
    PIP_CACHE_DIR="$repo_root/target/pip-cache" "$python" -m pip install \
        --disable-pip-version-check --no-input \
        "cargo-zigbuild==$zigbuild_version" "ziglang==$zig_version"
else
    printf 'cargo-zigbuild %s and Zig %s are already installed.\n' "$zigbuild_version" "$zig_version"
fi

rustup target add \
    x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    x86_64-apple-darwin aarch64-apple-darwin

verify_archive() {
    "$python" - "$1" "$sdk_sha256" <<'PY'
import hashlib
from pathlib import Path
import sys

digest = hashlib.sha256()
with Path(sys.argv[1]).open("rb") as archive:
    for block in iter(lambda: archive.read(1024 * 1024), b""):
        digest.update(block)
sys.exit(digest.hexdigest() != sys.argv[2])
PY
}

download=
staging=
cleanup() {
    if [ -n "$download" ]; then rm -f -- "$download"; fi
    if [ -n "$staging" ]; then rm -rf -- "$staging"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ ! -f "$sdk_archive" ] || ! verify_archive "$sdk_archive"; then
    printf 'Downloading %s...\n' "$sdk_name"
    download=$(mktemp "$toolchain/.sdk-download.XXXXXXXX")
    curl --fail --location --retry 3 --output "$download" "$sdk_url"
    if ! verify_archive "$download"; then
        printf 'setup: SDK SHA-256 verification failed; refusing to extract it.\n' >&2
        exit 1
    fi
    mv -- "$download" "$sdk_archive"
    download=
else
    printf 'Cached SDK archive verified.\n'
fi

installed_sha256=
if [ -f "$sdk_directory/.stalker-sdk-sha256" ]; then
    installed_sha256=$(cat "$sdk_directory/.stalker-sdk-sha256")
fi
if [ "$installed_sha256" != "$sdk_sha256" ] || [ ! -f "$sdk_directory/usr/lib/libSystem.tbd" ]; then
    printf 'Extracting %s...\n' "$sdk_name"
    staging=$(mktemp -d "$toolchain/.sdk-install.XXXXXXXX")
    tar -xJf "$sdk_archive" -C "$staging"
    if [ ! -f "$staging/$sdk_name/usr/lib/libSystem.tbd" ]; then
        printf 'setup: SDK archive does not contain the expected system libraries.\n' >&2
        exit 1
    fi
    printf '%s\n' "$sdk_sha256" > "$staging/$sdk_name/.stalker-sdk-sha256"
    # Replace only the generated SDK, after successful verification/extraction.
    rm -rf -- "$sdk_directory"
    mv -- "$staging/$sdk_name" "$sdk_directory"
else
    printf '%s is already installed.\n' "$sdk_name"
fi

printf 'Setup complete. Run `mise run dist` to build all four platforms.\n'
