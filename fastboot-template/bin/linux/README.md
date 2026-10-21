# Linux tools

Place the compiled `swiftflash-fastboot` in this directory and make it executable
with `chmod +x bin/linux/swiftflash-fastboot`. The Linux CI package already includes it.

Supply a Linux fastboot executable compatible with your ROM package. Either put
the executable `fastboot` here, or pass `--fastboot /path/to/platform-tools/fastboot` to the
launcher. SwiftFlash does not search `PATH` for fastboot. Configure USB access
for your device as required by your Linux distribution.

The reference uses literal `_ab` partition names, which may require its
vendor-compatible fastboot. Google's Platform-Tools are an option only when
compatible with the package and device:
https://developer.android.com/tools/releases/platform-tools
Follow the tool's license terms. SwiftFlash does not redistribute third-party tools.
