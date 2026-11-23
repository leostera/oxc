// The default export: the mutable `fs` object, not its frozen namespace.
import fs from "node:fs";
import * as nodeModule from "node:module";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * What a thread read from the file system, for `--cache`: a result that called into Node
 * (Prettier, the Tailwind sorter) is kept only until one of these changes.
 *
 * - `read`: files loaded as modules, read, or looked up (`stat`, `exists`, `access`, `realpath`), found or not
 * - `listed`: directories listed
 */
export interface Reads {
  files: string[];
  dirs: string[];
}

const read = new Set<string>();
const listed = new Set<string>();

function toPath(p: unknown): string | null {
  if (typeof p === "string") return resolve(p);
  if (p instanceof URL) return p.protocol === "file:" ? fileURLToPath(p) : null;
  if (Buffer.isBuffer(p)) return resolve(p.toString());
  // A file descriptor: its open was recorded.
  return null;
}

type AnyFn = (...args: any[]) => any;

function wrap(target: any, name: string, into: Set<string>): void {
  const original = target[name] as AnyFn | undefined;
  if (typeof original !== "function") return;
  const wrapped = function (this: unknown, ...args: any[]) {
    const path = toPath(args[0]);
    if (path !== null) into.add(path);
    return original.apply(this, args);
  };
  // Keep `fs.realpathSync.native` and friends.
  Object.assign(wrapped, original);
  target[name] = wrapped;
}

/**
 * Start recording what this thread reads: module loads (`module.registerHooks`) and the `fs` calls
 * that read files or look them up. Returns `false` when this Node.js can't (no `module.registerHooks`),
 * and records nothing.
 */
export function startTrackingReads(): boolean {
  const { registerHooks, syncBuiltinESMExports } = nodeModule as Partial<typeof nodeModule>;
  if (typeof registerHooks !== "function" || typeof syncBuiltinESMExports !== "function") {
    return false;
  }
  registerHooks({
    load(url, context, nextLoad) {
      if (url.startsWith("file:")) read.add(fileURLToPath(url));
      return nextLoad(url, context);
    },
  });
  for (const name of [
    "readFileSync",
    "readFile",
    "openSync",
    "open",
    "statSync",
    "stat",
    "lstatSync",
    "lstat",
    "existsSync",
    "exists",
    "accessSync",
    "access",
    "realpathSync",
    "realpath",
    "createReadStream",
  ]) {
    wrap(fs, name, read);
  }
  wrap(fs.realpathSync, "native", read);
  wrap(fs.realpath, "native", read);
  for (const name of ["readFile", "open", "stat", "lstat", "access", "realpath"]) {
    wrap(fs.promises, name, read);
  }
  for (const name of ["readdirSync", "readdir", "opendirSync", "opendir"]) wrap(fs, name, listed);
  for (const name of ["readdir", "opendir"]) wrap(fs.promises, name, listed);
  // Named ESM imports of `node:fs` read the patched functions from now on.
  syncBuiltinESMExports();
  return true;
}

/**
 * What was recorded since the last call, emptied.
 */
export function takeReads(): Reads {
  const reads = { files: [...read], dirs: [...listed] };
  read.clear();
  listed.clear();
  return reads;
}
