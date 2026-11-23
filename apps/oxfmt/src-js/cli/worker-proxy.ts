import Tinypool from "tinypool";
import { toFormatFileResult, toNullable } from "../libs/napi-callbacks";
import type { FormatFileResult } from "../libs/napi-callbacks";
import type {
  FormatFileParam,
  FormatEmbeddedDocParam,
  SortTailwindClassesArgs,
} from "../libs/apis";

import type { Tracked } from "../cli-worker";
import type { Reads } from "./track-reads";

// Worker pool for parallel Prettier formatting
let pool: Tinypool | null = null;
let poolSize: number | null = null;

export async function initExternalServices(numThreads: number): Promise<void> {
  // In LSP mode, this can be called repeatedly for the lifetime of the process.
  // e.g. on every workspace folder build, config-triggered rebuild, etc
  // The process-wide pool must never be recreated or destroyed on re-init:
  // that leaks the previous workers, and other workspace folders may have formats in-flight.
  // (https://github.com/oxc-project/oxc/issues/24147)
  // NOTE: `numThreads` never changes within a single session, so the first value wins.
  poolSize ??= numThreads;
}

// Create the pool lazily on first use,
// so runs that never send a task to the pool spawn no workers at all.
// (e.g. Rust-tier files only, without Tailwind sorting)
async function getPool(): Promise<Tinypool> {
  // Rust always calls `initExternalServices` before formatting, so this is defensive.
  if (poolSize === null) throw new Error("External services are not initialized");

  // NOTE: Default `runtime: worker_threads` requires napi >= 3.16.1,
  // older versions deadlock when a worker loads the addon during `block_on`. (napi-rs/napi-rs#3670)
  pool ??= new Tinypool({
    filename: new URL("./cli-worker.js", import.meta.url).href,
    // Spawn workers on demand up to `poolSize`, and keep them alive for later tasks.
    // - Each worker loads its own copy of JS deps (e.g. Prettier, Tailwind sorter),
    //   and many runs send only a few tasks to the pool
    // - The default `idleTimeout: 0` removes idle workers above `minThreads` right away
    // - Unlike Piscina, Tinypool does not special-case `Infinity` and passes it to `setTimeout` as is,
    //   where Node.js replaces it with 1 ms, so use the max delay `setTimeout` accepts instead
    minThreads: 1,
    maxThreads: poolSize,
    idleTimeout: 2 ** 31 - 1,
  });
  return pool;
}

// ---

/**
 * What the workers read for the calls so far, with `--cache`:
 * `undefined` until tracking starts, `null` once a worker couldn't record its reads.
 */
let reads: { files: Set<string>; dirs: Set<string> } | null | undefined;

/**
 * Have the workers report what they read for each call, so `--cache` can keep a result that
 * called into them until one of those files changes. Call before the first call.
 */
export function trackWorkerReads(): void {
  if (reads !== undefined) return;
  reads = { files: new Set(), dirs: new Set() };
  // Workers inherit the environment: they record reads from their start.
  process.env.OXFMT_CACHE_TRACK_READS = "1";
}

/**
 * What the workers read so far, or `null` when they can't say.
 */
export function workerReads(): Reads | null {
  return reads ? { files: [...reads.files], dirs: [...reads.dirs] } : null;
}

async function run<T>(task: object, name: string): Promise<T> {
  const pool = await getPool();
  if (reads === undefined) return pool.run(task, { name });
  const out: Tracked<T> = await pool.run(task, { name });
  if (out.reads === null) reads = null;
  else if (reads) {
    for (const file of out.reads.files) reads.files.add(file);
    for (const dir of out.reads.dirs) reads.dirs.add(dir);
  }
  if (!out.ok) throw out.error;
  return out.value;
}

export async function disposeExternalServices(): Promise<void> {
  await pool?.destroy();
  pool = null;
  poolSize = null;
}

// ---

export function formatFile(
  options: FormatFileParam["options"],
  code: string,
): Promise<FormatFileResult> {
  return toFormatFileResult(run({ options, code } satisfies FormatFileParam, "formatFile"));
}

// ---

export function formatEmbeddedDoc(
  options: FormatEmbeddedDocParam["options"],
  code: string,
): Promise<string | null> {
  return toNullable(run({ options, code } satisfies FormatEmbeddedDocParam, "formatEmbeddedDoc"));
}

export function sortTailwindClasses(
  options: SortTailwindClassesArgs["options"],
  classes: string[],
): Promise<string[] | null> {
  return toNullable(
    run({ classes, options } satisfies SortTailwindClassesArgs, "sortTailwindClasses"),
  );
}
