@echo off
setlocal
chcp 65001 >nul
node "%~dp0scripts\stop-all.mjs"
exit /b %errorlevel%
