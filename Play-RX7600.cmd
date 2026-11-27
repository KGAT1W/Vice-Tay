@echo off
setlocal
set "title=%~dp0..\..\Game\game_scarlett_master.exe"
if not "%~1"=="" set "title=%~1"
call "%~dp0Play-VIctory.cmd" -Exe "%title%" -Machine 7600 -Build diag
if errorlevel 1 pause
