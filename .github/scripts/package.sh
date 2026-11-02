#!/usr/bin/env bash
# Packs target/release/deskhand into dist/<name>.tar.gz (+ .sha256) with the
# files a manual install needs. Usage: package.sh NAME
set -euo pipefail
name=$1
bin=${DESKHAND_BIN:-target/release/deskhand}
out="dist/$name"
rm -rf "$out"
mkdir -p "$out/share/applications" "$out/share/icons/hicolor/scalable/apps" \
  "$out/share/bash-completion/completions" "$out/share/zsh/site-functions" \
  "$out/share/fish/vendor_completions.d" "$out/share/man/man1"
install -m755 "$bin" "$out/deskhand"
install -m644 README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE "$out/"
install -m644 data/io.github.ryanmwright.Deskhand.desktop "$out/share/applications/"
install -m644 data/io.github.ryanmwright.Deskhand.svg "$out/share/icons/hicolor/scalable/apps/"
"$bin" completions bash > "$out/share/bash-completion/completions/deskhand"
"$bin" completions zsh > "$out/share/zsh/site-functions/_deskhand"
"$bin" completions fish > "$out/share/fish/vendor_completions.d/deskhand.fish"
"$bin" man | gzip -9n > "$out/share/man/man1/deskhand.1.gz"
tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
"$out/deskhand" --version
echo "glibc symbols needed: $(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
