// The Effect plugin context (`Plugin.Context` from @opencode/plugin/effect) implemented as a proxy to
// Rust. Mirrors packages/core/src/plugin/host.ts: API groups become `api` requests, hooks and
// transforms are registered in the location runtime (and announced to Rust), storage is namespaced by
// Rust, `rpc` is the sidecar-side registry, `event.subscribe` reads the forwarded event stream.
//
// Promise plugins receive this same object through upstream's own `fromPromise` adapter.

import type { Plugin } from "@opencode/plugin/effect"
import { Effect, Scope, Stream } from "effect"
import { EventManifest } from "@opencode/schema/event-manifest"
import { Tool } from "@opencode/schema/tool"
import { method } from "./api.js"
import { declineDefect } from "./decline.js"
import type { Domain } from "./domains.js"
import { decodeTool } from "./domains.js"
import { log } from "./peer.js"
import type { LocationRuntime, Slot } from "./location.js"

export function makeContext(runtime: LocationRuntime, slot: Slot): Plugin.Context {
  const call = (name: string, input: unknown) =>
    Effect.tryPromise({
      try: (signal) => runtime.api(slot, name, input, signal),
      catch: (cause) => (cause instanceof Error ? tagged(cause) : new Error(String(cause))),
    })
  const api = (identifier: string) => method(call, identifier) as any

  const reload = (domain: Domain) => () =>
    call(`${domain}.reload`, {}).pipe(
      Effect.asVoid,
      Effect.catch((error) => Effect.sync(() => log("warn", `${domain}.reload failed`, { error: error.message }))),
    )

  const register = (add: () => () => void) =>
    Effect.gen(function* () {
      const scope = yield* Scope.Scope
      const remove = add()
      const dispose = Effect.sync(remove)
      yield* Scope.addFinalizer(scope, dispose)
      return { dispose }
    })

  const transform = (domain: Domain) => (callback: (editor: any) => void) =>
    register(() => runtime.registerTransform(slot, domain, callback))

  const hook =
    (domain: string) =>
    (name: string, callback: (event: any) => Effect.Effect<void, any>, options?: { readonly providerID?: string }) =>
      register(() => runtime.registerHook(slot, `${domain}.${name}`, callback, options))

  const raw = (name: string) => (input?: unknown) =>
    call(name, input ?? {}).pipe(Effect.map((value) => (value === null ? undefined : value)))

  // `PermissionDomain.assert` (core/src/plugin/host.ts): a deny rule, a rejection with feedback and a missing
  // session fail with a Tool.Error carrying the reason; a plain rejection stays a defect that ends the step.
  const assertPermission = (input: unknown) =>
    call("permission.assert", JSON.parse(JSON.stringify(input ?? {}))).pipe(
      Effect.asVoid,
      Effect.catch((error): Effect.Effect<void, Tool.Error> => {
        const defect = declineDefect(error)
        if (defect) return Effect.die(defect)
        if (["Permission.BlockedError", "Permission.CorrectedError", "Session.NotFoundError"].includes(error.name))
          return Effect.fail(new Tool.Error({ message: error.message, error }))
        return Effect.die(error)
      }),
    )

  const context = {
    app: runtime.app,
    location: runtime.location,
    options: {},
    rpc: Object.assign(runtime.rpc.client, { register: runtime.rpc.register }),
    agent: {
      get: api("agent.get"),
      list: api("agent.list"),
      reload: reload("agent"),
      transform: transform("agent"),
    },
    aisdk: {
      hook: (name: "sdk" | "language", callback: (event: any) => Effect.Effect<void, any>, options?: { readonly providerID?: string }) =>
        register(() => runtime.registerAisdkHook(slot, name, callback, options)),
    },
    provider: {
      list: api("provider.list"),
      get: api("provider.get"),
      reload: reload("provider"),
      transform: transform("provider"),
    },
    model: {
      list: api("model.list"),
      default: api("model.default"),
      reload: reload("model"),
      transform: transform("model"),
    },
    command: {
      list: api("command.list"),
      reload: reload("command"),
      transform: transform("command"),
    },
    credential: {
      activate: api("credential.activate"),
    },
    event: {
      // `event.subscribe({types})` (core/src/plugin/host.ts 2.2): only the requested server and `rpc.*` types;
      // unknown types are dropped with a warning, and a request with none left never emits.
      subscribe: (input?: { readonly types?: ReadonlyArray<string> }) => {
        if (!input?.types?.length) return runtime.subscribe()
        const unknown = new Array<string>()
        const types = new Set(
          input.types.filter((type) => {
            if (type.startsWith("rpc.") || EventManifest.Server.has(type)) return true
            unknown.push(type)
            return false
          }),
        )
        const events =
          types.size > 0 ? runtime.subscribe().pipe(Stream.filter((event: any) => types.has(event.type))) : Stream.never
        if (unknown.length === 0) return events
        return Stream.unwrap(
          Effect.sync(() => log("warn", "ignoring unknown event types", { pluginID: slot.plugin.id, types: unknown })).pipe(
            Effect.as(events),
          ),
        )
      },
    },
    experimental: {
      terminal: {
        read: api("persistentPty.read"),
      },
      // Stateless generation with a model and a prompt: no session, history or tools.
      generate: {
        text: api("generate.text"),
        image: api("generate.image"),
        decisions: api("generate.decisions"),
      },
    },
    // Deprecated since 2.2: `experimental.generate.text`.
    generate: {
      text: api("generate.text"),
    },
    integration: {
      list: api("integration.list"),
      get: api("integration.get"),
      connect: {
        key: api("integration.connect.key"),
        external: api("integration.connect.external"),
      },
      oauth: {
        connect: api("integration.oauth.connect"),
        status: api("integration.oauth.status"),
        complete: api("integration.oauth.complete"),
        cancel: api("integration.oauth.cancel"),
      },
      command: {
        connect: api("integration.command.connect"),
        status: api("integration.command.status"),
        cancel: api("integration.command.cancel"),
      },
      reload: reload("integration"),
      connection: {
        active: (integrationID: string) => raw("integration.connection.active")({ integrationID }).pipe(Effect.orDie),
        resolve: (connection: unknown) => raw("integration.connection.resolve")({ connection }),
        status: (input: unknown) => raw("integration.connection.status")(input).pipe(Effect.asVoid, Effect.orDie),
      },
      transform: transform("integration"),
    },
    mcp: {
      list: api("mcp.list"),
      reload: reload("mcp"),
      transform: transform("mcp"),
    },
    permission: {
      hook: hook("permission"),
      list: api("session.permission.list"),
      get: api("session.permission.get"),
      reply: api("session.permission.reply"),
      assert: assertPermission,
    },
    plugin: {
      list: api("plugin.list"),
    },
    reference: {
      list: api("reference.list"),
      reload: reload("reference"),
      transform: transform("reference"),
    },
    skill: {
      list: api("skill.list"),
      reload: reload("skill"),
      transform: transform("skill"),
    },
    storage: {
      get: (key: string) =>
        call("storage.get", { key }).pipe(
          Effect.map((value) => (value === null ? undefined : value)),
          Effect.orDie,
        ),
      set: (key: string, value: unknown) => call("storage.set", { key, value }).pipe(Effect.asVoid, Effect.orDie),
      remove: (key: string) => call("storage.remove", { key }).pipe(Effect.asVoid, Effect.orDie),
      scan: (options: { prefix: string; after?: string; limit?: number }) =>
        call("storage.scan", options).pipe(Effect.orDie),
    },
    shell: {
      hook: hook("shell"),
    },
    tool: {
      transform: transform("tool"),
      reload: reload("tool"),
      list: () =>
        call("tool.list", {}).pipe(
          Effect.map((value: any) =>
            ((value?.tools ?? []) as any[]).map((item) => decodeTool(item, runtime.codec("tool.list"))),
          ),
          Effect.orDie,
        ),
      hook: hook("tool"),
    },
    vcs: {
      get: api("vcs.get"),
      base: api("vcs.base"),
      branch: {
        list: api("vcs.branch.list"),
      },
      status: api("vcs.status"),
      diff: api("vcs.diff"),
      transform: transform("vcs"),
      reload: reload("vcs"),
    },
    websearch: {
      providers: api("websearch.providers"),
      query: api("websearch.query"),
      reload: reload("websearch"),
      transform: transform("websearch"),
    },
    worktree: {
      list: api("worktree.list"),
      create: api("worktree.create"),
      refresh: api("worktree.refresh"),
      remove: api("worktree.remove"),
      reload: reload("worktree"),
      transform: transform("worktree"),
    },
    session: {
      hook: hook("session"),
      list: api("session.list"),
      create: api("session.create"),
      get: api("session.get"),
      remove: api("session.remove"),
      switchAgent: api("session.switchAgent"),
      switchModel: api("session.switchModel"),
      prompt: api("session.prompt"),
      generate: api("session.generate"),
      command: api("session.command"),
      compact: api("session.compact"),
      update: api("session.update"),
      move: api("session.move"),
      synthetic: api("session.synthetic"),
      interrupt: api("session.interrupt"),
      wait: api("session.wait"),
      context: api("session.context"),
      // Keyed session instructions (2.2): the session must exist and the key must be a valid entry key.
      instructions: {
        entry: {
          list: api("session.instructions.entry.list"),
          put: api("session.instructions.entry.put"),
          remove: api("session.instructions.entry.remove"),
        },
      },
    },
  }
  return context as unknown as Plugin.Context
}

export type EventStream = Stream.Stream<any>

// The HTTP-only tags of errors whose core service error (what upstream's in-process host fails with) differs.
const CORE_TAGS: Record<string, string> = { SessionNotFoundError: "Session.NotFoundError" }

/**
 * An API failure as upstream's host raises it: a tagged error (`_tag` and the fields of the endpoint's error
 * body) instead of a plain `Error`, under the core's tag where the HTTP one differs.
 */
function tagged(error: Error) {
  const body = (error as { data?: unknown }).data
  if (typeof body !== "object" || body === null || typeof (body as { _tag?: unknown })._tag !== "string") return error
  const { _tag, message: _message, ...fields } = body as Record<string, unknown>
  const tag = CORE_TAGS[_tag as string] ?? (_tag as string)
  Object.assign(error, fields, { _tag: tag })
  error.name = tag
  return error
}
