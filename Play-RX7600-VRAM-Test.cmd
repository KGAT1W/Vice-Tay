@echo off
setlocal
set "VICE_TEST_GAME=%~dp0..\..\Game\game_scarlett_master.exe"
set "VICE_TEST_SCRIPT=%~dp0scripts\play.ps1"
set "VICE_TEST_PACKAGE=0"
set "VICE_TEST_FLIGHT=0"
set "VICE_TEST_BUILD=release"
if /I "%~1"=="-Final" (
    set "VICE_TEST_PACKAGE=1"
    set "VICE_TEST_FLIGHT=1"
    set "VICE_TEST_BUILD=rxfinal"
    shift
)
if /I "%~1"=="-Next" (
    set "VICE_TEST_PACKAGE=1"
    set "VICE_TEST_FLIGHT=1"
    set "VICE_TEST_BUILD=rxnext"
    shift
)
if /I "%~1"=="-Flight" (
    set "VICE_TEST_PACKAGE=1"
    set "VICE_TEST_FLIGHT=1"
    shift
)
if /I "%~1"=="-Package" (
    set "VICE_TEST_PACKAGE=1"
    shift
)
if not "%~1"=="" set "VICE_TEST_GAME=%~1"
rem Experimental release build, same RX 7600 graphics; use local copies for buffers of 1 MiB and larger.
rem -Package also keeps up to 64 MiB of retired texture readback buffers for reuse.
rem -Flight adds fresh title buffer batches, shared window reads and short worker handoffs.
pwsh -NoProfile -ExecutionPolicy Bypass -Command "$m=@{VICTORY_CARD_BUFFERS_MIN_KB='1024'}; if ($env:VICE_TEST_PACKAGE -eq '1') { $m.VICTORY_TEXTURE_CALENDAR='1'; $m.VICTORY_GUARD_REUSE='1'; $m.VICTORY_TITLE_PREWARM_COUNT='256'; $m.VICTORY_READBACK_POOL='1'; $m.VICTORY_READBACK_POOL_MB='64' }; if ($env:VICE_TEST_FLIGHT -eq '1') { $m.VICTORY_RX_TITLE_STOCK='1'; $m.VICTORY_TITLE_PREWARM_COUNT='3328'; $m.VICTORY_RX_WINDOW_LOCK='1'; $m.VICTORY_RX_POOL='1' }; if ($env:VICE_TEST_BUILD -in 'rxnext','rxfinal') { $m.VICTORY_RX_TEX_READ='1'; $m.VICTORY_RX_PRELUDE='1' }; if ($env:VICE_TEST_BUILD -eq 'rxfinal') { $m.VICTORY_RX_LIST_NEXT='1'; $m.VICTORY_RX_LAND_NEXT='1'; $m.VICTORY_RX_QUEUE_INDEX='1'; $m.VICTORY_PORT_GUARD='1'; $m.VICTORY_PORT_GUARD_PLAN='0' }; & $env:VICE_TEST_SCRIPT -Exe $env:VICE_TEST_GAME -Machine 7600 -Build $env:VICE_TEST_BUILD -More $m; exit $LASTEXITCODE"
if errorlevel 1 pause
