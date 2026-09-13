@echo off
setlocal
chcp 65001 >nul
node "%~dp0scripts\start-all.mjs"
exit /b %errorlevel%
