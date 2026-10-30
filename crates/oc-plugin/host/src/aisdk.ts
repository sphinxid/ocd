// The JS side of the AI SDK bridge: port of packages/core/src/aisdk.ts (`AISDK.Service`: hooks,
// SDK and language model caches, `prepareOptions` with its timed fetch, the HTTP hook middleware)
// plus the internal provider plugins that register `aisdk` hooks
// (packages/core/src/plugin/provider/{cohere,github-copilot,gitlab,perplexity,sap-ai-core,vercel,dynamic}.ts).
//
// Rust owns request lowering (`LanguageModelV3CallOptions` arrive ready) and raising the stream
// parts to LLM events; this side resolves/installs the provider package, runs the hooks, constructs
// the `LanguageModelV3`, calls `doStream` and forwards its parts. Errors are reported as plain
// "error shapes" that Rust classifies like upstream's `llmError`.

import { Effect } from "effect"
import { AsyncLocalStorage } from "node:async_hooks"
import os from "node:os"
import { pathToFileURL } from "node:url"
import { createOpenaiCompatible } from "@upstream-core/github-copilot/copilot-provider"
import { log, RemoteError } from "./peer.js"

export interface BridgeIO {
  request(method: string, params: unknown, signal?: AbortSignal): Promise<any>
  notify(method: string, params: unknown): void
}

type SDK = any

export interface SDKEvent {
  readonly model: any
  readonly package: string
  readonly options: Record<string, any>
  sdk?: SDK
}

export interface LanguageEvent {
  readonly model: any
  readonly sdk: SDK
  readonly options: Record<string, any>
  language?: any
}

type Hook<E> = (event: E) => Effect.Effect<void, unknown> | void | Promise<void>

/** `AISDK.InitError`: the hook chain or the construction failed. */
export class InitError extends Error {
  constructor(readonly causeValue: unknown) {
    super(causeMessage(causeValue) ?? "AISDK.InitError")
  }
}

// -------------------------------------------------------------------------------------------
// Helpers shared with upstream (Provider.mergeOverlay, Provider.timeouts, causeMessage)
// -------------------------------------------------------------------------------------------

const DEFAULT_TIMEOUT_MS = 300_000
const HEADER_TIMEOUT_MESSAGE = "Response headers timed out"

const isRecord = (value: unknown): value is Record<string, any> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

export function mergeOverlay(base: Record<string, any> | undefined, overlay: Record<string, any> | undefined) {
  if (base === undefined) return overlay && { ...overlay }
  if (overlay === undefined) return { ...base }
  const out: Record<string, any> = {}
  for (const key of new Set([...Object.keys(base), ...Object.keys(overlay)])) {
    const left = base[key]
    const right = overlay[key]
    if (right === undefined) out[key] = left
    else if (isRecord(left) && isRecord(right)) out[key] = mergeOverlay(left, right) ?? {}
    else out[key] = right
  }
  return out
}

function timeout(value: unknown): number | false | undefined {
  if (value === false) return false
  if (typeof value === "number" && Number.isFinite(value) && value > 0) return value
  return undefined
}

function timeouts(settings: Record<string, unknown>) {
  return {
    timeout: timeout(settings.timeout),
    headerTimeout: timeout(settings.headerTimeout) ?? DEFAULT_TIMEOUT_MS,
    chunkTimeout: timeout(settings.chunkTimeout) ?? DEFAULT_TIMEOUT_MS,
  }
}

/** `ModelResolver.causeMessage`. */
export function causeMessage(cause: unknown): string | undefined {
  if (typeof cause === "string") return cause.trim() || undefined
  if (!(cause instanceof Error)) return undefined
  const message = cause.message.trim()
  if (message) return message
  return causeMessage((cause as { cause?: unknown }).cause)
}

/** `App.useragent`. */
const useragent = (app: any) => `opencode/${app?.channel}/${app?.version}/${app?.name}`

const AISDK_PREFIX = "aisdk:"
export const packageName = (value: string) => (value.startsWith(AISDK_PREFIX) ? value.slice(AISDK_PREFIX.length) : value)

// -------------------------------------------------------------------------------------------
// prepareOptions: the SDK options and its fetch (timeouts, extra body, HTTP hooks)
// -------------------------------------------------------------------------------------------

type Fetch = (input: any, init?: any) => Promise<Response>

interface Middleware {
  readonly stream: number
  readonly response: boolean
  readonly io: BridgeIO
}

// The SDK's fetch is baked into the cached language model, so the active middleware of one model
// request rides along in async context (upstream does the same).
const httpMiddleware = new AsyncLocalStorage<Middleware>()

function wrapSSE(res: Response, ms: number, ctl: AbortController) {
  if (typeof ms !== "number" || ms <= 0) return res
  if (!res.body) return res
  if (!res.headers.get("content-type")?.includes("text/event-stream")) return res

  const reader = res.body.getReader()
  const decoder = new TextDecoder()
  let deadline: number | undefined
  // Effect's SSE parser resets the deadline on every complete event.
  let buffer = ""
  const feed = (text: string) => {
    buffer += text
    const events = buffer.split(/\r\n\r\n|\n\n|\r\r/)
    buffer = events.pop() ?? ""
    for (const event of events) if (/^data:/m.test(event)) deadline = Date.now() + ms
  }
  const body = new ReadableStream<Uint8Array>({
    async pull(ctrl) {
      const expires = deadline ?? Date.now() + ms
      deadline = expires
      const part = await new Promise<Awaited<ReturnType<typeof reader.read>>>((resolve, reject) => {
        const remaining = Math.max(0, expires - Date.now())
        const id = setTimeout(() => {
          const err = new Error("SSE read timed out")
          ctl.abort(err)
          void reader.cancel(err)
          reject(err)
        }, remaining)
        reader.read().then(
          (part) => {
            clearTimeout(id)
            resolve(part)
          },
          (err) => {
            clearTimeout(id)
            reject(err)
          },
        )
      })
      if (part.done) {
        ctrl.close()
        return
      }
      feed(decoder.decode(part.value, { stream: true }))
      ctrl.enqueue(part.value)
    },
    async cancel(reason) {
      ctl.abort(reason)
      await reader.cancel(reason)
    },
  })
  return new Response(body, { headers: new Headers(res.headers), status: res.status, statusText: res.statusText })
}

async function throughMiddleware(store: Middleware, send: Fetch, input: any, init: any): Promise<Response> {
  const request = input instanceof Request ? new Request(input, init) : new Request(String(input), init)
  // Hooks see a byte body like on the native route.
  const bytes = request.body ? new Uint8Array(await request.arrayBuffer()) : undefined
  const headers: Record<string, string> = Object.fromEntries(request.headers)
  if (bytes) headers["content-length"] = String(bytes.byteLength)
  const wire = {
    url: request.url,
    method: request.method,
    headers,
    body: bytes ? new TextDecoder().decode(bytes) : null,
  }
  const sent = await store.io.request("aisdk.http.request", { stream: store.stream, request: wire })
  const response = await send(sent.url, {
    ...init,
    method: sent.method,
    headers: sent.headers,
    body: sent.body === null || sent.body === undefined ? undefined : sent.body,
  })
  if (!store.response) return response
  const text = [204, 205, 304].includes(response.status) ? null : await response.text()
  const received = await store.io.request("aisdk.http.response", {
    stream: store.stream,
    request: sent,
    response: { status: response.status, statusText: "", headers: Object.fromEntries(response.headers), body: text },
  })
  const status = received.status ?? 200
  return new Response([204, 205, 304].includes(status) ? null : (received.body ?? null), {
    status,
    headers: received.headers ?? {},
  })
}

// `Provider.Settings` fields OpenCode reads itself; every other key is a provider option (`Provider.nativeSettings`).
const SETTINGS_KEYS = ["timeout", "headerTimeout", "chunkTimeout", "compaction", "cache", "transport", "encoding"]

function nativeSettings(settings: Record<string, unknown>) {
  return Object.fromEntries(Object.entries(settings).filter(([key]) => !SETTINGS_KEYS.includes(key)))
}

function requestSettings(settings: Record<string, unknown> | undefined) {
  if (settings === undefined) return undefined
  // These configure the SDK provider when it is created, not each request.
  const drop = ["accessToken", "apiKey", "authToken", "baseURL", "fetch"]
  const result = Object.fromEntries(Object.entries(nativeSettings(settings)).filter(([key]) => !drop.includes(key)))
  return Object.keys(result).length === 0 ? undefined : result
}

function mapBodyToProviderOptions(model: any, pkg: string) {
  const settings = requestSettings(model.settings)
  const pro = isRecord(model.body?.reasoning) && model.body.reasoning.mode === "pro"
  const forceReasoning =
    ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/amazon-bedrock/mantle"].includes(pkg) &&
    (pro || settings?.reasoningEffort !== undefined || settings?.reasoningSummary !== undefined)
  const normalized = forceReasoning ? mergeOverlay(settings, { forceReasoning: true }) : settings
  if (!pro) return { settings: normalized, body: model.body }
  const body = { ...model.body }
  delete body.reasoning
  return {
    settings: mergeOverlay(normalized, { reasoningMode: "pro" }),
    body: Object.keys(body).length === 0 ? undefined : body,
  }
}

function prepareOptions(model: any, pkg: string) {
  const projected = mapBodyToProviderOptions(model, pkg)
  const settings = model.settings ?? {}
  const options: Record<string, any> = {
    name: model.canonical ?? model.providerID,
    ...nativeSettings(settings),
    headers: model.headers,
    body: projected.body,
  }
  const customFetch = settings.fetch
  const limits = timeouts(settings)
  options.fetch = async (input: any, init?: any) => {
    const opts = { ...(init ?? {}) }
    // The header limit is cleared once headers arrive; the whole-request limit spans the body.
    const headers = new AbortController()
    const headerTimer =
      limits.headerTimeout === false
        ? undefined
        : setTimeout(() => headers.abort(new Error(HEADER_TIMEOUT_MESSAGE)), limits.headerTimeout)
    const chunks = new AbortController()
    opts.signal = AbortSignal.any(
      [opts.signal, headers.signal, chunks.signal, limits.timeout ? AbortSignal.timeout(limits.timeout) : undefined].filter(
        (item): item is AbortSignal => item !== undefined && item !== null,
      ),
    )
    if (typeof opts.body === "string" && model.body !== undefined) {
      let decoded: unknown
      try {
        decoded = JSON.parse(opts.body)
      } catch {}
      if (isRecord(decoded)) opts.body = JSON.stringify(mergeOverlay(decoded, model.body))
    }
    const send: Fetch = typeof customFetch === "function" ? customFetch : fetch
    const middleware = httpMiddleware.getStore()
    const res = await (
      middleware
        ? throughMiddleware(middleware, send, input, { ...opts, timeout: false })
        : send(input, { ...opts, timeout: false })
    ).finally(() => clearTimeout(headerTimer))
    if (limits.chunkTimeout === false) return res
    return wrapSSE(res, limits.chunkTimeout, chunks)
  }
  return options
}

// -------------------------------------------------------------------------------------------
// Package loading (`loadSDKFactory`) through Rust's npm cache
// -------------------------------------------------------------------------------------------

// The AI SDK bridge only supports AI SDK v6 providers: an unversioned official `@ai-sdk/*` package
// installs the `ai-v6` dist-tag.
function installSpec(name: string) {
  return /^@ai-sdk\/[^/@]+$/.test(name) ? `${name}@ai-v6` : name
}

async function importInstalled(io: BridgeIO, spec: string, specifier?: string) {
  const installed = await io.request("npm.add", { spec })
  const resolved = Bun.resolveSync(specifier ?? installed.name, installed.directory)
  return (await import(resolved.startsWith("node:") ? resolved : pathToFileURL(resolved).href)) as Record<string, unknown>
}

async function loadSDKFactory(io: BridgeIO, name: string) {
  const mod = name.startsWith("file://")
    ? ((await import(name)) as Record<string, unknown>)
    : await importInstalled(io, installSpec(name))
  const match = Object.keys(mod).find((key) => key.startsWith("create"))
  if (!match) throw new Error(`Package ${name} has no provider factory export`)
  return mod[match]
}

// Packages upstream bundles in its binary, at the versions it pins (packages/core/package.json);
// ocd installs exactly those versions on first use.
const PINNED: Record<string, string> = {
  "gitlab-ai-provider": "gitlab-ai-provider@6.19.0",
}

const pinned = (io: BridgeIO, name: string) => importInstalled(io, PINNED[name]!, name)

// -------------------------------------------------------------------------------------------
// The service (one per location, like upstream's location node)
// -------------------------------------------------------------------------------------------

type Entry<E> = { readonly callback: Hook<E>; readonly internal?: string }

export class AISDKService {
  private sdkHooks: Entry<SDKEvent>[] = []
  private languageHooks: Entry<LanguageEvent>[] = []
  /** Internal plugin ids still enabled by the location's configuration (`-opencode.provider.x`). */
  enabled: ReadonlySet<string> | undefined
  private readonly languages = new Map<string, any>()
  private readonly sdks = new Map<string, SDK>()
  private readonly functionIDs = new WeakMap<object, number>()
  private nextFunctionID = 0

  constructor(
    private readonly io: BridgeIO,
    private readonly app: () => unknown,
  ) {
    for (const [id, plugin] of internalPlugins) plugin(this, io, app, id)
  }

  private cacheKey(input: unknown) {
    return (
      JSON.stringify(input, (_key, value: unknown) => {
        if (typeof value !== "function") return value
        const existing = this.functionIDs.get(value)
        if (existing !== undefined) return `function:${existing}`
        const id = this.nextFunctionID++
        this.functionIDs.set(value, id)
        return `function:${id}`
      }) ?? ""
    )
  }

  /** Registers a hook (`internal`: the internal plugin id); returns its disposer. */
  hook(name: "sdk" | "language", callback: Hook<any>, internal?: string) {
    const entry: Entry<any> = internal === undefined ? { callback } : { callback, internal }
    if (name === "sdk") this.sdkHooks = [...this.sdkHooks, entry]
    else this.languageHooks = [...this.languageHooks, entry]
    let active = true
    return () => {
      if (!active) return
      active = false
      if (name === "sdk") this.sdkHooks = this.sdkHooks.filter((item) => item !== entry)
      else this.languageHooks = this.languageHooks.filter((item) => item !== entry)
    }
  }

  private async run<E>(hooks: readonly Entry<E>[], event: E) {
    for (const hook of hooks) {
      if (hook.internal !== undefined && this.enabled !== undefined && !this.enabled.has(hook.internal)) continue
      const result = hook.callback(event)
      if (Effect.isEffect(result)) await Effect.runPromise(result as Effect.Effect<void, unknown>)
      else await result
    }
    return event
  }

  /** `AISDK.language(model)`. */
  async language(model: any) {
    const key = this.cacheKey({
      providerID: model.providerID,
      canonical: model.canonical,
      id: model.id,
      modelID: model.modelID,
      package: model.package,
      settings: model.settings,
      headers: model.headers,
      body: model.body,
      limit: model.limit,
    })
    const existing = this.languages.get(key)
    if (existing) return existing
    if (typeof model.package !== "string" || !model.package.startsWith(AISDK_PREFIX))
      throw new InitError(new Error(`Unsupported package ${model.package}`))
    const pkg = packageName(model.package)
    const options = prepareOptions(model, pkg)
    const sdkKey = this.cacheKey({
      providerID: model.providerID,
      canonical: model.canonical,
      package: pkg,
      settings: model.settings,
      headers: model.headers,
      body: model.body,
    })
    let sdk = this.sdks.get(sdkKey)
    if (!sdk) {
      try {
        sdk = (await this.run(this.sdkHooks, { model, package: pkg, options } as SDKEvent)).sdk
      } catch (cause) {
        throw new InitError(cause)
      }
    }
    if (!sdk) throw new InitError(new Error("No AISDK provider plugin returned an SDK"))
    this.sdks.set(sdkKey, sdk)
    let result: LanguageEvent
    try {
      result = await this.run(this.languageHooks, { model, sdk, options } as LanguageEvent)
    } catch (cause) {
      throw new InitError(cause)
    }
    let language: any
    try {
      language = result.language ?? sdk.languageModel(model.modelID ?? model.id)
    } catch (cause) {
      throw new InitError(cause)
    }
    this.languages.set(key, language)
    return language
  }
}

// -------------------------------------------------------------------------------------------
// Internal provider plugins with AI SDK hooks, in upstream's ProviderPlugins order. They run
// before any user plugin hook (internal `pre` plugins activate first).
// -------------------------------------------------------------------------------------------

type Internal = (service: AISDKService, io: BridgeIO, app: () => unknown, id: string) => void

// opencode.provider.gitlab (plugin/provider/gitlab.ts)
const gitlab: Internal = (service, io, app, id) => {
  service.hook("sdk", async (evt: SDKEvent) => {
    if (evt.package !== "gitlab-ai-provider") return
    const mod: any = await pinned(io, "gitlab-ai-provider")
    evt.sdk = mod.createGitLab({
      ...evt.options,
      instanceUrl:
        typeof evt.options.instanceUrl === "string"
          ? evt.options.instanceUrl
          : (process.env.GITLAB_INSTANCE_URL ?? "https://gitlab.com"),
      apiKey: typeof evt.options.apiKey === "string" ? evt.options.apiKey : process.env.GITLAB_TOKEN,
      aiGatewayHeaders: {
        "User-Agent": `${useragent(app())} gitlab-ai-provider/${mod.VERSION} (${os.platform()} ${os.release()}; ${os.arch()})`,
        "anthropic-beta": "context-1m-2025-08-07",
        ...evt.options.aiGatewayHeaders,
      },
      featureFlags: {
        duo_agent_platform_agentic_chat: true,
        duo_agent_platform: true,
        ...evt.options.featureFlags,
      },
    })
  }, id)
  service.hook("language", async (evt: LanguageEvent) => {
    if (evt.model.providerID !== "gitlab") return
    const featureFlags = typeof evt.options.featureFlags === "object" && evt.options.featureFlags ? evt.options.featureFlags : {}
    const model = evt.model.modelID ?? evt.model.id
    if (model.startsWith("duo-workflow-")) {
      const mod: any = await pinned(io, "gitlab-ai-provider")
      const workflowRef = typeof evt.model.settings?.workflowRef === "string" ? evt.model.settings.workflowRef : undefined
      const workflowDefinition =
        typeof evt.model.settings?.workflowDefinition === "string" ? evt.model.settings.workflowDefinition : undefined
      const language = evt.sdk.workflowChat(mod.isWorkflowModel(model) ? model : "duo-workflow", { featureFlags, workflowDefinition })
      if (workflowRef) language.selectedModelRef = workflowRef
      evt.language = language
      return
    }
    evt.language = evt.sdk.agenticChat(model, { aiGatewayHeaders: evt.options.aiGatewayHeaders, featureFlags })
  }, id)
}

// opencode.provider.sap.ai.core (plugin/provider/sap-ai-core.ts)
const sapAICore: Internal = (service, io, _app, id) => {
  service.hook("sdk", async (evt: SDKEvent) => {
    if (evt.model.providerID !== "sap-ai-core") return
    const serviceKey =
      process.env.AICORE_SERVICE_KEY ?? (typeof evt.options.serviceKey === "string" ? evt.options.serviceKey : undefined)
    if (serviceKey && !process.env.AICORE_SERVICE_KEY) process.env.AICORE_SERVICE_KEY = serviceKey
    const factory = await loadSDKFactory(io, evt.package)
    if (typeof factory !== "function") throw new Error(`Package ${evt.package} provider factory export is not callable`)
    evt.sdk = factory(
      serviceKey ? { deploymentId: process.env.AICORE_DEPLOYMENT_ID, resourceGroup: process.env.AICORE_RESOURCE_GROUP } : {},
    )
  }, id)
  service.hook("language", (evt: LanguageEvent) => {
    if (evt.model.providerID !== "sap-ai-core") return
    evt.language = evt.sdk(evt.model.modelID ?? evt.model.id)
  }, id)
}

// opencode.provider.github.copilot (plugin/provider/github-copilot.ts:252-265, 317-344): upstream's
// in-tree Copilot AI SDK provider (core/src/github-copilot, bundled from the checkout) with the
// Copilot headers on every request, and the chat/responses route per model. The plugin's catalog,
// session and HTTP hooks run in Rust (oc-ai catalog/copilot.rs, oc-core copilot.rs).
const githubCopilot: Internal = (service, _io, app, id) => {
  service.hook(
    "sdk",
    (evt: SDKEvent) => {
      if (evt.model.providerID !== "github-copilot") return
      if (evt.package !== "@ai-sdk/github-copilot") return
      evt.options.fetch = copilotFetch(
        typeof evt.options.apiKey === "string" ? evt.options.apiKey : undefined,
        evt.options.fetch,
        app(),
      )
      evt.sdk = createOpenaiCompatible(evt.options)
    },
    id,
  )
  service.hook(
    "language",
    (evt: LanguageEvent) => {
      if (evt.model.providerID !== "github-copilot") return
      if (evt.sdk.responses === undefined && evt.sdk.chat === undefined) {
        evt.language = evt.sdk.languageModel(evt.model.modelID ?? evt.model.id)
        return
      }
      if (evt.options.endpoint === "responses" && evt.sdk.responses) {
        evt.language = evt.sdk.responses(evt.model.modelID ?? evt.model.id)
        return
      }
      if (evt.options.endpoint === "chat" && evt.sdk.chat) {
        evt.language = evt.sdk.chat(evt.model.modelID ?? evt.model.id)
        return
      }
      const id = evt.model.modelID ?? evt.model.id
      // Copilot serves Grok and MAI Code only on /responses, and Gemini only on
      // /chat/completions; advertised endpoint metadata above wins whenever the
      // live model list provides it.
      const gpt = /^gpt-(\d+)/.exec(id)
      const responses =
        (gpt !== null && Number(gpt[1]) >= 5 && !id.startsWith("gpt-5-mini")) ||
        id.startsWith("grok-") ||
        id.startsWith("mai-code-")
      evt.language = responses ? evt.sdk.responses(id) : evt.sdk.chat(id)
    },
    id,
  )
}

// Verbatim from plugin/provider/github-copilot.ts (`decodeBody` is a lenient JSON.parse).
const COPILOT_API_VERSION = "2026-08-01"

function decodeBody(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return undefined
  }
}

function copilotFetch(token: string | undefined, upstream: Fetch | undefined, app: any): Fetch {
  const send = upstream ?? fetch
  return async (input, init) => {
    const requestHeaders = new Headers(init?.headers)
    const url = input instanceof URL ? input.href : typeof input === "string" ? input : input.url
    const body = typeof init?.body === "string" ? decodeBody(init.body) : undefined
    applyHeaders(requestHeaders, token, app, requestMetadata(url, body), false)
    return send(input, { ...init, headers: requestHeaders })
  }
}

function applyHeaders(headers: Headers, token: string | undefined, app: any, metadata: RequestMetadata, anthropic: boolean) {
  if (token) {
    headers.delete("authorization")
    headers.delete("x-api-key")
    headers.set("Authorization", `Bearer ${token}`)
  }
  headers.set("User-Agent", useragent(app))
  headers.set("Openai-Intent", "conversation-edits")
  headers.set("X-GitHub-Api-Version", COPILOT_API_VERSION)
  // The step may already have declared itself agent-initiated (subagent, title, compaction);
  // the body can only ever escalate to "agent", never back to "user".
  if (metadata.agent) headers.set("x-initiator", "agent")
  else if (!headers.has("x-initiator")) headers.set("x-initiator", "user")
  if (metadata.vision) headers.set("Copilot-Vision-Request", "true")
  if (anthropic) headers.set("anthropic-beta", "interleaved-thinking-2025-05-14")
}

type RequestMetadata = ReturnType<typeof requestMetadata>

function requestMetadata(url: string, body: unknown) {
  if (!record(body)) return { agent: false, vision: false }
  if (Array.isArray(body.input)) {
    const last = body.input.at(-1)
    return {
      agent: !record(last) || last.role !== "user",
      vision: body.input.some(
        (item) =>
          record(item) &&
          Array.isArray(item.content) &&
          item.content.some((part) => record(part) && part.type === "input_image"),
      ),
    }
  }
  if (!Array.isArray(body.messages)) return { agent: false, vision: false }
  const last = body.messages.at(-1)
  if (url.includes("completions")) {
    return {
      agent: !record(last) || last.role !== "user",
      vision: body.messages.some(
        (message) =>
          record(message) &&
          Array.isArray(message.content) &&
          message.content.some((part) => record(part) && part.type === "image_url"),
      ),
    }
  }
  const content = record(last) && Array.isArray(last.content) ? last.content : []
  return {
    agent:
      !record(last) || last.role !== "user" || !content.some((part) => record(part) && part.type !== "tool_result"),
    vision: body.messages.some(
      (message) =>
        record(message) &&
        Array.isArray(message.content) &&
        message.content.some(
          (part) =>
            record(part) &&
            (part.type === "image" ||
              (part.type === "tool_result" &&
                Array.isArray(part.content) &&
                part.content.some((nested) => record(nested) && nested.type === "image"))),
        ),
    ),
  }
}

function record(input: unknown): input is Record<string, any> {
  return typeof input === "object" && input !== null && !Array.isArray(input)
}

// opencode.provider.dynamic (plugin/provider/dynamic.ts): any other package from npm (or file://).
const dynamic: Internal = (service, io, _app, id) => {
  service.hook("sdk", async (evt: SDKEvent) => {
    if (evt.sdk) return
    evt.sdk = ((await loadSDKFactory(io, evt.package)) as (options: any) => any)(evt.options)
  }, id)
}

// ProviderPlugins order: github-copilot, gitlab, sap-ai-core, ..., dynamic. Cohere, Perplexity and Vercel are
// no longer special-cased (2.2): they install from npm like any other AI SDK package.
const internalPlugins: [string, Internal][] = [
  ["opencode.provider.github.copilot", githubCopilot],
  ["opencode.provider.gitlab", gitlab],
  ["opencode.provider.sap.ai.core", sapAICore],
  ["opencode.provider.dynamic", dynamic],
]

// -------------------------------------------------------------------------------------------
// Plugin registration (`ctx.aisdk.hook`, packages/core/src/plugin/host.ts)
// -------------------------------------------------------------------------------------------

/** Wraps a plugin callback like upstream's host: provider scoping and copying back the result. */
export function pluginHook(
  name: "sdk" | "language",
  callback: (event: any) => Effect.Effect<void, unknown>,
  options: { readonly providerID?: string } | undefined,
): Hook<any> {
  if (name === "sdk")
    return (event: SDKEvent) => {
      if (options?.providerID !== undefined && options.providerID !== event.model.providerID) return
      const output = { model: event.model, package: event.package, options: event.options, sdk: event.sdk }
      return callback(output).pipe(Effect.tap(() => Effect.sync(() => (event.sdk = output.sdk))))
    }
  return (event: LanguageEvent) => {
    if (options?.providerID !== undefined && options.providerID !== event.model.providerID) return
    const output = { model: event.model, options: event.options, sdk: event.sdk, language: event.language }
    return callback(output).pipe(Effect.tap(() => Effect.sync(() => (event.language = output.language))))
  }
}

// -------------------------------------------------------------------------------------------
// Streaming
// -------------------------------------------------------------------------------------------

// Parts upstream drops (`streamPartEvents` answers []): not forwarded.
const SKIP = new Set(["stream-start", "response-metadata", "raw", "file", "source", "tool-approval-request"])

/** Wire call options → LanguageModelV3CallOptions (`$bytes` → Uint8Array, `$url` → URL). */
export function reviveOptions(options: any) {
  const data = (value: any) => {
    if (isRecord(value) && typeof value.$bytes === "string") return Uint8Array.from(Buffer.from(value.$bytes, "base64"))
    if (isRecord(value) && typeof value.$url === "string") return new URL(value.$url)
    return value
  }
  for (const message of options?.prompt ?? []) {
    if (!Array.isArray(message?.content)) continue
    for (const part of message.content) if (part?.type === "file") part.data = data(part.data)
  }
  return options
}

function jsonPart(part: unknown) {
  return JSON.parse(
    JSON.stringify(part, (_key, value) => {
      if (value instanceof Uint8Array) return Buffer.from(value).toString("base64")
      if (value instanceof Error) return { message: value.message }
      if (typeof value === "bigint") return Number(value)
      return value
    }) ?? "null",
  )
}

export async function stream(
  service: AISDKService,
  io: BridgeIO,
  params: { model: any; options: any; stream: number; http?: { response?: boolean } },
  signal: AbortSignal,
) {
  const language = await service.language(params.model).catch((error) => {
    throw failure("request", error instanceof InitError ? error.causeValue : error)
  })
  const options = reviveOptions(params.options)
  const store: Middleware | undefined = params.http
    ? { stream: params.stream, response: params.http.response === true, io }
    : undefined
  let result: any
  try {
    result = await (store ? httpMiddleware.run(store, () => language.doStream(options)) : language.doStream(options))
  } catch (error) {
    throw failure("request", error)
  }
  const reader = (result.stream as ReadableStream<any>).getReader()
  const abort = () => void reader.cancel(signal.reason).catch(() => undefined)
  signal.addEventListener("abort", abort, { once: true })
  try {
    while (true) {
      let next: ReadableStreamReadResult<any>
      try {
        next = await reader.read()
      } catch (error) {
        throw failure("read", error)
      }
      if (next.done) break
      if (signal.aborted) break
      const part = next.value
      if (part?.type === "error") throw failure("read", part.error)
      if (SKIP.has(part?.type)) continue
      io.notify("aisdk.part", { stream: params.stream, part: jsonPart(part) })
    }
  } finally {
    signal.removeEventListener("abort", abort)
  }
  return null
}

function failure(operation: "request" | "read", error: unknown) {
  return new RemoteError({ message: errorShape(error).message, type: "aisdk.error", data: { operation, shape: errorShape(error) } })
}

// -------------------------------------------------------------------------------------------
// Error shapes (the JS half of upstream's `llmError`)
// -------------------------------------------------------------------------------------------

const API_CALL_ERROR = Symbol.for("vercel.ai.error.AI_APICallError")
const isAPICallError = (error: unknown): error is any =>
  typeof error === "object" && error !== null && (error as any)[API_CALL_ERROR] === true

const NETWORK_ERROR_CODES = new Set([
  "ECONNRESET",
  "ECONNREFUSED",
  "ETIMEDOUT",
  "EPIPE",
  "ConnectionRefused",
  "ConnectionClosed",
  "FailedToOpenSocket",
])
const NETWORK_ERROR_MESSAGES = new Set([
  "fetch failed",
  "failed to fetch",
  "terminated",
  "other side closed",
  "sse read timed out",
  HEADER_TIMEOUT_MESSAGE.toLowerCase(),
])

function nativeShape(error: unknown): { message: string; code?: string; cause?: unknown } | undefined {
  if (typeof error !== "object" || error === null) return undefined
  const value = error as Record<string, unknown>
  if (typeof value.message !== "string") return undefined
  if ("code" in value && value.code !== undefined && typeof value.code !== "string") return undefined
  return { message: value.message, code: value.code as string | undefined, cause: value.cause }
}

function networkFailure(error: unknown, depth = 0): { message: string; code?: string } | undefined {
  if (depth > 4) return undefined
  const shape = nativeShape(error)
  if (!shape) return undefined
  const cause = networkFailure(shape.cause, depth + 1)
  if (cause) return cause
  if (shape.code !== undefined && (NETWORK_ERROR_CODES.has(shape.code) || shape.code.startsWith("UND_ERR")))
    return { message: shape.message, code: shape.code }
  if (NETWORK_ERROR_MESSAGES.has(shape.message.trim().toLowerCase())) return { message: shape.message, code: shape.code }
  return undefined
}

type ProviderDetail = { message?: string; code?: string | number }

function detail(value: unknown): ProviderDetail | undefined {
  if (!isRecord(value)) return undefined
  if ("message" in value && value.message !== undefined && typeof value.message !== "string") return undefined
  if (
    "code" in value &&
    value.code !== undefined &&
    typeof value.code !== "string" &&
    !(typeof value.code === "number" && Number.isFinite(value.code))
  )
    return undefined
  return value as ProviderDetail
}

function decodeProviderError(value: unknown): (ProviderDetail & { error?: ProviderDetail }) | undefined {
  let input = value
  if (typeof input === "string") {
    try {
      input = JSON.parse(input)
    } catch {
      return undefined
    }
  }
  const outer = detail(input)
  if (!outer) return undefined
  const inner = (input as Record<string, unknown>).error
  if (inner !== undefined && !detail(inner)) return undefined
  return { ...outer, ...(inner === undefined ? {} : { error: inner as ProviderDetail }) }
}

const errorValue = (error: unknown) => (isRecord(error) ? error.value : undefined)

function isJson(value: unknown, depth = 0): boolean {
  if (depth > 100) return false
  if (value === null || typeof value === "string" || typeof value === "boolean") return true
  if (typeof value === "number") return Number.isFinite(value)
  if (Array.isArray(value)) return value.every((item) => isJson(item, depth + 1))
  if (typeof value === "object") {
    const prototype = Object.getPrototypeOf(value)
    if (prototype !== Object.prototype && prototype !== null) return false
    return Object.values(value as object).every((item) => isJson(item, depth + 1))
  }
  return false
}

function errorBody(value: unknown) {
  if (typeof value === "string") return value
  if (value instanceof Error || !isJson(value)) return undefined
  return JSON.stringify(value)
}

function unknownErrorMessage(error: unknown) {
  const message =
    error instanceof Error
      ? error.message
      : typeof error === "string"
        ? error
        : ([error, errorValue(error)]
            .map((value) => decodeProviderError(value))
            .flatMap((decoded) => [decoded?.error?.message, decoded?.message])
            .find((value) => value?.trim()) ?? "")
  return message.trim() === "" ? "Provider request failed" : message
}

function providerErrorMessage(error: any) {
  const data = decodeProviderError(error.data)
  const body = decodeProviderError(error.responseBody)
  const details = [data?.error, data, body?.error, body]
  const message = details.map((item) => item?.message).find((value) => value?.trim())
  const value = details.map((item) => item?.code).find((value) => value !== undefined)
  const code = value === undefined ? undefined : String(value)
  const prefix =
    error.statusCode === undefined ? "Provider request failed" : `Provider request failed with HTTP ${error.statusCode}`
  return typeof error.message === "string" && error.message.trim() !== ""
    ? error.message
    : (message ?? (code === undefined ? prefix : `${prefix}: ${code}`))
}

function safe(value: unknown) {
  if (value === undefined) return undefined
  try {
    const text = JSON.stringify(value)
    return text === undefined ? undefined : JSON.parse(text)
  } catch {
    return undefined
  }
}

function describe(error: unknown) {
  if (error instanceof Error) return error.stack ?? `${error.name}: ${error.message}`
  try {
    return typeof error === "string" ? error : JSON.stringify(error)
  } catch {
    return String(error)
  }
}

export function errorShape(error: unknown) {
  if (isAPICallError(error))
    return {
      kind: "api",
      message: providerErrorMessage(error),
      url: error.url,
      statusCode: error.statusCode,
      data: safe(error.data),
      responseHeaders: error.responseHeaders,
      responseBody: error.responseBody ?? errorBody(error.data),
      isRetryable: error.isRetryable === true,
      cause: describe(error),
    }
  const network = networkFailure(error)
  if (network)
    return {
      kind: "network",
      message: network.message.trim() === "" ? unknownErrorMessage(error) : network.message,
      code: network.code,
      cause: describe(error),
    }
  return {
    kind: "unknown",
    message: unknownErrorMessage(error),
    data: safe(errorValue(error) ?? error),
    responseBody: errorBody(error),
    cause: describe(error),
  }
}

export function initFailure(error: unknown) {
  const cause = error instanceof InitError ? error.causeValue : error
  log("warn", "AI SDK model initialization failed", { cause: describe(cause) })
  return new RemoteError({
    message: causeMessage(cause) ?? "AISDK.InitError",
    type: "aisdk.init",
    data: { detail: causeMessage(cause) ?? null },
  })
}
