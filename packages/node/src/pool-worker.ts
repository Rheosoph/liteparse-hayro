// Entry point for pool worker processes, forked by pool.ts.

import { native, type LiteParseNative } from "./native.js";
import { toParseResult } from "./lib.js";

// A worker must never outlive its pool: when the parent exits (or kills the
// IPC channel), shut down instead of lingering as an orphan.
process.on("disconnect", () => process.exit(0));

/** IPC deserialization yields plain Uint8Arrays; the native API wants Buffers. */
function asBuffer(data: Buffer | Uint8Array): Buffer {
  return Buffer.isBuffer(data)
    ? data
    : Buffer.from(data.buffer, data.byteOffset, data.byteLength);
}

type WorkerMessage =
  | { type: "init"; config: Record<string, unknown> }
  | { type: "parse"; payload: string | Uint8Array }
  | { type: "stop" };

let parser: LiteParseNative | null = null;

process.on("message", async (msg: WorkerMessage) => {
  switch (msg.type) {
    case "init": {
      try {
        parser = new native.LiteParse(msg.config);
      } catch (e) {
        process.send!({
          type: "initError",
          message: e instanceof Error ? e.message : String(e),
        });
        process.exit(1);
      }
      process.send!({ type: "ready" });
      return;
    }
    case "parse": {
      if (parser === null) {
        process.send!({ type: "err", message: "worker not initialized" });
        return;
      }
      try {
        const input =
          typeof msg.payload === "string" ? msg.payload : asBuffer(msg.payload);
        const result = await parser.parse(input);
        process.send!({ type: "ok", result: toParseResult(result) });
      } catch (e) {
        process.send!({
          type: "err",
          message: e instanceof Error ? e.message : String(e),
        });
      }
      return;
    }
    case "stop":
      process.exit(0);
  }
});
