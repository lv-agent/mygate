#!/usr/bin/env bash
set -euo pipefail

# ── MyGate build script ──────────────────────────────────────
# Produces a self-contained dist/ directory that can be copied
# to any Linux machine (same architecture) and run directly.
#
# Usage:
#   ./build.sh          # debug build
#   ./build.sh release  # optimized release build

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR"
DIST_DIR="$PROJECT_DIR/dist"
BIN_NAME="mygate"
# cr-208: 默认 release 构建, debug 需显式指定 (./build.sh debug)
BUILD_MODE="${1:-release}"

echo "=== MyGate Build ==="
echo "Mode: $BUILD_MODE"

# ── 1. Compile ────────────────────────────────────────────────

if [ "$BUILD_MODE" = "release" ]; then
    echo "[1/4] Compiling (release) ..."
    cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"
    BIN_PATH="$PROJECT_DIR/target/release/$BIN_NAME"
else
    echo "[1/4] Compiling (debug) ..."
    cargo build --manifest-path "$PROJECT_DIR/Cargo.toml"
    BIN_PATH="$PROJECT_DIR/target/debug/$BIN_NAME"
fi

if [ ! -f "$BIN_PATH" ]; then
    echo "ERROR: binary not found at $BIN_PATH"
    exit 1
fi

# ── 2. Clean dist/ ────────────────────────────────────────────

echo "[2/4] Preparing dist/ ..."
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# ── 3. Assemble ───────────────────────────────────────────────

echo "[3/4] Assembling dist/ ..."

# Binary
cp "$BIN_PATH" "$DIST_DIR/$BIN_NAME"
chmod +x "$DIST_DIR/$BIN_NAME"

# Config template — user edits this on the target machine
cp "$PROJECT_DIR/config.example.toml" "$DIST_DIR/config.example.toml"

# Startup helper script
cat > "$DIST_DIR/run.sh" << 'RUNEOF'
#!/usr/bin/env bash
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

# Create config from example if it doesn't exist
if [ ! -f config.toml ]; then
    if [ -f config.example.toml ]; then
        cp config.example.toml config.toml
        echo "Created config.toml from config.example.toml"
        echo "Please edit config.toml with your API keys, then run again."
        exit 0
    else
        echo "ERROR: config.toml not found and no config.example.toml to copy."
        exit 1
    fi
fi

# RUST_LOG: 默认 info，可通过环境变量覆盖（如 RUST_LOG=mygate=debug ./run.sh）
export RUST_LOG="${RUST_LOG:-info,mygate=debug}"

exec ./mygate "$@"
RUNEOF
chmod +x "$DIST_DIR/run.sh"

# ── 4. Report ─────────────────────────────────────────────────

BIN_SIZE=$(du -h "$DIST_DIR/$BIN_NAME" | cut -f1)

echo "[4/4] Done!"
echo ""
echo "dist/"
ls -1 "$DIST_DIR/" | sed 's/^/  /'
echo ""
echo "Binary size: $BIN_SIZE"
echo ""
echo "To deploy: copy dist/ to target machine, edit config.toml, then ./run.sh"
