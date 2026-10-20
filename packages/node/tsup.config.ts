import { defineConfig } from "tsup";

// Dual ESM/CJS build. `shims: true` is the important bit: native.ts locates the
// .node binary via `import.meta.url`, which has no CommonJS equivalent, so tsup
// rewrites it to a __filename-derived shim in the .cjs output.
// dist/ is cleared once by scripts/clean-dist.mjs rather than tsup's `clean`,
// which wipes the whole outDir and would race the concurrent configs.
const shared = {
  platform: "node",
  target: "node18",
  outDir: "dist",
  sourcemap: true,
  shims: true,
} as const;

export default defineConfig([
  {
    ...shared,
    // The library is the only dual-format entry: `import` gets lib.js,
    // `require()` gets lib.cjs, and dts emits lib.d.ts + lib.d.cts so
    // TypeScript resolves types correctly under both conditions.
    entry: { lib: "src/lib.ts" },
    format: ["esm", "cjs"],
    dts: true,
  },
  {
    ...shared,
    // The bins are launched by Node directly, never require()'d, so ESM only.
    entry: { cli: "src/cli.ts" },
    format: ["esm"],
    dts: false,
  },
  {
    ...shared,
    // Pool worker: forked as a standalone child process by pool.ts, never
    // imported, so it needs its own entry (nothing else would emit the file)
    // and ESM suffices — fork() runs an ESM entry regardless of whether the
    // parent loaded lib.js or lib.cjs. pool.ts resolves the file relative to
    // import.meta.url, which `shims` keeps working from the .cjs build.
    entry: { "pool-worker": "src/pool-worker.ts" },
    format: ["esm"],
    dts: false,
  },
]);
