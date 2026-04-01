#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="$SCRIPT_DIR/smelter-plugin"
REPO_ROOT="$SCRIPT_DIR/../.."

# Build the plugin
cargo build --manifest-path "$PLUGIN_DIR/Cargo.toml" --target=wasm32-wasip2 --release

# Initialize the plugin
javy init-plugin "$REPO_ROOT/target/wasm32-wasip2/release/smelter_plugin.wasm" -o "$PLUGIN_DIR/plugin.wasm"

# Build main.wasm with the plugin
javy build "$SCRIPT_DIR/smelter.js" -o "$SCRIPT_DIR/main.wasm" -C "plugin=$PLUGIN_DIR/plugin.wasm"
