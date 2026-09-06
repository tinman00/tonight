#!/bin/sh
# "tonight" launcher (Linux/macOS) - run with:  sh start.sh
cd "$(dirname "$0")" || exit 1
chmod +x ./tonight 2>/dev/null || true
echo "Starting tonight ... browser will open http://127.0.0.1:8668"
echo "(help: see README.txt)"
./tonight serve --open
echo "Server stopped."
