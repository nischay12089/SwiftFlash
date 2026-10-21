# Windows tools

Place the compiled `swiftflash-fastboot.exe` in this directory. The Windows CI
package already includes it.

Supply a Windows fastboot executable compatible with your ROM package. Either
put `fastboot.exe` and its required DLLs here, or pass
`--fastboot "C:\path\to\platform-tools\fastboot.exe"` to the launcher.
SwiftFlash does not search `PATH` for fastboot.

The reference uses literal `_ab` partition names, which may require its
vendor-compatible fastboot. Google's Platform-Tools are an option only when
compatible with the package and device:
https://developer.android.com/tools/releases/platform-tools
Follow the tool's license terms. SwiftFlash does not redistribute third-party tools.
