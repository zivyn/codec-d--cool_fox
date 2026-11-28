@echo off
setlocal EnableExtensions DisableDelayedExpansion
if /i "%~1"=="get-dlss" goto get_dlss
if not "%~1"=="" if /i not "%~1"=="pareto-residency" (
  echo Usage: Play-VIctory.cmd [pareto-residency]   or   Play-VIctory.cmd get-dlss 1>&2
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
rem NVIDIA's DLSS upscaler runs by default on a GeForce RTX once NVIDIA's files are in place (README, "DLSS"). They
rem are looked for in VICTORY_HOME, else beside the folder of VICTORY_CACHE_DIR, else in victory-home beside the exe:
rem never in your user profile. Without them the title's own upscaler runs. Here, once, if they are missing and you are
rem on a GeForce RTX, you are asked whether to fetch them from NVIDIA; "Play-VIctory.cmd get-dlss" fetches them without
rem asking; VICTORY_DLSS=0 turns DLSS off and stops the question.
for %%R in ("%runner%") do set "exe_dir=%%~dpR"
if not defined VICTORY_HOME if not defined VICTORY_CACHE_DIR set "VICTORY_HOME=%exe_dir%victory-home"
set "dlss_ps=%~dp0get-nvidia-dlss.ps1"
if not exist "%dlss_ps%" set "dlss_ps=%~dp0scripts\get-nvidia-dlss.ps1"
if defined VICTORY_STREAMLINE_DIR goto dlss_done
if /i "%VICTORY_DLSS%"=="0" goto dlss_done
if not exist "%dlss_ps%" goto dlss_done
powershell -NoProfile -ExecutionPolicy Bypass -File "%dlss_ps%" -Ask -ExeDir "%exe_dir%."
:dlss_done
rem The settings the title is played with are built into the executable (README, "Run"): nothing to set here
rem but where your files are and how much is logged.
rem The cache goes beside the exe, on its drive, not into your user profile; a cache an earlier version made there is kept in use.
if not defined VICTORY_CACHE_DIR if exist "%LOCALAPPDATA%\VIctory\cache-pareto\" set "VICTORY_CACHE_DIR=%LOCALAPPDATA%\VIctory\cache-pareto"
if not defined VICTORY_CACHE_DIR set "VICTORY_CACHE_DIR=%exe_dir%cache-pareto"
if not defined VICTORY_LOG if not defined RUST_LOG set "VICTORY_LOG=warn"
"%runner%" run "%VICTORY_GAME_DIR%\%VICTORY_GAME_EXE%" --console series-s --execute
exit /b %errorlevel%

:get_dlss
set "dlss_ps=%~dp0get-nvidia-dlss.ps1"
if not exist "%dlss_ps%" set "dlss_ps=%~dp0scripts\get-nvidia-dlss.ps1"
if not exist "%dlss_ps%" echo get-nvidia-dlss.ps1 is not next to this file. 1>&2
if not exist "%dlss_ps%" exit /b 2
set "exe_dir=%~dp0"
if not exist "%~dp0victory.exe" if not exist "%~dp0victory-3d-rtx-pareto-residency.exe" if exist "%~dp0target\release\victory.exe" set "exe_dir=%~dp0target\release\"
if not defined VICTORY_HOME if not defined VICTORY_CACHE_DIR set "VICTORY_HOME=%exe_dir%victory-home"
powershell -NoProfile -ExecutionPolicy Bypass -File "%dlss_ps%" -Yes -ExeDir "%exe_dir%."
exit /b %errorlevel%
