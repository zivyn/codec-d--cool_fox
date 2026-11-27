@echo off
rem Downloads the AMD files that FSR 3 frame generation needs (about 30 MB) from AMD's own GitHub
rem (the FidelityFX SDK) and puts them where the launcher looks for them (amd-sdk\ in this folder).
rem Run it once. They are AMD's files under AMD's licence, which is saved beside them; this repository
rem does not contain them. FSR 3 is the frame generation for cards without DLSS frame generation.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\get-amd-fsr.ps1" %*
echo.
pause
