#!/bin/sh
# Build Linux/macOS distributions:
#   dist/tonight-linux-x64/   -> dist/tonight-linux-x64.tar.gz
#   dist/tonight-macos-x64/   -> dist/tonight-macos-x64.tar.gz
#   dist/tonight-macos-arm64/ -> dist/tonight-macos-arm64.tar.gz
# Requirements: cargo-zigbuild + zig (pip install ziglang) + rustup targets.
# On Windows the project path must NOT contain spaces (zig cc rejects them) —
# this script auto-creates a subst drive X: when needed.
# NOTE: keep this file LF-only (bash chokes on CRLF).
set -e
cd "$(dirname "$0")/.." || exit 1

# ---- Windows + 含空格路径 → subst X: 虚拟盘 ----
case "$(uname -s)" in
    *MINGW*|*MSYS*|*CYGWIN*)
        case "$(pwd -W 2>/dev/null || pwd)" in
            *" "*)
                if [ ! -e /x/Cargo.toml ] && [ ! -e "X:\\Cargo.toml" ]; then
                    wpath="$(pwd -W 2>/dev/null || pwd)"
                    echo "路径含空格，创建 X: 虚拟盘指向 $wpath ..."
                    powershell -NoProfile -Command "subst X: '$wpath'" || {
                        echo "FAILED: 无法创建 X: 虚拟盘。请手动执行: subst X: \"<仓库路径>\" 后在 X:\\ 下重跑本脚本"
                        exit 1
                    }
                fi
                cd /x || exit 1
                ;;
        esac
        ;;
esac

echo "[1/5] cargo zigbuild --release (3 targets) ..."
for t in x86_64-unknown-linux-gnu.2.17 x86_64-apple-darwin aarch64-apple-darwin; do
    cargo zigbuild --release --target "$t" || { echo "Build FAILED for $t"; exit 1; }
done

# 每个包名 -> rust target triple 的映射
name_of() {
    case "$1" in
        x86_64-unknown-linux-gnu*) echo "tonight-linux-x64" ;;
        x86_64-apple-darwin) echo "tonight-macos-x64" ;;
        aarch64-apple-darwin) echo "tonight-macos-arm64" ;;
    esac
}

for t in x86_64-unknown-linux-gnu.2.17 x86_64-apple-darwin aarch64-apple-darwin; do
    name="$(name_of "$t")"
    DIST="dist/$name"
    # glibc 后缀 target（…gnu.2.17）的产物目录不带后缀
    BIN="target/${t%%.*}/release/tonight"
    [ -f "$BIN" ] || { echo "MISSING $BIN"; exit 1; }

    # 用户状态（data\ .env config.toml）绝不能进 tar 包：带状态的 DB 会让
    # 首次引导永不弹出。先挪走，打包校验后再还给本地文件夹。
    KEEP="dist/.keep-$name"
    rm -rf "$KEEP"; mkdir -p "$KEEP"
    [ -f "$DIST/.env" ] && mv "$DIST/.env" "$KEEP/" || true
    [ -f "$DIST/config.toml" ] && mv "$DIST/config.toml" "$KEEP/" || true
    [ -d "$DIST/data" ] && mv "$DIST/data" "$KEEP/data" || true

    echo "[2/5] assemble $DIST ..."
    rm -rf "$DIST"
    mkdir -p "$DIST/web"
    cp "$BIN" "$DIST/tonight"
    cp packaging/start-tonight.sh "$DIST/start.sh"
    chmod +x "$DIST/tonight" "$DIST/start.sh"
    cp packaging/readme-dist.txt "$DIST/README.txt"
    cp .env.example "$DIST/" 
    cp web/*.html web/*.css web/*.js "$DIST/web/"

    echo "[3/5] tar $name ..."
    tar -czf "dist/$name.tar.gz" -C dist "$name"

    echo "[4/5] verify $name.tar.gz has no user state ..."
    if tar -tzf "dist/$name.tar.gz" | grep -E '(^|/)(data/|\.env$|config\.toml$)' > /tmp/tonight-tar-bad.txt; then
        cat /tmp/tonight-tar-bad.txt
        echo "Tar verification FAILED - refusing to ship user state."
        exit 1
    fi

    echo "[5/5] restore local user state into $DIST ..."
    [ -f "$KEEP/.env" ] && mv "$KEEP/.env" "$DIST/.env" || true
    [ -f "$KEEP/config.toml" ] && mv "$KEEP/config.toml" "$DIST/config.toml" || true
    [ -d "$KEEP/data" ] && mv "$KEEP/data" "$DIST/data" || true
    rm -rf "$KEEP"
    echo "    done: dist/$name.tar.gz"
done

echo
echo "All done:"
ls -la dist/*.tar.gz
