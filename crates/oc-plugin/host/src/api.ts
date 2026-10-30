// Host API methods backed by Rust.
//
// Every ctx API method that upstream exposes through an HTTP endpoint is forwarded to Rust with the
// endpoint identifier as the method name and the merged endpoint input (path params, query and
// JSON body) as `input`. Rust answers with exactly what the HTTP endpoint would send (a lone `data`
// wrapper removed, as upstream's Promise adapter does). The Effect host decodes that answer with the
// endpoint's success schema, so Effect plugins receive the same typed values upstream's core hands
// them; upstream's own Promise adapter then encodes them again for Promise plugins.

import { Effect, Schema, SchemaAST } from "effect"
import { HttpApiSchema, type HttpApiEndpoint } from "effect/http-api"
import { ClientApi } from "@opencode/protocol/client"

type Codec = Schema.Codec<unknown, unknown>

interface Compiled {
  readonly inputs: ReadonlyArray<Codec>
  readonly output: Codec
  readonly noContent: boolean
  /** The success schema is a lone `data` wrapper and `output` is its content. */
  readonly unwrapped: boolean
}

const compiled = new Map<string, Compiled>()

function endpoint(identifier: string): HttpApiEndpoint.Top {
  for (const group of Object.values(ClientApi.groups) as Array<{ endpoints: Record<string, HttpApiEndpoint.Top> }>) {
    const found = group.endpoints[identifier]
    if (found) return found
  }
  throw new Error(`Unknown API endpoint: ${identifier}`)
}

// Same derivation as `compileEndpoint` in @opencode/plugin/promise/adapter.ts.
function compile(identifier: string): Compiled {
  const cached = compiled.get(identifier)
  if (cached) return cached
  const target = endpoint(identifier)
  const payloadSchemas = Array.from(target.payload.values()).flatMap(({ schemas }) => schemas)
  const successSchemas = Array.from(target.success)
  if (payloadSchemas.length > 1 || successSchemas.length > 1)
    throw new Error(`Unsupported API schema cardinality: ${target.identifier}`)
  const inputs = [
    target.params,
    target.query === undefined ? undefined : Schema.toType(target.query),
    target.headers,
    ...payloadSchemas,
  ].filter((schema): schema is Schema.Top => schema !== undefined) as Array<Codec>
  const success = (successSchemas[0] ?? HttpApiSchema.NoContent) as Codec
  const noContent = HttpApiSchema.isNoContent(success.ast)
  const type = Schema.toType(success).ast
  const data = SchemaAST.isObjects(success.ast)
    ? success.ast.propertySignatures.find((property) => property.name === "data")
    : undefined
  const unwrapped =
    !noContent &&
    SchemaAST.isObjects(type) &&
    type.indexSignatures.length === 0 &&
    type.propertySignatures.length === 1 &&
    type.propertySignatures[0]?.name === "data" &&
    data !== undefined
  const output = unwrapped ? (Schema.make<Schema.Top>(data!.type) as Codec) : success
  const result = { inputs, output, noContent, unwrapped }
  compiled.set(identifier, result)
  return result
}

const jsonSafe = (value: unknown) => (value === undefined ? {} : JSON.parse(JSON.stringify(value)))

/** Encodes a decoded host input back to its wire form, field group by field group. */
export function encodeInput(identifier: string, input: unknown) {
  const { inputs } = compile(identifier)
  if (input === undefined || input === null) return {}
  const parts: Array<Record<string, unknown>> = []
  for (const schema of inputs) {
    try {
      const encoded = Schema.encodeUnknownSync(schema)(input)
      if (encoded && typeof encoded === "object") parts.push(encoded as Record<string, unknown>)
    } catch {
      return jsonSafe(input)
    }
  }
  // Schemas drop keys they do not declare; keep any extra keys the caller passed.
  return { ...jsonSafe(input), ...jsonSafe(Object.assign({}, ...parts)) }
}

export function decodeOutput(identifier: string, output: unknown) {
  const { output: schema, noContent, unwrapped } = compile(identifier)
  if (noContent) return Effect.succeed(undefined)
  // Rust removes the wrapper of the endpoints it knows as `{data}` ones; others (such as `generate.text`, whose
  // `{data}` response is a named schema) arrive with it.
  if (unwrapped && isWrapped(output)) output = output.data
  return Schema.decodeUnknownEffect(schema)(output).pipe(
    Effect.catch(() => Effect.succeed(output)),
  )
}

function isWrapped(value: unknown): value is { data: unknown } {
  return typeof value === "object" && value !== null && !Array.isArray(value) && Object.keys(value).length === 1 && "data" in value
}

export type Call = (method: string, input: unknown) => Effect.Effect<unknown, Error>

/** An Effect host method backed by the HTTP-equivalent Rust method `identifier`. */
export function method(call: Call, identifier: string) {
  return (input?: unknown) =>
    Effect.suspend(() => call(identifier, encodeInput(identifier, input))).pipe(
      Effect.flatMap((output) => decodeOutput(identifier, output)),
    )
}
