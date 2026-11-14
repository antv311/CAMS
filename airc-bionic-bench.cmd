@echo off
setlocal
set "ROOT=%~dp0"
set "PYTHONPATH=%ROOT%clients\python;%PYTHONPATH%"
if defined AIRCD_PYTHON (
    "%AIRCD_PYTHON%" -m aircd.bionic_bench %*
) else (
    python -m aircd.bionic_bench %*
)
exit /b %ERRORLEVEL%
