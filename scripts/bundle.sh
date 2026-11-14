#!/usr/bin/env bash
# Builds a plugin crate in release and lays it out in <output dir> as hosts look for it:
#   Linux    <name>.clap, <name>.vst3/Contents/<arch>-linux/<name>.so
#   Windows  <name>.clap, <name>.vst3/Contents/x86_64-win/<name>.vst3
#   macOS    <name>.clap and <name>.vst3 as bundles of one universal binary, Contents/MacOS/<name>,
#            signed ad hoc
# With CARGO_BUILD_TARGET set to a Linux or Windows target it builds for that one, e.g. Windows from
# Linux with MinGW.
#
# Usage: scripts/bundle.sh <crate> <name> <bundle identifier> <output dir>
set -euo pipefail

if [ $# -ne 4 ]; then
    echo "usage: $0 <crate> <name> <bundle identifier> <output dir>" >&2
    exit 2
fi
crate=$1
name=$2
identifier=$3
out=$4
lib=${crate//-/_}
target_dir=${CARGO_TARGET_DIR:-target}
target=${CARGO_BUILD_TARGET:-}

if [ -n "$target" ]; then
    case "$target" in
        *-linux-*) os=linux ;;
        *-windows-*) os=windows ;;
        *-apple-darwin) os=macos ;;
        *) echo "unsupported target: $target" >&2; exit 1 ;;
    esac
    arch=${target%%-*}
    release=$target_dir/$target/release
else
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        MINGW* | MSYS* | CYGWIN*) os=windows ;;
        *) echo "unsupported system: $(uname -s)" >&2; exit 1 ;;
    esac
    arch=$(uname -m)
    release=$target_dir/release
fi

mkdir -p "$out"
rm -rf "${out:?}/$name.clap" "${out:?}/$name.vst3"

case "$os" in
    linux)
        cargo build --release -p "$crate" --lib
        cp "$release/lib$lib.so" "$out/$name.clap"
        mkdir -p "$out/$name.vst3/Contents/$arch-linux"
        cp "$release/lib$lib.so" "$out/$name.vst3/Contents/$arch-linux/$name.so"
        ;;
    windows)
        cargo build --release -p "$crate" --lib
        cp "$release/$lib.dll" "$out/$name.clap"
        mkdir -p "$out/$name.vst3/Contents/x86_64-win"
        cp "$release/$lib.dll" "$out/$name.vst3/Contents/x86_64-win/$name.vst3"
        ;;
    macos)
        cargo build --release -p "$crate" --lib --target aarch64-apple-darwin --target x86_64-apple-darwin
        universal=$(mktemp -d)
        trap 'rm -rf "$universal"' EXIT
        lipo -create -output "$universal/$name" \
            "$target_dir/aarch64-apple-darwin/release/lib$lib.dylib" \
            "$target_dir/x86_64-apple-darwin/release/lib$lib.dylib"
        for bundle in "$out/$name.clap" "$out/$name.vst3"; do
            mkdir -p "$bundle/Contents/MacOS"
            cp "$universal/$name" "$bundle/Contents/MacOS/$name"
            cat > "$bundle/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>English</string>
    <key>CFBundleExecutable</key>
    <string>$name</string>
    <key>CFBundleIdentifier</key>
    <string>$identifier</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$name</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
</dict>
</plist>
EOF
            codesign --force --sign - --deep "$bundle"
        done
        ;;
esac

echo "$out/$name.clap"
echo "$out/$name.vst3"
