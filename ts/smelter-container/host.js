import { readFile } from "node:fs/promises";

const wasmPath = process.argv[2] || "./main.wasm";
const wasmBytes = await readFile(wasmPath);
const module = await WebAssembly.compile(wasmBytes);

let instance;

const importObject = {
  "$root": {
    "console-log": (ptr, len) => {
      const memory = instance.exports.memory;
      const bytes = new Uint8Array(memory.buffer, ptr, len);
      const msg = new TextDecoder().decode(bytes);
      console.log("[plugin]", msg);
    },
    "console-warn": (ptr, len) => {
      const memory = instance.exports.memory;
      const bytes = new Uint8Array(memory.buffer, ptr, len);
      const msg = new TextDecoder().decode(bytes);
      console.warn("[plugin]", msg);
    },
    "console-error": (ptr, len) => {
      const memory = instance.exports.memory;
      const bytes = new Uint8Array(memory.buffer, ptr, len);
      const msg = new TextDecoder().decode(bytes);
      console.error("[plugin]", msg);
    },
  },
};

instance = await WebAssembly.instantiate(module, importObject);
instance.exports._start();
