import * as nodeModule from "node:module";
import { fileURLToPath } from "node:url";
import { importJsConfig, loadViteConfigField } from "@oxapps/shared";

/**
 * Load and validate a standard oxfmt JS/TS config file.
 * The default export must be a plain object containing oxfmt options.
 *
 * @param path - Absolute path to the JavaScript/TypeScript config file
 * @returns Config object
 */
export function loadJsConfig(path: string): Promise<object> {
  return importJsConfig(path, Date.now());
}

/**
 * Load a Vite+ config file (`vite.config.*`) via `vite-plus` and extract the `.fmt` field.
 *
 * @param path - Absolute path to the Vite config file
 * @returns Config object from `.fmt` field, or `null` to signal "skip"
 */
export async function loadVitePlusConfig(path: string): Promise<object | null> {
  return loadViteConfigField(path, "fmt", (files) => {
    for (const file of files) loaded?.add(file);
  });
}

// ---

/**
 * Files Node loaded while evaluating JS/TS configs, for `--cache`:
 * `undefined` until tracking starts, `null` when this Node.js can't report them.
 */
let loaded: Set<string> | null | undefined;

/**
 * Start recording every file Node loads from now on (`module.registerHooks`, Node.js 22.15+ / 23.5+),
 * so `--cache` can keep a config's value until one of them changes.
 * Call before any config is loaded.
 */
export function trackJsConfigDependencies(): void {
  if (loaded !== undefined) return;
  // Not a named import: older Node.js has no such export, which would fail at link time.
  const { registerHooks } = nodeModule as Partial<typeof nodeModule>;
  if (typeof registerHooks !== "function") {
    loaded = null;
    return;
  }
  const files = new Set<string>();
  loaded = files;
  registerHooks({
    load(url, context, nextLoad) {
      if (url.startsWith("file:")) files.add(fileURLToPath(url));
      return nextLoad(url, context);
    },
  });
}

/**
 * The files recorded since `trackJsConfigDependencies()`, or `null` when they can't be known.
 */
export async function jsConfigDependencies(): Promise<string[] | null> {
  return loaded ? [...loaded] : null;
}
