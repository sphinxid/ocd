// Plugin-defined RPC endpoints (port of packages/core/src/rpc.ts).
//
// Registrations live in the sidecar because their schemas are JS values. Rust routes
// `POST /api/rpc/:rpcID/:method` to `rpc.call`. Events are published through Rust's bus (as
// `rpc.<id>.<name>`, ephemeral, with the location attached) and come back through the forwarded
// event stream.

import type { RpcClient, RpcHandlers } from "@opencode/plugin/effect/rpc"
import type { Rpc } from "@opencode/schema/rpc"
import type { Tool } from "@opencode/schema/tool"
import type { StandardSchemaV1 } from "@standard-schema/spec"
import { Effect, JsonSchema, Schema, SchemaRepresentation, Stream } from "effect"
import { log } from "./peer.js"

class DeclaredError extends Error {
  constructor(
    readonly type: string,
    message: string,
    readonly data?: unknown,
  ) {
    super(message)
  }
}

export interface RpcFailure {
  readonly type: string
  readonly message: string
  readonly data?: unknown
}

export interface RpcHost {
  readonly location: { readonly directory: string; readonly workspaceID?: string }
  /** Publishes an ephemeral bus event; Rust adds id, created and location. */
  readonly publish: (type: string, data: Record<string, unknown>) => Effect.Effect<void, Error>
  /** Server events visible to this location. */
  readonly events: () => Stream.Stream<{ readonly type: string; readonly data: unknown; readonly location?: unknown }>
  /** Calls an RPC this sidecar does not own (owned by Rust or absent). */
  readonly remote: (rpcID: string, method: string, input: unknown) => Effect.Effect<unknown, RpcFailure>
  readonly changed: () => void
}

export function makeRpc(host: RpcHost) {
  let closed = false
  const callContext = {
    error: (type: string, message: string, data?: unknown) => new DeclaredError(type, message, data),
  }
  const registrations = new Map<
    string,
    Array<{ readonly definition: Rpc.Definition; readonly handlers: Readonly<Record<string, Function>> }>
  >()

  const register = <const D extends Rpc.Definition>(definition: D, handlers: RpcHandlers<NoInfer<D>>) =>
    Effect.gen(function* () {
      const entry = { definition, handlers: handlers as unknown as Readonly<Record<string, Function>> }
      const dispose = Effect.sync(() => {
        const remaining = (registrations.get(definition.id) ?? []).filter((candidate) => candidate !== entry)
        if (remaining.length === 0) registrations.delete(definition.id)
        else registrations.set(definition.id, remaining)
        host.changed()
      })
      yield* Effect.acquireRelease(
        Effect.sync(() => {
          registrations.set(definition.id, [...(registrations.get(definition.id) ?? []), entry])
          host.changed()
        }),
        () => dispose,
      )
      return {
        dispose,
        events: {
          emit: (...args: [string, unknown]) =>
            Effect.gen(function* () {
              const event = definition.events[args[0]]
              if (!event || !Object.hasOwn(definition.events, args[0]))
                return yield* Effect.fail(new Error(`Unknown RPC event: ${definition.id}.${args[0]}`))
              const data = (yield* encode(event.schema as Tool.ValueSchema, args[1])) as Record<string, unknown>
              return yield* host.publish(`rpc.${definition.id}.${args[0]}`, data)
            }),
        },
      }
    })

  const failure = (type: string, message: string, data?: unknown): RpcFailure =>
    data === undefined ? { type, message } : { type, message, data }

  const local = (rpcID: string) => registrations.get(rpcID)?.at(-1)

  const call = (rpcID: string, name: string, input: unknown): Effect.Effect<unknown, RpcFailure> =>
    Effect.gen(function* () {
      if (closed) return yield* Effect.fail(failure("rpc.unavailable", `RPC is unavailable: ${rpcID}`))
      const entry = local(rpcID)
      if (!entry) return yield* Effect.fail(failure("rpc.unavailable", `RPC is unavailable: ${rpcID}`))
      if (!Object.hasOwn(entry.definition.methods, name) || !Object.hasOwn(entry.handlers, name))
        return yield* Effect.fail(failure("rpc.method_not_found", `Unknown RPC method: ${rpcID}.${name}`))
      const method = entry.definition.methods[name]
      const handler = entry.handlers[name]
      const parsed = yield* parse(method.input, input).pipe(
        Effect.mapError((error) => failure("rpc.invalid_input", errorMessage(error, "Invalid RPC input"))),
      )
      const result = yield* Effect.suspend(() => handler(parsed, callContext) as Effect.Effect<unknown, unknown>).pipe(
        Effect.catch((error) => encodeError(method, error)),
        Effect.catchDefect((defect) =>
          Effect.sync(() =>
            log("error", "rpc handler failed", { rpc: rpcID, method: name, defect: String(defect) }),
          ).pipe(Effect.andThen(Effect.fail(failure("rpc.internal", "RPC call failed")))),
        ),
      )
      return yield* encode(method.output, result).pipe(
        Effect.mapError((error) => failure("rpc.invalid_output", errorMessage(error, "Invalid RPC output"))),
      )
    })

  // Calls from plugins go to the local registry first, then to Rust.
  const dispatch = (rpcID: string, name: string, input: unknown) =>
    local(rpcID) ? call(rpcID, name, input) : host.remote(rpcID, name, input)

  const client = <D extends Rpc.Definition>(definition: D): RpcClient<D, Rpc.SystemError, never, unknown> => {
    const methods = Object.fromEntries(
      Object.entries(definition.methods).map(([name, method]) => [
        name,
        (input: unknown) =>
          dispatch(definition.id, name, input).pipe(
            Effect.catch((error) => decodeError(method, error)),
            Effect.flatMap((value) => read(method.output, value).pipe(Effect.catch((cause) => Effect.die(cause)))),
          ),
      ]),
    )
    return {
      ...methods,
      events: {
        subscribe: (name: string) => {
          if (!Object.hasOwn(definition.events, name))
            return Stream.fail(new Error(`Unknown RPC event: ${definition.id}.${name}`))
          const type = `rpc.${definition.id}.${name}`
          return host.events().pipe(
            Stream.filter((payload) => payload.type === type && sameLocation(payload.location, host.location)),
            Stream.mapEffect((payload) =>
              read(definition.events[name].schema as Tool.ValueSchema, payload.data).pipe(
                Effect.map((data) => ({
                  ...payload,
                  type,
                  data,
                  location: {
                    directory: host.location.directory,
                    ...(host.location.workspaceID === undefined ? {} : { workspaceID: host.location.workspaceID }),
                  },
                })),
              ),
            ),
          )
        },
      },
    } as unknown as RpcClient<D, Rpc.SystemError, never, unknown>
  }

  return {
    register,
    client,
    call,
    ids: () => Array.from(registrations.keys()),
    close: () => {
      closed = true
    },
  }
}

function sameLocation(value: unknown, location: { readonly directory: string }) {
  if (!value || typeof value !== "object") return false
  return (value as { directory?: unknown }).directory === location.directory
}

const jsonSchemas = new WeakMap<object, Schema.Codec<unknown>>()

function parse(schema: Tool.ValueSchema, value: unknown): Effect.Effect<unknown, unknown> {
  if (Schema.isSchema(schema)) return Schema.decodeUnknownEffect(schema as Schema.Codec<unknown>)(value)
  if (isStandardSchema(schema)) {
    return Effect.gen(function* () {
      const parsed = yield* Effect.try({ try: () => schema["~standard"].validate(value), catch: (cause) => cause })
      const result =
        parsed instanceof Promise ? yield* Effect.tryPromise({ try: () => parsed, catch: (cause) => cause }) : parsed
      if (result.issues) return yield* Effect.fail(new Error(result.issues.map((issue) => issue.message).join("\n")))
      return result.value
    })
  }
  return Effect.try({
    try: () => {
      const existing = jsonSchemas.get(schema as object)
      if (existing) return existing
      const codec = Schema.make<Schema.Codec<unknown>>(
        SchemaRepresentation.fromJsonSchemaDocument(JsonSchema.fromSchemaDraft2020_12(schema as JsonSchema.JsonSchema))
          .ast,
      )
      jsonSchemas.set(schema as object, codec)
      return codec
    },
    catch: (cause) => cause,
  }).pipe(Effect.flatMap((codec) => Schema.decodeUnknownEffect(codec)(value)))
}

function encode(schema: Tool.ValueSchema, value: unknown): Effect.Effect<unknown, unknown> {
  return Schema.isSchema(schema)
    ? Schema.encodeUnknownEffect(schema as Schema.Codec<unknown>)(value)
    : parse(schema, value)
}

function encodeError(method: Rpc.Method, error: unknown): Effect.Effect<never, RpcFailure> {
  if (!(error instanceof DeclaredError)) return Effect.die(error)
  if (!method.errors || !Object.hasOwn(method.errors, error.type))
    return Effect.die(new Error(`Undeclared RPC error: ${error.type}`))
  return encode(method.errors[error.type], error.data).pipe(
    Effect.catch((cause) => Effect.die(cause)),
    Effect.flatMap((data) =>
      Effect.fail(data === undefined ? { type: error.type, message: error.message } : { type: error.type, message: error.message, data }),
    ),
  )
}

function decodeError(method: Rpc.Method, error: RpcFailure): Effect.Effect<never, RpcFailure> {
  if (!method.errors || !Object.hasOwn(method.errors, error.type)) return Effect.fail(error)
  return read(method.errors[error.type], error.data).pipe(
    Effect.catch((cause) => Effect.die(cause)),
    Effect.flatMap((data) =>
      Effect.fail(data === undefined ? { type: error.type, message: error.message } : { type: error.type, message: error.message, data }),
    ),
  )
}

function errorMessage(error: unknown, fallback: string) {
  if (error instanceof Error) return error.message
  if (typeof error === "string") return error
  return fallback
}

function isStandardSchema(schema: Tool.ValueSchema): schema is Extract<Tool.ValueSchema, StandardSchemaV1> {
  return typeof schema === "object" && schema !== null && "~standard" in schema
}

function read(schema: Tool.ValueSchema, value: unknown): Effect.Effect<unknown, unknown> {
  return Schema.isSchema(schema) ? Schema.decodeUnknownEffect(schema as Schema.Codec<unknown>)(value) : Effect.succeed(value)
}
