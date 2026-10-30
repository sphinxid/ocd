// Recording instrumentation for the Rust replay fixtures (see record.sh). Inert unless CODEMODE_RECORD_DIR is set:
// every entry point returns its input unchanged and records nothing. All encoding and output lives here.
import fs from "node:fs"
import path from "node:path"
import { Cause, Effect, Exit, Schema } from "effect"
import { HttpClientRequest } from "effect/http"
import { isAsync } from "./extension.js"
import { isNamespace } from "./namespace.js"
import { isTool, type Tool } from "./tool.js"
import {
  identifierSegment,
  inputProperties,
  inputTypeScript,
  isEmptyInput,
  outputTypeScript,
} from "./tool-schema-impl.js"

const DIR = process.env.CODEMODE_RECORD_DIR
export const enabled = DIR !== undefined && DIR !== ""

// --- Test context (driven by record-preload.ts) ---

type TestContext = { readonly file: string; readonly test: string; readonly counts: Map<string, number> }
let current: TestContext | undefined
let lastFile: string | undefined

export const registerFile = (file: string): void => {
  lastFile = file
}

export const testStart = (file: string, test: string): TestContext | undefined => {
  if (!enabled) return undefined
  if (current !== undefined) testEnd(current)
  lastFile = file
  current = { file, test, counts: new Map() }
  return current
}

export const testEnd = (context: TestContext | undefined): void => {
  if (!enabled || context === undefined || current !== context) return
  current = undefined
  write("tests.jsonl", { file: context.file, test: context.test, counts: Object.fromEntries(context.counts) })
}

const count = (kind: string): void => {
  if (current !== undefined) current.counts.set(kind, (current.counts.get(kind) ?? 0) + 1)
}

const where = (): { file: string; test?: string } =>
  current !== undefined ? { file: current.file, test: current.test } : { file: lastFile ?? "unknown" }

// --- Output ---

const PART_LIMIT = 9 * 1024 * 1024
const LINE_LIMIT = 2 * 1024 * 1024
const parts = new Map<string, { index: number; size: number }>()

const partPath = (name: string, index: number) =>
  path.join(DIR!, index === 1 ? name : name.replace(/\.jsonl$/, `.part${index}.jsonl`))

const sizeOf = (file: string) => {
  try {
    return fs.statSync(file).size
  } catch {
    return -1
  }
}

const partState = (name: string) => {
  let state = parts.get(name)
  if (state === undefined) {
    // Another process (pass 1/pass 2) may already have written this file.
    let index = 1
    while (sizeOf(partPath(name, index + 1)) >= 0) index++
    state = { index, size: Math.max(0, sizeOf(partPath(name, index))) }
    parts.set(name, state)
    fs.mkdirSync(path.dirname(partPath(name, 1)), { recursive: true })
  }
  return state
}

const truncateReplacer = (_key: string, value: unknown) => {
  if (typeof value === "string" && value.length > 20_000) {
    return { $truncatedString: { length: value.length, prefix: value.slice(0, 200) } }
  }
  if (Array.isArray(value) && value.length > 2_000) {
    return [...value.slice(0, 100), { $truncatedArray: value.length }]
  }
  return value
}

const write = (name: string, record: Record<string, unknown>): void => {
  let line = JSON.stringify(record)
  if (Buffer.byteLength(line) > LINE_LIMIT) {
    line = JSON.stringify({ ...record, truncated: true }, truncateReplacer)
  }
  const bytes = Buffer.byteLength(line) + 1
  const state = partState(name)
  if (state.size > 0 && state.size + bytes > PART_LIMIT) {
    state.index++
    state.size = 0
  }
  fs.appendFileSync(partPath(name, state.index), `${line}\n`)
  state.size += bytes
}

// --- Value encoding ---

const NODE_BUDGET = 200_000

const className = (value: unknown): string => {
  if (value === null) return "null"
  if (typeof value !== "object" && typeof value !== "function") return typeof value
  try {
    const name = (value as { constructor?: { name?: unknown } }).constructor?.name
    return typeof name === "string" && name !== "" ? name : "Object"
  } catch {
    return "Object"
  }
}

const isPlain = (value: object) => {
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

// Plain assignment would turn an own "__proto__" key into a prototype change.
const setKey = (target: Record<string, unknown>, key: string, value: unknown) =>
  Object.defineProperty(target, key, { value, enumerable: true, writable: true, configurable: true })

export const encode = (value: unknown): unknown => {
  let budget = NODE_BUDGET
  const ancestors = new Set<object>()
  const fields = (object: object): Record<string, unknown> => {
    const out: Record<string, unknown> = {}
    for (const key of Object.keys(object)) setKey(out, key, go((object as Record<string, unknown>)[key]))
    return out
  }
  const go = (item: unknown): unknown => {
    if (--budget < 0) return { $tooLarge: true }
    if (item === undefined) return { $undefined: true }
    if (item === null || typeof item === "string" || typeof item === "boolean") return item
    if (typeof item === "number") {
      if (Object.is(item, -0)) return { $number: "-0" }
      if (Number.isNaN(item)) return { $number: "NaN" }
      if (item === Infinity) return { $number: "Infinity" }
      if (item === -Infinity) return { $number: "-Infinity" }
      return item
    }
    if (typeof item === "bigint") return { $bigint: item.toString() }
    if (typeof item === "symbol") return { $symbol: item.description ?? null }
    if (typeof item === "function") return { $function: item.name }
    if (typeof item !== "object") return { $unencodable: `unknown type ${typeof item}` }
    if (ancestors.has(item)) return { $circular: true }
    ancestors.add(item)
    try {
      if (Array.isArray(item)) {
        const out: Array<unknown> = []
        for (let index = 0; index < item.length; index++) out.push(index in item ? go(item[index]) : { $hole: true })
        return out
      }
      if (item instanceof Date) return { $date: Number.isNaN(item.getTime()) ? null : item.toISOString() }
      if (item instanceof RegExp) return { $regexp: { source: item.source, flags: item.flags } }
      if (item instanceof Uint8Array) return { $bytes: Buffer.from(item).toString("base64") }
      if (item instanceof ArrayBuffer) return { $bytes: Buffer.from(new Uint8Array(item)).toString("base64") }
      if (item instanceof Map) return { $map: [...item].map(([key, entry]) => [go(key), go(entry)]) }
      if (item instanceof Set) return { $set: [...item].map(go) }
      if (item instanceof URL) return { $url: item.href }
      if (item instanceof URLSearchParams) return { $urlsearchparams: item.toString() }
      if (item instanceof Headers) return { $headers: [...item] }
      if (item instanceof Promise) return { $promise: true }
      if (item instanceof Error) {
        const error = item as Error & Record<string, unknown>
        const out: Record<string, unknown> = { name: go(error.name), message: go(error.message) }
        if (Object.hasOwn(error, "cause")) out.cause = go(error.cause)
        for (const key of Object.keys(error)) {
          if (key === "name" || key === "message" || key === "stack" || key === "cause") continue
          setKey(out, key, go(error[key]))
        }
        return { $error: out }
      }
      if (isPlain(item)) return fields(item)
      return { $instance: className(item), fields: fields(item) }
    } catch (error) {
      return { $unencodable: String(error) }
    } finally {
      ancestors.delete(item)
    }
  }
  try {
    return go(value)
  } catch (error) {
    return { $unencodable: String(error) }
  }
}

const messageOf = (error: unknown): string => {
  if (error instanceof Error) return error.message
  try {
    return String(error)
  } catch {
    return className(error)
  }
}

const throwsOf = (error: unknown) => ({
  name: error instanceof Error ? error.name : className(error),
  message: messageOf(error),
})

const attempt = (render: () => unknown): unknown => {
  try {
    return render()
  } catch (error) {
    return { $throws: messageOf(error) }
  }
}

// --- Schemas and tool trees ---

type SchemaRole = "input" | "output" | "schema"

const documentOf = (render: () => unknown) => {
  try {
    return encode(render())
  } catch {
    return null
  }
}

export const schemaRecord = (schema: unknown, role: SchemaRole): unknown => {
  if (Schema.isSchema(schema)) {
    const top = schema as Schema.Top
    const record: Record<string, unknown> = {
      kind: "effect",
      document: documentOf(() =>
        Schema.toJsonSchemaDocument(role === "output" ? Schema.toType(top) : top, { onExcessProperty: "error" }),
      ),
    }
    if (role !== "output") {
      record.emptyDocument = documentOf(
        () => Schema.toJsonSchemaDocument(top, { referencePolicy: () => undefined } as never).schema,
      )
    }
    if (role === "schema") {
      record.typeDocument = documentOf(() =>
        Schema.toJsonSchemaDocument(Schema.toType(top), { onExcessProperty: "error" }),
      )
    }
    return record
  }
  return { kind: "json", schema: encode(schema) }
}

export const toolExpression = (toolPath: string) =>
  "tools" +
  toolPath
    .split(".")
    .map((segment) => (identifierSegment.test(segment) ? `.${segment}` : `[${JSON.stringify(segment)}]`))
    .join("")

const renderTool = (tool: Tool<any>, toolPath: string, full: boolean) => {
  const record: Record<string, unknown> = {
    description: encode(tool.description),
    input: attempt(() => schemaRecord(tool.input, "input")),
    output: tool.output === undefined ? null : attempt(() => schemaRecord(tool.output, "output")),
  }
  if (!full) return record
  record.signature = attempt(() =>
    [
      toolExpression(toolPath),
      isEmptyInput(tool) ? "()" : `(${inputTypeScript(tool, true)})`,
      `: Promise<${outputTypeScript(tool, true)}>`,
    ].join(""),
  )
  record.inputTs = attempt(() => inputTypeScript(tool, false))
  record.outputTs = attempt(() => outputTypeScript(tool, false))
  record.emptyInput = attempt(() => isEmptyInput(tool))
  record.inputProperties = attempt(() => encode(inputProperties(tool)))
  return record
}

type TrieNode = {
  tool?: Tool<any>
  namespace?: { readonly description?: string }
  readonly children: Map<string, TrieNode>
}

const renderTrie = (node: TrieNode, segments: Array<string>, full: boolean): Record<string, unknown> => {
  const record: Record<string, unknown> = {
    children: [...node.children].map(([segment, child]) => [segment, renderTrie(child, [...segments, segment], full)]),
  }
  if (node.namespace !== undefined) {
    record.namespace = node.namespace.description === undefined ? {} : { description: node.namespace.description }
  }
  if (node.tool !== undefined) record.tool = renderTool(node.tool, segments.join("."), full)
  return record
}

const openapiTools = new WeakSet<object>()

const trieHasOpenAPI = (node: TrieNode): boolean =>
  (node.tool !== undefined && openapiTools.has(node.tool)) ||
  [...node.children.values()].some((child) => trieHasOpenAPI(child))

const treeCache = new WeakMap<object, { tree: Record<string, unknown>; openapi: boolean; json: string }>()

const treeOf = (root: TrieNode) => {
  let cached = treeCache.get(root)
  if (cached === undefined) {
    const tree = renderTrie(root, [], true)
    cached = { tree, openapi: trieHasOpenAPI(root), json: JSON.stringify(tree) }
    treeCache.set(root, cached)
  }
  return cached
}

// The same trie `ToolRuntime.prepare` builds, for tool records that never reach a runtime (OpenAPI.fromSpec).
const buildTrie = (tools: Record<string, unknown>): TrieNode => {
  const root: TrieNode = { children: new Map() }
  const insert = (node: TrieNode, group: Record<string, unknown>): void => {
    for (const [name, value] of Object.entries(group)) {
      let currentNode = node
      for (const segment of name.split(".")) {
        const child = currentNode.children.get(segment) ?? { children: new Map() }
        currentNode.children.set(segment, child)
        currentNode = child
      }
      if (isTool(value as never)) currentNode.tool = value as Tool<any>
      else if (isNamespace(value as never)) {
        currentNode.namespace = value as { description?: string }
        insert(currentNode, (value as { tools: Record<string, unknown> }).tools)
      } else insert(currentNode, value as Record<string, unknown>)
    }
  }
  insert(root, tools)
  return root
}

// --- Exits ---

const channelOf = (cause: Cause.Cause<unknown>) => (Cause.hasFails(cause) ? "fail" : "die")

const failureFields = (cause: Cause.Cause<unknown>) => {
  const error = Cause.squash(cause)
  return { error, message: messageOf(error), errorClass: className(error), channel: channelOf(cause) }
}

// The "turn token" of the README: valid until a microtask queued when it was taken runs.
const turnToken = () => {
  const token = { valid: true }
  queueMicrotask(() => {
    token.valid = false
  })
  return token
}

// --- Executions ---

const seqs = new Map<string, number>()
const open = new Set<Execution>()

type Event = Record<string, unknown>

export class Execution {
  private t = 0
  private nextId = 0
  private nextXid = 0
  readonly events: Array<Event> = []
  readonly file: string
  readonly test: string | undefined
  readonly seq: number
  private done = false

  constructor(
    readonly via: "codemode" | "executeProgram",
    readonly info: ExecutionInfo,
  ) {
    const place = where()
    this.file = place.file
    this.test = place.test
    this.seq = seqs.get(this.file) ?? 0
    seqs.set(this.file, this.seq + 1)
    open.add(this)
  }

  push(event: Event): Event {
    event.t = this.t++
    this.events.push(event)
    return event
  }

  tick(): number {
    return this.t++
  }

  callId(): number {
    return this.nextId++
  }

  extensionId(): number {
    return this.nextXid++
  }

  finish(ending: Record<string, unknown>): void {
    if (this.done) return
    this.done = true
    open.delete(this)
    const tree = treeOf(this.info.root as TrieNode)
    const record: Record<string, unknown> = { file: this.file }
    if (this.test !== undefined) record.test = this.test
    record.seq = this.seq
    record.via = this.via
    if (tree.openapi) record.openapi = true
    record.code = this.info.code
    record.limits = encode(
      Object.fromEntries(Object.entries(this.info.limits ?? {}).filter(([, value]) => value !== undefined)),
    )
    record.hooks = Object.keys(this.info.hooks ?? {})
    record.extensions =
      this.info.extensions === null
        ? null
        : this.info.extensions.map((extension) => ({
            name: extension.name,
            globals: Object.keys(extension.globals),
            async: Object.entries(extension.globals)
              .filter(([, global]) => typeof global === "function" && isAsync(global as Function))
              .map(([name]) => name),
          }))
    record.tree = tree.tree
    record.events = this.events
    Object.assign(record, ending)
    const name =
      this.file === "test262.test.ts" && this.test !== undefined
        ? `test262/${this.test.split("/").slice(0, 2).join("/")}.jsonl`
        : `${this.file.replace(/\.ts$/, "")}.jsonl`
    write(name, record)
  }
}

export type ExecutionInfo = {
  readonly code: string
  readonly root: unknown
  readonly limits: Record<string, number | undefined> | undefined
  readonly hooks: Record<string, unknown> | undefined
  readonly extensions: ReadonlyArray<{ readonly name: string; readonly globals: Record<string, unknown> }> | null
}

/**
 * Wraps one program execution. `run` builds the execution's Effect for a recording (or `undefined`); it is called once
 * per run of the returned Effect when recording, and once, eagerly, otherwise.
 */
export const execution = <A, E, R>(
  via: "codemode" | "executeProgram",
  info: ExecutionInfo,
  run: (recording: Execution | undefined) => Effect.Effect<A, E, R>,
): Effect.Effect<A, E, R> => {
  if (!enabled) return run(undefined)
  count("execute")
  return Effect.suspend(() => {
    const recording = new Execution(via, info)
    return Effect.onExit(run(recording), (exit) =>
      Effect.sync(() => {
        if (Exit.isSuccess(exit)) recording.finish({ result: encode(exit.value) })
        else if (Cause.hasInterruptsOnly(exit.cause)) recording.finish({ interrupted: true })
        else recording.finish({ defect: encode(Cause.squash(exit.cause)) })
      }),
    )
  })
}

process.on("exit", () => {
  for (const recording of [...open]) recording.finish({ unfinished: true })
  if (current !== undefined) testEnd(current)
})

const executions = new WeakMap<object, Execution>()

export const bindTools = (tools: object, recording: Execution | undefined): void => {
  if (recording !== undefined) executions.set(tools, recording)
}

export const executionOf = (tools: object): Execution | undefined => (enabled ? executions.get(tools) : undefined)

// --- Hooks ---

type HookFn = (...args: Array<any>) => Effect.Effect<void, unknown, any>

const recordHook = (
  recording: Execution,
  hook: string,
  idKey: "id" | "xid",
  id: number,
  fn: HookFn,
  onFailure?: (cause: Cause.Cause<unknown>) => void,
): HookFn => {
  return (...args: Array<any>) => {
    const event: Event = { type: "hook", hook, [idKey]: id, call: encode(args[0]) }
    if (args.length > 1) event.result = encode(args[1])
    recording.push(event)
    return Effect.onExit(fn(...args), (exit) =>
      Effect.sync(() => {
        if (Exit.isSuccess(exit)) event.outcome = "success"
        else if (Cause.hasInterruptsOnly(exit.cause)) event.outcome = "interrupted"
        else {
          const failure = failureFields(exit.cause)
          event.outcome = "failure"
          event.failed = failure.message
          event.errorClass = failure.errorClass
          event.errorValue = encode(failure.error)
          event.channel = failure.channel
        }
        event.settledT = recording.tick()
        if (!Exit.isSuccess(exit)) onFailure?.(exit.cause)
      }),
    )
  }
}

const wrapHooks = <H extends Record<string, unknown>>(
  hooks: H,
  names: ReadonlyArray<string>,
  wrap: (name: string, fn: HookFn) => HookFn,
): H => {
  const out: Record<string, unknown> = { ...hooks }
  for (const name of names) {
    const fn = hooks[name]
    if (typeof fn === "function") out[name] = wrap(name, fn as HookFn)
  }
  return out as H
}

// --- Tool calls ---

export class ToolCallRecording {
  private toolRan = false
  private toolFields: Record<string, unknown> = {}

  constructor(
    private readonly recording: Execution,
    readonly id: number,
  ) {}

  static start(
    recording: Execution | undefined,
    name: string,
    rawPath: ReadonlyArray<string> | undefined,
    args: ReadonlyArray<unknown>,
  ): ToolCallRecording | undefined {
    if (recording === undefined) return undefined
    const id = recording.callId()
    const event: Event = { type: "call", name }
    if (rawPath !== undefined) event.path = [...rawPath]
    event.args = encode(args)
    event.id = id
    recording.push(event)
    return new ToolCallRecording(recording, id)
  }

  resolveFailed(error: unknown): void {
    this.recording.push({ type: "resolve", id: this.id, ok: false, error: messageOf(error) })
  }

  decodeArity(error: unknown): void {
    this.recording.push({ type: "decode", id: this.id, ok: false, error: messageOf(error), arity: true })
  }

  decodeFailed(error: unknown): void {
    const event: Event = { type: "decode", id: this.id, ok: false, error: messageOf(error) }
    const suggestions = (error as { suggestions?: ReadonlyArray<string> }).suggestions
    if (suggestions !== undefined) event.suggestions = [...suggestions]
    this.recording.push(event)
  }

  decoded(input: unknown): void {
    this.recording.push({ type: "decode", id: this.id, ok: true, input: encode(input) })
  }

  admitted(count: number): void {
    this.recording.push({ type: "admit", id: this.id, ok: true, count })
  }

  admitFailed(error: unknown): void {
    this.recording.push({ type: "admit", id: this.id, ok: false, error: messageOf(error) })
  }

  hooks<H extends Record<string, unknown>>(hooks: H): H {
    return wrapHooks(hooks, ["tool.before", "tool.after"], (name, fn) =>
      recordHook(this.recording, name, "id", this.id, fn),
    )
  }

  /** The tool's own Effect. */
  tool<A, E, R>(effect: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> {
    return Effect.suspend(() => {
      this.toolRan = true
      const token = turnToken()
      const started = performance.now()
      return Effect.onExit(effect, (exit) =>
        Effect.sync(() => {
          const fields: Record<string, unknown> = {}
          if (Exit.isSuccess(exit)) {
            fields.toolOutcome = "success"
            fields.raw = encode(exit.value)
          } else if (Cause.hasInterruptsOnly(exit.cause)) fields.toolOutcome = "interrupted"
          else {
            const failure = failureFields(exit.cause)
            fields.toolOutcome = "failure"
            fields.rawError = {
              class: failure.errorClass,
              channel: failure.channel,
              message: failure.message,
              value: encode(failure.error),
            }
          }
          fields.sync = token.valid
          fields.toolMs = Math.round(performance.now() - started)
          this.toolFields = fields
        }),
      )
    })
  }

  /** The whole call, from resolution to the JSON handed back. */
  call<A, E, R>(effect: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> {
    const token = turnToken()
    return Effect.onExit(effect, (exit) =>
      Effect.sync(() => {
        const event: Event = { type: "settle", id: this.id }
        if (Exit.isSuccess(exit)) {
          event.outcome = "success"
          event.value = encode(exit.value)
        } else if (Cause.hasInterruptsOnly(exit.cause)) event.outcome = "interrupted"
        else {
          const failure = failureFields(exit.cause)
          const error = failure.error as { name?: unknown; kind?: unknown } | undefined
          const runtime = error instanceof Error && error.name === "ToolRuntimeError" && typeof error.kind === "string"
          event.outcome = runtime ? (error.kind === "InvalidToolOutput" ? "invalidOutput" : "rejected") : "failure"
          event.error = failure.message
          event.errorClass = failure.errorClass
          if (runtime) event.errorKind = error.kind
          else if (failure.errorClass !== "ToolError") event.errorValue = encode(failure.error)
          event.channel = failure.channel
        }
        event.toolRan = this.toolRan
        if (this.toolRan) Object.assign(event, this.toolFields)
        event.callSync = token.valid
        this.recording.push(event)
      }),
    )
  }
}

// --- Extension calls ---

export class ExtensionCallRecording {
  private settled = false

  private constructor(
    private readonly recording: Execution,
    private readonly event: Event,
  ) {}

  static start(
    tools: object,
    label: string,
    call: { readonly extension: string; readonly name: string } | undefined,
    args: ReadonlyArray<unknown>,
    host: Function,
  ): ExtensionCallRecording | undefined {
    const recording = executionOf(tools)
    if (recording === undefined) return undefined
    const event: Event = { type: "extension", xid: recording.extensionId(), label, global: call !== undefined }
    if (call !== undefined) {
      event.extension = call.extension
      event.name = call.name
    }
    event.async = isAsync(host)
    event.args = encode(args)
    recording.push(event)
    return new ExtensionCallRecording(recording, event)
  }

  hooks<H extends Record<string, unknown>>(hooks: H): H {
    return wrapHooks(hooks, ["extension.before", "extension.after"], (name, fn) =>
      recordHook(
        this.recording,
        name,
        "xid",
        this.event.xid as number,
        fn,
        name === "extension.before"
          ? (cause) => {
              // Denied: the host function never runs; record how the global fails instead.
              if (this.event.returned === undefined && !Cause.hasInterruptsOnly(cause)) this.returned(this.event.async ? "promise" : "throw")
              this.end(Exit.failCause(cause))
            }
          : undefined,
      ),
    )
  }

  returned(kind: "value" | "throw" | "promise"): void {
    this.event.returned = kind
  }

  end(exit: Exit.Exit<unknown, unknown>): void {
    if (this.settled) return
    this.settled = true
    const event = this.event
    if (Exit.isSuccess(exit)) {
      event.outcome = "success"
      event.value = encode(exit.value)
    } else if (Cause.hasInterruptsOnly(exit.cause)) event.outcome = "interrupted"
    else {
      const failure = failureFields(exit.cause)
      event.outcome = "failure"
      event.error = failure.message
      event.errorClass = failure.errorClass
      event.errorValue = encode(failure.error)
      event.channel = failure.channel
    }
    event.settledT = this.recording.tick()
  }

  /** Taps how the call settled, hooks included, before its value crosses back. */
  settle<A, E, R>(effect: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> {
    return Effect.onExit(effect, (exit) => Effect.sync(() => this.end(exit)))
  }
}

// --- Catalog reads, make errors, direct schema calls ---

const catalogs = new Set<string>()

export const catalog = (root: unknown, entries: ReadonlyArray<unknown>): void => {
  if (!enabled) return
  count("catalog")
  const place = where()
  const tree = treeOf(root as TrieNode)
  const encoded = encode(entries)
  const key = `${place.file}\u0000${tree.json}\u0000${JSON.stringify(encoded)}`
  if (catalogs.has(key)) return
  catalogs.add(key)
  const record: Record<string, unknown> = { ...place }
  if (tree.openapi) record.openapi = true
  record.tree = tree.tree
  record.catalog = encoded
  write("catalog.jsonl", record)
}

const encodeToolsConfig = (tools: unknown): unknown => {
  if (tools === null || typeof tools !== "object") return encode(tools)
  if (isTool(tools as never)) return { $tool: { description: encode((tools as Tool<any>).description) } }
  if (isNamespace(tools as never)) {
    const namespace = tools as { description?: string; tools: unknown }
    const inner: Record<string, unknown> = {}
    if (namespace.description !== undefined) inner.description = namespace.description
    inner.tools = encodeToolsConfig(namespace.tools)
    return { $namespace: inner }
  }
  const out: Record<string, unknown> = {}
  for (const [key, value] of Object.entries(tools)) setKey(out, key, encodeToolsConfig(value))
  return out
}

export const makeError = (
  options: {
    readonly tools?: unknown
    readonly limits?: unknown
    readonly extensions?: ReadonlyArray<{ readonly name: string; readonly globals: Record<string, unknown> }>
  },
  error: unknown,
): void => {
  if (!enabled) return
  count("makeError")
  write("make-errors.jsonl", {
    ...where(),
    tools: encodeToolsConfig(options.tools ?? {}),
    limits: encode(options.limits),
    extensions: (options.extensions ?? []).map((extension) => ({
      name: extension.name,
      globals: Object.keys(extension.globals),
    })),
    error: throwsOf(error),
  })
}

const toolArgument = (tool: Tool<any>) => ({
  tool: {
    description: encode(tool.description),
    input: schemaRecord(tool.input, "input"),
    output: tool.output === undefined ? null : schemaRecord(tool.output, "output"),
  },
})

const schemaArguments: Record<string, (args: ReadonlyArray<unknown>) => Array<unknown>> = {
  toTypeScript: (args) => [schemaRecord(args[0], "schema"), ...args.slice(1).map(encode)],
  jsonSchemaToTypeScript: (args) => [{ kind: "json", schema: encode(args[0]) }, ...args.slice(1).map(encode)],
}

export const schemaCall = <F extends (...args: Array<any>) => unknown>(fn: string, impl: F): F => {
  if (!enabled) return impl
  const encodeArgs =
    schemaArguments[fn] ?? ((args) => [toolArgument(args[0] as Tool<any>), ...args.slice(1).map(encode)])
  return ((...args: Array<unknown>) => {
    count("schemaCall")
    const record: Record<string, unknown> = { ...where(), fn, args: encodeArgs(args) }
    try {
      const result = impl(...args)
      record.result = encode(result)
      write("schema-calls.jsonl", record)
      return result
    } catch (error) {
      record.throws = throwsOf(error)
      write("schema-calls.jsonl", record)
      throw error
    }
  }) as F
}

// --- OpenAPI ---

const markOpenAPI = (tools: unknown): void => {
  if (tools === null || typeof tools !== "object") return
  if (isTool(tools as never)) {
    openapiTools.add(tools)
    return
  }
  for (const value of Object.values(tools)) markOpenAPI(value)
}

export const fromSpec = <O extends { readonly spec: unknown }, Result extends { tools: unknown; skipped: unknown }>(
  impl: (options: O) => Result,
): ((options: O) => Result) => {
  if (!enabled) return impl
  return (options) => {
    count("fromSpec")
    const { spec, ...rest } = options as O & Record<string, unknown>
    const record: Record<string, unknown> = { ...where(), spec: encode(spec), options: encode(rest) }
    try {
      const result = impl(options)
      markOpenAPI(result.tools)
      record.tree = renderTrie(buildTrie(result.tools as Record<string, unknown>), [], false)
      record.skipped = encode(result.skipped)
      write("openapi-fromspec.jsonl", record)
      return result
    } catch (error) {
      record.throws = throwsOf(error)
      write("openapi-fromspec.jsonl", record)
      throw error
    }
  }
}

export class HttpRecording {
  private requestRecord: unknown = null
  private responseRecord: Record<string, unknown> | null = null

  request(request: HttpClientRequest.HttpClientRequest): void {
    const url = HttpClientRequest.toUrl(request)
    const body = request.body as { _tag: string; contentType?: string; body?: unknown }
    const bodyRecord: Record<string, unknown> = { tag: body._tag }
    if (body.contentType !== undefined) bodyRecord.contentType = body.contentType
    if (body._tag === "Uint8Array" && body.body instanceof Uint8Array) {
      bodyRecord.text = new TextDecoder().decode(body.body)
    } else if (body._tag === "Raw" && typeof body.body === "string") bodyRecord.text = body.body
    this.requestRecord = {
      method: request.method,
      url: url._tag === "Some" ? url.value.href : request.url,
      headers: encode({ ...request.headers }),
      body: bodyRecord,
    }
  }

  response(response: { readonly status: number; readonly headers: Record<string, string> }): void {
    this.responseRecord = { status: response.status, headers: encode({ ...response.headers }) }
  }

  body(text: string): void {
    if (this.responseRecord !== null) this.responseRecord.body = text
  }

  static wrap<A, E, R>(
    plan: { readonly operation: unknown; readonly url: string },
    input: unknown,
    run: (recording: HttpRecording | undefined) => Effect.Effect<A, E, R>,
  ): Effect.Effect<A, E, R> {
    if (!enabled) return run(undefined)
    return Effect.suspend(() => {
      count("openapiInvoke")
      const place = where()
      const recording = new HttpRecording()
      const encodedInput = encode(input)
      return Effect.onExit(run(recording), (exit) =>
        Effect.sync(() => {
          const record: Record<string, unknown> = {
            ...place,
            operation: encode(plan.operation),
            planUrl: plan.url,
            input: encodedInput,
            request: recording.requestRecord,
            response: recording.responseRecord,
          }
          if (Exit.isSuccess(exit)) {
            record.outcome = "success"
            record.value = encode(exit.value)
          } else if (Cause.hasInterruptsOnly(exit.cause)) record.outcome = "interrupted"
          else {
            const failure = failureFields(exit.cause)
            record.outcome = "failure"
            record.error = failure.message
            record.errorClass = failure.errorClass
          }
          write("openapi-http.jsonl", record)
        }),
      )
    })
  }
}
