#!/usr/bin/env bash
# Runs clap-validator on every *.clap and Steinberg's VST3 validator on every *.vst3 in <bundle dir>
# and exits non-zero when either reports a failure. Warnings go to $GITHUB_STEP_SUMMARY when it is set.
#
# Builds both validators at the versions in scripts/validators.env on first use, into
# $VALIDATORS_DIR (default ~/.cache/plugin-format/validators), where a later run finds them. Needs
# cargo, git, cmake, a C++ compiler and jq.
#
# Usage: scripts/validate.sh <bundle dir>
set -euo pipefail

if [ $# -ne 1 ] || [ ! -d "$1" ]; then
    echo "usage: $0 <bundle dir>" >&2
    exit 2
fi
bundles=$1
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=validators.env
. "$here/validators.env"
cache=${VALIDATORS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/plugin-format/validators}
mkdir -p "$cache"
cache=$(cd "$cache" && pwd)

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) exe=.exe ;;
    *) exe= ;;
esac

for tool in cargo git cmake jq; do
    command -v "$tool" > /dev/null || { echo "$tool is needed to validate" >&2; exit 1; }
done

clap_validator=$cache/clap-validator-$CLAP_VALIDATOR_REV/bin/clap-validator$exe
if [ ! -x "$clap_validator" ]; then
    echo "building clap-validator $CLAP_VALIDATOR_REV"
    cargo install --git https://github.com/free-audio/clap-validator --rev "$CLAP_VALIDATOR_REV" --locked \
        --root "$cache/clap-validator-$CLAP_VALIDATOR_REV"
fi

vst3_validator=$cache/vst3-validator-$VST3_SDK_TAG/validator$exe
if [ ! -x "$vst3_validator" ]; then
    echo "building the VST3 validator $VST3_SDK_TAG"
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    git clone --quiet --depth 1 --branch "$VST3_SDK_TAG" https://github.com/steinbergmedia/vst3sdk "$work/sdk"
    git -C "$work/sdk" submodule update --quiet --init --depth 1 base cmake pluginterfaces public.sdk
    cmake -S "$work/sdk" -B "$work/build" -DCMAKE_BUILD_TYPE=Release \
        -DSMTG_ENABLE_VST3_PLUGIN_EXAMPLES=OFF -DSMTG_ENABLE_VST3_HOSTING_EXAMPLES=OFF \
        -DSMTG_ENABLE_VSTGUI_SUPPORT=OFF -DSMTG_CREATE_PLUGIN_LINK=OFF -DSMTG_RUN_VST_VALIDATOR=OFF
    cmake --build "$work/build" --config Release --target validator --parallel
    built=$(find "$work/build/bin" -name "validator$exe" -type f | head -n 1)
    [ -n "$built" ] || { echo "the VST3 validator was not built" >&2; exit 1; }
    mkdir -p "$(dirname "$vst3_validator")"
    cp "$built" "$vst3_validator"
fi

summary() {
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        cat >> "$GITHUB_STEP_SUMMARY"
    else
        cat > /dev/null
    fi
}

failed=0
found=0
results=$(mktemp)
trap 'rm -rf "${work:-}" "$results"' EXIT

for plugin in "$bundles"/*.clap; do
    [ -e "$plugin" ] || continue
    found=1
    echo "== clap-validator $plugin"
    "$clap_validator" validate --hide-output "$plugin" || failed=1
    # The JSON run repeats the validation, which takes seconds, for the warnings in the summary.
    if "$clap_validator" validate --hide-output --json "$plugin" > "$results" 2> /dev/null || [ -s "$results" ]; then
        jq -r --arg plugin "$(basename "$plugin")" '
            [.results[] | select(.status.code == "warning")] as $warnings
            | if ($warnings | length) > 0 then
                "### clap-validator warnings for \($plugin)\n" +
                ($warnings | map("- `\(.test[].test)`: \(.status.details | gsub("\n"; " "))") | join("\n")) + "\n"
              else empty end' "$results" | summary
    fi
done

for plugin in "$bundles"/*.vst3; do
    [ -e "$plugin" ] || continue
    found=1
    echo "== VST3 validator $plugin"
    "$vst3_validator" "$plugin" || failed=1
done

if [ "$found" = 0 ]; then
    echo "no .clap or .vst3 in $bundles" >&2
    exit 1
fi
exit "$failed"
