#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
tag="${1:?usage: sh scripts/package-release.sh TAG TARGET}"
target="${2:?target triple is required}"
cargo run --locked --example validate_release -- "$tag"
case "$target" in
  aarch64-apple-darwin|x86_64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;;
  *) printf 'Unsupported release target: %s\n' "$target" >&2; exit 1 ;;
esac
binary="${CARGO_TARGET_DIR:-target}/$target/release/herdr-git-panel"
test -x "$binary"
package="herdr-git-panel-$tag-$target"
staging_root=$(mktemp -d "${TMPDIR:-/tmp}/herdr-git-release.XXXXXX")
staging="$staging_root/$package"
mkdir -p "$staging/bin" "$staging/assets" "$staging/docs" dist
cp "$binary" "$staging/bin/herdr-git-panel"
cp herdr-plugin.toml README.md LICENSE CHANGELOG.md GLOSSARY.md "$staging/"
cp assets/README.md assets/herdr-palettes.json "$staging/assets/"
cp -R docs/. "$staging/docs/"
chmod 755 "$staging/bin/herdr-git-panel"
tar -czf "dist/$package.tar.gz" -C "$staging_root" "$package"
printf 'Packaged dist/%s.tar.gz\n' "$package"
