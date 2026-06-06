#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="${BRAINDRAIN_APP_NAME:-BrainDrain}"
PRODUCT_NAME="${BRAINDRAIN_PRODUCT_NAME:-BrainDrain}"
BUNDLE_ID="${BRAINDRAIN_BUNDLE_ID:-dev.sargunv.braindrain}"
MIN_MACOS_VERSION="${BRAINDRAIN_MIN_MACOS_VERSION:-14.0}"
VERSION="${BRAINDRAIN_VERSION:-${GITHUB_REF_NAME:-0.0.0}}"
VERSION="${VERSION#v}"
BUILD_NUMBER="${BRAINDRAIN_BUILD_NUMBER:-$(git -C "$ROOT" rev-list --count HEAD)}"
RUST_TARGET="${BRAINDRAIN_RUST_TARGET:-$(rustc -vV | awk '/^host:/ {print $2}')}"
SWIFT_TRIPLE="${BRAINDRAIN_SWIFT_TRIPLE:-}"

case "$RUST_TARGET" in
    aarch64-apple-darwin)
        ARCH="${BRAINDRAIN_ARCH:-arm64}"
        SWIFT_TRIPLE="${SWIFT_TRIPLE:-arm64-apple-macosx${MIN_MACOS_VERSION}}"
        ;;
    x86_64-apple-darwin)
        ARCH="${BRAINDRAIN_ARCH:-x86_64}"
        SWIFT_TRIPLE="${SWIFT_TRIPLE:-x86_64-apple-macosx${MIN_MACOS_VERSION}}"
        ;;
    *)
        ARCH="${BRAINDRAIN_ARCH:-$RUST_TARGET}"
        ;;
esac

BUILD_ROOT="$ROOT/build/macos/$ARCH"
DIST_DIR="$ROOT/dist/macos"
APP="$BUILD_ROOT/$APP_NAME.app"
RUST_DYLIB_NAME="libbraindrain_bindings_uniffi.dylib"
RUST_LIBRARY_DIR="$ROOT/target/$RUST_TARGET/release"
RUST_DYLIB="$RUST_LIBRARY_DIR/$RUST_DYLIB_NAME"
FINAL_ZIP="$DIST_DIR/$APP_NAME-$VERSION-macOS-$ARCH.zip"
NOTARY_ZIP="$BUILD_ROOT/$APP_NAME-$VERSION-notary.zip"

log() {
    printf '==> %s\n' "$*"
}

require() {
    if [[ -z "${!1:-}" ]]; then
        printf 'error: %s is required\n' "$1" >&2
        exit 1
    fi
}

SWIFT_ARGS=(
    --package-path "$ROOT/apps/macos"
    --scratch-path "$BUILD_ROOT/swiftpm"
    --cache-path "$BUILD_ROOT/swiftpm-cache"
    --config-path "$BUILD_ROOT/swiftpm-config"
    --security-path "$BUILD_ROOT/swiftpm-security"
    --disable-sandbox
    -c release
    --triple "$SWIFT_TRIPLE"
)

generate_swift_bindings() {
    log "Generating Swift UniFFI bindings"
    rm -rf "$ROOT/crates/bindings-uniffi/.generated"
    mkdir -p "$ROOT/crates/bindings-uniffi/.generated/swift"
    cargo build -p braindrain-bindings-uniffi
    cargo run -p braindrain-uniffi-bindgen -- generate \
        --library "$ROOT/target/debug/$RUST_DYLIB_NAME" \
        --language swift \
        --out-dir "$ROOT/crates/bindings-uniffi/.generated/swift"
}

build_app() {
    generate_swift_bindings

    log "Building Rust FFI dylib for $RUST_TARGET"
    cargo build -p braindrain-bindings-uniffi --release --target "$RUST_TARGET"

    log "Building Swift app for $SWIFT_TRIPLE"
    env \
        CLANG_MODULE_CACHE_PATH="$BUILD_ROOT/clang-module-cache" \
        BRAINDRAIN_RUST_LIBRARY_DIR="$RUST_LIBRARY_DIR" \
        BRAINDRAIN_RUST_RPATH="@executable_path/../Frameworks" \
        swift build "${SWIFT_ARGS[@]}"

    SWIFT_BIN_PATH="$(
        env \
            CLANG_MODULE_CACHE_PATH="$BUILD_ROOT/clang-module-cache" \
            BRAINDRAIN_RUST_LIBRARY_DIR="$RUST_LIBRARY_DIR" \
            BRAINDRAIN_RUST_RPATH="@executable_path/../Frameworks" \
            swift build "${SWIFT_ARGS[@]}" --show-bin-path
    )"
    SWIFT_EXECUTABLE="$SWIFT_BIN_PATH/$PRODUCT_NAME"

    if [[ ! -x "$SWIFT_EXECUTABLE" ]]; then
        printf 'error: expected Swift executable at %s\n' "$SWIFT_EXECUTABLE" >&2
        exit 1
    fi

    if [[ ! -f "$RUST_DYLIB" ]]; then
        printf 'error: expected Rust dylib at %s\n' "$RUST_DYLIB" >&2
        exit 1
    fi

    log "Assembling $APP"
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources"
    cp "$SWIFT_EXECUTABLE" "$APP/Contents/MacOS/$PRODUCT_NAME"
    cp "$RUST_DYLIB" "$APP/Contents/Frameworks/$RUST_DYLIB_NAME"

    cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>$PRODUCT_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$BUILD_NUMBER</string>
  <key>LSMinimumSystemVersion</key>
  <string>$MIN_MACOS_VERSION</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

    OLD_DYLIB_ID="$(otool -D "$RUST_DYLIB" | sed -n '2p')"
    install_name_tool -id "@rpath/$RUST_DYLIB_NAME" "$APP/Contents/Frameworks/$RUST_DYLIB_NAME"
    if [[ -n "$OLD_DYLIB_ID" ]]; then
        install_name_tool -change "$OLD_DYLIB_ID" "@rpath/$RUST_DYLIB_NAME" "$APP/Contents/MacOS/$PRODUCT_NAME" || true
    fi
    if ! otool -l "$APP/Contents/MacOS/$PRODUCT_NAME" | grep -q '@executable_path/../Frameworks'; then
        install_name_tool -add_rpath "@executable_path/../Frameworks" "$APP/Contents/MacOS/$PRODUCT_NAME"
    fi
}

sign_app() {
    require CODESIGN_IDENTITY

    log "Signing embedded dylib"
    codesign --force --timestamp --options runtime \
        --sign "$CODESIGN_IDENTITY" \
        "$APP/Contents/Frameworks/$RUST_DYLIB_NAME"

    log "Signing app bundle"
    codesign --force --timestamp --options runtime \
        --sign "$CODESIGN_IDENTITY" \
        "$APP"

    codesign --verify --strict --deep --verbose=2 "$APP"
}

set_notarytool_args() {
    if [[ -n "${NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
        NOTARYTOOL_ARGS=(--keychain-profile "$NOTARY_KEYCHAIN_PROFILE")
    elif [[ -n "${APPLE_API_KEY_PATH:-}" && -n "${APPLE_API_KEY_ID:-}" && -n "${APPLE_API_ISSUER_ID:-}" ]]; then
        NOTARYTOOL_ARGS=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID")
    elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
        NOTARYTOOL_ARGS=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
    else
        printf 'error: notarization requires NOTARY_KEYCHAIN_PROFILE, or APPLE_API_KEY_PATH/APPLE_API_KEY_ID/APPLE_API_ISSUER_ID, or APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID\n' >&2
        exit 1
    fi
}

notarize_app() {
    log "Creating notarization zip"
    mkdir -p "$BUILD_ROOT"
    rm -f "$NOTARY_ZIP"
    ditto -c -k --keepParent "$APP" "$NOTARY_ZIP"

    log "Submitting to Apple notarization"
    set_notarytool_args
    xcrun notarytool submit "$NOTARY_ZIP" "${NOTARYTOOL_ARGS[@]}" --wait

    log "Stapling notarization ticket"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
}

zip_app() {
    log "Creating release zip"
    mkdir -p "$DIST_DIR"
    rm -f "$FINAL_ZIP"
    ditto -c -k --keepParent "$APP" "$FINAL_ZIP"
    printf '%s\n' "$FINAL_ZIP"
}

case "${1:-package}" in
    package)
        build_app
        zip_app
        ;;
    release)
        build_app
        sign_app
        notarize_app
        zip_app
        ;;
    *)
        printf 'usage: %s [package|release]\n' "$0" >&2
        exit 2
        ;;
esac
