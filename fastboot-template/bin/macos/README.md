# macOS tools

Place the compiled `swiftflash-fastboot` for your Mac's architecture in this
directory and make it executable with `chmod +x bin/macos/swiftflash-fastboot`.
The macOS CI packages already include the appropriate executable; choose ARM64
for Apple silicon or x86_64 for Intel.

Supply a macOS fastboot executable compatible with your ROM package. Either put
the executable `fastboot` here, or pass `--fastboot /path/to/platform-tools/fastboot` to the
launcher. SwiftFlash does not search `PATH` for fastboot.

The reference uses literal `_ab` partition names, which may require its
vendor-compatible fastboot. Google's Platform-Tools are an option only when
compatible with the package and device:
https://developer.android.com/tools/releases/platform-tools
Follow the tool's license terms. SwiftFlash does not redistribute third-party tools.
