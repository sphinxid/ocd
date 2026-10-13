// ocd plugin sidecar entrypoint. Speaks newline-delimited JSON-RPC with the Rust server on
// stdin/stdout; logs go to stderr as JSON lines (`{"$log": level, "message": ...}`).

import { ensurePluginRuntime } from "@opencode/plugin/runtime"
import { format } from "node:util"
import { makeLoader, type Installed, type Operation } from "./loader.js"
import { LocationRuntime, type Generation } from "./location.js"
import { isDomain } from "./domains.js"
import { log, logError, Peer, RemoteError } from "./peer.js"
import { AISDKService, initFailure, stream as aisdkStream } from "./aisdk.js"

export const PROTOCOL = 1

// Plugins share this bundle's `effect` and `@opencode/plugin` instances (upstream runtime.ts).
ensurePluginRuntime()

// stdout carries the protocol: keep plugin console output off it.
const stderr = (...args: unknown[]) => {
  try {
    process.stderr.write(format(...args) + "\n")
  } catch {}
}
console.log = stderr
console.info = stderr
console.debug = stderr
console.warn = stderr
console.trace = stderr
console.dir = stderr

process.on("uncaughtException", (error) => logError("uncaught exception in plugin host", error))
process.on("unhandledRejection", (error) => logError("unhandled rejection in plugin host", error))

const writeOut = (line: string) => {
  process.stdout.write(line)
}

const peer = new Peer(writeOut)
let app: unknown = { name: "opencode", version: "0.0.0", channel: "local" }
const locations = new Map<string, LocationRuntime>()
let locationCounter = 0

const loader = makeLoader((file) => peer.notify("watch", { file }))

const io = {
  request: (method: string, params: unknown, signal?: AbortSignal) => peer.request(method, params, signal),
  notify: (method: string, params: unknown) => peer.notify(method, params),
}

// One AI SDK service per location (upstream `AISDK` is a location node), shared by the location's
// plugin runtime (`ctx.aisdk.hook`) and the bridge requests. Locations without plugins get one too.
const aisdk = new Map<string, AISDKService>()
function aisdkService(key: string) {
  let service = aisdk.get(key)
  if (!service) {
    service = new AISDKService(io, () => app)
    aisdk.set(key, service)
  }
  return service
}

function location(key: string) {
  const found = locations.get(key)
  if (!found) throw new Error(`Unknown plugin location: ${key}`)
  return found
}

peer.handle("initialize", (params: { app?: unknown; protocol?: number }) => {
  if (params?.app) app = params.app
  return { protocol: PROTOCOL, bun: Bun.version, pid: process.pid }
})

peer.handle("module.load", (params: { operation: Operation; install?: Installed }) =>
  loader.load(params.operation, params.install ?? undefined),
)

peer.handle(
  "location.activate",
  async (params: {
    location: string
    ref: { directory: string; workspaceID?: string }
    info: unknown
    plugins: Array<{ target: string; revision: string }>
    failures?: unknown[]
  }) => {
    let runtime = locations.get(params.location)
    if (!runtime) {
      const key = params.location
      runtime = new LocationRuntime(key, params.ref, params.info, app, io, `${++locationCounter}.`, () => aisdkService(key))
      locations.set(params.location, runtime)
    }
    const definitions: Generation[] = []
    for (const item of params.plugins) {
      const generation = loader.get(item.target, item.revision)
      if (!generation) throw new Error(`Plugin module is not loaded: ${item.target}`)
      definitions.push(generation)
    }
    const inventory = await runtime.activate(definitions, params.failures ?? [])
    return {
      inventory,
      hooks: runtime.hookList(),
      domains: runtime.transformDomains(),
      rpc: runtime.rpc.ids(),
    }
  },
)

peer.handle("location.close", async (params: { location: string }) => {
  aisdk.delete(params.location)
  for (const slot of Array.from(inputs.keys())) {
    if (slot.startsWith(`${params.location}\u0000`)) inputs.delete(slot)
  }
  const runtime = locations.get(params.location)
  if (!runtime) return null
  locations.delete(params.location)
  await runtime.close()
  return null
})

const withEnabled = (service: AISDKService, enabled?: string[] | null) => {
  if (enabled) service.enabled = new Set(enabled)
  return service
}

peer.handle("aisdk.load", async (params: { location: string; model: unknown; enabled?: string[] }) => {
  try {
    await withEnabled(aisdkService(params.location), params.enabled).language(params.model)
  } catch (error) {
    throw initFailure(error)
  }
  return {}
})

peer.handle(
  "aisdk.stream",
  (
    params: { location: string; model: unknown; options: unknown; stream: number; http?: { response?: boolean }; enabled?: string[] },
    context,
  ) => aisdkStream(withEnabled(aisdkService(params.location), params.enabled), io, params, context.signal),
)

peer.handle("aisdk.close", (params: { location: string }) => {
  aisdk.delete(params.location)
  return null
})

peer.handle("location.list", (params: { location: string }) => location(params.location).list())

peer.handle("hook.trigger", (params: { location: string; key: string; event: unknown }, context) =>
  location(params.location)
    .trigger(params.key, params.event, context.signal)
    .then((event) => ({ event })),
)

// The last keyed transform input per location and domain. The catalog domains send every provider
// record and model, which rarely change between rebuilds: the core sends such a value once with a
// `key` and later names it by that key alone (`transform.input` asks it to send the value again).
// Transforms never mutate an input value, so it can be reused as is.
const inputs = new Map<string, { key: string; value: unknown }>()

peer.handle("transform", (params: { location: string; domain: string; value?: unknown; key?: string }) => {
  if (!isDomain(params.domain)) throw new Error(`Unknown transform domain: ${params.domain}`)
  let value = params.value
  if (params.key !== undefined) {
    const slot = `${params.location}\u0000${params.domain}`
    if (value === undefined) {
      const cached = inputs.get(slot)
      if (!cached || cached.key !== params.key) {
        throw Object.assign(new Error(`No ${params.domain} transform input ${params.key}`), { type: "transform.input" })
      }
      value = cached.value
    } else {
      inputs.set(slot, { key: params.key, value })
    }
  }
  return { value: location(params.location).transform(params.domain, value) }
})

peer.handle("invoke", (params: { location: string; handle: string; args?: unknown[]; scope?: string }, context) =>
  location(params.location).invoke(params.handle, params.args ?? [], { scope: params.scope, signal: context.signal }),
)

peer.handle("scope.close", async (params: { location: string; scope: string }) => {
  await locations.get(params.location)?.closeScope(params.scope)
  return null
})

peer.handle(
  "tool.execute",
  (params: { location: string; handle: string; input: unknown; context: unknown }, context) =>
    location(params.location).executeTool(params.handle, params.input, params.context, context.signal),
)

peer.handle("rpc.call", async (params: { location: string; rpcID: string; method: string; input: unknown }) => {
  const { Effect, Exit, Cause } = await import("effect")
  const exit = await Effect.runPromiseExit(location(params.location).rpc.call(params.rpcID, params.method, params.input))
  if (Exit.isSuccess(exit)) return { output: exit.value ?? null }
  const failure = Cause.squash(exit.cause) as { type?: string; message?: string; data?: unknown }
  throw new RemoteError({
    message: failure?.message ?? "RPC call failed",
    type: failure?.type ?? "rpc.internal",
    ...(failure?.data === undefined ? {} : { data: failure.data }),
  })
})

peer.on("event", (params: { location?: string; event: unknown }) => {
  if (params.location !== undefined && params.location !== null) {
    locations.get(params.location)?.deliver(params.event)
    return
  }
  for (const runtime of locations.values()) runtime.deliver(params.event)
})

let stopping = false
async function shutdown(code: number) {
  if (stopping) return
  stopping = true
  for (const [key, runtime] of Array.from(locations)) {
    locations.delete(key)
    await runtime.close().catch((error) => logError("failed to close plugin location", error))
  }
  loader.dispose()
  peer.close()
  setTimeout(() => process.exit(code), 0)
}

peer.handle("shutdown", async () => {
  setTimeout(() => void shutdown(0), 0)
  return null
})

async function readInput() {
  const decoder = new TextDecoder()
  let buffer = ""
  for await (const chunk of Bun.stdin.stream()) {
    buffer += decoder.decode(chunk, { stream: true })
    let index = buffer.indexOf("\n")
    while (index !== -1) {
      const line = buffer.slice(0, index)
      buffer = buffer.slice(index + 1)
      peer.receive(line)
      index = buffer.indexOf("\n")
    }
  }
  if (buffer.trim()) peer.receive(buffer)
  // The server closed our stdin: it is gone or wants us gone.
  await shutdown(0)
}

log("debug", "plugin host started", { pid: process.pid, bun: Bun.version })
void readInput()
