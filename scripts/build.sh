#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --release --locked
mkdir -p bin
cp target/release/herdr-git-panel bin/herdr-git-panel.new
chmod +x bin/herdr-git-panel.new
mv bin/herdr-git-panel.new bin/herdr-git-panel
