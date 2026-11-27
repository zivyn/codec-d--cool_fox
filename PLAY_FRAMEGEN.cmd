@echo off
rem One-click the title on VIctory-NVIDIA (DLSS + frame generation). ASCII only.
rem   PLAY_FRAMEGEN.cmd          menu
rem   PLAY_FRAMEGEN.cmd 1..6     start that mode directly
rem   PLAY_FRAMEGEN.cmd check    only show paths and what is ready
setlocal
set "PS1=%~dp0scripts\play-framegen.ps1"
set "PSH=powershell"
where pwsh >nul 2>nul && set "PSH=pwsh"
if not exist "%PS1%" (
  echo Not found: %PS1%
  pause
  exit /b 1
)
if /i "%~1"=="check" (
  %PSH% -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Check
  pause
  exit /b 0
)
set "PICK=%~1"
if not "%PICK%"=="" goto :run

echo ================================================
echo   the title - VIctory-NVIDIA, DLSS + frame generation
echo   (RTX 50: frame generation 4x automatically)
echo ================================================
echo   1. Series X 1440p + DLSS Quality + frame gen   (recommended)
echo   2. Series X 1440p + DLSS Quality, no frame gen
echo   3. Series S + DLSS Quality + frame gen         (faster, softer)
echo   4. Series X 1440p + DLSS Performance + frame gen
echo   5. EXTREME Series S: DLSS Performance + frame gen + big pools + High priority
echo   6. EXTREME Series X: 1440p DLSS Performance + frame gen + big pools + High priority
echo   Q. Quit
echo.
choice /c 123456Q /n /m "Choose 1-6 or Q: "
if errorlevel 7 goto :end
if errorlevel 6 (set "PICK=6") else if errorlevel 5 (set "PICK=5") else if errorlevel 4 (set "PICK=4") else if errorlevel 3 (set "PICK=3") else if errorlevel 2 (set "PICK=2") else (set "PICK=1")

:run
echo.
%PSH% -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Mode %PICK%

:end
echo.
pause
