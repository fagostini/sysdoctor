#!/bin/sh
# Build target/debian/sysdoctor_<version>_<arch>.deb. Needs: cargo install cargo-deb
set -eu
cd "$(dirname "$0")/.."
command -v cargo-deb >/dev/null || { echo "cargo-deb missing: cargo install cargo-deb" >&2; exit 1; }
cargo build --release --features gui
bin=target/release/sysdoctor
mkdir -p target/assets
"$bin" man > target/assets/sysdoctor.1
"$bin" completions bash > target/assets/sysdoctor.bash
"$bin" completions zsh > target/assets/_sysdoctor
"$bin" completions fish > target/assets/sysdoctor.fish
cargo deb --no-build --features gui
