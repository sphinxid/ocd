// Transform domains.
//
// Rust owns each domain's state. To rebuild a domain it sends the value produced by the native
// transforms registered before the plugins (`transform {domain, value}`); the sidecar turns that
// JSON snapshot into the in-memory shape upstream's editor works on, runs every plugin transform for
// the domain in registration order against the upstream editor semantics (packages/core/src/<domain>.ts
// editor + packages/core/src/plugin/host.ts wrapping), and returns the snapshot again.
//
// Functions inside a snapshot cross as `{ "$fn": handle }` (see codec.ts).

import { Agent } from "@opencode/schema/agent"
import { Integration } from "@opencode/schema/integration"
import { ServerConfig } from "@opencode/schema/mcp"
import { Model } from "@opencode/schema/model"
import { Provider } from "@opencode/schema/provider"
import { Reference } from "@opencode/schema/reference"
import { Skill } from "@opencode/schema/skill"
import { Tool } from "@opencode/schema/tool"
import { ToolDefinition } from "@opencode/ai/effect"
import { Result, Schema, SchemaIssue } from "effect"
import { definition, effectiveName, normalizedName } from "@upstream-core/tool/runtime"
import { FN, fromWire, isWireFn, stub, stubHandle, toWire, type Handles, type Remote } from "./codec.js"
import { log } from "./peer.js"

export const DOMAINS = [
  "agent",
  "command",
  "integration",
  "mcp",
  "model",
  "provider",
  "reference",
  "skill",
  "tool",
  "vcs",
  "websearch",
  "worktree",
] as const
export type Domain = (typeof DOMAINS)[number]

export interface Codec {
  readonly handles: Handles
  readonly remote: Remote
  /** Handle group for functions minted while encoding this domain. */
  readonly group: string
}

export interface Spec<State, Editor> {
  readonly decode: (value: any, codec: Codec) => State
  readonly editor: (state: State) => Editor
  readonly encode: (state: State, codec: Codec) => unknown
}

// ---------------------------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------------------------

function decodeWith<A>(schema: Schema.Top, value: unknown): A {
  try {
    return structuredClone(Schema.decodeUnknownSync(schema as Schema.Codec<unknown, unknown>)(value)) as A
  } catch {
    return structuredClone(value) as A
  }
}

function encodeWith(schema: Schema.Top, value: unknown): unknown {
  try {
    return toWire(Schema.encodeUnknownSync(schema as Schema.Codec<unknown, unknown>)(value), undefined)
  } catch {
    return toWire(value, undefined)
  }
}

const wireFunctions = (value: unknown, codec: Codec) => toWire(value, codec.handles, codec.group)
const liveFunctions = (value: unknown, codec: Codec) => fromWire(value, codec.handles, codec.remote)

// ---------------------------------------------------------------------------------------------
// agent (core/src/agent.ts)
// ---------------------------------------------------------------------------------------------

type AgentState = { agents: Map<string, any>; default?: string }

const agent: Spec<AgentState, unknown> = {
  decode: (value) => ({
    agents: new Map(
      ((value?.agents ?? []) as any[]).map((item) => [item.id, decodeWith<any>(Agent.Info, item)]),
    ),
    ...(value?.default === undefined || value?.default === null ? {} : { default: value.default }),
  }),
  editor: (state) => ({
    list: () => Array.from(state.agents.values()),
    get: (id: string) => state.agents.get(id),
    default: (id: string | undefined) => {
      state.default = id
    },
    update: (id: string, fn: (agent: any) => void) => {
      const current = state.agents.get(id) ?? (Agent.Info as any).default(id)
      if (!state.agents.has(id)) state.agents.set(id, current)
      fn(current)
      current.id = id
    },
    remove: (id: string) => {
      state.agents.delete(id)
    },
  }),
  encode: (state) => ({
    agents: Array.from(state.agents.values(), (item) => encodeWith(Agent.Info, item)),
    ...(state.default === undefined ? {} : { default: state.default }),
  }),
}

// ---------------------------------------------------------------------------------------------
// command (core/src/command.ts)
// ---------------------------------------------------------------------------------------------

type CommandState = Map<string, { name: string; description?: string; execute: Function }>

const command: Spec<CommandState, unknown> = {
  decode: (value, codec) =>
    new Map(
      ((value?.commands ?? []) as any[]).map((item) => [
        item.name,
        {
          name: item.name,
          ...(item.description === undefined || item.description === null ? {} : { description: item.description }),
          execute: liveFunctions(item.execute, codec) as Function,
        },
      ]),
    ),
  editor: (state) => ({
    add: (definition: { name: string; description?: string; execute: Function }) => state.set(definition.name, definition),
  }),
  encode: (state, codec) => ({
    commands: Array.from(state.values(), (item) => ({
      name: item.name,
      ...(item.description === undefined ? {} : { description: item.description }),
      execute: wireFunctions(item.execute, codec),
    })),
  }),
}

// ---------------------------------------------------------------------------------------------
// skill (core/src/skill.ts)
// ---------------------------------------------------------------------------------------------

type SkillState = { skills: Map<string, any> }

const skill: Spec<SkillState, unknown> = {
  decode: (value) => ({
    skills: new Map(((value?.skills ?? []) as any[]).map((item) => [item.id, decodeWith<any>(Skill.Info, item)])),
  }),
  editor: (state) => ({
    list: () => Array.from(state.skills.values()),
    get: (id: string) => state.skills.get(id),
    add: (value: unknown) => {
      const decoded = Schema.decodeUnknownSync(Skill.Info)(value) as any
      state.skills.set(decoded.id, { ...decoded })
    },
    update: (id: string, update: (skill: any) => void) => {
      const current = state.skills.get(id)
      if (!current) return
      update(current)
      current.id = id
    },
    remove: (id: string) => {
      state.skills.delete(id)
    },
  }),
  encode: (state) => ({ skills: Array.from(state.skills.values(), (item) => encodeWith(Skill.Info, item)) }),
}

// ---------------------------------------------------------------------------------------------
// reference (core/src/reference.ts)
// ---------------------------------------------------------------------------------------------

type ReferenceState = { sources: Map<string, any> }

const reference: Spec<ReferenceState, unknown> = {
  decode: (value) => ({
    sources: new Map(
      ((value?.sources ?? []) as any[]).map(([name, source]) => [name, decodeWith<any>(Reference.Source, source)]),
    ),
  }),
  editor: (state) => ({
    add: (name: string, source: unknown) => state.sources.set(name, Schema.decodeUnknownSync(Reference.Source)(source)),
    remove: (name: string) => state.sources.delete(name),
    list: () => Array.from(state.sources),
    get: (name: string) => state.sources.get(name),
  }),
  encode: (state) => ({
    sources: Array.from(state.sources, ([name, source]) => [name, encodeWith(Reference.Source, source)]),
  }),
}

// ---------------------------------------------------------------------------------------------
// mcp (core/src/mcp/index.ts)
// ---------------------------------------------------------------------------------------------

type McpState = { servers: Map<string, any>; removed: Set<string> }

const mcp: Spec<McpState, unknown> = {
  decode: (value) => ({
    servers: new Map(
      ((value?.servers ?? []) as any[]).map(([name, config]) => [name, decodeWith<any>(ServerConfig, config)]),
    ),
    removed: new Set((value?.removed ?? []) as string[]),
  }),
  editor: (state) => ({
    list: () => Array.from(state.servers),
    get: (name: string) => state.servers.get(name),
    set: (name: string, config: unknown) => {
      const decoded = Schema.decodeUnknownSync(ServerConfig)(config)
      if (state.removed.has(name)) return
      state.servers.set(name, structuredClone(decoded))
    },
    update: (name: string, update: (config: any) => void) => {
      const current = state.servers.get(name)
      if (!current) return
      update(current)
    },
    remove: (name: string) => state.servers.delete(name),
  }),
  encode: (state) => ({
    servers: Array.from(state.servers, ([name, config]) => [name, encodeWith(ServerConfig, config)]),
    removed: Array.from(state.removed),
  }),
}

// ---------------------------------------------------------------------------------------------
// tool (core/src/tool.ts)
// ---------------------------------------------------------------------------------------------

type ToolInfo = Tool.Info & { id: string }
type ToolState = {
  tools: Map<string, ToolInfo>
  namespaces: Map<string, Tool.Namespace>
  errors: Array<{ kind: "tool" | "namespace"; name: string; namespace?: string; message: string }>
}

const formatSchemaIssue = SchemaIssue.makeFormatterDefault()

function schemaMakeError(error: unknown) {
  if (error instanceof Error && SchemaIssue.isIssue(error.cause)) return formatSchemaIssue(error.cause)
  return error instanceof Error ? error.message : String(error)
}

function namespaceError(name: string) {
  if (name.split(".").every((segment) => /^[A-Za-z0-9_-]+$/.test(segment))) return
  return `Invalid tool namespace: ${JSON.stringify(name)}`
}

function registrationError(tool: Tool.Info): { name: string; message: string } | undefined {
  const namespace = tool.options?.namespace
  if (namespace !== undefined) {
    const error = namespaceError(namespace)
    if (error) return { name: namespace, message: error }
  }
  const name = normalizedName(tool)
  if (!/^[A-Za-z0-9_-]+$/.test(name)) return { name, message: `Invalid tool name: ${name}` }
  const id = effectiveName(tool)
  if (tool.options?.codemode === false && id === "execute")
    return { name: id, message: 'Tool name "execute" is reserved for CodeMode' }
  const result = Result.try({
    try: () => ToolDefinition.make(definition(tool) as any),
    catch: (error) => ({ name: id, message: `Invalid tool definition ${id}: ${schemaMakeError(error)}` }),
  })
  return Result.isFailure(result) ? result.failure : undefined
}

/** Hydrates one tool from the wire: a sidecar tool handle yields the original Tool.Info. */
export function decodeTool(item: any, codec: Codec): ToolInfo {
  if (isWireFn(item?.execute)) {
    const entry = codec.handles.get(item.execute[FN])
    if (entry?.kind === "tool") {
      const original = entry.value as ToolInfo
      // Rust may have renamed or re-described the entry; the executable schemas stay the original ones.
      return { ...original, id: item.id ?? original.id }
    }
  }
  return {
    id: item.id ?? item.name,
    name: item.name,
    description: item.description,
    input: item.input ?? { type: "object", properties: {} },
    ...(item.output === undefined || item.output === null ? {} : { output: item.output }),
    ...(item.options === undefined || item.options === null ? {} : { options: item.options }),
    execute: isWireFn(item.execute)
      ? (stub(item.execute[FN], codec.remote) as unknown as Tool.Info["execute"])
      : ((() => {
          throw new Error(`Tool has no executor: ${item.name}`)
        }) as unknown as Tool.Info["execute"]),
  } as ToolInfo
}

/** Serializes one tool; plugin executors become a `tool` handle bound to the whole Tool.Info. */
export function encodeTool(tool: ToolInfo, codec: Codec) {
  const native = stubHandle(tool.execute)
  const def = definition(tool)
  return {
    id: tool.id,
    name: tool.name,
    description: tool.description,
    input: def.inputSchema,
    ...(def.outputSchema === undefined ? {} : { output: def.outputSchema }),
    ...(tool.options === undefined ? {} : { options: toWire(tool.options, undefined) }),
    execute: {
      [FN]: native ?? codec.handles.register(tool, { group: codec.group, kind: "tool" }),
    },
  }
}

const tool: Spec<ToolState, unknown> = {
  decode: (value, codec) => ({
    tools: new Map(((value?.tools ?? []) as any[]).map((item) => {
      const decoded = decodeTool(item, codec)
      return [decoded.id, decoded]
    })),
    namespaces: new Map(((value?.namespaces ?? []) as any[]).map((item) => [item.name, { ...item }])),
    errors: [],
  }),
  editor: (editor) => ({
    list: () => Array.from(editor.tools.values()),
    get: (id: string) => editor.tools.get(id),
    namespace: (namespace: Tool.Namespace) => {
      const error = namespaceError(namespace.name)
      if (error) {
        editor.errors.push({ kind: "namespace", name: namespace.name, namespace: namespace.name, message: error })
        return
      }
      editor.namespaces.set(namespace.name, { ...namespace })
    },
    add: (tool: Tool.Info) => {
      const error = registrationError(tool)
      if (error) {
        editor.errors.push({ kind: "tool", name: tool.name, namespace: tool.options?.namespace, message: error.message })
        return
      }
      const id = effectiveName(tool)
      editor.tools.set(id, { ...tool, id, options: tool.options && { ...tool.options } } as ToolInfo)
    },
    update: (id: string, update: (tool: any) => void) => {
      const current = editor.tools.get(id)
      if (!current) return
      const next: any = { ...current, options: current.options && { ...current.options } }
      update(next)
      next.name = current.name
      next.id = id
      if (next.options?.namespace !== current.options?.namespace)
        next.options = { ...next.options, namespace: current.options?.namespace }
      const error = registrationError(next)
      if (error) {
        editor.errors.push({ kind: "tool", name: next.name, namespace: next.options?.namespace, message: error.message })
        return
      }
      editor.tools.set(id, next)
    },
    remove: (id: string) => {
      editor.tools.delete(id)
    },
  }),
  encode: (state, codec) => {
    for (const error of state.errors)
      log("error", `Skipping invalid ${error.kind} registration`, {
        name: error.name,
        namespace: error.namespace,
        error: error.message,
      })
    return {
      tools: Array.from(state.tools.values(), (item) => encodeTool(item, codec)),
      namespaces: Array.from(state.namespaces.values()),
      errors: state.errors,
    }
  },
}

// ---------------------------------------------------------------------------------------------
// vcs / websearch / worktree: add-only registries of function-bearing definitions
// ---------------------------------------------------------------------------------------------

type Registry = { providers: Map<string, any>; selection?: unknown }

function registry(key: "providers"): Spec<Registry, unknown> {
  return {
    decode: (value, codec) => ({
      providers: new Map(
        ((value?.[key] ?? []) as any[]).map((item) => [item.id, liveFunctions(item, codec) as any]),
      ),
      ...(value?.selection === undefined || value?.selection === null ? {} : { selection: value.selection }),
    }),
    editor: (state) => ({
      add: (provider: { id: string }) => state.providers.set(provider.id, provider),
      default: {
        get: () => state.selection,
        set: (selection: unknown) => {
          state.selection = selection
        },
      },
    }),
    encode: (state, codec) => ({
      [key]: Array.from(state.providers.values(), (item) => wireFunctions(item, codec)),
      ...(state.selection === undefined ? {} : { selection: state.selection }),
    }),
  }
}

type WorktreeState = { strategies: Map<string, any>; selected?: string }

const worktree: Spec<WorktreeState, unknown> = {
  decode: (value, codec) => ({
    strategies: new Map(
      ((value?.strategies ?? []) as any[]).map((item) => [item.id, liveFunctions(item, codec) as any]),
    ),
    ...(value?.selected === undefined || value?.selected === null ? {} : { selected: value.selected }),
  }),
  editor: (state) => ({
    add: (strategy: { id: string }) => {
      state.strategies.delete(strategy.id)
      state.strategies.set(strategy.id, strategy)
      state.selected = strategy.id
    },
  }),
  encode: (state, codec) => ({
    strategies: Array.from(state.strategies.values(), (item) => wireFunctions(item, codec)),
    ...(state.selected === undefined ? {} : { selected: state.selected }),
  }),
}

// ---------------------------------------------------------------------------------------------
// provider (core/src/provider.ts)
// ---------------------------------------------------------------------------------------------

type ProviderRecord = { provider: any; models: Map<string, any>; sourceConnection?: unknown }

// Catalog values arrive in their wire form and are decoded on first access. A plugin transform
// usually touches a few providers, while the catalog holds every models.dev provider (thousands of
// models): decoding and re-encoding all of them through the schemas made each rebuild cost about a
// second. Entries a transform never reached go back as `{ "$same": ... }` and the core keeps its own
// copy, like upstream, whose transforms edit its in-memory records without a schema round trip.
const RAW = Symbol("raw")
type Raw = { readonly [RAW]: any }
const isRaw = (value: unknown): value is Raw => typeof value === "object" && value !== null && RAW in value
const raw = (value: any): Raw => ({ [RAW]: value })

type ProviderState = Map<string, ProviderRecord | Raw>

function lazyRecords(items: any[]): ProviderState {
  return new Map(items.map((item) => [item?.provider?.id, raw(item)]))
}

/** The record of `id`, decoded on first access (in place, so the map keeps its order). */
function recordAt(records: ProviderState, id: string): ProviderRecord | undefined {
  const current = records.get(id)
  if (current === undefined || !isRaw(current)) return current
  const record = decodeRecord(current[RAW])
  records.set(id, record)
  return record
}

function allRecords(records: ProviderState): ProviderRecord[] {
  return Array.from(records.keys(), (id) => recordAt(records, id)!)
}

function decodeRecord(item: any): ProviderRecord {
  const provider = decodeWith<any>(Provider.Info, item.provider)
  return {
    provider,
    models: new Map(
      ((item.models ?? []) as any[]).map((model) => {
        const decoded = decodeWith<any>(Model.Info, model)
        return [decoded.id, decoded.providerID === provider.id ? decoded : { ...decoded, providerID: provider.id }]
      }),
    ),
    ...(item.sourceConnection === undefined || item.sourceConnection === null
      ? {}
      : { sourceConnection: structuredClone(item.sourceConnection) }),
  }
}

function encodeRecord(record: ProviderRecord) {
  return {
    provider: encodeWith(Provider.Info, record.provider),
    models: Array.from(record.models.values(), (model) => encodeWith(Model.Info, model)),
    ...(record.sourceConnection === undefined ? {} : { sourceConnection: toWire(record.sourceConnection, undefined) }),
  }
}

// TODO(phase 2): upstream also runs AISDKNative.rewrite on provider/model edits (aisdk: package specifiers);
// the Rust core applies its own provider normalisation after the transform.
const provider: Spec<ProviderState, unknown> = {
  decode: (value) => lazyRecords((value?.records ?? []) as any[]),
  editor: (records) => {
    const entry = (id: string) => {
      const current = recordAt(records, id)
      if (current) return current
      const added = { provider: (Provider.Info as any).empty(id), models: new Map<string, any>() }
      records.set(id, added)
      return added
    }
    return {
      list: () => allRecords(records),
      get: (id: string) => recordAt(records, id),
      add: (definition: { info: any; models: readonly any[]; sourceConnection?: unknown }) => {
        records.set(definition.info.id, {
          provider: structuredClone(definition.info),
          models: new Map(
            definition.models.map((model) => [
              model.id,
              model.providerID === definition.info.id ? model : { ...model, providerID: definition.info.id },
            ]),
          ),
          ...(definition.sourceConnection === undefined ? {} : { sourceConnection: definition.sourceConnection }),
        })
      },
      update: (id: string, update: (provider: any) => void) => {
        const record = entry(id)
        update(record.provider)
        record.provider.id = id
      },
      remove: (id: string) => {
        records.delete(id)
      },
      models: {
        set: (id: string, values: readonly any[]) => {
          entry(id).models = new Map(values.map((model) => [model.id, model.providerID === id ? model : { ...model, providerID: id }]))
        },
        update: (providerID: string, modelID: string, update: (model: any) => void) => {
          const record = entry(providerID)
          const current = record.models.get(modelID)
          const model = current ? structuredClone(current) : (Model.Info as any).default(providerID, modelID)
          update(model)
          model.id = modelID
          model.providerID = providerID
          record.models.set(modelID, model)
        },
        remove: (providerID: string, modelID: string) => {
          recordAt(records, providerID)?.models.delete(modelID)
        },
      },
    }
  },
  encode: (records) => ({
    records: Array.from(records, ([id, record]) => (isRaw(record) ? { $same: id } : encodeRecord(record))),
  }),
}

// ---------------------------------------------------------------------------------------------
// model (core/src/model.ts)
// ---------------------------------------------------------------------------------------------

type ModelState = {
  models: Map<string, Map<string, any> | Raw>
  defaultModel?: { providerID: string; modelID: string }
  providers: ProviderState
}

/** The models of `providerID`, decoded on first access (see `RAW`). */
function modelsAt(data: ModelState, providerID: string): Map<string, any> | undefined {
  const current = data.models.get(providerID)
  if (current === undefined || !isRaw(current)) return current
  const models = new Map(((current[RAW] ?? []) as any[]).map((item) => [item.id, decodeWith<any>(Model.Info, item)]))
  data.models.set(providerID, models)
  return models
}

const model: Spec<ModelState, unknown> = {
  decode: (value) => ({
    models: new Map(((value?.models ?? []) as any[]).map(([providerID, models]) => [providerID, raw(models)])),
    ...(value?.default ? { defaultModel: { ...value.default } } : {}),
    providers: lazyRecords((value?.providers ?? []) as any[]),
  }),
  editor: (data) => ({
    list: (providerID?: string) => {
      const ids = providerID === undefined ? Array.from(data.models.keys()) : [providerID]
      return ids.flatMap((id) => Array.from(modelsAt(data, id)?.values() ?? []))
    },
    get: (providerID: string, modelID: string) => modelsAt(data, providerID)?.get(modelID),
    update: (providerID: string, modelID: string, update: (model: any) => void) => {
      const models = modelsAt(data, providerID)
      if (!models) return
      const model = models.get(modelID) ?? (Model.Info as any).default(providerID, modelID)
      update(model)
      model.id = modelID
      model.providerID = providerID
      models.set(modelID, model)
    },
    remove: (providerID: string, modelID: string) => {
      modelsAt(data, providerID)?.delete(modelID)
    },
    default: {
      get: () => data.defaultModel,
      set: (providerID: string, modelID: string) => {
        data.defaultModel = { providerID, modelID }
      },
    },
    provider: {
      list: () => allRecords(data.providers),
      get: (providerID: string) => recordAt(data.providers, providerID),
    },
  }),
  encode: (data) => ({
    models: Array.from(data.models, ([providerID, models]) => [
      providerID,
      isRaw(models) ? { $same: true } : Array.from(models.values(), (item) => encodeWith(Model.Info, item)),
    ]),
    ...(data.defaultModel === undefined ? {} : { default: data.defaultModel }),
  }),
}

// ---------------------------------------------------------------------------------------------
// integration (core/src/integration.ts)
// ---------------------------------------------------------------------------------------------

type IntegrationEntry = { ref: { id: string; name: string }; methods: any[]; implementations: Map<string, any> }
type IntegrationState = Map<string, IntegrationEntry>

const sameMethod = (left: any, right: any) => {
  if (left.type !== right.type) return false
  if (left.type === "oauth" || left.type === "command" || left.type === "external") return left.id === right.id
  return true
}

const integration: Spec<IntegrationState, unknown> = {
  decode: (value, codec) =>
    new Map(
      ((value?.integrations ?? []) as any[]).map((item) => [
        item.ref.id,
        {
          ref: { ...item.ref },
          methods: structuredClone(item.methods ?? []),
          implementations: new Map(
            ((item.implementations ?? []) as any[]).map((implementation) => [
              implementation.method.id,
              liveFunctions(implementation, codec),
            ]),
          ),
        },
      ]),
    ),
  editor: (integrations) => ({
    list: () => Array.from(integrations.values(), (entry) => entry.ref),
    get: (id: string) => integrations.get(id)?.ref,
    update: (id: string, update: (ref: { id: string; name: string }) => void) => {
      const current = integrations.get(id) ?? { ref: { id, name: id }, methods: [], implementations: new Map() }
      if (!integrations.has(id)) integrations.set(id, current)
      update(current.ref)
      current.ref.id = id
    },
    remove: (id: string) => integrations.delete(id),
    method: {
      list: (integrationID: string) => integrations.get(integrationID)?.methods ?? [],
      update: (implementation: any) => {
        const current = integrations.get(implementation.integrationID) ?? {
          ref: { id: implementation.integrationID, name: implementation.integrationID },
          methods: [],
          implementations: new Map(),
        }
        if (!integrations.has(implementation.integrationID)) integrations.set(implementation.integrationID, current)
        const index = current.methods.findIndex((method) => sameMethod(method, implementation.method))
        if (index === -1) current.methods.push(implementation.method)
        else current.methods[index] = implementation.method
        if (implementation.method.type === "oauth") current.implementations.set(implementation.method.id, implementation)
      },
      remove: (integrationID: string, method: unknown) => {
        const decoded = Schema.decodeUnknownSync(Integration.Method)(method) as any
        const current = integrations.get(integrationID)
        if (!current) return
        const index = current.methods.findIndex((candidate) => sameMethod(candidate, decoded))
        if (index !== -1) current.methods.splice(index, 1)
        if (decoded.type === "oauth") current.implementations.delete(decoded.id)
      },
    },
  }),
  encode: (integrations, codec) => ({
    integrations: Array.from(integrations.values(), (entry) => ({
      ref: entry.ref,
      methods: toWire(entry.methods, undefined),
      implementations: Array.from(entry.implementations.values(), (implementation) =>
        wireFunctions(implementation, codec),
      ),
    })),
  }),
}

export const SPECS: Record<Domain, Spec<any, any>> = {
  agent,
  command,
  integration,
  mcp,
  model,
  provider,
  reference,
  skill,
  tool,
  vcs: registry("providers"),
  websearch: registry("providers"),
  worktree,
}

export function isDomain(value: string): value is Domain {
  return (DOMAINS as readonly string[]).includes(value)
}
