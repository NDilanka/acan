@echo off
rem Starts Acan on the git repository saved last time. The first run asks for the repository folder
rem and saves it under %LOCALAPPDATA%\Acan.
setlocal EnableDelayedExpansion
set "SAVED=%LOCALAPPDATA%\Acan\repo.txt"
if not exist "%SAVED%" (
  echo Acan works on a git repository. Type the folder of your repository.
  set /p "NEW=Repository folder: "
  if not exist "%LOCALAPPDATA%\Acan" mkdir "%LOCALAPPDATA%\Acan"
  > "%SAVED%" echo(!NEW!
)
set "REPO="
set /p REPO=<"%SAVED%"
if not exist "%REPO%\.git" (
  echo "%REPO%" is not a git repository.
  echo To choose another folder, delete "%SAVED%" and start Acan again.
  pause
  exit /b 1
)
"%~dp0acan.exe" serve --repo "%REPO%"
if errorlevel 1 pause
endlocal
