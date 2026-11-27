@echo off
rem One-click the title on VIctory-NVIDIA (DLSS + frame generation). ASCII only.
rem   PLAY_FRAMEGEN.bat          menu
rem   PLAY_FRAMEGEN.bat 1..5     start that mode directly
rem   PLAY_FRAMEGEN.bat check    only show paths and what is ready
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
  %PSH% -NoProfile -File "%PS1%" -Check
  pause
  exit /b 0
)
set "PICK=%~1"
if not "%PICK%"=="" goto :run

echo ================================================
echo   the title - VIctory-NVIDIA, DLSS + frame generation
echo ================================================
echo   1. Series X 1440p + DLSS Quality + frame gen 2x   (recommended)
echo   2. Series X 1440p + DLSS Quality, no frame gen
echo   3. Series S + DLSS Quality + frame gen 2x         (faster, softer)
echo   4. Series X 1440p + DLSS Performance + frame gen 2x
echo   5. EXTREME: Series S + DLSS Performance + frame gen 2x + big pools + High priority
echo   Q. Quit
echo.
choice /c 12345Q /n /m "Choose 1-5 or Q: "
if errorlevel 6 goto :end
if errorlevel 5 (set "PICK=5") else if errorlevel 4 (set "PICK=4") else if errorlevel 3 (set "PICK=3") else if errorlevel 2 (set "PICK=2") else (set "PICK=1")

:run
echo.
%PSH% -NoProfile -File "%PS1%" -Mode %PICK%

:end
echo.
pause
