#!/usr/bin/env bash
# User-local install: binary, .desktop, icons. No root needed.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/avro ~/.local/bin/avro
# Launchers often lack ~/.local/bin on PATH, and .desktop files don't expand ~,
# so bake the absolute binary path into the installed copy.
mkdir -p ~/.local/share/applications
sed "s#^Exec=.*#Exec=$HOME/.local/bin/avro#" data/dev.turbinebmw.Avro.desktop \
  > ~/.local/share/applications/dev.turbinebmw.Avro.desktop
chmod 644 ~/.local/share/applications/dev.turbinebmw.Avro.desktop
for n in 16 32 48 64 128 256 512; do
  install -Dm644 data/icons/avro-$n.png ~/.local/share/icons/hicolor/${n}x${n}/apps/dev.turbinebmw.Avro.png
done
# The scalable icon wraps the full-size art, so launchers that take an SVG over
# PNGs (Omarchy's does) draw it sharp at any size instead of an upscaled 16px PNG.
# avro.svg stays the editable source: Qt's SVG renderer ignores its clipPath, so
# Qt-based launchers would draw the patina spilling past the disc.
install -Dm644 data/icons/dev.turbinebmw.Avro.svg ~/.local/share/icons/hicolor/scalable/apps/dev.turbinebmw.Avro.svg
gtk4-update-icon-cache -q -t -f ~/.local/share/icons/hicolor 2>/dev/null || true
update-desktop-database ~/.local/share/applications 2>/dev/null || true
echo "installed: ~/.local/bin/avro"
