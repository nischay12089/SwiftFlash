#!/bin/sh
set -eu
SWIFTFLASH_PACKAGE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
case "$(uname -s)" in
    Linux) exec /bin/sh "$SWIFTFLASH_PACKAGE/linux_install_upgrade.sh" "$@" ;;
    Darwin) exec /bin/sh "$SWIFTFLASH_PACKAGE/macos_install_upgrade.sh" "$@" ;;
    *) printf '%s\n' 'Unsupported operating system. Use flash.bat on Windows.' >&2; exit 1 ;;
esac
