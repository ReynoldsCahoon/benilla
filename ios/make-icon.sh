#!/bin/bash
# The app icon, taken from the player's own WoW.exe: its largest icon, scaled to 1024 px over black,
# into the asset catalog (`Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png`, gitignored, as is
# everything taken from the install). Without it the app builds with no icon.
#
#   ios/make-icon.sh <path to WoW.exe>
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
exe="${1:?usage: ios/make-icon.sh <path to WoW.exe>}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
python3 -I "$here/tools/exe_icon.py" "$exe" "$work" >/dev/null
swift "$here/tools/icon.swift" "$work/group0.ico" "$here/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png"
echo "icon: $here/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png"
