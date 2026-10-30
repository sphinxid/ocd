// Plugin module loading (port of packages/core/src/plugin/module.ts `load`).
//
// Rust resolves configuration, policy and npm installation; the sidecar resolves entrypoints with
// upstream's `Host.resolve`, imports the module (local sources through upstream's fingerprinting
// `createPluginSources`, so edited local plugins re-import on the next activation), validates the
// default export, and wraps Promise plugins with upstream's `fromPromise` adapter.

import { Host } from "@opencode/plugin/host"
import { createPluginSources } from "@opencode/plugin/source"
import { fromPromise } from "@opencode/plugin/promise/adapter"
import type { Plugin } from "@opencode/plugin/effect/plugin"
import { Effect, Schema } from "effect"
import { createHash } from "node:crypto"
import path from "node:path"
import { readFile, stat } from "node:fs/promises"
import { fileURLToPath, pathToFileURL } from "node:url"
import type { Generation } from "./location.js"
import { log } from "./peer.js"

export interface Operation {
  readonly type: "add"
  readonly target: string
  readonly options?: Record<string, unknown>
  readonly mtime?: number
}

export interface Installed {
  readonly directory: string
  readonly name: string
  readonly version?: string
  readonly revision?: string
}

export class LoadError extends Error {}

// `GuiBundle.fields.options` (@opencode/schema/plugin): the options a `./gui-experimental` bundle receives.
const GuiOptions = Schema.Record(Schema.String, Schema.Json)

// `Hash.sha256` (@opencode/util/hash).
const sha256 = (input: string | Buffer) => createHash("sha256").update(input).digest("hex")

const Module = Schema.Struct({
  default: Schema.Union([
    Schema.Struct({
      id: Schema.String,
      effect: Schema.declare<Plugin["effect"]>((input): input is Plugin["effect"] => typeof input === "function"),
    }),
    Schema.Struct({
      id: Schema.String,
      setup: Schema.declare<(context: unknown) => unknown>(
        (input): input is (context: unknown) => unknown => typeof input === "function",
      ),
    }),
  ]),
})

export function makeLoader(watch: (file: string) => void) {
  const sources = createPluginSources(async (file) => watch(file))
  const generations = new Map<string, Generation>()

  const key = (target: string, revision: string) => `${target}\u0000${revision}`

  const loadServer = async (entrypoint: string, local: boolean, revision: string | undefined) => {
    const loaded = await (local
      ? sources.read(entrypoint)
      : Host.load(entrypoint).then((module) => ({ module, version: revision })))
    let value: any
    try {
      value = Schema.decodeUnknownSync(Module)(loaded.module).default
    } catch {
      throw new LoadError("Plugin must export a default definition with an id and an effect or setup function.")
    }
    const plugin: Plugin = "effect" in value ? value : fromPromise(value)
    return { plugin, version: loaded.version }
  }

  const load = async (operation: Operation, installed: Installed | undefined) => {
    const local = path.isAbsolute(operation.target)
    // Legacy auto-discovery still admits standalone server sources. Configured local plugins always arrive here
    // as directories.
    const entrypoints: Host.Entrypoints =
      local && (await stat(operation.target)).isFile()
        ? { server: pathToFileURL(operation.target).href }
        : Host.resolve(installed ?? { directory: operation.target })
    const entrypoint = entrypoints.server
    // A TUI-only package is not the server's to list; the TUI loads it from its own config.
    const found = entrypoint || entrypoints.gui
    if (!found) throw new LoadError(`Plugin entrypoint not found: ${operation.target}`)
    log("info", "loading plugin", { id: operation.target, entrypoint: found })
    // A package with only a GUI part (and maybe a TUI part) has nothing to run here: its id is its package name.
    const server = entrypoint ? await loadServer(entrypoint, local, installed?.revision) : undefined
    const plugin: Plugin = server?.plugin ?? {
      id: Host.packageID(installed ?? { directory: operation.target }),
      effect: () => Effect.void,
    }
    const revision = JSON.stringify([operation, server ? server.version : installed?.revision])
    // A local plugin's `./gui-experimental` bundle is outside the server entry's import graph: watch it, and let
    // its content change the GUI's revision.
    const gui = local && entrypoints.gui ? fileURLToPath(entrypoints.gui) : undefined
    if (gui) watch(gui)
    const bundle = gui ? sha256(await readFile(gui)) : undefined
    // The GUI receives the same options as the server entry; they cross to the app, so they must be JSON.
    let guiOptions: Record<string, unknown> = {}
    if (entrypoints.gui) {
      try {
        guiOptions = Schema.decodeUnknownSync(GuiOptions)(operation.options) as Record<string, unknown>
      } catch {
        throw new LoadError("Plugin options must be JSON to reach its ./gui-experimental")
      }
    }
    const generation: Generation = {
      id: plugin.id,
      target: operation.target,
      features: {
        ...(entrypoint ? { server: true as const } : {}),
        ...(entrypoints.tui ? { tui: true as const } : {}),
        ...(entrypoint && entrypoints.rpc ? { rpc: true as const } : {}),
        ...(entrypoints.gui ? { gui: true as const } : {}),
      },
      gui: entrypoints.gui
        ? { file: entrypoints.gui, revision: sha256(JSON.stringify([revision, bundle])), options: guiOptions }
        : undefined,
      revision,
      // A local plugin's directory (or legacy standalone file), where clients resolve its other parts.
      source: local
        ? { type: "local" as const, path: operation.target }
        : {
            type: "package" as const,
            target: operation.target,
            ...(installed?.version ? { version: installed.version } : {}),
          },
      effect: (host) => plugin.effect({ ...host, options: operation.options ?? {} }) as any,
    }
    generations.set(key(generation.target, generation.revision), generation)
    return generation
  }

  return {
    /** Returns `{id, revision, features, source}` or `{error, ref}` like the supervisor's catch. */
    load: async (operation: Operation, installed: Installed | undefined) => {
      try {
        const generation = await load(operation, installed)
        return {
          id: generation.id,
          revision: generation.revision,
          features: generation.features,
          source: generation.source,
        }
      } catch (cause) {
        const ref = `err_${crypto.randomUUID().slice(0, 8)}`
        log("warn", "failed to load plugin", {
          target: operation.target,
          ref,
          cause: cause instanceof Error ? (cause.stack ?? cause.message) : String(cause),
        })
        return { error: cause instanceof LoadError ? cause.message : "Plugin failed to load", ref }
      }
    },
    get: (target: string, revision: string) => generations.get(key(target, revision)),
    dispose: () => sources.dispose(),
  }
}
