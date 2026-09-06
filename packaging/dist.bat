@echo off
rem Build Windows x64 distribution: dist\tonight-win64\ + dist\tonight-windows-x64.zip
rem Usage: double-click, or run packaging\dist.bat from repo root.
rem NOTE: keep this file ASCII-only and CRLF; cmd mis-parses UTF-8 text after chcp.
setlocal
cd /d %~dp0..

echo [1/5] cargo build --release ...
cargo build --release
if errorlevel 1 (
    echo Build FAILED.
    exit /b 1
)

set DIST=dist\tonight-win64
set ZIP=dist\tonight-windows-x64.zip
rem User state (data\ .env config.toml) must NEVER enter the zip: a shipped DB
rem would carry onboarded=1 and the onboarding wizard would never open for users.
rem Keep it aside, zip a clean tree, then put it back into the local folder.
set KEEP=%TEMP%\tonight-dist-keep-%RANDOM%
mkdir "%KEEP%" 2>nul
if exist "%DIST%\.env" copy /y "%DIST%\.env" "%KEEP%\" >nul
if exist "%DIST%\config.toml" copy /y "%DIST%\config.toml" "%KEEP%\" >nul
if exist "%DIST%\data" robocopy "%DIST%\data" "%KEEP%\data" /e >nul

echo [2/5] assemble %DIST% ...
if exist "%DIST%" rmdir /s /q "%DIST%"
mkdir "%DIST%\web"
copy /y target\release\tonight.exe "%DIST%\" >nul
copy /y packaging\start-tonight.bat "%DIST%\start.bat" >nul
copy /y packaging\readme-dist.txt "%DIST%\README.txt" >nul
copy /y .env.example "%DIST%\" >nul
xcopy /e /i /y web "%DIST%\web" >nul

echo [3/5] compress zip ...
powershell -NoProfile -Command "Compress-Archive -Path 'dist\tonight-win64' -DestinationPath '%ZIP%' -Force"
if errorlevel 1 (
    echo Compress FAILED ^(dir is ready: %DIST%^)
    exit /b 1
)

echo [4/5] verify zip has no user state ...
powershell -NoProfile -Command "Add-Type -AssemblyName 'System.IO.Compression.FileSystem'; $z=[IO.Compression.ZipFile]::OpenRead('%ZIP%'); $bad=$z.Entries | Where-Object { $_.FullName -match '(^|\\)(data\\|\.env$|config\.toml$)' }; $n=$z.Entries.Count; $z.Dispose(); if ($bad) { $bad | ForEach-Object { Write-Output ('BAD ENTRY: ' + $_.FullName) }; exit 1 } else { Write-Output ('zip clean, ' + $n + ' entries') }"
if errorlevel 1 (
    echo Zip verification FAILED - refusing to ship user state.
    exit /b 1
)

echo [5/5] restore local user state into %DIST% ...
if exist "%KEEP%\.env" move /y "%KEEP%\.env" "%DIST%\.env" >nul
if exist "%KEEP%\config.toml" move /y "%KEEP%\config.toml" "%DIST%\config.toml" >nul
if exist "%KEEP%\data" robocopy "%KEEP%\data" "%DIST%\data" /e >nul
rmdir /s /q "%KEEP%" 2>nul

echo Done.
echo    output dir : %DIST%
echo    output zip : %ZIP%
endlocal
