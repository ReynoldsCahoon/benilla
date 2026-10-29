#!/bin/bash
# Xcode's pre-build step: the player build as a static library for the device. Run in a clean
# environment, because Xcode's own variables (SDKROOT for iphoneos, its PATH) break cargo's
# host-side build scripts.
set -euo pipefail
cd "$(dirname "$0")/.."
exec env -i \
    HOME="$HOME" \
    PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    IPHONEOS_DEPLOYMENT_TARGET=15.0 \
    cargo build --release --target aarch64-apple-ios -p benilla-ios
