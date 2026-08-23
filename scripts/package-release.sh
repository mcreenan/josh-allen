#!/usr/bin/env bash
# Build allen and josh, then package them as one release archive.
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  printf 'usage: %s <platform> <rust-target>\n' "$0" >&2
  exit 2
fi

platform="$1"
rust_target="$2"

case "$platform:$rust_target" in
  linux-x86_64:x86_64-unknown-linux-musl | macos-aarch64:aarch64-apple-darwin) ;;
  *)
    printf 'unsupported release target: %s %s\n' "$platform" "$rust_target" >&2
    exit 2
    ;;
esac

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
package_root="$repo_root/target/release-package/$platform"
archive="$repo_root/target/release-package/josh-allen-${platform}.tar.gz"
version="$($repo_root/scripts/check-release-version.sh)"

cd "$repo_root"
cargo build --locked --release --target "$rust_target" --bin allen --bin josh

rm -rf -- "$package_root"
mkdir -p "$package_root"
install -m 755 "target/$rust_target/release/allen" "$package_root/allen"
install -m 755 "target/$rust_target/release/josh" "$package_root/josh"
printf '%s\n' "$version" >"$package_root/VERSION"

"$package_root/allen" --help >/dev/null 2>&1
"$package_root/josh" --help >/dev/null 2>&1

tar -czf "$archive" -C "$package_root" allen josh VERSION

printf '%s\n' "$archive"
