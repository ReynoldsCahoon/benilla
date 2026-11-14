#!/bin/bash
# Xcode's pre-build step: the player build as a static library for the device. Run in a clean
# environment, because Xcode's own variables (SDKROOT for iphoneos, its PATH) break cargo's
# host-side build scripts. `BENILLA_IOS_FEATURES` (e.g. `route-probe`) is forwarded as cargo
# features: the test walks' build; unset for the player build. `BENILLA_IOS_SHIP=1` builds
# `release` with the `ship` profile's settings (fat LTO, one codegen unit; `Cargo.toml`), into the
# same `release` directory the Xcode project links from: the build to hand to a player.
set -euo pipefail
cd "$(dirname "$0")/.."
ship=()
if [ "${BENILLA_IOS_SHIP:-}" = 1 ]; then
    ship=(CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1)
fi
exec env -i \
    HOME="$HOME" \
    PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    IPHONEOS_DEPLOYMENT_TARGET=15.0 \
    ${ship[@]+"${ship[@]}"} \
    cargo build --release --target aarch64-apple-ios -p benilla-ios \
    ${BENILLA_IOS_FEATURES:+--features "$BENILLA_IOS_FEATURES"}
