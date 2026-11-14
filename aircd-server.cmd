@echo off
setlocal
set "ROOT=%~dp0"
set "EXE="
if defined AIRCD_EXECUTABLE (
    if exist "%AIRCD_EXECUTABLE%" set "EXE=%AIRCD_EXECUTABLE%"
)
if not defined EXE (
    for /f "usebackq delims=" %%I in (`powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%ROOT%scripts\Get-AircdExecutable.ps1"`) do set "EXE=%%I"
)
if not defined EXE goto build
"%EXE%" %*
exit /b %ERRORLEVEL%
:build
cargo run --manifest-path "%ROOT%Cargo.toml" -- %*
exit /b %ERRORLEVEL%
