@echo off
setlocal
chcp 65001 >nul
node "%~dp0scripts\stop-service.mjs" backend
exit /b %errorlevel%
