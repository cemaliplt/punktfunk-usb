#!/usr/bin/env bash
# Build PunktfunkCore.xcframework for iOS arm64 device only.
set -euo pipefail
cd "$(dirname "$0")/../.."
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# Proc-macro linker without fixup chains for macOS host links.
HOST_LINKER="$(mkdir -p "$TARGET_DIR" && cd "$TARGET_DIR" && pwd)/proc-macro-linker"
HOST_LINKER_SH='#!/bin/sh
exec cc -Wl,-no_fixup_chains "$@"'
if [[ "$(cat "$HOST_LINKER" 2>/dev/null)" != "$HOST_LINKER_SH" ]]; then
    printf '%s\n' "$HOST_LINKER_SH" > "$HOST_LINKER"
    chmod +x "$HOST_LINKER"
fi
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$HOST_LINKER"
export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$HOST_LINKER"

# Prebuilt Skia binaries for iOS arm64.
SKIA_TAG=0.153.3
SKIA_HASH=b7f043e0b1e2a850e702
SKIA_FEATURES=ganesh-jpegd-jpege-metal-pdf-textlayout
SKIA_DIR="$(cd "$TARGET_DIR" && pwd)/skia-binaries"
mkdir -p "$SKIA_DIR"

f="skia-binaries-$SKIA_HASH-aarch64-apple-ios-$SKIA_FEATURES.tar.gz"
want="1b78ad184fb7b40e39feda07181b9cce68b44082704462bee5f0a21a23b7292a"
if [[ "$(shasum -a 256 "$SKIA_DIR/$f" 2>/dev/null | cut -d' ' -f1)" != "$want" ]]; then
    url="https://github.com/rust-skia/skia-binaries/releases/download/$SKIA_TAG/$f"
    curl -fsSL --retry 3 -o "$SKIA_DIR/$f.part" "$url"
    if [[ "$(shasum -a 256 "$SKIA_DIR/$f.part" | cut -d' ' -f1)" != "$want" ]]; then
        echo "ERROR: $url does not match its pinned SHA-256" >&2
        exit 1
    fi
    mv "$SKIA_DIR/$f.part" "$SKIA_DIR/$f"
fi
export SKIA_BINARIES_URL="file://$SKIA_DIR/skia-binaries-{key}.tar.gz"

# Build Rust core for iOS device.
IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-client-apple --target aarch64-apple-ios

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# C header generation and modulemap.
cargo run -q -p gen-headers
mkdir -p "$STAGE/include"
{
    echo "#define PUNKTFUNK_FEATURE_QUIC 1"
    cat include/punktfunk_core.h
} > "$STAGE/include/punktfunk_core.h"
cp include/punktfunk_console.h "$STAGE/include/"
cat > "$STAGE/include/module.modulemap" <<'EOF'
module PunktfunkCore {
    header "punktfunk_core.h"
    header "punktfunk_console.h"
    export *
}
EOF

# Assemble xcframework with iOS arm64 slice.
rm -rf clients/apple/PunktfunkCore.xcframework
xcodebuild -create-xcframework \
    -library "$TARGET_DIR"/aarch64-apple-ios/release/libpunktfunk_apple.a \
    -headers "$STAGE/include" \
    -output clients/apple/PunktfunkCore.xcframework

# Ad-hoc sign so Xcode embeds without complaining.
codesign --force --timestamp=none --sign - clients/apple/PunktfunkCore.xcframework
echo "OK: clients/apple/PunktfunkCore.xcframework (iOS arm64)"
