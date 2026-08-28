#!/usr/bin/env bash
#
# Validate ios/project.yml **on Linux**, in seconds rather than a CI round trip.
#
# XcodeGen is a Swift package with no Darwin-only dependencies, so it builds and
# runs here. The .xcodeproj it produces is thrown away; this only proves the
# manifest is well-formed and that every source path it names exists. Xcode is
# still the only thing that can *build* the result.
#
# First run clones and builds XcodeGen (~90s); afterwards it is cached.
set -euo pipefail

# XcodeGen names the xcuserdata directory after whoever is running it and asks
# the environment who that is. A container image often sets neither variable,
# and it stops with "Couldn't find current username" — which reads like a
# problem with the manifest and is nothing of the sort.
export LOGNAME="${LOGNAME:-${USER:-builder}}"
export USER="${USER:-$LOGNAME}"

VERSION=${XCODEGEN_VERSION:-2.44.1}
# The commit that tag pointed at when it was pinned. See the check below.
COMMIT=${XCODEGEN_COMMIT:-21ac9944b0ab546a07422dbed86f33dd2ebd76f8}
CACHE=${XCODEGEN_CACHE:-target/xcodegen}
BIN="$CACHE/.build/release/xcodegen"

if [ ! -x "$BIN" ]; then
    echo "building XcodeGen $VERSION (once)…"
    rm -rf "$CACHE"
    git clone --depth 1 --branch "$VERSION" https://github.com/yonaskolb/XcodeGen.git "$CACHE"
    # A tag is a name somebody else can repoint, and this clone is built and run
    # on every push and every pull request. Checking the commit it resolved to
    # is what makes the version above mean one particular tree.
    got="$(cd "$CACHE" && git rev-parse HEAD)"
    if [ "$got" != "$COMMIT" ]; then
        echo "XcodeGen $VERSION is $got, expected $COMMIT" >&2
        echo "If the bump is deliberate, update XCODEGEN_COMMIT in this script." >&2
        rm -rf "$CACHE"
        exit 1
    fi
    (cd "$CACHE" && swift build -c release)
fi

# The manifest names the generated bindings; stand in for them so the path check
# passes without having to run the whole Rust build first.
mkdir -p ios/Generated && touch ios/Generated/acrylius_ffi.swift

"$PWD/$BIN" generate --spec ios/project.yml --project ios
echo
echo "sources picked up:"
# `sourcecode.swift` is a lastKnownFileType attribute, not a file.
grep -oE '[A-Za-z_]+\.swift' ios/Acrylius.xcodeproj/project.pbxproj \
    | grep -v '^sourcecode\.swift$' | sort -u | sed 's/^/  /'

# CI builds with `xcodebuild -scheme Acrylius`, and XcodeGen emits no scheme
# unless the manifest asks for one. Without this check that is a macOS-only
# failure reading "does not contain a scheme named Acrylius", which sounds like
# a broken project rather than a missing declaration.
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

# Which target compiles what. A build file appears twice per target it is in:
# once as a definition and once in that target's Sources phase. So 2 is one
# target, 4 is both.
#
# All three of these are silent when wrong. Two @main in one module does at
# least fail the build, but it names neither file; the other two produce an app
# that builds and misbehaves — duplicate Siri phrases, or an extension
# compiling against pasteboard APIs it has no business touching.
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
# BLEProbe links CoreBluetooth. In the widget it would be a permission the
# extension can never prompt for — its Info.plist carries no usage description
# and an extension has no way to show one. BLEDiagnostics holds no CoreBluetooth
# and is harmless in both, but there is nothing in the widget that reads it.
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

# An extension that is built but never embedded looks, on the phone, exactly
# like a widget iOS declines to offer: no error, no widget in the gallery.
grep -q 'AcryliusWidgets.appex in Embed' "$PBX" || {
    echo
    echo "the widget is not embedded in the app"
    rm -rf ios/Acrylius.xcodeproj; exit 1
}

rm -rf ios/Acrylius.xcodeproj

# The App Group is named in three places and all three must agree. A typo in
# any of them builds, installs, runs, and produces a widget that is empty
# forever with nothing anywhere saying why — so it is checked here, where it
# costs nothing, rather than discovered on a device.
#
# This is the group as *built*. A sideloading tool rewrites it at signing to
# keep it unique to its team, which is why the code discovers the real one from
# the provisioning profile and treats this only as a fallback. The three still
# have to agree: a build signed the ordinary way uses exactly this.
echo
echo "app group:"
# Quoted, so `group.flatMap` in the Swift is not mistaken for an identifier.
# In the entitlements the quotes are XML <string> delimiters; in the Swift they
# are a literal. Both come out the same.
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

echo
echo "project.yml is valid."
