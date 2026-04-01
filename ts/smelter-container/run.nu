def main [mode?: string] {
    if $mode == "with_plugin" {
        javy build main.js -o main.wasm -C plugin=./smelter-plugin/plugin.wasm
    } else {
        javy build main.js -o main.wasm
    }
    wasmtime main.wasm
}
