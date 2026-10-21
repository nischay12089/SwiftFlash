#!/bin/sh
set -eu
SWIFTFLASH_PACKAGE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SWIFTFLASH_BINARY="$SWIFTFLASH_PACKAGE/bin/linux/swiftflash-fastboot"
if [ ! -f "$SWIFTFLASH_BINARY" ]; then
    printf '%s\n' "SwiftFlash executable is missing: $SWIFTFLASH_BINARY" >&2
    printf '%s\n' 'Build or download the Linux fastboot package first.' >&2
    exit 1
fi
for SWIFTFLASH_TOOL in "$SWIFTFLASH_BINARY" "$SWIFTFLASH_PACKAGE/bin/linux/fastboot"; do
    if [ -f "$SWIFTFLASH_TOOL" ] && [ ! -x "$SWIFTFLASH_TOOL" ] && ! chmod +x "$SWIFTFLASH_TOOL"; then
        printf '%s\n' "Cannot make tool executable: $SWIFTFLASH_TOOL" >&2
        exit 1
    fi
done
exec "$SWIFTFLASH_BINARY" --package "$SWIFTFLASH_PACKAGE" --mode upgrade --yes "$@"
