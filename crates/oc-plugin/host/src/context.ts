// The Effect plugin context (`Plugin.Context` from @opencode/plugin/effect) implemented as a proxy to
// Rust. Mirrors packages/core/src/plugin/host.ts: API groups become `api` requests, hooks and
// transforms are registered in the location runtime (and announced to Rust), storage is namespaced by
// Rust, `rpc` is the sidecar-side registry, `event.subscribe` reads the forwarded event stream.
//
// Promise plugins receive this same object through upstream's own `fromPromise` adapter.

import type { Plugin } from "@opencode/plugin/effect"
import { Effect, Scope, Stream } from "effect"
import { method } from "./api.js"
import type { Domain } from "./domains.js"
import { decodeTool } from "./domains.js"
import { log } from "./peer.js"
import type { LocationRuntime, Slot } from "./location.js"

export function makeContext(runtime: LocationRuntime, slot: Slot): Plugin.Context {
  const call = (name: string, input: unknown) =>
    Effect.tryPromise({
      try: (signal) => runtime.api(slot, name, input, signal),
      catch: (cause) => (cause instanceof Error ? cause : new Error(String(cause))),
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
    event: {
      subscribe: () => runtime.subscribe(),
    },
    experimental: {
      terminal: {
        read: api("persistentPty.read"),
      },
    },
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
    },
  }
  return context as unknown as Plugin.Context
}

export type EventStream = Stream.Stream<any>
