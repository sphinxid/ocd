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
import { Schema } from "effect"
import path from "node:path"
import { stat } from "node:fs/promises"
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

  const load = async (operation: Operation, installed: Installed | undefined) => {
    const local = path.isAbsolute(operation.target)
    const entrypoints: Host.Entrypoints =
      local && (await stat(operation.target)).isFile()
        ? { server: pathToFileURL(operation.target).href }
        : Host.resolve(installed ?? { directory: operation.target })
    const entrypoint = entrypoints.server
    if (!entrypoint) throw new LoadError(`Plugin entrypoint not found: ${operation.target}`)
    log("info", "loading plugin", { id: operation.target, entrypoint })
    const loaded = await (local
      ? sources.read(entrypoint)
      : Host.load(entrypoint).then((module) => ({ module, version: installed?.revision })))
    let value: any
    try {
      value = Schema.decodeUnknownSync(Module)(loaded.module).default
    } catch {
      throw new LoadError("Plugin must export a default definition with an id and an effect or setup function.")
    }
    const plugin: Plugin = "effect" in value ? value : fromPromise(value)
    const generation: Generation = {
      id: plugin.id,
      target: operation.target,
      features: {
        ...(entrypoints.tui ? { tui: true as const } : {}),
        ...(entrypoints.rpc ? { rpc: true as const } : {}),
      },
      revision: JSON.stringify([operation, loaded.version]),
      source: local
        ? { type: "local" as const, path: fileURLToPath(entrypoint) }
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
