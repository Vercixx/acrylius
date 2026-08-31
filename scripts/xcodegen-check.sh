#!/usr/bin/env bash
#
# Validate ios/project.yml on Linux. The generated .xcodeproj is thrown away;
# this only proves the manifest is well-formed and its source paths exist.
# First run clones and builds XcodeGen (~90s); afterwards it is cached.
set -euo pipefail

# A container image often sets neither variable, and XcodeGen then stops with
# "Couldn't find current username".
export LOGNAME="${LOGNAME:-${USER:-builder}}"
export USER="${USER:-$LOGNAME}"

# Same pinned XcodeGen as the macOS job; see scripts/xcodegen-bin.sh.
BIN="$("$(dirname "$0")/xcodegen-bin.sh")"

# The manifest names the generated bindings; stand in for them so the path check
# passes without having to run the whole Rust build first.
mkdir -p ios/Generated && touch ios/Generated/acrylius_ffi.swift

"$BIN" generate --spec ios/project.yml --project ios
echo
echo "sources picked up:"
# `sourcecode.swift` is a lastKnownFileType attribute, not a file.
grep -oE '[A-Za-z_]+\.swift' ios/Acrylius.xcodeproj/project.pbxproj \
    | grep -v '^sourcecode\.swift$' | sort -u | sed 's/^/  /'

# XcodeGen emits no scheme unless the manifest asks for one, and CI builds
# with `xcodebuild -scheme Acrylius`.
echo
echo "shared schemes:"
schemes=$(find ios/Acrylius.xcodeproj -name '*.xcscheme' -exec basename {} .xcscheme \; | sort)
if [ -z "$schemes" ]; then
    echo "  none: xcodebuild -scheme will fail"
    rm -rf ios/Acrylius.xcodeproj
    exit 1
fi
echo "$schemes" | sed 's/^/  /'
echo "$schemes" | grep -qx Acrylius || {
    echo "  no scheme named Acrylius, which is what CI builds"
    rm -rf ios/Acrylius.xcodeproj
    exit 1
}

PBX=ios/Acrylius.xcodeproj/project.pbxproj

echo
echo "targets:"
targets=$(grep -oE 'PBXNativeTarget "[A-Za-z]+"' "$PBX" | cut -d'"' -f2 | sort -u)
echo "$targets" | sed 's/^/  /'
for want in Acrylius AcryliusWidgets; do
    echo "$targets" | grep -qx "$want" || {
        echo "  missing target $want"; rm -rf ios/Acrylius.xcodeproj; exit 1
    }
done

# A build file appears twice per target (definition plus Sources phase): 2 means one target, 4 means both.
# Wrong membership is silent: the app builds and just misbehaves.
echo
echo "target membership:"
membership() {
    local count; count=$(grep -c "$1 in Sources" "$PBX" || true)
    case "$count" in
        2) echo "one target" ;;
        4) echo "both targets" ;;
        *) echo "$((count / 2)) targets" ;;
    esac
}
# BLETransport links CoreBluetooth — in the widget that is a permission the
# extension can never prompt for.
for pair in "AcryliusWidget.swift:one target" "Shortcuts.swift:one target" \
            "IosEffector.swift:one target" "BLETransport.swift:one target" \
            "PCEntity.swift:both targets" \
            "SharedContainer.swift:both targets"; do
    file=${pair%%:*}; want=${pair#*:}
    got=$(membership "$file")
    printf '  %-24s %s\n' "$file" "$got"
    [ "$got" = "$want" ] || {
        echo "    expected $want"; rm -rf ios/Acrylius.xcodeproj; exit 1
    }
done

# An extension built but never embedded: no error, no widget in the gallery.
grep -q 'AcryliusWidgets.appex in Embed' "$PBX" || {
    echo
    echo "the widget is not embedded in the app"
    rm -rf ios/Acrylius.xcodeproj; exit 1
}

rm -rf ios/Acrylius.xcodeproj

# The App Group is named in three places and all three must agree; a mismatch builds fine but leaves the widget empty forever.
# A sideloading tool rewrites the group at signing, so the code discovers the real one from the provisioning profile; an ordinarily signed build uses this value directly.
echo
echo "app group:"
# Quoted, so `group.flatMap` in the Swift is not mistaken for an identifier.
group_in() {
    grep -oE '(<string>|")group\.[a-z0-9.]+' "$1" \
        | sed -E 's/^(<string>|")//' | head -1
}
app_group=$(group_in ios/Acrylius/Acrylius.entitlements)
widget_group=$(group_in ios/Acrylius/Widgets/AcryliusWidgets.entitlements)
code_group=$(group_in ios/Acrylius/Runtime/SharedContainer.swift)
echo "  app         $app_group"
echo "  widget      $widget_group"
echo "  source      $code_group"
if [ -z "$app_group" ] || [ "$app_group" != "$widget_group" ] || [ "$app_group" != "$code_group" ]; then
    echo "  they must all be the same"
    exit 1
fi

# The icon is three things that must agree: an appiconset, the file it names,
# and a build setting. Miss one and the home screen shows a blank square.
echo
echo "app icon:"
SET=ios/Acrylius/Assets.xcassets/AppIcon.appiconset
named=$(grep -oE '"filename"[[:space:]]*:[[:space:]]*"[^"]+"' "$SET/Contents.json" \
    | head -1 | cut -d'"' -f4)
setting=$(grep -oE 'ASSETCATALOG_COMPILER_APPICON_NAME:[[:space:]]*[A-Za-z]+' ios/project.yml \
    | head -1 | awk -F': *' '{print $2}')
echo "  names        ${named:-nothing}"
echo "  setting      ${setting:-unset}"
[ -n "$named" ] && [ -f "$SET/$named" ] || {
    echo "  the appiconset names a file that is not there"; exit 1
}
[ "$setting" = "AppIcon" ] || {
    echo "  project.yml must set ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon"; exit 1
}

# iOS composites an icon's alpha over black. Colour type read from the PNG
# IHDR (4 and 6 carry alpha); the CI container has no image tool.
colour=$(od -An -tu1 -j 25 -N 1 "$SET/$named" | tr -d ' ')
case "$colour" in
    4|6) echo "  alpha        yes — iOS will composite it over black"; exit 1 ;;
    *)   echo "  alpha        none" ;;
esac

echo
echo "project.yml is valid."
