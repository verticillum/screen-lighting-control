#!/usr/bin/env bash
# Publishes the GitHub Release for the version in Cargo.toml (this was release.yml):
# tests, builds slc.exe with MSVC (on Linux: the Linux binary), checks its version, tags main as v<version>,
# and publishes it, its .sha256, and that version's CHANGELOG section. Run from
# main, clean and equal to origin/main, after the version bump is merged.
set -euo pipefail
cd "$(dirname "$0")/.."
git fetch -q origin main --tags

[ "$(git branch --show-current)" = main ] || { echo "release: check out main first." >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "release: the working tree has changes." >&2; exit 1; }
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "release: main is not origin/main; git pull first." >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="v$version"
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then
	echo "release: $tag already exists." >&2
	exit 1
fi

dist=$(mktemp -d)
trap 'rm -rf "$dist"' EXIT
if command -v cmd.exe >/dev/null; then
	scripts/cargo-msvc.sh test
	scripts/cargo-msvc.sh build --release
	win_home=$(wslpath "$(cmd.exe /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r')")
	exe="$win_home/.slc-tools/target-msvc/release/slc.exe"
	product=$(powershell.exe -NoProfile -Command "(Get-Item '$(wslpath -w "$exe")').VersionInfo.ProductVersion" | tr -d '\r')
	[ "$product" = "$version" ] || { echo "release: slc.exe says $product, Cargo.toml says $version." >&2; exit 1; }
	asset=slc.exe
	latest=true
else
	# Linux only: not "latest", which must stay a release with slc.exe.
	export PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/slc-target}"
	target=x86_64-unknown-linux-musl
	cargo test --target $target
	cargo build --release --target $target
	exe="$CARGO_TARGET_DIR/$target/release/slc"
	[ "$("$exe" --version)" = "SLC $version" ] || { echo "release: slc is not version $version." >&2; exit 1; }
	asset=slc-linux-x86_64
	latest=false
fi
cp "$exe" "$dist/$asset"
(cd "$dist" && sha256sum "$asset" >"$asset.sha256")
echo "$asset: $(stat -c %s "$dist/$asset") bytes, sha256 $(cut -d' ' -f1 "$dist/$asset.sha256")"
awk -v v="$version" '
	index($0, "## [" v "]") == 1 { found = 1; next }
	found && /^## \[/ { exit }
	found { print }
' CHANGELOG.md >"$dist/notes.md"
[ -s "$dist/notes.md" ] || { echo "release: no CHANGELOG section for $version." >&2; exit 1; }

git tag "$tag"
git push origin "refs/tags/$tag"
gh release create "$tag" "$dist/$asset" "$dist/$asset.sha256" --latest=$latest --title "SLC $tag" --notes-file "$dist/notes.md" --verify-tag
