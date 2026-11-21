@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Only a process-scoped policy is requested; Group Policy still applies.
rem No-argument launches include Explorer double-click. Suppress this pause for
rem automation with SOLASCOPE_INSTALL_NO_PAUSE=1, or invoke install.ps1 directly.
set "SOLASCOPE_PAUSE="
if "%~1"=="" if not defined SOLASCOPE_INSTALL_NO_PAUSE set "SOLASCOPE_PAUSE=1"
set "SOLASCOPE_POWERSHELL=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%SOLASCOPE_POWERSHELL%" (
    >&2 echo Windows PowerShell 5.1 is required.
    set "SOLASCOPE_EXIT=1"
    goto finish
)
if not exist "%~dp0install.ps1" (
    >&2 echo install.ps1 must be next to install.cmd.
    set "SOLASCOPE_EXIT=1"
    goto finish
)
"%SOLASCOPE_POWERSHELL%" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set "SOLASCOPE_EXIT=%ERRORLEVEL%"
:finish
if defined SOLASCOPE_PAUSE pause
exit /b %SOLASCOPE_EXIT%
