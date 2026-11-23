// `oxfmt` CLI - Worker Thread Entry Point

import { startTrackingReads, takeReads, type Reads } from "./cli/track-reads";
import type * as Apis from "./libs/apis";

/**
 * A call's outcome with what the worker read for it (since its last call), for `--cache`.
 * A throw is carried as data, so the reads arrive either way.
 */
export type Tracked<T> =
  | { ok: true; value: T; reads: Reads | null }
  | { ok: false; error: unknown; reads: Reads | null };

// With `--cache` (see `cli/worker-proxy.ts`), every call also reports what the worker read,
// `null` when this Node.js can't record it.
const tracking = process.env.OXFMT_CACHE_TRACK_READS === "1" ? startTrackingReads() : undefined;
// Imported once tracking started, so the modules it loads (Prettier among them) are recorded.
const apis = import("./libs/apis");

function tracked<K extends "formatFile" | "formatEmbeddedDoc" | "sortTailwindClasses">(name: K) {
  type Args = Parameters<(typeof Apis)[K]>[0];
  type Value = Awaited<ReturnType<(typeof Apis)[K]>>;
  const call = async (args: Args): Promise<Value> =>
    ((await apis)[name] as (args: Args) => Promise<Value>)(args);
  if (tracking === undefined) return call;
  return async (args: Args): Promise<Tracked<Value>> => {
    try {
      const value = await call(args);
      return { ok: true, value, reads: tracking ? takeReads() : null };
    } catch (error) {
      return { ok: false, error, reads: tracking ? takeReads() : null };
    }
  };
}

// Re-exports core functions for use in `worker_threads`
export const formatFile = tracked("formatFile");
export const formatEmbeddedDoc = tracked("formatEmbeddedDoc");
export const sortTailwindClasses = tracked("sortTailwindClasses");
