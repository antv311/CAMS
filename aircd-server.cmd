@echo off
setlocal
set "ROOT=%~dp0"
if exist "%ROOT%target\release\aircd.exe" (
  "%ROOT%target\release\aircd.exe" %*
  exit /b %ERRORLEVEL%
)
if exist "%ROOT%target\debug\aircd.exe" (
  "%ROOT%target\debug\aircd.exe" %*
  exit /b %ERRORLEVEL%
)
cargo run --manifest-path "%ROOT%Cargo.toml" -- %*
exit /b %ERRORLEVEL%
