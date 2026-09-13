@echo off
setlocal
chcp 65001 >nul
node "%~dp0scripts\start-service.mjs" backend
exit /b %errorlevel%
