# Sourced from the gxwi-event-viewer root, not run: what cargo needs to build
# libgxwi and the `peios` crate against sibling checkouts. The Rust it is
# built with is rust-toolchain.toml's.
REPO="$(cd .. && pwd)"
export BINDGEN_EXTRA_CLANG_ARGS="-isystem $(gcc -print-file-name=include)"
export PEIOS_LIB_DIR="$REPO/libpeios/target/debug"
export PEIOS_INCLUDE="$REPO/libpeios/include"
export PKM_UAPI="$REPO/pkm/out/build/headers/usr/include"
export LD_LIBRARY_PATH="$REPO/libpeios/target/debug"
