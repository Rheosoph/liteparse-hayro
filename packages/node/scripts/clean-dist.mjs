// Clears dist/ before tsup runs.
//
// tsup's own `clean` option always wipes the whole outDir, which races when the
// lib and cli configs build concurrently. Cleaning once up front is deterministic.
import { rm } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const packageDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
await rm(join(packageDir, "dist"), { recursive: true, force: true });
