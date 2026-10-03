#!/bin/sh
# Runs on the host: build Event Viewer and put it in the share of the VM that
# ../gxwi/dev/boot.sh started, with its icon for the base theme and its
# declaration for the catalogue. GXWI's dev service puts them where a package
# would, and its dev loop restarts on a new one.
#
# The rename makes the new binary appear whole, never half-written. It links
# libpeios by way of libgxwi, so it is an ordinary glibc build and uses the
# libpeios the guest image ships.
set -eu
cd "$(dirname "$0")/.."
. dev/env.sh
share=../gxwi/target/vmshare
[ -d "$share" ] || { echo "no $share: boot the VM from ../gxwi first" >&2; exit 1; }
cargo build --release
mkdir -p "$share/icons/base"
cp gxwi-event-viewer.svg "$share/icons/base/dev.peios.gxwi-event-viewer.svg"
mkdir -p "$share/apps"
cp dev.peios.gxwi-event-viewer.toml "$share/apps/dev.peios.gxwi-event-viewer.toml"
cp target/release/gxwi-event-viewer "$share/gxwi-event-viewer.new"
mv "$share/gxwi-event-viewer.new" "$share/gxwi-event-viewer"
