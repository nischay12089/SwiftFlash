@echo off
setlocal DisableDelayedExpansion
set "SWIFTFLASH_BINARY=%~dp0bin\windows\swiftflash-fastboot.exe"
if not exist "%SWIFTFLASH_BINARY%" (
    echo SwiftFlash executable is missing: "%SWIFTFLASH_BINARY%" 1>&2
    exit /b 1
)
"%SWIFTFLASH_BINARY%" --package "%~dp0." --mode upgrade --yes %*
set "SWIFTFLASH_EXIT_CODE=%ERRORLEVEL%"
endlocal & exit /b %SWIFTFLASH_EXIT_CODE%
