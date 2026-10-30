// One server location inside the sidecar: its activated plugins, runtime hooks, transforms, RPC
// registry, function handles and forwarded event stream.
//
// Activation ports packages/core/src/plugin.ts (prefix diff by (id, revision), reverse teardown,
// fallback to the previous activation); hooks port packages/core/src/plugin/hooks.ts; transform
// failures port the State.group behaviour (the plugin is disabled with
// "Plugin disabled after <domain>.transform failed. Check server logs for details.").

import { Location } from "@opencode/schema/location"
import { EventManifest } from "@opencode/schema/event-manifest"
import { Tool } from "@opencode/schema/tool"
import { OpenCodeEvent } from "@opencode/protocol/groups/event"
import { Cause, Effect, Exit, Logger, PubSub, Schema, Scope, Stream } from "effect"
import { execute as executeTool } from "@upstream-core/tool/runtime"
import { fromWire, Handles, toWire, type Remote } from "./codec.js"
import { makeContext } from "./context.js"
import { SPECS, type Codec, type Domain } from "./domains.js"
import { log, RemoteError } from "./peer.js"
import { makeRpc, type RpcFailure } from "./rpc.js"
import { pluginHook, type AISDKService } from "./aisdk.js"
import { declineOf } from "./decline.js"

export interface HostIO {
  request(method: string, params: unknown, signal?: AbortSignal): Promise<any>
  notify(method: string, params: unknown): void
}

/** A loaded plugin definition (upstream `Plugin.Generation`). */
export interface Generation {
  readonly id: string
  readonly revision: string
  readonly target: string
  readonly source?: unknown
  readonly features?: Record<string, true>
  /** The `./gui-experimental` bundle (`Generation.gui` upstream); the desktop app's, never served by ocd. */
  readonly gui?: { readonly file: string; readonly revision: string; readonly options: Record<string, unknown> }
  readonly effect: (host: any) => Effect.Effect<void, never, Scope.Scope>
}

/** The registrations of one activation; closing its scope removes them. */
export interface Slot {
  readonly plugin: Generation
  readonly scope: Scope.Closeable
  readonly registrations: Set<() => void>
  failure?: { readonly error: string; readonly ref: string }
}

type SlotEntry = { plugin: Generation; activation?: Slot; error?: string }

type HookEntry = {
  readonly owner: Slot
  readonly callback: (event: unknown) => Effect.Effect<void, unknown>
  readonly options?: { readonly providerID?: string }
}

type TransformEntry = { readonly owner: Slot; readonly callback: (editor: unknown) => void }

const decodeEvent = Schema.decodeUnknownEffect(OpenCodeEvent as unknown as Schema.Codec<unknown, unknown>)

export class LocationRuntime {
  readonly location: unknown
  readonly handles: Handles
  readonly rpc: ReturnType<typeof makeRpc>
  private readonly hooks = new Map<string, HookEntry[]>()
  private readonly transforms = new Map<Domain, TransformEntry[]>()
  private readonly events = Effect.runSync(PubSub.unbounded<any>())
  private readonly scopes = new Map<string, Scope.Closeable>()
  private slots: SlotEntry[] = []
  private failures: unknown[] = []
  private inventory: unknown[] = []
  private subscribers = 0
  private batch = 0
  private readonly changed = new Set<Domain>()
  private hooksDirty = false
  private rpcDirty = false
  private inventoryDirty = false
  private scheduled = false
  private closed = false
  private readonly remote: Remote

  constructor(
    readonly key: string,
    readonly ref: { readonly directory: string; readonly workspaceID?: string },
    info: unknown,
    readonly app: unknown,
    private readonly io: HostIO,
    prefix: string,
    private readonly aisdk: () => AISDKService,
  ) {
    this.location = decodeLocation(info)
    this.handles = new Handles(prefix)
    this.remote = {
      invoke: (handle, args, signal) => this.io.request("invoke", { location: this.key, handle, args }, signal),
    }
    this.rpc = makeRpc({
      location: ref,
      publish: (type, data) =>
        Effect.tryPromise({
          try: () => this.io.request("api", { location: this.key, plugin: null, method: "event.publish", input: { type, data } }),
          catch: (cause) => (cause instanceof Error ? cause : new Error(String(cause))),
        }).pipe(Effect.asVoid),
      events: () => this.subscribe(),
      remote: (rpcID, method, input) =>
        Effect.tryPromise({
          try: () =>
            this.io.request("api", { location: this.key, plugin: null, method: "rpc.call", input: { rpcID, method, input } }),
          catch: (cause): RpcFailure => {
            if (cause instanceof RemoteError && cause.type)
              return cause.data === undefined
                ? { type: cause.type, message: cause.message }
                : { type: cause.type, message: cause.message, data: cause.data }
            return { type: "rpc.unavailable", message: `RPC is unavailable: ${rpcID}` }
          },
        }),
      changed: () => {
        this.rpcDirty = true
        this.schedule()
      },
    })
  }

  codec(group: string): Codec {
    return { handles: this.handles, remote: this.remote, group }
  }

  // -------------------------------------------------------------------------------------------
  // Notifications to Rust (batched like State.batch during activation)
  // -------------------------------------------------------------------------------------------

  private schedule() {
    if (this.batch > 0 || this.scheduled) return
    this.scheduled = true
    queueMicrotask(() => {
      this.scheduled = false
      this.flush()
    })
  }

  /** Sends pending change notifications now. Called before every request to Rust to keep ordering. */
  flush() {
    if (this.batch > 0 || this.closed) return
    if (this.changed.size > 0) {
      const domains = Array.from(this.changed)
      this.changed.clear()
      this.io.notify("domains.changed", { location: this.key, domains, active: this.transformDomains() })
    }
    if (this.hooksDirty) {
      this.hooksDirty = false
      this.io.notify("hooks.changed", { location: this.key, hooks: this.hookList() })
    }
    if (this.rpcDirty) {
      this.rpcDirty = false
      this.io.notify("rpc.changed", { location: this.key, ids: this.rpc.ids() })
    }
    if (this.inventoryDirty) {
      this.inventoryDirty = false
      this.io.notify("plugins.changed", { location: this.key, inventory: this.inventory })
    }
  }

  hookList() {
    return Array.from(this.hooks, ([key, entries]) =>
      entries.map((entry) => ({
        key,
        ...(entry.options?.providerID === undefined ? {} : { providerID: entry.options.providerID }),
      })),
    ).flat()
  }

  transformDomains() {
    return Array.from(this.transforms.keys())
  }

  // -------------------------------------------------------------------------------------------
  // Context services
  // -------------------------------------------------------------------------------------------

  api(owner: Slot, method: string, input: unknown, signal?: AbortSignal) {
    this.flush()
    return this.io.request("api", { location: this.key, plugin: owner.plugin.id, method, input: toWire(input, undefined) ?? {} }, signal)
  }

  registerHook(owner: Slot, key: string, callback: HookEntry["callback"], options?: HookEntry["options"]) {
    const entry: HookEntry = { owner, callback, ...(options ? { options } : {}) }
    this.hooks.set(key, [...(this.hooks.get(key) ?? []), entry])
    this.hooksDirty = true
    let active = true
    const remove = () => {
      if (!active) return
      active = false
      owner.registrations.delete(remove)
      const next = (this.hooks.get(key) ?? []).filter((item) => item !== entry)
      if (next.length === 0) this.hooks.delete(key)
      else this.hooks.set(key, next)
      this.hooksDirty = true
      this.schedule()
    }
    owner.registrations.add(remove)
    this.schedule()
    return remove
  }

  /** `ctx.aisdk.hook(name, callback, options)`: registered in the location's AI SDK service. */
  registerAisdkHook(
    owner: Slot,
    name: "sdk" | "language",
    callback: (event: unknown) => Effect.Effect<void, unknown>,
    options?: { readonly providerID?: string },
  ) {
    const plugin = owner.plugin.id
    const wrapped = pluginHook(
      name,
      (event) => {
        if (owner.failure) return Effect.void
        return Effect.suspend(() => callback(event)).pipe(withLogger(plugin))
      },
      options,
    )
    const dispose = this.aisdk().hook(name, wrapped)
    let active = true
    const remove = () => {
      if (!active) return
      active = false
      owner.registrations.delete(remove)
      dispose()
    }
    owner.registrations.add(remove)
    return remove
  }

  registerTransform(owner: Slot, domain: Domain, callback: TransformEntry["callback"]) {
    const entry: TransformEntry = { owner, callback }
    this.transforms.set(domain, [...(this.transforms.get(domain) ?? []), entry])
    this.changed.add(domain)
    let active = true
    const remove = () => {
      if (!active) return
      active = false
      owner.registrations.delete(remove)
      const next = (this.transforms.get(domain) ?? []).filter((item) => item !== entry)
      if (next.length === 0) this.transforms.delete(domain)
      else this.transforms.set(domain, next)
      this.changed.add(domain)
      this.schedule()
    }
    owner.registrations.add(remove)
    this.schedule()
    return remove
  }

  subscribe(): Stream.Stream<any> {
    return Stream.unwrap(
      Effect.sync(() => {
        this.subscribers++
        this.io.notify("events.interest", { location: this.key, count: this.subscribers })
        return Stream.fromPubSub(this.events).pipe(
          Stream.ensuring(
            Effect.sync(() => {
              this.subscribers--
              if (!this.closed) this.io.notify("events.interest", { location: this.key, count: this.subscribers })
            }),
          ),
        )
      }),
    )
  }

  /** A server event forwarded by Rust (`event` notification). */
  deliver(event: any) {
    if (this.closed || this.subscribers === 0) return
    if (!event || typeof event.type !== "string") return
    if (!(EventManifest.isServer(event) || event.type.startsWith("rpc."))) return
    Effect.runFork(
      decodeEvent(event).pipe(
        Effect.flatMap((decoded) => PubSub.publish(this.events, decoded)),
        Effect.catch((error) =>
          Effect.sync(() => log("debug", "dropping undecodable event", { type: event.type, error: String(error) })),
        ),
      ),
    )
  }

  // -------------------------------------------------------------------------------------------
  // Transforms
  // -------------------------------------------------------------------------------------------

  transform(domain: Domain, value: unknown) {
    const spec = SPECS[domain]
    const before = this.changed.has(domain)
    this.handles.advance(domain)
    const codec = this.codec(domain)
    try {
      while (true) {
        const state = spec.decode(value, codec)
        const editor = spec.editor(state)
        let failed = false
        for (const entry of [...(this.transforms.get(domain) ?? [])]) {
          if (entry.owner.failure) continue
          try {
            entry.callback(editor)
          } catch (cause) {
            this.disable(entry.owner, domain, cause)
            failed = true
            break
          }
        }
        if (failed) continue
        return spec.encode(state, codec)
      }
    } finally {
      // The returned value already reflects registrations removed while rebuilding this domain.
      if (!before) this.changed.delete(domain)
      this.schedule()
    }
  }

  private disable(owner: Slot, domain: Domain, cause: unknown) {
    if (owner.failure) return
    owner.failure = {
      error: `Plugin disabled after ${domain}.transform failed. Check server logs for details.`,
      ref: `err_${crypto.randomUUID().slice(0, 8)}`,
    }
    log("warn", "disabled plugin after transform failure", {
      "plugin.id": owner.plugin.id,
      state: domain,
      ref: owner.failure.ref,
      cause: cause instanceof Error ? (cause.stack ?? cause.message) : String(cause),
    })
    for (const remove of Array.from(owner.registrations)) remove()
    this.inventory = this.computeInventory()
    this.inventoryDirty = true
    // User finalizers may wait on the host; never close the scope while a rebuild is running.
    setTimeout(() => {
      void Effect.runPromise(Scope.close(owner.scope, Exit.void)).catch((error) =>
        log("warn", "failed to clean up disabled plugin", { "plugin.id": owner.plugin.id, error: String(error) }),
      )
    }, 0)
  }

  // -------------------------------------------------------------------------------------------
  // Hooks
  // -------------------------------------------------------------------------------------------

  hasHooks(key: string) {
    return (this.hooks.get(key)?.length ?? 0) > 0
  }

  async trigger(key: string, wire: unknown, signal?: AbortSignal) {
    const entries = [...(this.hooks.get(key) ?? [])]
    const event = await hydrateEvent(key, wire, this)
    for (const entry of entries) {
      if (entry.owner.failure) continue
      if (entry.options?.providerID !== undefined && entry.options.providerID !== eventProviderID(event)) continue
      const exit = await Effect.runPromiseExit(
        Effect.suspend(() => entry.callback(event)).pipe(withLogger(entry.owner.plugin.id)),
        { signal },
      )
      if (Exit.isFailure(exit)) throw hookFailure(exit.cause)
    }
    return dehydrateEvent(key, event, this)
  }

  // -------------------------------------------------------------------------------------------
  // Function handles
  // -------------------------------------------------------------------------------------------

  async invoke(handle: string, args: unknown[], options: { scope?: string; signal?: AbortSignal }) {
    const entry = this.handles.get(handle)
    if (!entry) throw new Error(`Unknown plugin function handle: ${handle}`)
    const live = (args ?? []).map((arg) => fromWire(arg, this.handles, this.remote))
    let result: unknown = typeof entry.value === "function" ? (entry.value as Function)(...live) : entry.value
    if (Effect.isEffect(result)) result = await this.run(result as Effect.Effect<unknown, unknown, Scope.Scope>, options)
    else result = await result
    return toWire(result, this.handles) ?? null
  }

  private async run(effect: Effect.Effect<unknown, unknown, Scope.Scope>, options: { scope?: string; signal?: AbortSignal }) {
    const program =
      options.scope === undefined
        ? Effect.scoped(effect)
        : Effect.provideService(effect, Scope.Scope, this.namedScope(options.scope))
    const exit = await Effect.runPromiseExit(program, { signal: options.signal })
    if (Exit.isSuccess(exit)) return exit.value
    throw Cause.squash(exit.cause)
  }

  private namedScope(id: string) {
    const existing = this.scopes.get(id)
    if (existing) return existing
    const scope = Scope.makeUnsafe()
    this.scopes.set(id, scope)
    return scope
  }

  async closeScope(id: string) {
    const scope = this.scopes.get(id)
    if (!scope) return
    this.scopes.delete(id)
    await Effect.runPromise(Scope.close(scope, Exit.void))
  }

  async executeTool(handle: string, input: unknown, context: any, signal?: AbortSignal) {
    const entry = this.handles.get(handle)
    if (!entry || entry.kind !== "tool") throw new Error(`Unknown plugin tool handle: ${handle}`)
    const tool = entry.value as Tool.Info
    const toolContext = {
      sessionID: context.sessionID,
      agent: context.agent,
      messageID: context.messageID,
      id: context.id,
      progress: (update: unknown) =>
        Effect.sync(() =>
          this.io.notify("tool.progress", { location: this.key, id: context.id, metadata: toWire(update, undefined) }),
        ),
    }
    const exit = await Effect.runPromiseExit(executeTool(tool, input, toolContext as Tool.Context), { signal })
    if (Exit.isSuccess(exit)) return toWire(exit.value, undefined)
    const error = Cause.squash(exit.cause)
    throw toolError(error)
  }

  // -------------------------------------------------------------------------------------------
  // Activation
  // -------------------------------------------------------------------------------------------

  async activate(definitions: readonly Generation[], failures: readonly unknown[]) {
    if (this.closed) return this.inventory
    this.batch++
    try {
      this.failures = [...failures]
      const current = this.slots
      const changed = definitions.findIndex((definition, index) => {
        const entry = current[index]
        return entry?.plugin.id !== definition.id || entry.plugin.revision !== definition.revision
      })
      const prefix = changed === -1 ? definitions.length : changed
      for (let index = 0; index < prefix; index++) current[index] = { ...current[index], plugin: definitions[index] }
      if (prefix === definitions.length && current.length === definitions.length) {
        const next = this.computeInventory()
        if (JSON.stringify(next) !== JSON.stringify(this.inventory)) {
          this.inventory = next
          this.inventoryDirty = true
        }
        return this.inventory
      }
      const previous = new Map(current.slice(prefix).map((slot) => [slot.plugin.id, slot]))
      for (const slot of current.slice(prefix).toReversed()) {
        if (slot.activation && !slot.activation.failure) await this.teardown(slot.activation)
      }
      const next = current.slice(0, prefix)
      for (const definition of definitions.slice(prefix)) {
        const slot = previous.get(definition.id)
        if (slot?.activation?.failure && slot.plugin.revision === definition.revision) {
          next.push({ ...slot, plugin: definition })
          continue
        }
        const result = await this.load(definition)
        if ("activation" in result) {
          next.push({ plugin: definition, activation: result.activation })
          continue
        }
        const fallback = slot?.activation
        if (!fallback || fallback.failure) {
          next.push({ plugin: definition, error: result.error })
          continue
        }
        const restored = await this.load(fallback.plugin)
        if ("activation" in restored) {
          next.push({ plugin: definition, activation: restored.activation, error: result.error })
          continue
        }
        log("error", "failed to restore plugin; deactivating", { "plugin.id": definition.id })
        next.push({ plugin: definition, error: result.error })
      }
      this.slots = next
      this.inventory = this.computeInventory()
      this.inventoryDirty = true
      return this.inventory
    } finally {
      this.batch--
      this.flush()
    }
  }

  private async load(plugin: Generation): Promise<{ activation: Slot } | { error: string }> {
    const activation: Slot = { plugin, scope: Scope.makeUnsafe(), registrations: new Set() }
    const context = makeContext(this, activation)
    const exit = await Effect.runPromiseExit(
      Effect.suspend(() => plugin.effect(context)).pipe(
        Effect.provideService(Scope.Scope, activation.scope),
        withLogger(plugin.id),
      ),
    )
    if (activation.failure || Exit.isSuccess(exit)) return { activation }
    log("warn", "failed to load plugin", { "plugin.id": plugin.id, cause: Cause.pretty(exit.cause) })
    for (const remove of Array.from(activation.registrations)) remove()
    await Effect.runPromise(Scope.close(activation.scope, exit)).catch(() => undefined)
    return { error: Cause.pretty(exit.cause) }
  }

  private async teardown(activation: Slot) {
    const exit = await Effect.runPromiseExit(Scope.close(activation.scope, Exit.void))
    for (const remove of Array.from(activation.registrations)) remove()
    if (Exit.isFailure(exit))
      log("warn", "failed to unload plugin", { "plugin.id": activation.plugin.id, cause: Cause.pretty(exit.cause) })
  }

  private computeInventory() {
    return [...this.slots.map(slotInfo), ...this.failures]
  }

  list() {
    return this.inventory
  }

  async close() {
    if (this.closed) return
    this.batch++
    try {
      for (const slot of this.slots.toReversed()) {
        if (slot.activation && !slot.activation.failure) await this.teardown(slot.activation)
      }
      for (const id of Array.from(this.scopes.keys())) await this.closeScope(id)
      this.slots = []
      this.rpc.close()
      this.handles.clear()
    } finally {
      this.closed = true
      this.batch--
    }
  }
}

function slotInfo(slot: SlotEntry) {
  const failure = slot.activation?.failure ?? (slot.error === undefined ? undefined : { error: slot.error })
  return {
    id: slot.plugin.id,
    source: slot.plugin.source ?? { type: "builtin" },
    state: failure === undefined ? { status: "active" } : { status: "failed", ...failure },
    // Built-in and SDK plugins are server parts; a package lists its own parts.
    features: slot.plugin.features ?? { server: true },
  }
}

function decodeLocation(info: unknown) {
  try {
    return Schema.decodeUnknownSync(Location.Info as unknown as Schema.Codec<unknown, unknown>)(info)
  } catch {
    return info
  }
}

const eventProviderID = (event: unknown) => {
  if (typeof event !== "object" || event === null || !("model" in event)) return undefined
  const model = (event as { model: unknown }).model
  if (typeof model !== "object" || model === null || !("providerID" in model)) return undefined
  const providerID = (model as { providerID: unknown }).providerID
  return typeof providerID === "string" ? providerID : undefined
}

function isToolError(error: unknown): error is Tool.Error {
  return error instanceof Tool.Error || (typeof error === "object" && error !== null && (error as { _tag?: unknown })._tag === "Tool.Error")
}

// What upstream's toSessionError makes of a Tool.Error's `error` cause: permission rejections keep their type,
// nested tool failures unwrap, anything else is `unknown` (packages/core/src/session/to-session-error.ts).
function sessionCause(value: unknown): { type: string; message: string } {
  const record = typeof value === "object" && value !== null ? (value as Record<string, unknown>) : undefined
  const tag = typeof record?._tag === "string" ? record._tag : typeof record?.name === "string" ? record.name : undefined
  const message = value instanceof Error ? value.message : typeof record?.message === "string" ? record.message : String(value)
  if (tag === "Permission.BlockedError") return { type: "permission.rejected", message }
  if (tag === "Permission.CorrectedError")
    return { type: "permission.rejected", message: typeof record?.feedback === "string" ? record.feedback : message }
  if (isToolError(value)) {
    if (value.error === undefined) return { type: "tool.execution", message: value.message }
    const unwrapped = sessionCause(value.error)
    return unwrapped.message === "" ? { type: "tool.execution", message: value.message } : unwrapped
  }
  return { type: "unknown", message }
}

function toolError(error: unknown) {
  // A decline, or a decline a tool rethrew and its wrapper attached to a Tool.Error, stays a decline.
  const decline = declineOf(error)
  if (decline) return new RemoteError({ message: decline.message, type: "declined", data: { kind: decline.kind } })
  if (isToolError(error)) {
    const failure = new RemoteError({
      message: error.message,
      type: "tool.error",
      ...(error.metadata === undefined ? {} : { metadata: toWire(error.metadata, undefined) }),
      ...(error.error === undefined ? {} : { data: { cause: sessionCause(error.error) } }),
    })
    return failure
  }
  return new RemoteError({
    message: error instanceof Error ? error.message : String(error),
    type: "defect",
  })
}

function hookFailure(cause: Cause.Cause<unknown>) {
  const error = Cause.squash(cause)
  if (isToolError(error) || declineOf(error)) return toolError(error)
  return new RemoteError({ message: error instanceof Error ? error.message : String(error), type: "defect" })
}

// -------------------------------------------------------------------------------------------
// Hook event conversion: plain JSON except for the few fields upstream types as JS objects.
// -------------------------------------------------------------------------------------------

async function hydrateEvent(key: string, wire: unknown, runtime: LocationRuntime): Promise<any> {
  const event: any = wire && typeof wire === "object" ? structuredClone(wire) : wire
  if (key === "tool.execute.after" && event?.status === "error" && event.error && !isToolError(event.error)) {
    event.error = new Tool.Error({
      message: String(event.error.message ?? ""),
      ...(event.error.metadata === undefined ? {} : { metadata: event.error.metadata }),
    })
  }
  if (key === "session.http.request" || key === "session.http.response") {
    if (event?.request) event.request = toRequest(event.request)
    if (key === "session.http.response" && event?.response) event.response = toResponse(event.response)
  }
  void runtime
  return event
}

async function dehydrateEvent(key: string, event: any, runtime: LocationRuntime): Promise<unknown> {
  if (key === "session.http.request" || key === "session.http.response") {
    const out = { ...event }
    if (event.request instanceof Request) out.request = await fromRequest(event.request)
    if (key === "session.http.response" && event.response instanceof Response) out.response = await fromResponse(event.response)
    return toWire(out, undefined)
  }
  if (key === "tool.execute.after" && event?.status === "error" && isToolError(event.error)) {
    return toWire(
      {
        ...event,
        error: {
          message: event.error.message,
          ...(event.error.metadata === undefined ? {} : { metadata: event.error.metadata }),
        },
      },
      undefined,
    )
  }
  void runtime
  return toWire(event, undefined)
}

type WireRequest = { url: string; method?: string; headers?: Record<string, string>; body?: string | null }
type WireResponse = { status?: number; statusText?: string; headers?: Record<string, string>; body?: string | null }

function toRequest(value: WireRequest) {
  return new Request(value.url, {
    method: value.method ?? "GET",
    headers: value.headers ?? {},
    ...(value.body === undefined || value.body === null || value.method === "GET" || value.method === "HEAD"
      ? {}
      : { body: value.body }),
  })
}

async function fromRequest(request: Request): Promise<WireRequest> {
  const body = request.method === "GET" || request.method === "HEAD" ? null : await request.clone().text()
  return { url: request.url, method: request.method, headers: Object.fromEntries(request.headers), body }
}

function toResponse(value: WireResponse) {
  return new Response(value.body ?? null, {
    status: value.status ?? 200,
    statusText: value.statusText ?? "",
    headers: value.headers ?? {},
  })
}

async function fromResponse(response: Response): Promise<WireResponse> {
  return {
    status: response.status,
    statusText: response.statusText,
    headers: Object.fromEntries(response.headers),
    body: await response.clone().text(),
  }
}

// Route plugin Effect logs to the Rust log (stderr JSON lines).
export function withLogger(pluginID: string) {
  const logger = Logger.make(({ message, logLevel, cause }) => {
    const level = String(logLevel).toLowerCase()
    const mapped =
      level === "fatal" || level === "error"
        ? "error"
        : level === "warning" || level === "warn"
          ? "warn"
          : level === "debug"
            ? "debug"
            : level === "trace"
              ? "trace"
              : "info"
    const parts = Array.isArray(message) ? message : [message]
    const text = parts.map((part) => (typeof part === "string" ? part : safeJson(part))).join(" ")
    log(mapped, text, {
      plugin: pluginID,
      ...(cause && !Cause.isEmpty(cause) ? { cause: Cause.pretty(cause) } : {}),
    })
  })
  return <A, E, R>(effect: Effect.Effect<A, E, R>) =>
    Effect.provideService(effect, Logger.CurrentLoggers, new Set([logger]))
}

function safeJson(value: unknown) {
  try {
    return JSON.stringify(value)
  } catch {
    return String(value)
  }
}
