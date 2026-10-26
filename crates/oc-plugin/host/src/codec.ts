// Functions crossing the process boundary.
//
// A function (or a lazy Effect value) never crosses as code: it crosses as `{ "$fn": "<handle>" }`.
// Handles minted by the sidecar start with `js:`; every other handle belongs to Rust. A Rust handle
// arriving here becomes a stub function that calls back over `invoke`, and the stub turns back into
// the same handle when it crosses again, so untouched native entries stay native.

import { Effect } from "effect"
import { Tool } from "@opencode/schema/tool"
import { declineDefect } from "./decline.js"

export const FN = "$fn"

export type WireFn = { readonly [FN]: string }

export function isWireFn(value: unknown): value is WireFn {
  return typeof value === "object" && value !== null && typeof (value as Record<string, unknown>)[FN] === "string"
}

type Entry = {
  readonly value: unknown
  // Owning group (a transform domain) and its generation; undefined for transient handles.
  readonly group?: string
  readonly generation: number
  readonly created: number
  readonly kind?: "tool"
}

const stubs = new WeakMap<object, string>()

const TRANSIENT_TTL = 60 * 60 * 1000
const KEEP_GENERATIONS = 3

export class Handles {
  private next = 1
  private readonly entries = new Map<string, Entry>()
  private readonly ids = new WeakMap<object, string>()
  private readonly generations = new Map<string, number>()

  constructor(private readonly prefix: string) {}

  /** Starts a new generation for a group and drops handles older than the retained generations. */
  advance(group: string) {
    const generation = (this.generations.get(group) ?? 0) + 1
    this.generations.set(group, generation)
    const now = Date.now()
    for (const [id, entry] of this.entries) {
      if (entry.group === group && entry.generation <= generation - KEEP_GENERATIONS) this.entries.delete(id)
      else if (entry.group === undefined && now - entry.created > TRANSIENT_TTL) this.entries.delete(id)
    }
    return generation
  }

  register(value: object, options?: { group?: string; kind?: "tool" }) {
    const stub = stubs.get(value)
    if (stub) return stub
    const group = options?.group
    const generation = group === undefined ? 0 : (this.generations.get(group) ?? 0)
    const existing = options?.kind === undefined ? this.ids.get(value) : undefined
    if (existing) {
      const entry = this.entries.get(existing)
      if (entry) {
        // Refresh ownership so a value reused across rebuilds survives garbage collection.
        if (group !== undefined && (entry.group !== group || entry.generation < generation))
          this.entries.set(existing, { ...entry, group, generation })
        return existing
      }
    }
    const id = `js:${this.prefix}${this.next++}`
    this.entries.set(id, { value, group, generation, created: Date.now(), kind: options?.kind })
    if (options?.kind === undefined) this.ids.set(value, id)
    return id
  }

  get(id: string) {
    return this.entries.get(id)
  }

  clear() {
    this.entries.clear()
  }
}

export interface Remote {
  invoke(handle: string, args: unknown[], signal?: AbortSignal): Promise<unknown>
}

/**
 * A stub that calls a Rust-owned function and returns an Effect of its JSON result. A native tool's
 * `Tool.Error` fails as a `Tool.Error`, and a decline dies with the defect upstream's native tools die with.
 */
export function stub(handle: string, remote: Remote): (...args: unknown[]) => Effect.Effect<unknown, Error> {
  const fn = (...args: unknown[]) =>
    Effect.tryPromise({
      try: (signal) => remote.invoke(handle, args.map((arg) => toWire(arg, undefined)), signal),
      catch: (cause) => (cause instanceof Error ? cause : new Error(String(cause))),
    }).pipe(
      Effect.catch((error): Effect.Effect<never, Error> => {
        const defect = declineDefect(error)
        if (defect) return Effect.die(defect)
        if (error.name === "Tool.Error") {
          const metadata = (error as { data?: unknown }).data
          return Effect.fail(
            new Tool.Error({
              message: error.message,
              ...(metadata && typeof metadata === "object" ? { metadata: metadata as Record<string, unknown> } : {}),
            }) as unknown as Error,
          )
        }
        return Effect.fail(error)
      }),
    )
  stubs.set(fn, handle)
  return fn
}

export function stubHandle(value: unknown) {
  return typeof value === "function" || (typeof value === "object" && value !== null)
    ? stubs.get(value as object)
    : undefined
}

/**
 * Converts a value to JSON for the wire. Functions and Effect values become handles when a
 * handle table is given, otherwise they are dropped.
 */
export function toWire(value: unknown, handles: Handles | undefined, group?: string, seen = new Set<object>()): unknown {
  if (value === null || value === undefined) return value
  if (typeof value === "function" || Effect.isEffect(value)) {
    const existing = stubHandle(value)
    if (existing) return { [FN]: existing }
    if (!handles) return undefined
    return { [FN]: handles.register(value as object, { group }) }
  }
  if (typeof value === "bigint") return Number(value)
  if (typeof value !== "object") return value
  if (value instanceof Date) return value.toISOString()
  if (typeof AbortSignal !== "undefined" && value instanceof AbortSignal) return undefined
  if (seen.has(value)) return undefined
  if (typeof (value as { toJSON?: unknown }).toJSON === "function" && !(value instanceof Map) && !(value instanceof Set))
    return toWire((value as { toJSON: () => unknown }).toJSON(), handles, group, seen)
  seen.add(value)
  try {
    if (Array.isArray(value)) return value.map((item) => toWire(item, handles, group, seen) ?? null)
    if (value instanceof Map)
      return Array.from(value.entries()).map(([key, item]) => [key, toWire(item, handles, group, seen) ?? null])
    if (value instanceof Set) return Array.from(value, (item) => toWire(item, handles, group, seen) ?? null)
    if (value instanceof Uint8Array) return Buffer.from(value).toString("base64")
    const out: Record<string, unknown> = {}
    for (const [key, item] of Object.entries(value)) {
      const encoded = toWire(item, handles, group, seen)
      if (encoded !== undefined) out[key] = encoded
    }
    return out
  } finally {
    seen.delete(value)
  }
}

/** Replaces handles in a wire value: sidecar handles resolve to their values, Rust handles to stubs. */
export function fromWire(value: unknown, handles: Handles, remote: Remote): unknown {
  if (value === null || typeof value !== "object") return value
  if (Array.isArray(value)) return value.map((item) => fromWire(item, handles, remote))
  if (isWireFn(value)) {
    const id = value[FN]
    if (id.startsWith("js:")) {
      const entry = handles.get(id)
      if (entry) return entry.value
    }
    return stub(id, remote)
  }
  const out: Record<string, unknown> = {}
  for (const [key, item] of Object.entries(value)) out[key] = fromWire(item, handles, remote)
  return out
}

/** Deep copy of a JSON value. */
export function clone<T>(value: T): T {
  return value === undefined ? value : (JSON.parse(JSON.stringify(value)) as T)
}
