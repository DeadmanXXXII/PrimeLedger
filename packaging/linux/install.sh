#!/bin/sh
# User-level install (no root): binaries -> ~/.local/bin, launcher + icon -> ~/.local/share
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/256x256/apps"
install -m 755 "$DIR/primeledger" "$DIR/prime" "$HOME/.local/bin/"
install -m 644 "$DIR/primeledger.png" "$HOME/.local/share/icons/hicolor/256x256/apps/primeledger.png"
install -m 644 "$DIR/primeledger.desktop" "$HOME/.local/share/applications/primeledger.desktop"
echo "Installed. Launch PrimeLedger from your app menu, or run: primeledger"
echo "(make sure ~/.local/bin is on your PATH)"
