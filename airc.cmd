@echo off
setlocal
set "ROOT=%~dp0"
set "PYTHONPATH=%ROOT%clients\python;%PYTHONPATH%"
python -m aircd.cli %*
exit /b %ERRORLEVEL%
