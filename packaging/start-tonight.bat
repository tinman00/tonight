@echo off
rem "tonight" launcher - double-click to start server and open the web UI.
rem Keep this file ASCII-only and CRLF: cmd mis-parses UTF-8 text after chcp.
cd /d %~dp0
echo Starting tonight ... browser will open http://127.0.0.1:8668
echo (help: see README.txt)
tonight.exe serve --open
echo Server stopped.
pause
