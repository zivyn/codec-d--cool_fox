@echo off
setlocal EnableExtensions DisableDelayedExpansion
if not "%~1"=="" if /i not "%~1"=="pareto-residency" (
  echo Usage: Play-VIctory.cmd pareto-residency 1>&2
  exit /b 2
)
if not defined VICTORY_GAME_DIR (
  echo Set VICTORY_GAME_DIR to the folder of your own dump, for example: set "VICTORY_GAME_DIR=D:\Games\MyDump" 1>&2
  exit /b 2
)
rem The title is the executable of the folder: VICTORY_GAME_EXE names it, else the largest .exe there.
if not defined VICTORY_GAME_EXE for /f "delims=" %%f in ('dir /b /a-d /o-s "%VICTORY_GAME_DIR%\*.exe" 2^>nul') do if not defined VICTORY_GAME_EXE set "VICTORY_GAME_EXE=%%f"
if not defined VICTORY_GAME_EXE (
  echo No .exe in VICTORY_GAME_DIR: set VICTORY_GAME_DIR to your own dump folder, or VICTORY_GAME_EXE to the title file name. 1>&2
  exit /b 2
)
if not exist "%VICTORY_GAME_DIR%\%VICTORY_GAME_EXE%" (
  echo VICTORY_GAME_EXE is not in VICTORY_GAME_DIR. 1>&2
  exit /b 2
)
set "runner=%~dp0victory-3d-rtx-pareto-residency.exe"
if not exist "%runner%" set "runner=%~dp0victory.exe"
if not exist "%runner%" set "runner=%~dp0target\release\victory.exe"
if not exist "%runner%" (
  echo Extract the Windows release or run cargo build --release --locked -p victory. 1>&2
  exit /b 2
)
rem The settings the title is played with are built into the executable (README, "Run"): nothing to set here
rem but where your files are and how much is logged.
if not defined VICTORY_CACHE_DIR set "VICTORY_CACHE_DIR=%LOCALAPPDATA%\VIctory\cache-pareto"
if not defined VICTORY_LOG if not defined RUST_LOG set "VICTORY_LOG=warn"
"%runner%" run "%VICTORY_GAME_DIR%\%VICTORY_GAME_EXE%" --console series-s --execute
exit /b %errorlevel%
