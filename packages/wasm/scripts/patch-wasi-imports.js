#!/usr/bin/env node
/**
 * Post-build script that patches the wasm-bindgen generated JS glue to
 * provide stub implementations of WASI preview1 syscalls and any remaining
 * "env" imports.
 *
 * The WASI syscalls (wasi_snapshot_preview1::*) are baked into the WASM binary
 * by wasi-libc and cannot be resolved at Rust link time. Instead we inject
 * no-op / error-returning stubs at the JS level so the WASM module can
 * instantiate in the browser.
 */

import { readFileSync, writeFileSync } from "fs";
import { join, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const GLUE_PATH = join(__dirname, "..", "pkg", "liteparse_wasm.js");

let source = readFileSync(GLUE_PATH, "utf-8");

// WASI errno constants
const ERRNO_SUCCESS = 0;
const ERRNO_BADF = 8;
const ERRNO_NOENT = 44;
const ERRNO_NOSYS = 52;

// The stubs object we'll inject into the JS source
const STUBS_CODE = `
// --- WASI / env stubs (injected by patch-wasi-imports.js) ---
const __wasi_stubs = {
  // wasi_snapshot_preview1
  environ_sizes_get(count, buf_size) {
    const view = new DataView(wasm.memory.buffer);
    view.setUint32(count, 0, true);
    view.setUint32(buf_size, 0, true);
    return ${ERRNO_SUCCESS};
  },
  environ_get() { return ${ERRNO_SUCCESS}; },
  clock_time_get(clock_id, precision, time) {
    const view = new DataView(wasm.memory.buffer);
    view.setBigUint64(time, BigInt(0), true);
    return ${ERRNO_SUCCESS};
  },
  fd_close(fd) { return fd <= 2 ? ${ERRNO_SUCCESS} : ${ERRNO_BADF}; },
  fd_fdstat_get(fd, stat) {
    if (fd > 2) return ${ERRNO_BADF};
    // Report stdio fds as character devices (filetype=2, no special flags)
    const view = new DataView(wasm.memory.buffer);
    view.setUint8(stat, 2);       // filetype = CHARACTER_DEVICE
    view.setUint16(stat + 2, 0, true); // fdflags = 0
    view.setBigUint64(stat + 8, BigInt(0), true);  // rights_base
    view.setBigUint64(stat + 16, BigInt(0), true); // rights_inheriting
    return ${ERRNO_SUCCESS};
  },
  fd_fdstat_set_flags() { return ${ERRNO_NOSYS}; },
  fd_filestat_get() { return ${ERRNO_BADF}; },
  fd_filestat_set_size() { return ${ERRNO_BADF}; },
  fd_prestat_get() { return ${ERRNO_BADF}; },
  fd_prestat_dir_name() { return ${ERRNO_BADF}; },
  fd_read() { return ${ERRNO_BADF}; },
  fd_readdir() { return ${ERRNO_BADF}; },
  fd_seek() { return ${ERRNO_NOSYS}; },
  fd_sync() { return ${ERRNO_NOSYS}; },
  fd_write(fd, iovs, iovs_len, nwritten) {
    // Support stdout (1) and stderr (2) — compute total bytes and discard
    if (fd !== 1 && fd !== 2) return ${ERRNO_BADF};
    const view = new DataView(wasm.memory.buffer);
    const mem = new Uint8Array(wasm.memory.buffer);
    let total = 0;
    for (let i = 0; i < iovs_len; i++) {
      const ptr = view.getUint32(iovs + i * 8, true);
      const len = view.getUint32(iovs + i * 8 + 4, true);
      // Optionally log stderr to console
      if (fd === 2 && len > 0) {
        try {
          const text = new TextDecoder().decode(mem.subarray(ptr, ptr + len));
          console.warn("[pdfium]", text);
        } catch (_) {}
      }
      total += len;
    }
    view.setUint32(nwritten, total, true);
    return ${ERRNO_SUCCESS};
  },
  path_filestat_get() { return ${ERRNO_NOENT}; },
  path_open() { return ${ERRNO_NOENT}; },
  path_remove_directory() { return ${ERRNO_NOSYS}; },
  path_unlink_file() { return ${ERRNO_NOSYS}; },
  proc_exit(code) { throw new Error("WASI proc_exit called with code " + code); },
};

const __env_stubs = {
  // __c_longjmp is a WASM exception handling tag used for setjmp/longjmp.
  // It must be a WebAssembly.Tag, not a function.
  __c_longjmp: new WebAssembly.Tag({ parameters: ["i32"] }),
};
// --- end stubs ---
`;

// 1. Remove the top-level `import ... from "env"` and `import ... from "wasi_snapshot_preview1"`
//    These are ES module imports that can't resolve in a browser.
source = source.replace(/^import \* as import\d+ from "(?:env|wasi_snapshot_preview1)";?\n/gm, "");

// 2. Inject stubs before the __wbg_get_imports function
source = source.replace(
  "function __wbg_get_imports() {",
  STUBS_CODE + "\nfunction __wbg_get_imports() {"
);

// 3. In the return object of __wbg_get_imports, replace the env/wasi entries
//    with our stubs objects. The generated code emits a run of duplicate
//    "wasi_snapshot_preview1": importN keys (and, depending on the pdfium build,
//    an "env": importN key). The order and presence of these vary between
//    wasm-bindgen versions, so collapse each independently rather than assuming
//    a fixed env-then-wasi layout.
const wasiBefore = source;
source = source.replace(
  /(?:[ \t]*"wasi_snapshot_preview1": import\d+,\n)+/,
  `        "wasi_snapshot_preview1": __wasi_stubs,\n`
);
if (source === wasiBefore) {
  throw new Error(
    "patch-wasi-imports: failed to find wasi_snapshot_preview1 import entries — " +
      "the wasm-bindgen glue layout changed, update this script."
  );
}

// The "env" import only exists when a symbol (e.g. __c_longjmp) is left
// unresolved at link time. When pdfium's libsetjmp.a is linked it disappears,
// so this replacement is best-effort.
source = source.replace(
  /[ \t]*"env": import\d+,\n/,
  `        "env": __env_stubs,\n`
);

writeFileSync(GLUE_PATH, source, "utf-8");
console.log("Patched WASI/env stubs into", GLUE_PATH);
