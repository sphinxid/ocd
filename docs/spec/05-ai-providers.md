# 05 — AI providers, protocols, model catalog and credentials

Status: implementation spec for the Rust port of opencode v2. Behavioral source of truth is the TypeScript
checkout (MIT). All citations are `path:line` relative to the repo root (`packages/...`). Strings, URLs, header names
and regexes are quoted verbatim. Where this spec says "MUST", the Rust port must reproduce the behavior byte-for-byte
(wire bodies, header names, event ordering), because recorded HTTP fixtures and provider caches depend on it.

Contents

1. Architecture overview
2. Normalized request / response / stream model
3. Request pipeline: compile, routes, endpoints, auth, transport, framing
4. Chat protocols (4.1 Anthropic Messages ... 4.18 google-interactions)
5. Provider registry, model catalog (models.dev), custom providers
6. Credentials, OAuth and provider login
7. Retries, timeouts, rate limits, error classification
8. Non-chat modalities (brief)
9. Open questions

---

## 1. Architecture overview

Two layers exist in v2:

* `packages/ai` (`@opencode/ai`): a provider-neutral LLM client built on Effect. It owns the normalized model
  (`LLMRequest`, `Message`, `ContentPart`, `LLMEvent`, `Usage`), protocol lowering/raising, HTTP/WebSocket transport,
  SSE/AWS-event-stream framing and error classification. It does NOT run a tool loop (only `ToolRuntime.dispatch`
  for a single call, `packages/ai/src/tool-runtime.ts:23-42`).
* `packages/core`: the agent runtime. It owns the provider registry/plugins (`packages/core/src/plugin/provider/*`),
  the models.dev catalog, credentials/OAuth, the session runner (tool loop, retries, compaction), and an
  AI-SDK bridge (`packages/core/src/aisdk.ts`) used for providers that are not served by a native `packages/ai` route.

The `packages/ai` design composes a **Route** from four orthogonal pieces (`packages/ai/src/route/client.ts:518-535`):

| Axis | Type | Responsibility |
|---|---|---|
| Protocol | `Protocol<Body, Frame, Event, State>` (`packages/ai/src/route/protocol.ts:37-68`) | `body.from(LLMRequest) -> Body`, body schema, `stream.event` frame decoder, `stream.initial/step/onHalt/terminal` state machine |
| Endpoint | `Endpoint.Definition` (`packages/ai/src/route/endpoint.ts:21-25`) | `baseURL`, `path` (string or fn of `{request, body}`), static `query` |
| Auth | `Auth.Definition` (`packages/ai/src/route/auth.ts:36-41`) | `apply({request, method, url, body, headers}) -> headers` |
| Framing | `Framing.Definition<Frame>` (`packages/ai/src/route/framing.ts:21-26`) | bytes -> frames (SSE data strings, AWS event-stream records, NDJSON lines, whole document) |

plus optional `headers(request)` (deployment headers) and `defaults` (headers/generation/providerOptions/http).
A `LanguageModel` (`packages/ai/src/schema/options.ts:221-286`) is `{ id, provider, route, defaults?, compatibility? }`.
Recommended Rust shape: `trait Protocol { type Body: Serialize; type Event: DeserializeOwned; type State; fn body_from(&LLMRequest) -> Result<Body>; fn initial(&LLMRequest) -> State; fn step(&mut State, Event) -> Result<Vec<LLMEvent>>; fn on_halt(&mut State) -> Result<Vec<LLMEvent>>; fn terminal(&Event) -> bool }`, and a `Route` struct holding `Arc<dyn ...>` of the other axes.

---

## 2. Normalized request / response / stream model

### 2.1 Identifiers

All ids are plain strings (`packages/ai/src/schema/ids.ts:1-24`): `ProtocolID`, `RouteID`, `ModelID` (branded
`"AI.ModelID"`), `ProviderID` (branded `"AI.ProviderID"`), `ResponseID`, `ContentBlockID`, `ToolCallID`.

### 2.2 `LLMRequest` (`packages/ai/src/schema/messages.ts:385-399`)

| Field | Type | Notes |
|---|---|---|
| `id?` | string | diagnostic only; `compileRequest` falls back to `"request"` (`packages/ai/src/route/client.ts:593`) |
| `model` | `LanguageModel` | carries the route |
| `system` | `SystemPart[]` | `SystemPart = { type:"text", text, cache?: CacheHint, metadata? }` (`messages.ts:22-27`). Input may be string / part / array (`messages.ts:34-37`) |
| `messages` | `Message[]` | see 2.3 |
| `tools` | `ToolEntry[]` | `ToolDefinition` or `ToolNamespace` (2.5) |
| `toolChoice?` | `ToolChoice` | 2.6 |
| `generation?` | `GenerationOptions` | 2.7 |
| `providerOptions?` | `Record<string, unknown>` | free-form FLAT object whose keys are interpreted by the selected protocol (e.g. Anthropic reads `thinking`, `effort`; OpenAI Chat reads `reasoningEffort`, ...). Not namespaced by provider. |
| `http?` | `HttpOptions` | 2.8 |
| `cache?` | `CachePolicy` | `"auto"` (default when undefined) / `"none"` / object (2.10) |
| `promptCacheKey?` | string | stable cache affinity key for providers with `prompt_cache_key` etc. |
| `metadata?` | record | not sent |

`LLM.request(input)` (`packages/ai/src/llm.ts:60-84`) builds a request: `system` normalized by `SystemPart.content`;
`messages = [...messages.map(Message.make), ...(prompt ? [Message.user(prompt)] : [])]`; tools via `ToolEntry.make`;
`toolChoice` via `ToolChoice.make`.

### 2.3 `Message` and roles (`packages/ai/src/schema/messages.ts:19-286`)

`Message = { id?, role: "system"|"user"|"assistant"|"tool", content: ContentPart[], metadata?, providerMetadata?, native? }`.

* `role:"system"` inside `messages` is a **chronological system update** (distinct from `LLMRequest.system`),
  text-only (`messages.ts:273-279`). Routes without native support lower it to user text via
  `wrapSystemUpdate`: `"<system-update>\n" + escape(join(parts.text, "\n")) + "\n</system-update>"` where escape
  replaces `&`→`&amp;`, `<`→`&lt;`, `>`→`&gt;` in that order (`packages/ai/src/protocols/shared.ts:140-150`). The
  wrapped part inherits the cache hint of the last original part (`shared.ts:170-176`). Non-text content in a system
  update fails with `"<route> system messages only support text content for now"` (`shared.ts:157-167, 353-357`).
* `Message.effort({effort, previous})` is a system message with exactly one `effort` part (`messages.ts:281-282`).
* `Message.tool(result)` wraps a single `tool-result` part in `role:"tool"` (`messages.ts:284-285`).

`ProviderMetadata = Record<string, Record<string, unknown>>` keyed by provider/route metadata namespace
(`packages/ai/src/schema/options.ts:42-45`).

### 2.4 Content parts (`ContentPart`, tagged by `type`; `messages.ts:226-235`)

| `type` | Fields | Notes |
|---|---|---|
| `text` | `text`, `cache?`, `metadata?`, `providerMetadata?` | |
| `media` | `media: Media.Asset`, `filename?`, `cache?`, `metadata?`, `providerMetadata?` | images, PDFs, audio, etc. |
| `tool-call` | `id`, `name`, `namespace?`, `input: unknown`, `providerExecuted?`, `cache?`, `metadata?`, `providerMetadata?` | |
| `tool-result` | `id`, `name`, `namespace?`, `result: ToolResultValue`, `providerExecuted?`, `cache?`, `metadata?`, `providerMetadata?` | |
| `reasoning` | `text`, `encrypted?`, `cache?`, `metadata?`, `providerMetadata?` | `encrypted` = opaque signature/encrypted content |
| `compaction` | `provider: ProviderID`, `id?`, exactly one of `encrypted: string` or `text: string|null` | `text:null` = provider failed to summarize (`messages.ts:192-216`); refine message `"Compaction requires either encrypted content or a summary"` |
| `effort` | `effort?`, `previous?` | per-message reasoning-effort change marker |

`ToolResultValue` (`messages.ts:59-87`): `{type:"json",value}` | `{type:"text",value}` | `{type:"error",value}` |
`{type:"content", value: Tool.Content[]}` where `Tool.Content = {type:"text",text} | {type:"file", uri, mime, name?}`
(`packages/schema/src/tool.ts:66-83`). `ToolResultValue.make(value, type="json")` passes through an already-valid
value, else wraps (`content` with non-array → `[]`).

`ToolOutput = { structured: unknown, content: Tool.Content[] }` (`messages.ts:89-120`):
`fromResultValue`: json→`{structured:value, content:[]}`; text→`{structured:{}, content:[{type:"text", text: str(value)}]}`
(str = value if string else `JSON.stringify` else `String`); content→`{structured:{}, content}`; error→none.
`toResultValue`: no content → json(structured); one text item → text; else content.

`Media.Asset` (`packages/ai/src/media.ts`): `source` is one of
`{type:"bytes", data: bytes, mediaType}`, `{type:"base64", data, mediaType}`, `{type:"url", url, mediaType?, expiresAt?}`,
`{type:"ref", provider, id, mediaType?}` (`media.ts:20-50`). `mediaType` = declared, else sniffed from bytes, else
`"application/octet-stream"`; `kind` = image/video/audio/document(`application/pdf` or `text/*`)/other
(`media.ts:63-73, 127-138`). `inline()` returns `{mime: lower(mediaType), base64, dataUrl:"data:<mime>;base64,<b64>"}`
only for bytes/base64 sources (`media.ts:141-147`). JSON form: `{source (bytes→base64 string), info?, providerMetadata?}`
(`media.ts:184-191`). Data-URL regex: `/^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$/s` (`media.ts:291`).
Tool-result `file` items are lifted to media parts by `toolFileMedia`: data URL → base64 asset with declared `mime`;
`^https?://` → url asset; otherwise raw base64 (`packages/ai/src/protocols/shared.ts:228-236`).

Helpers used by protocols:
* `toolResultText(part)` (`shared.ts:238-248`): text → `String(value)`; error → JSON-encode if plain object/array and
  JSON-valid else `String(value)`; json/content → JSON-encode value.
* `mediaReference(asset, provider, label)` (`shared.ts:208-222`): inline → data URL; url → url; own-provider ref → id;
  else InvalidRequest `"<label> accepts inline bytes, data URLs, http(s) URLs[, and <provider> references][; got p:id]"`.
* `requireInlineMedia` error: `"<route> requires inline media (bytes or base64); <type> sources must be materialized first"` (`shared.ts:188-196`).

### 2.5 Tools (`messages.ts:288-359`)

`ToolDefinition = { type:"tool", name, description, inputSchema: JSON Schema object, outputSchema?, cache?, metadata?, native? }`.
`ToolNamespace = { type:"namespace", name, description?, tools: ToolEntry[] }` (recursive).
Protocols without namespaces flatten via `flattenTools` (`shared.ts:295-302`): leaf name = `[...path, name].join("_")`,
dedup by final name (last wins, Map insertion order of first occurrence). `flattenToolRequest` (`shared.ts:304-320`)
rewrites history tool-call/tool-result parts with a namespace to `name = "<namespace>_<name>"`, `namespace = undefined`.

Executable tool helper (`packages/ai/src/tool.ts`): `toDefinitions(tools)` maps a record name→tool into definitions
(`tool.ts:223-232`). Empty-struct parameter schemas become
`{...annotations, type:"object", properties:{}, additionalProperties:false}` (`tool.ts:243-250`).
`ToolRuntime.dispatch(tools, call)` (`packages/ai/src/tool-runtime.ts:23-111`): lookup key is `name` or
`"<namespace>.<name>"`; missing tool → error result
`No tool named "<name>" is currently available. Please use a tool from the available tool list.`; no execute →
`Tool has no execute handler: <name>`; decode failure → `Invalid tool input: <msg>`; encode failure →
`Tool returned an invalid value for its success schema: <msg>`. Error results emit `tool-error` then `tool-result`
(type error); success emits a single `tool-result` with `output`.

`generateObject` (`packages/ai/src/llm.ts:86-196`) forces a synthetic tool named `"generate_object"` with description
`"Return the structured result by calling this tool."` and `toolChoice = named("generate_object")`; missing call →
InvalidProviderOutput ``generateObject: model did not call the forced `generate_object` tool``.

### 2.6 `ToolChoice` (`messages.ts:361-383`)

`{ type: "auto"|"none"|"required"|"tool", name?, disableParallelToolUse? }`. String input: `"auto"|"none"|"required"`
→ mode, any other string → `{type:"tool", name}`. A `tool` choice without name fails in `matchToolChoice` with
`"<route> tool choice requires a tool name"` (`shared.ts:322-338`).

### 2.7 `GenerationOptions` (`packages/ai/src/schema/options.ts:94-142`)

`{ maxTokens?, temperature?, topP?, topK?, frequencyPenalty?, presencePenalty?, seed?, stop?: string[] }`.
Merging (`mergeGenerationOptions`) is per-field "last defined wins"; result is `undefined` if every field undefined.

Reasoning effort enum (open string): `"none","minimal","low","medium","high","xhigh","max"` (`options.ts:173-178`).
Reasoning effort itself is not a `GenerationOptions` field; it is passed via `providerOptions` per protocol
(see each protocol section).

### 2.8 `HttpOptions` (`options.ts:51-92`)

`{ body?: JSON object overlay, headers?: Record<string,string>, query?: Record<string,string>, timeout?, headerTimeout?, chunkTimeout? }`,
each timeout is ms `> 0` or `false` (disabled). `DEFAULT_HTTP_TIMEOUT_MS = 300_000` applies to `headerTimeout` and
`chunkTimeout` when unset; `timeout` (whole request) is unbounded when unset (`options.ts:55-56`,
`packages/ai/src/route/transport/http.ts:112-143`).
`mergeHttpOptions`: `body` deep-merged (`mergeJsonRecords`: later wins, nested records merged recursively, `undefined`
values skipped), `headers`/`query` shallow merged (later wins), each timeout last-defined wins.

### 2.9 `LanguageModelCompatibility` (`options.ts:180-219`)

Per-model wire quirks consulted by protocols:
`sanitizer?: "gemini"|"moonshot"|"none"`, `reasoningField?: string`, `requireReasoning?`, `maxTokensField?: "max_completion_tokens"|"max_tokens"`,
`requireFinishReason?`, `requireAssistantAfterTool?`, `supportsStore?`, `supportsUsageInStreaming?`, `supportsStrictMode?`,
`supportsPromptCacheKey?` (Chat omits `prompt_cache_key` unless set), `zaiToolStream?`, `requireSignature?`,
`supportsThinkingBlockBinding?`, `supportsEffortUpdates?`.

`LanguageModelDefaults = { generation?, providerOptions?, http? }` (`options.ts:144-168`).

### 2.10 Cache hints and cache policy

`CacheHint = { type: "ephemeral"|"persistent", ttlSeconds? }` (`options.ts:317-320`).
`CachePolicy = "auto" | "none" | { tools?: bool, system?: bool, messages?: "latest-user-message"|"latest-assistant"|{tail: n}, ttlSeconds? }` (`options.ts:332-347`).

`applyCachePolicy` (`packages/ai/src/cache-policy.ts:173-195`) runs at compile time:

1. Only for route ids in `RESPECTS_INLINE_HINTS` = `alibaba-chat, alibaba-messages, anthropic-messages,
   anthropic-compatible-messages, bedrock-mantle-messages, cloudflare-ai-gateway-messages, google-vertex-messages,
   meta-messages, minimax-messages, moonshot-messages, vercel-ai-gateway-messages, zai-coding-messages,
   bedrock-converse, openrouter, digitalocean` (`cache-policy.ts:40-56`). Other routes: request unchanged.
2. Policy resolution: `undefined`/`"auto"` → `AUTO = {tools:true, system:true, messages:{tail:1}}`; `"none"` → `{}`;
   object → as given (`cache-policy.ts:17-35`). Overrides when cache is undefined/"auto":
   * routes `openrouter` / `vercel-ai-gateway-messages`: by model id with leading `~` stripped — `anthropic/` → AUTO;
     `qwen/` or `alibaba/qwen` → `QWEN = {system:true, messages:{tail:1}}`; else NONE (`cache-policy.ts:58-68`).
   * route `alibaba-chat`: model id lower-case starts with `qwen` → QWEN else NONE.
3. Hint = `{type:"ephemeral", ttlSeconds?: policy.ttlSeconds}`. Budget = `max(0, 4 - existingHints)` where existing
   hints are counted over tools (recursively), system parts and message parts (`cache-policy.ts:160-171, 188`).
4. Order of placement: (a) last tool (recursing into a trailing namespace; skip if already hinted);
   (b) system part index 0 and index `len-1` (one part if len 1), skipping already-hinted parts;
   (c) messages: `latest-user-message`/`latest-assistant` → mark the last message of that role; `{tail:n}` → walk back
   from the end counting only non-effort-update messages until n counted, then mark every non-effort message from that
   start. Marking a message sets `cache` on its **last text part**, or the last part of any type if it has no text
   (`cache-policy.ts:110-135`). Each placement decrements the budget; at 0 nothing more is placed.

Wire lowering is per protocol. The shared Anthropic-style helper `cacheControl()` (`packages/ai/src/protocols/utils/cache.ts:16-23`)
allows at most 4 markers per request and emits `{type:"ephemeral", ttl?: "1h"}` where `ttl:"1h"` iff
`ttlSeconds >= 3600` (else omitted).

### 2.11 Effort updates (`packages/ai/src/effort-updates.ts`)

`effortUpdate(message)` = the single `effort` part of a system message with exactly one part. `applyEffortUpdates`:
if `route.supportsEffortUpdates?.(request)` keep markers, else strip all such messages (`effort-updates.ts:16-17`).
`resolveEffortUpdates(request, current)` (used by protocols that support it): no markers → `{request, effort: current}`;
last marker's `effort !== current` → strip markers, use `current`; else keep markers and use the **first** marker's
`previous` as top-level effort (`effort-updates.ts:20-25`). This keeps the top-level effort frozen so the prompt cache
is not invalidated.

### 2.12 Tool history normalization (`packages/ai/src/tool-history.ts`)

Run first in `resolveRequestOptions` (`packages/ai/src/route/client.ts:265-267`). Algorithm:
* Track `pending` = non-provider-executed tool calls from assistant messages.
* On a `user` or `assistant` message: if pending is non-empty, insert one synthetic `role:"tool"` message containing an
  error tool-result per pending call with value `"Tool result missing"` (`tool-history.ts:4, 50-63`), clear pending, then
  release held system messages.
* `system` messages arriving while calls are pending are held and emitted after all pending calls are answered.
* For `tool` messages: each non-provider-executed `tool-result` consumes its pending call; its `name/namespace` are
  rewritten to the call's; empty text or empty error value → `"(no tool output)"`; `content` results drop empty text
  items, and if nothing is left become text `"(no tool output)"` (`tool-history.ts:84-98`).
* At the end, pending calls get the synthetic missing-results message.
* Returns the same array instance when nothing changed.

### 2.13 Usage (`packages/ai/src/schema/events.ts:65-90`)

`Usage = { contextTokens?, inputTokens?, outputTokens?, nonCachedInputTokens?, cacheReadInputTokens?, cacheWriteInputTokens?, reasoningTokens?, totalTokens?, providerMetadata? }`.
Contract: `inputTokens` inclusive of cache read+write; `outputTokens` inclusive of reasoning;
`nonCached + cacheRead + cacheWrite = inputTokens`; `reasoningTokens <= outputTokens`; derived fields clamp at 0.
`visibleOutputTokens = max(0, output - reasoning)`. Helpers (`packages/ai/src/protocols/shared.ts:74-119`):
`totalTokens(in,out,total)` = total if defined, else undefined if both undefined, else `(in??0)+(out??0)`;
`subtractTokens(total, sub)` = undefined if total undefined, total if sub undefined, else `max(0,total-sub)`;
`sumTokens(...)` = undefined if all undefined else sum treating undefined as 0;
`fitThinkingBudget(budget, maxTokens, minimum=1)` = budget if no maxTokens or `budget <= maxTokens/2`, else
`max(minimum, floor(maxTokens/2))`. `providerMetadata` carries the raw usage keyed by provider namespace.
`contextTokens` = "effective input size of the final message iteration" (only some protocols set it).

### 2.14 Finish reasons

`FinishReason = "stop" | "length" | "tool-calls" | "content-filter" | "error" | "unknown"` (`packages/schema/src/llm.ts:5`).
`FinishReasonDetails = { normalized: FinishReason, raw?: string, category?: string, explanation?: string }`
(`events.ts:264-272`).

### 2.15 Stream events (`LLMEvent`, tagged by `type`; `events.ts:136-327`)

| `type` | Fields |
|---|---|
| `compaction` | same as `CompactionPart` |
| `step-start` | `index: number` |
| `text-start` | `id`, `providerMetadata?` |
| `text-delta` | `id`, `text`, `providerMetadata?` |
| `text-end` | `id`, `text?` (authoritative full value replacing deltas), `providerMetadata?` |
| `reasoning-start` / `reasoning-delta` / `reasoning-end` | same shapes as text |
| `tool-input-start` | `id`, `name`, `namespace?`, `providerExecuted?`, `providerMetadata?` |
| `tool-input-delta` | `id`, `name`, `namespace?`, `text` (raw JSON fragment), `input?` (lazy best-effort partial parse of all fragments so far) |
| `tool-input-end` | `id`, `name`, `namespace?`, `providerMetadata?` |
| `tool-input-error` | `id`, `name`, `namespace?`, `raw` (local call whose final JSON could not be parsed) |
| `tool-call` | `id`, `name`, `namespace?`, `input`, `providerExecuted?`, `providerMetadata?` |
| `tool-result` | `id`, `name`, `namespace?`, `result: ToolResultValue`, `output?: ToolOutput`, `providerExecuted?`, `providerMetadata?` |
| `tool-error` | `id`, `name`, `namespace?`, `message`, `error?`, `providerMetadata?` |
| `media` | `media: Asset`, `providerMetadata?` |
| `step-finish` | `index`, `reason: FinishReasonDetails`, `usage?`, `providerMetadata?` |
| `finish` | `reason`, `usage?`, `providerMetadata?` |
| `provider-error` | `message`, `classification?: "context-overflow"|"payload-too-large"`, `providerMetadata?` |

**Lifecycle helper** (`packages/ai/src/protocols/utils/lifecycle.ts`), used by most protocols:
* First content event of a response emits `step-start {index:0}` once.
* `textStart(id)`/`reasoningStart(id)` emit start once per open id; `textDelta` implies start; `*End` only if open.
* `finish(state, {reason, usage, providerMetadata})`: emit step-start if needed, then close all open reasoning blocks
  (`reasoning-end {id}`) and then all open text blocks (`text-end {id}`), then `step-finish {index:0, reason, usage, providerMetadata}`
  followed by `finish {reason, usage, providerMetadata}` (`lifecycle.ts:92-118`). Step index is always 0 at the
  protocol level (multi-step indices are a session concern).

**Terminal rule** (`packages/ai/src/route/client.ts:341-355`): a stream MUST end with exactly one terminal event
(`finish` or `provider-error`). Any event after a terminal → InvalidProviderOutput
`"Provider emitted <type> after the terminal event"`. Stream end without terminal → InvalidProviderOutput
`"The provider response ended unexpectedly."` with `classification: "incomplete-stream"` (`client.ts:332-339`).

**Tool-call streaming** (`packages/ai/src/protocols/utils/tool-stream.ts`): pending tools keyed by a stream-local key
(content index or item id). First append emits `tool-input-start`; non-empty fragments emit `tool-input-delta`.
Finalization (`finish`/`finishWithInput`/`finishAll`) parses the accumulated JSON with `parseToolInput`
(empty string → `"{}"`; strict JSON; error message `Invalid JSON input for <route> tool call <name>`,
`packages/ai/src/protocols/shared.ts:184-185`) and emits `tool-input-end` then `tool-call`. If parse fails and the
tool is **not** provider-executed, emits `tool-input-end` then `tool-input-error {raw}` instead of failing the stream;
provider-executed parse failure fails the stream (`tool-stream.ts:79-99`). `appendOrStart` errors if neither the
stored nor the delta id/name is known (route-specific `missingToolMessage`) (`tool-stream.ts:148-176`).

**Partial JSON** for `tool-input-delta.input` (`packages/ai/src/protocols/utils/partial-json.ts`): port of
promplate `partial-json` with `Allow.ALL`; first tries strict parse, then a repair pass that, inside strings,
re-escapes invalid backslash escapes as `\\\\` and control chars `<= 0x1f` as `\u00XX`, then the partial parser.
Failure yields `{}`. (Rust: any partial-JSON parser with the same leniency is acceptable since this field is advisory.)

### 2.16 Response assembly (`LLMResponse`, `events.ts:405-742`)

`LLMResponse = { message: assistant Message, events, usage?, finishReason }`. Reducer: text/reasoning parts are
created on first start/delta in order of first appearance and updated in place; `*-end.text` replaces accumulated
text; `tool-call` appends a tool-call part; `tool-result` appends a tool-result part; `media` appends media;
`compaction` appends itself; `usage` = last usage seen on any event; `finish` sets reason; `provider-error` sets reason
to `{normalized:"error"}` if none. `generate()` fails with the incomplete-stream error if no terminal event
(`packages/ai/src/route/client.ts:610-616`).

### 2.17 Compaction responses

`CompactionResponse = { replacement: Message[], usage? }`; `CompactionCheckpointResponse = { checkpoint: CompactionPart (encrypted non-empty), responseID: non-blank string, usage? }`
(`events.ts:115-134`). Routes may expose `compact.endpoint` (explicit compaction endpoint) and/or `compact.trigger`
(checkpoint); unsupported → UnsupportedOperation `"<provider>/<route> does not support explicit|trigger compaction"`,
operation `"compact"` / `"compact.trigger"` (`packages/ai/src/route/client.ts:236-259`).

---

## 3. Request pipeline: compile, routes, endpoints, auth, transport, framing

### 3.1 Compile steps (`packages/ai/src/route/client.ts:555-587`)

For every `stream`/`generate`:

1. `resolveRequestOptions` (`client.ts:265-280`): normalize tool history (2.12); `generation = merge(route.defaults.generation, model.defaults.generation, request.generation) ?? {}`;
   `providerOptions = deepMerge(route.defaults.providerOptions, model.defaults.providerOptions, request.providerOptions)`;
   `http = mergeHttp(route.defaults.http, model.defaults.http, request.http)`. (Route default headers are also folded
   into `route.defaults.http.headers` at route construction, `client.ts:147-161`.)
2. `sanitizeSurrogates` on every string and record key of the request except the model: lone UTF-16 surrogates are
   replaced via `String.prototype.toWellFormed` (U+FFFD); `Uint8Array`, `Error` and `Media.Asset` untouched
   (`packages/ai/src/utils/sanitize.ts:4-14`). Rust: strings are already valid UTF-8; apply the equivalent when
   decoding JSON that contains escaped lone surrogates (`\ud800`) → U+FFFD.
3. Tool dedupe per sibling level by key `"<type>:<name>"` (last definition wins, position of first) (`client.ts:558-562`).
4. Tool schema projection `ToolSchemaProjection.tools` (3.2).
5. `applyEffortUpdates` (2.11) then `applyCachePolicy` (2.10).
6. Route deployment headers: `headers = route.headers?.({request})`; if defined,
   `request.http = mergeHttp({headers}, request.http)` — i.e. request/model/route-default HTTP headers override the
   route header function output (`client.ts:568-571`).
7. `body = route.body.from(request)` (protocol lowering), then `route.prepareTransport(body, request, options)`.

### 3.2 Tool schema projection (`packages/ai/src/protocols/utils/tool-schema.ts`)

Applied to every tool (including nested) before lowering:
* `objectRoot` (`tool-schema.ts:60-75`): if `type` undefined or `"object"` and `not == {type:"null"}` exactly →
  `{type:"object", ...schema without not}`; else if `type` defined → unchanged; else if `anyOf` contains a branch exactly
  `{type:"object"}` → `{type:"object", ...without anyOf}`; else `{type:"object", ...schema}`. (Note key order: `type`
  first.)
* Sanitizer selection: `model.compatibility.sanitizer ?? route.sanitizer ?? (model.id =~ /gemini/i ? "gemini" : model.id =~ /kimi/i ? "moonshot" : none)`.
  `"gemini"` → `GeminiJsonSchema.normalize` (see 4.17); `"moonshot"` (`tool-schema.ts:5-44`): recursively, `$ref`
  nodes reduced to `{ $ref }`; nodes with `enum` and no `type` get `type` inferred (single JS type, or
  `[type, "null"]` when exactly one non-null type + null) inserted as the first key; array-valued `items` (tuple) →
  `{}` / the single schema / `{anyOf:[...]}`; `prefixItems` → `items` (dropped if `items` also present);
  `unevaluatedItems` removed. `"none"`/undefined → object root only.

### 3.3 Endpoint rendering (`packages/ai/src/route/endpoint.ts:38-61`)

`url = new URL(trimTrailingSlashes(baseURL ?? "") + renderedPath)`; then each static `endpoint.query` entry is `set`
on the search params. `Endpoint.merge` overrides baseURL/path and shallow-merges query. Then the HTTP transport
applies `request.http.query` entries with `searchParams.set` (`packages/ai/src/route/transport/http.ts:34-39, 53-56`).
`Route.model(...)` throws ProviderConfigurationError
`"Route.model(<id>) requires an endpoint baseURL — configure it on the route first"` when the route has no string
baseURL (`client.ts:129-145`).

### 3.4 Headers and auth (`packages/ai/src/route/transport/http.ts:51-69`, `packages/ai/src/route/auth.ts`)

Header assembly for an HTTP JSON request:
1. Start from `request.http.headers` (which already contains route headers + defaults, step 3.1.6).
2. `auth.apply({request, method:"POST", url, body: bodyText, headers})` adds/overrides auth headers.
3. `jsonPost` sets `content-type: application/json` last (overrides anything) and sends the body text
   (`packages/ai/src/protocols/shared.ts:377-381`).

Auth combinators (`auth.ts`):

| Combinator | Effect |
|---|---|
| `Auth.none` / `passthrough` | headers unchanged |
| `bearer(secret)` / `apiKey` | `authorization: Bearer <secret>` |
| `header(name, secret)` | `<name>: <secret>` |
| `bearerHeader(name, secret)` | `<name>: Bearer <secret>` |
| `scheme(name, secret)` | `authorization: <name> <secret>` (e.g. fal `Key`) |
| `headers(map)` | set all |
| `remove(name)` | delete header |
| `custom(fn)` | arbitrary (SigV4, OAuth refresh, etc.) |
| `config(ENV)` | credential from env/config var; empty or unset → `MissingCredentialError("<ENV> is not set")` |
| `a.orElse(b)`, `a.andThen(b)` | fallback / chaining |

`AuthOptions.bearer(options, envVar|envVars)` (`packages/ai/src/route/auth-options.ts:51-62`): explicit
`options.auth` wins; otherwise `optional(options.apiKey).orElse(config(env1)).orElse(config(env2))...` rendered as
bearer. Empty-string secrets count as missing (`auth.ts:73-77`). Auth failures map to
`AuthenticationError` with message `"Missing auth credential: <source>"` / `"<ENV> is not set"` /
`"Failed to resolve auth config: <msg>"` (`auth.ts:157-167`).

### 3.5 Body overlay (`packages/ai/src/route/transport/http.ts:41-49`)

If `request.http.body` is set, it is deep-merged over the protocol body (`mergeJsonRecords(body, overlay)`) and
re-encoded; non-object bodies fail with `"http.body can only overlay JSON object request bodies"`. This is the escape
hatch for arbitrary provider-specific body fields (core uses it for config `options` passthrough).

### 3.6 HTTP execution and timeouts (`packages/ai/src/route/transport/http.ts:81-143`, `packages/ai/src/route/executor.ts`)

* Method `POST`, body = JSON text. Executed through `RequestExecutor` (fetch-based HttpClient) with optional
  per-call middleware.
* Header timeout: `min(headerTimeout ?? 300000ms, timeout ?? ∞)`; on expiry TransportError
  `{message:"Timed out waiting for response headers", transport:"http", operation:"request", code:"Timeout", url}`.
* Chunk (idle) timeout: `chunkTimeout ?? 300000ms` between body chunks → `"Timed out waiting for response data"`
  (operation `"read"`).
* Total timeout remaining after headers → `"Timed out waiting for the response to complete"` (operation `"read"`).
  `false` disables a timeout.
* Status `>= 400`: body read as text and classified (section 7.2). Success statuses stream body bytes into framing.
* Transport failures (`executor.ts:171-216`): message prefers native `code: message` (e.g. `ECONNRESET: ...`);
  timeouts get code `"Timeout"`; body read failures become `"Connection lost while reading the response"`
  (optionally `": <detail>"`).

### 3.7 Framing (`packages/ai/src/route/framing.ts`)

* `sse` (`framing.ts:36-81`): UTF-8 decode, standard SSE parser (Effect's `makeParser`, WHATWG semantics: `data:`
  lines joined with `\n`, `event:` names, comments `:` ignored, `retry:` ignored, CR/LF/CRLF line endings, dispatch on
  blank line). Emit `event.data` when: data non-empty AND `data != "null"` AND `data != ": keepalive"` AND
  (`data != "[DONE]"` OR includeDone OR (event filter set and `event.event != "message"`)) AND (no event filter OR
  `event.event ∈ filter`). Parser errors → InvalidProviderOutput route `"sse"` with the chunk as body.
* `sseWithDone`: same but keeps `[DONE]`. `sseEvents(set)`: filter by event name.
* `lines`: NDJSON, non-blank lines. `document`: whole body as one frame.
* AWS event stream framing: see 4.2.

### 3.8 Stream decoding (`packages/ai/src/route/client.ts:423-501`)

Per frame: decode with `protocol.stream.event` schema (failure → InvalidProviderOutput
`"Invalid <provider>/<route> stream event"` with raw frame as `body`); optional `terminal(event)` stops consumption
after that event (`takeUntil`); `step(state, event)` emits events; at end `onHalt(state)` may flush; then the terminal
rule (2.15) is enforced. Errors are re-wrapped to carry `body` (raw frame) and `http` context (status/headers of the
response).

### 3.9 Transports

* `http-json` (`HttpTransport.httpJson({framing})`) — default for `Route.make`.
* WebSocket transport (`packages/ai/src/route/transport/websocket.ts`, `websocket-channel.ts`) — used by the OpenAI
  Responses WebSocket mode; see 4.16.

---

## 4. Chat protocols

Notation: "PO" = `request.providerOptions` (a flat object whose keys are protocol-specific — e.g. Anthropic reads
`PO.thinking`, `PO.effort`; it is NOT namespaced by provider at this layer). "gen" = `request.generation`.
All routes stream (`stream: true` or SSE endpoints); non-streaming generation is the stream folded by `LLMResponse`.
Every protocol below uses the lifecycle helper (2.15) unless stated.

### 4.1 anthropic-messages (and derivatives)

Source: `packages/ai/src/protocols/anthropic-messages.ts` (abbrev. `AM:`).

#### 4.1.1 Route, URL, headers

* Route id `anthropic-messages`, provider `anthropic`, metadata key `anthropic` (`AM:1663-1674`).
* Base URL `https://api.anthropic.com/v1` (`AM:37`). Path: `"/messages?beta=true"` when `model.provider === "anthropic"`,
  else `"/messages"` (`AM:1668`).
* Deployment header: `anthropic-version: 2023-06-01` (`AM:1673`).
* Auth (provider `anthropic`, `packages/ai/src/providers/anthropic.ts:32-37`): explicit `auth` override, else
  `apiKey` option, else env `ANTHROPIC_API_KEY`, sent as header `x-api-key: <key>`. Settings `authToken` instead uses
  `Authorization: Bearer <token>`; `apiKey` + `authToken` together → ProviderConfigurationError
  `"Anthropic apiKey cannot be combined with authToken"` (`anthropic.ts:55-71`).
* `anthropic-beta` header (`AM:1616-1661`): the transport merges required betas into any existing comma-separated
  `anthropic-beta` header (split on `,`, trimmed, de-duplicated preserving first-seen order, existing first), joined
  with `,` (no spaces). Required betas, in order:
  1. `interleaved-thinking-2025-05-14` — always.
  2. `compact-2026-01-12` — if `body.context_management.edits` non-empty OR any message has a `compaction` block.
  3. `mid-conversation-output-config-2026-07-01` — if any `role:"system"` message has `output_config`.
  4. `thinking-binding-controls-2026-08-01` (`THINKING_BINDING_BETA`, `packages/ai/src/protocols/utils/claude-model.ts:3`) — if
     `body.thinking` is set, not disabled/between_tools, and has `block_binding`.
* SSE framing restricted to event names: `message, message_start, message_delta, message_stop, content_block_start,
  content_block_delta, content_block_stop, ping, error` (`AM:43-54`).

#### 4.1.2 Provider options (`AM:337-359`)

Decoded from PO (unknown keys ignored); snake_case wins over camelCase where both exist:
`contextManagement {edits:[{type:"compact_20260112", trigger?:{type:"input_tokens", value:int>=50000}, pauseAfterCompaction?, instructions?}]}`,
`thinking` (`{type:"enabled", budgetTokens|budget_tokens, display?, block_binding?}` | `{type:"adaptive", display?, block_binding?}` | `{type:"disabled"|"between_tools"}`; `display ∈ "summarized"|"omitted"|string`; `block_binding {prefix_mismatch_behavior?: "error"|"drop_block"|string}`),
`effort` (string), `service_tier`/`serviceTier` (`"auto"|"standard_only"|string`), `metadata {user_id?}`,
`container` (string | `{id?, skills?}`), `inference_geo`/`inferenceGeo`, `cache_control`/`cacheControl`
(`{type:"ephemeral", ttl?: "5m"|"1h"}`), `output_config`/`outputConfig` (`{effort?, format?: {type:"json_schema", schema}}`).
Decode failure → InvalidRequest with the schema error message.

#### 4.1.3 Body (`AM:1038-1104`)

Field order as emitted (undefined fields omitted by JSON encoding):

```json
{
  "model": "<model.id>",
  "system": [{"type":"text","text":"...","cache_control":{"type":"ephemeral"}}],
  "messages": [...],
  "tools": [{"name":"...","description":"...","input_schema":{...},"cache_control":{...}}],
  "tool_choice": {"type":"auto"},
  "stream": true,
  "max_tokens": 32000,
  "temperature": 0.5, "top_p": 1, "top_k": 40, "stop_sequences": ["..."],
  "thinking": {"type":"enabled","budget_tokens":16000},
  "output_config": {"effort":"high","format":{...}},
  "cache_control": {...}, "container": ..., "inference_geo": "...", "metadata": {"user_id":"..."}, "service_tier": "...",
  "context_management": {"edits":[{"type":"compact_20260112","trigger":{...},"pause_after_compaction":true,"instructions":"..."}]}
}
```

Rules:
* Effort: `resolveEffortUpdates(request, PO.effort ?? output_config.effort)` (2.11). `output_config` emitted only if
  resolved effort or `format` defined: `{effort, format}`.
* Tools: namespaces flattened (`flattenToolRequest`, 2.5). `tools` omitted when empty. `tool_choice` only when tools
  are present and `request.toolChoice` set: auto→`{type:"auto"}`, none→`{type:"none"}`, required→`{type:"any"}`,
  tool→`{type:"tool", name}`; each (except none) adds `disable_parallel_tool_use` when `ToolChoice.disableParallelToolUse`
  is defined (`AM:554-576`).
* `system`: `request.system` parts with non-empty text → text blocks; omitted when none (`AM:1053-1061`).
* `max_tokens = gen.maxTokens ?? 32000` (`DEFAULT_MAX_TOKENS`, `AM:39`).
* `temperature/top_p/top_k/stop_sequences` from gen; `frequencyPenalty/presencePenalty/seed` ignored.
* Thinking: if `type:"enabled"`, `budget_tokens = fitThinkingBudget(budget, max_tokens, 1024)` (2.13). Then
  `applyThinkingBindingDefault` (`AM:1017-1027`): if thinking is not disabled AND the model supports block binding
  (`compatibility.supportsThinkingBlockBinding` override, else Claude version `major > 5 || (major == 5 && minor >= 1)`),
  thinking becomes `{...(thinking ?? {type:"adaptive"}), block_binding: {prefix_mismatch_behavior:"drop_block", ...thinking.block_binding}}`.
  So Claude ≥5.1 always gets adaptive thinking by default.
* `claudeVersion(id)` regex (`claude-model.ts:6-12`) on lower-cased id:
  `/(?:^|[./])claude-(?<family>[a-z]+)-(?<major>\d+)(?:[.-](?<minor>\d{1,2}))?(?:$|[-:@])/` → `{family, major, minor (default 0)}`.
* Cache markers: a single 4-breakpoint budget allocated in order tools → system → messages (`AM:1045-1050`);
  `cache_control` = `{type:"ephemeral"}` or `{type:"ephemeral", ttl:"1h"}` when `ttlSeconds >= 3600`; hints beyond 4
  are dropped with log warning `"Anthropic Messages: dropped N cache breakpoint(s); the API allows at most 4 per request."`
  (`AM:518-531, 1063-1067`).

#### 4.1.4 Message lowering (`AM:871-999`)

Iterate `messages` (after flattening):
* **system with effort part** → push `{role:"system", content:[], output_config:{effort: effort ?? "high"}}` at its
  position (`AM:887-893`). (Only reachable when `supportsEffortUpdates`: thinking not disabled, and override or
  Claude opus major ≥ 5 or version ≥ 5.1, `AM:1007-1015`.)
* **system text update**: if it would sit between a local tool call and its result →
  InvalidRequest `"Anthropic Messages system updates cannot split a local tool call from its tool result"`.
  If model supports native system updates (Claude opus 4.x with minor ≥ 8, or major ≥ 5, `AM:819-824`) the update is
  held and released before the next assistant message (or at the end): if the last non-system lowered message is a
  `user` turn or an assistant turn ending in `server_tool_use`, emit native `{role:"system", content:[text blocks]}`;
  otherwise append the wrapped `<system-update>` text to the last user turn (or new user turn). Without native support
  it is always wrapped into the user turn immediately.
* **user**: text parts with non-blank text → `{type:"text", text, cache_control?}` (blank skipped); media → 4.1.5;
  other part types → InvalidRequest `"Anthropic Messages user messages only support text and media content for now"`.
  Empty user messages are dropped.
* **assistant**: `compaction` (must match provider and have `text` field, else
  `"Compaction state must be replayed to its originating provider and API"`) → `{type:"compaction", content: text}`;
  non-blank text → text block; `reasoning`:
  - signature = `part.encrypted ?? providerMetadata[key].signature`; redacted = `providerMetadata[key].redactedData`.
  - no signature and redacted present → `{type:"redacted_thinking", data}`.
  - blank/missing signature: skip if text blank; if `requireThinkingSignature` is false → `{type:"thinking", thinking, signature:""}`;
    else demote to a text block (with cache_control).
  - otherwise `{type:"thinking", thinking: text, signature}`.
  - `requireThinkingSignature` (`AM:799-816`): `compatibility.requireSignature` override; false for providers
    `kimi-for-coding`, `moonshotai`, `moonshotai-cn`, model ids starting `kimi-`, base URLs containing
    `api.kimi.com/coding`, `api.moonshot.ai/anthropic`, `api.moonshot.cn/anthropic`, or provider containing `xiaomi`,
    model containing `mimo`, base URL containing `xiaomimimo.com`; else true.
  - `tool-call` → `{type:"tool_use"|"server_tool_use" (if providerExecuted), id: scrub(id), name, input}` where
    `scrub` replaces `[^a-zA-Z0-9_-]` with `_` (`AM:578`).
  - provider-executed `tool-result` → `{type: web_search_tool_result|code_execution_tool_result|web_fetch_tool_result, tool_use_id, content}`
    by tool name `web_search|code_execution|web_fetch`; content = `providerMetadata[key].result ?? result.value`;
    unknown name → InvalidRequest `"Anthropic Messages does not know how to round-trip server tool result for <name>"`.
  - anything else → `"Anthropic Messages assistant messages only support text, reasoning, and tool-call content for now"`.
  - Empty assistant content → message dropped. Held system updates are released before pushing the assistant message.
* **tool**: each part must be `tool-result` (else `"Anthropic Messages tool messages only support tool-result content for now"`) →
  `{type:"tool_result", tool_use_id: scrub(id), content, is_error: true (only when result.type=="error"), cache_control?}`.
  `content` is the `toolResultText` string for json/text/error; for `content` results an array of text blocks and
  media blocks (tool `file` items lifted via `toolFileMedia`; media lowered **without** cache markers).
  If the previous lowered message is a user message consisting only of `tool_result` blocks, append to it; else push
  a new user message (`AM:991-994`).

`providerMetadataKey` = `route.providerMetadataKey ?? provider` (`AM:873`).

#### 4.1.5 Media lowering (`AM:674-781`)

`mime = lower(mediaType)`; cache marker computed if a budget is passed.
1. File id from `metadata.anthropic.file_id|fileId` or `metadata.file_id|fileId`: image mime →
   `{type:"image", source:{type:"file", file_id}, cache_control?, transformations?}`; else
   `{type:"document", source:{type:"file", file_id}, cache_control?, title?, context?, citations?}`.
2. `url` source matching `/^https?:\/\//i`: image → `{type:"image", source:{type:"url", url}}`; `application/pdf` →
   document url source.
3. Otherwise inline required (`"Anthropic Messages requires inline media ..."`):
   `text/plain` → `{type:"document", source:{type:"text", media_type:"text/plain", data: utf8-decoded}}`;
   `application/pdf` → `{type:"document", source:{type:"base64", media_type:"application/pdf", data}}`;
   `image/*` → `{type:"image", source:{type:"base64", media_type: mime, data}}`;
   else InvalidRequest `"Anthropic Messages does not support media type <mediaType>"`.
* `title` = `metadata.anthropic.title` ?? `metadata.title` ?? non-empty `filename`; `context` = `metadata.anthropic.context ?? metadata.context`;
  `citations` = `{enabled}` from `metadata.anthropic.citations ?? metadata.citations`;
  `transformations.oversized_image ∈ {"downsize","error"}` from `metadata.anthropic.transformations`, `metadata.transformations`
  or `metadata.anthropic.oversized_image`.

#### 4.1.6 Stream parsing (`AM:1106-1585`)

State: `{provider, compactions: index→text|null, providerMetadataKey, tools (keyed by content index), reasoningSignatures: index→sig, usage?, pendingFinish?, lifecycle}`.

| Event | Handling |
|---|---|
| unknown `type` (not in SSE set) | ignored |
| `message_start` | `usage = merge(usage, mapUsage(message.usage))`; no events |
| `content_block_start` `tool_use`/`server_tool_use` | requires `index` (`"Anthropic <type> missing index"`) and `id` (`"Anthropic tool_use missing id at index N"`). Start pending tool with initial input = JSON of `block.input` if it is a non-empty object or non-object; emit `step-start` (once) + `tool-input-start {id, name, providerExecuted?}` |
| `content_block_start` `text` | id `text-<index>`; text-start; if `block.text` non-empty emit text-delta |
| `content_block_start` `thinking` | id `reasoning-<index>`; reasoning-start with metadata `{[key]:{signature}}` if present; delta if non-empty; remember signature |
| `content_block_start` `redacted_thinking` | reasoning-start with metadata `{[key]:{redactedData: data}}` |
| `content_block_start` `compaction` | store `compactions[index] = content` |
| `content_block_start` `*_tool_result` | emit `tool-result {id: tool_use_id, name: web_search|code_execution|web_fetch, result: error if content.type ends with "_tool_result_error" else json, providerExecuted:true, providerMetadata:{[key]:{blockType, result: content}}}` |
| other block types | ignored |
| `content_block_delta` `text_delta` | text-delta on `text-<index>` if open |
| `thinking_delta` | reasoning-delta if open |
| `signature_delta` | store signature for index (no event) |
| `input_json_delta` | append `partial_json` to the pending tool; emits `tool-input-delta` |
| `compaction_delta` | `compactions[index] = delta.content` (error `"Compaction delta is missing its block or content"` if no block) |
| other delta types | ignored |
| `content_block_stop` | compaction → emit `compaction {provider, text}`; else finish pending tool at index (tool-input-end + tool-call / tool-input-error); otherwise close `text-<i>` (text-end) and `reasoning-<i>` (reasoning-end with `{[key]:{signature}}` if a signature was stored) |
| `message_delta` | `usage = merge(usage, mapUsage(event.usage))`; if `delta.stop_reason` set: `pendingFinish = {reason:{normalized: map(stop_reason), raw: stop_reason, category?, explanation?}, providerMetadata: {[key]:{stopSequence}} when stop_sequence set}` (category/explanation from `delta.stop_details`) |
| `message_stop` | error if compactions pending (`"Response ended with an incomplete compaction block"`); finish all pending tools; close reasoning blocks with stored signatures; `Lifecycle.finish({reason: pendingFinish.reason ?? {normalized:"unknown"}, usage, providerMetadata})` |
| `ping` | ignored |
| `error` | fail with `classifyProviderFailure({message: "<error.type>: <error.message>" (or whichever exists, or "Anthropic Messages stream error"), rawBody: JSON(event)})` (section 7) |

Validation: events with a `content_block` (other than start) or `delta` that fail the loose schemas →
InvalidProviderOutput `"Invalid anthropic/anthropic-messages stream event"`.

Finish reason map (`AM:1109-1115`): `end_turn|stop_sequence|pause_turn` → stop; `max_tokens|model_context_window_exceeded` → length;
`tool_use` → tool-calls; `refusal` → content-filter; else unknown.

Usage (`AM:1123-1183`): iterations = `usage.iterations` if non-empty else `[usage]`.
`nonCached = Σ input_tokens`, `cacheRead = Σ cache_read_input_tokens`, `cacheWrite = Σ cache_creation_input_tokens`,
`inputTokens = sum(nonCached, cacheRead, cacheWrite)`, `outputTokens = Σ output_tokens`,
`reasoningTokens = Σ output_tokens_details.thinking_tokens`, `contextTokens` = input+cacheRead+cacheWrite of the last
iteration when its `type == "message"`, `totalTokens = in + out`, `providerMetadata = {[key]: rawUsage}`.
Merge (start + delta): right-biased per field, `inputTokens` recomputed, provider metadata deep-merged.

#### 4.1.7 Derivatives

| Route id | Protocol | Differences |
|---|---|---|
| `anthropic-compatible-messages` (`packages/ai/src/providers/anthropic-compatible.ts:31-58`) | anthropic-messages | provider configurable; baseURL required (`"Anthropic-compatible providers require a baseURL"`); auth `x-api-key` from apiKey only (no env); `authToken` → Bearer |
| `google-vertex-messages` (`packages/ai/src/providers/google-vertex-messages.ts:37-91`) | anthropic-messages body minus `model`, plus `"anthropic_version":"vertex-2023-10-16"` | path `/<model.id>:streamRawPredict`; baseURL `https://<host(location)>/v1/projects/<project>/locations/<location>/publishers/anthropic/models`; header `anthropic-version: 2023-06-01`; metadata key `vertex`; Google OAuth bearer (4.17); API keys rejected (`"Google Vertex Messages does not support API keys"`) |
| `minimax-messages` (`packages/ai/src/providers/minimax.ts:77-85`) | anthropic-messages | base `https://api.minimax.io/anthropic/v1`, path `/messages`, bearer `MINIMAX_API_KEY`, `anthropic-version: 2023-06-01`, no beta transport |
| `alibaba-messages` (`packages/ai/src/protocols/alibaba-messages.ts`) | anthropic body with PO.thinking removed, then `thinking = {type: PO.thinking.type, budget_tokens?: fitThinkingBudget(budgetTokens|budget_tokens, max_tokens)}` (no 1024 minimum, no block-binding default); effort-updates not forwarded (stripped) |
| `zai-messages` (`packages/ai/src/protocols/zai-messages.ts`) | anthropic body with thinking replaced by `PO.thinking` verbatim (`{type}` only) |
| `meta-messages` (`packages/ai/src/protocols/meta-messages.ts`) | tools whose definition has `native.meta` (validated as MetaResponses.WebSearch) become `{type:"web_search", name:"web_search", user_location}`; `search_context_size` → `"Meta Messages does not support searchContextSize"` |

Routes/base URLs for alibaba/zai/meta/moonshot/etc. are listed in section 5.

### 4.2 bedrock-converse (+ AWS event stream, + Bedrock Mantle)

Source: `packages/ai/src/protocols/bedrock-converse.ts` (abbrev. `BC:`), `bedrock-event-stream.ts` (`BES:`),
`utils/bedrock-cache.ts` (`BCa:`), `utils/bedrock-media.ts` (`BM:`),
`packages/ai/src/providers/amazon-bedrock.ts` (`AB:`).

#### 4.2.1 Route, URL, auth

* Route id `bedrock-converse`, provider `amazon-bedrock` when configured (route default `bedrock`), metadata key `bedrock` (`BC:878-891`, `AB:40-54`).
* URL: `<baseURL>/model/<encodeURIComponent(body.modelId)>/converse-stream` (`BC:886-888`); baseURL default
  `https://bedrock-runtime.<region>.amazonaws.com` (`AB:38`).
* Region: `config.region ?? credentials.region ?? $AWS_REGION ?? $AWS_DEFAULT_REGION ?? "us-east-1"` (`packages/ai/src/protocols/utils/bedrock-auth.ts:116-120`).
* Auth precedence (`resolveAuth`, `packages/ai/src/protocols/utils/bedrock-auth.ts:140-145`):
  1. Bearer: `apiKey ?? $AWS_BEARER_TOKEN_BEDROCK` (ignored when `mode:"sigv4"`) → `Authorization: Bearer <token>`.
  2. Static `credentials {region, accessKeyId, secretAccessKey, sessionToken?}` → SigV4 with the resolved region.
  3. AWS default provider chain (env vars, shared config/SSO with `ignoreCache: true`, web identity, process,
     container/instance metadata), optional `profile`, re-resolved on **every request**. Failure →
     AuthenticationError `"AWS default credential chain failed: <msg>"`.
  Config errors: `auth:"bearer"` without key/env → `"Amazon Bedrock bearer auth requires apiKey"`;
  `auth:"sigv4"` with apiKey → `"Amazon Bedrock SigV4 auth does not accept apiKey"` (`AB:42-45`).
* SigV4 (`packages/ai/src/protocols/utils/bedrock-auth.ts:58-111`): set `content-type: application/json` first, then sign
  method `POST`, the exact URL and JSON body bytes, all current headers, service `"bedrock"` (Mantle:
  `"bedrock-mantle"`), region from credentials; merge signed headers (`authorization`, `x-amz-date`,
  `x-amz-content-sha256`?, `x-amz-security-token` when session token). Implementation used: `aws4fetch` `AwsV4Signer`
  (signs `host`, `x-amz-date`, `x-amz-security-token`, `content-type` and other provided headers; Rust: use `aws-sigv4`
  with equivalent canonical headers). Signing failure → InvalidRequest `"<name> SigV4 signing failed: <msg>"`;
  no credentials → `"Bedrock Converse requires either route bearer auth or AWS credentials configured on the route"`.

#### 4.2.2 Body (`BC:497-569`)

```json
{
  "modelId": "<model.id>",
  "messages": [...],
  "system": [{"text":"..."},{"cachePoint":{"type":"default"}}],
  "inferenceConfig": {"maxTokens":4096,"temperature":0.2,"topP":0.9,"stopSequences":["..."]},
  "toolConfig": {"tools":[{"toolSpec":{"name":"...","description":"...","inputSchema":{"json":{...}}}},{"cachePoint":{"type":"default"}}],"toolChoice":{"auto":{}}},
  "additionalModelRequestFields": {"top_k":40,"thinking":{"type":"enabled","budget_tokens":2048},"anthropic_beta":["thinking-binding-controls-2026-08-01"]}
}
```

* PO: `thinking? {type:"enabled", budgetTokens:number}` only (`BC:470-474`).
* `maxTokens = gen.maxTokens`, except `undefined` when the model id matches `/\bamazon\.nova-2-/` AND
  `request.http.body.additionalModelRequestFields.reasoningConfig.maxReasoningEffort == "high"` (`BC:459-468, 502-503`).
* `thinking`: from PO → `{type:"enabled", budget_tokens: fitThinkingBudget(budgetTokens, maxTokens, 1024)}`; then
  block-binding default (unless `http.body.additionalModelRequestFields.thinking.type ∈ {disabled, between_tools}`):
  if Claude ≥5.1 (4.1.3) → `{...(thinking ?? {type:"adaptive"}), block_binding:{prefix_mismatch_behavior:"drop_block"}}` (`BC:486-495`).
* `additionalModelRequestFields` present iff topK or thinking defined: `{top_k?, thinking?, anthropic_beta?: ["thinking-binding-controls-2026-08-01"] (iff thinking.block_binding)}`.
* `inferenceConfig` omitted when maxTokens, temperature, topP undefined and stop empty/undefined.
* `toolConfig` only when flattened tools non-empty; each tool → `toolSpec {name, description (omitted when blank), inputSchema:{json}}`
  followed by a `cachePoint` when hinted. `toolChoice`: auto→`{auto:{}}`, required→`{any:{}}`, tool→`{tool:{name}}`,
  none→omitted (definitions kept).
* `system`: non-empty system parts → `{text}` + optional `cachePoint` (omitted if none).
* Cache points (`BCa:8-56`): positional `{cachePoint:{type:"default"}}` or with `"ttl":"1h"`; budget 4 shared
  tools → system → messages; only emitted if `supported` = model id contains `anthropic.claude-` and (is one of the
  CLAUDE_5M ids OR not a legacy prefix `anthropic.claude-instant|claude-v1|claude-v2|claude-3-`). `ttl:"1h"` only when
  `ttlSeconds >= 3600` and model not in CLAUDE_5M list
  (`anthropic.claude-3-5-sonnet-20241022-v2:0, anthropic.claude-3-5-haiku-20241022-v1:0, anthropic.claude-3-7-sonnet-20250219-v1:0, anthropic.claude-sonnet-4-20250514-v1:0, anthropic.claude-opus-4-20250514-v1:0, anthropic.claude-opus-4-1-20250805-v1:0`).
  Dropped markers log `"Bedrock Converse: dropped N cache breakpoint(s); the API allows at most 4 per request."`

Message lowering (`BC:326-448`):
* Consecutive user-role outputs are merged into one user message (system updates, user, tool results all map to
  `role:"user"`).
* system update → wrapped `<system-update>` text (+cachePoint) appended to the previous user message or new one.
* user: text → `{text}` (+cachePoint; blank text is NOT skipped here); media → 4.2.3.
* assistant: text → `{text}`+cachePoint; reasoning: signature = `encrypted ?? metadata[key].signature`;
  `redactedData` (metadata) without signature → `{reasoningContent:{redactedContent: data}}`; no/blank signature →
  text block if non-blank text else skipped; else `{reasoningContent:{reasoningText:{text, signature}}}`;
  tool-call → `{toolUse:{toolUseId, name, input}}` where name = `name.replace(/[^a-zA-Z0-9_-]/g,"_").slice(0,64) || "_"`
  and `input` has empty-string keys removed recursively. Empty assistant messages dropped.
* tool: `{toolResult:{toolUseId, content, status:"error"|"success"}}`; content: text/error →
  `[{text: toolResultText}]`, json → `[{json: value}]`, content → text items and media blocks. Unless the model id
  contains `anthropic.claude-` or `amazon.nova-`, images are hoisted out of tool results (result content replaced by
  `[{text:"See attached image."}]` if nothing else remains) and appended to the same user message after all tool
  results, flushed when a non-tool message arrives (`BC:323-344, 417-444`).
* Tool ids: when model id contains `mistral.`, ids not matching `/^[A-Za-z0-9]{9}$/` are mapped to a 9-char base36
  FNV-style hash (see 4.5 MistralToolID) unique within the request.

#### 4.2.3 Media (`BM:95-119`)

Inline required. Images: `image/png|jpeg|jpg|gif|webp` → `{image:{format: png|jpeg|gif|webp, source:{bytes: base64}}}`;
other `image/*` → `"Bedrock Converse does not support image media type <t>"`. Documents by mime:
`application/pdf→pdf, text/csv→csv, application/msword→doc, ...wordprocessingml.document→docx, application/vnd.ms-excel→xls, ...spreadsheetml.sheet→xlsx, text/html→html, text/plain→txt, text/markdown→md`
→ `{document:{format, name, source:{bytes}}}`. Name: filename minus extension, chars outside `[a-zA-Z0-9 ()[\]-]`
→ space, collapse whitespace, trim, max 200, fallback `"document"`; uniqueness across the request with suffix `" 2"`,
`" 3"`...; if the label differs from `filename`, prepend a text block
`Attached file "<filename>" has document label "<name>".` (JSON-quoted). Other → `"Bedrock Converse does not support media type <t>"`.

#### 4.2.4 AWS event-stream framing (`BES:13-141`)

Binary frames `[total_len u32 BE][headers_len u32 BE][prelude_crc u32][headers][payload][message_crc u32]`, CRC32
validated (smithy codec). Buffer across chunks; at end of stream leftover bytes → InvalidProviderOutput
`"Incomplete Bedrock Converse event-stream frame: N buffered bytes remain at end of stream"` (classification
`incomplete-stream`, body = base64 of remainder). Decode errors →
`"Failed to decode Bedrock Converse event-stream frame: <msg>"`.
Header `:message-type`:
* `error` → fail `"<:error-code>: <:error-message>"` (or `"Bedrock Converse event-stream error"`).
* `event` → event name from `:event-type`; `exception` → name from `:exception-type`; other → skip. Empty payload →
  skip. Payload JSON parsed (`"Failed to parse Bedrock Converse event-stream payload"`), key `p` (padding) deleted,
  frame becomes `{[eventType]: payload}` or `{exception:{type, details: payload}}`, plus `rawBody` =
  `JSON({headers, body: payloadText})` for diagnostics.

#### 4.2.5 Stream events (`BC:618-846`)

| Frame | Handling |
|---|---|
| `contentBlockStart.start.toolUse {toolUseId, name}` | start tool at `contentBlockIndex`; step-start; `tool-input-start` |
| `contentBlockDelta.delta.text` (non-empty) | text-delta `text-<index>` (implicit text-start) |
| `contentBlockDelta.delta.reasoningContent {text?, signature?, redactedContent? (base64), data?}` | `redactedContent` base64-decoded and accumulated per index (invalid → `"Bedrock Converse reasoningContent.redactedContent contains invalid base64 data"`); metadata = `{signature}` if signature, else `{redactedData: data}` if `data` and no redactedContent; emit reasoning-delta (text or "") when text or metadata; else reasoning-start if only redacted bytes; signature stored per index |
| `contentBlockDelta.delta.toolUse.input` | append to open tool (ignored if not open) |
| `contentBlockStop` | finish tool (tool-input-end + tool-call/tool-input-error; marks `hasToolCalls`) else close text and reasoning (metadata `{signature}` or `{redactedData: base64(concat chunks)}`) |
| `messageStop.stopReason` | `malformed_model_output`/`malformed_tool_use` → InvalidProviderOutput `"Bedrock Converse stopped with <reason>"`; else store `{normalized: map, raw}` |
| `metadata.usage` | store mapped usage |
| `exception` | fail with `classifyProviderFailure({message: details.message ?? details.originalMessage ?? "Bedrock Converse stream error", rawBody: JSON(event)})` |
| `messageStart` | ignored |

`onHalt` (end of byte stream): if a finish reason was stored, close remaining redacted reasoning blocks and emit
`Lifecycle.finish` with `normalized = "tool-calls"` when mapped `stop` but tool calls were seen. No stored reason →
no terminal → incomplete-stream error.

Finish map (`BC:574-580`): `end_turn|stop_sequence` → stop; `max_tokens|model_context_window_exceeded` → length;
`tool_use` → tool-calls; `content_filtered|guardrail_intervened` → content-filter; else unknown.
Usage (`BC:584-600`): `nonCached = inputTokens`, `cacheRead/Write` direct, `inputTokens = sum(all three)`,
`outputTokens`, `totalTokens = provider totalTokens ?? in+out`, raw usage under `{bedrock: ...}`; no reasoning tokens.

#### 4.2.6 Bedrock Mantle (`packages/ai/src/providers/amazon-bedrock-mantle.ts`)

Same provider id `amazon-bedrock`, metadata key `mantle`, SigV4 service `bedrock-mantle`, name `"Bedrock Mantle"`.
Routes: `bedrock-mantle-responses` (open-responses, path `/responses`, defaults PO `{store:false, include:["reasoning.encrypted_content"]}`),
`bedrock-mantle-chat` (openai-chat), `bedrock-mantle-messages` (anthropic-messages with beta transport,
`anthropic-version: 2023-06-01`, effort updates only for Claude ≥5.1 unless compat override). Base URLs:
`https://bedrock-mantle.<region>.api.aws/v1` (responses/chat) and `https://bedrock-mantle.<region>.api.aws/anthropic/v1` (messages).
Default model factory = responses.

### 4.3 openai-chat (Chat Completions)

Source: `packages/ai/src/protocols/openai-chat.ts` (abbrev. `OC:`).

#### 4.3.1 Route, URL, headers

* Route id `openai-chat`, provider `openai`, metadata key `openai`; base `https://api.openai.com/v1`, path
  `/chat/completions` (`OC:33-34, 1306-1314`). Framing: SSE **with** `[DONE]` retained; `[DONE]` is a terminal
  event (`OC:1297-1303`).
* Auth (OpenAI provider, `packages/ai/src/providers/openai.ts:81`): `AuthOptions.bearer(options, "OPENAI_API_KEY")` →
  `Authorization: Bearer <key>`. Settings `organization` → header `OpenAI-Organization`, `project` → `OpenAI-Project`
  (`openai.ts:136-159`). `queryParams` → endpoint query. OpenAI chat models get `compatibility.supportsPromptCacheKey = true` (`openai.ts:105-109`).
* OpenAI provider default PO (`packages/ai/src/providers/openai-options.ts:35-61`), merged under caller PO:
  `{store:false}`; additionally for model ids containing `gpt-5` but not `gpt-5-chat`/`gpt-5-pro`:
  `{reasoningEffort:"medium", reasoningSummary:"auto", include:["reasoning.encrypted_content"]}`.

#### 4.3.2 Provider options (shared with Responses; `packages/ai/src/protocols/utils/open-responses-options.ts:54-88`)

Each key decoded leniently (malformed value → dropped individually): `store: bool`, `metadata: Record<string,string>`,
`safetyIdentifier`, `streamOptions {includeObfuscation?}`, `topLogprobs: int 0..20`, `reasoningEffort: string`,
`reasoningSummary: "auto"|"concise"|"detailed"`, `include: string[]` (empty → undefined), `textVerbosity`,
`serviceTier`, `truncation: "auto"|"disabled"`, `allowedTools {toolNames, mode?}` (empty names → undefined;
mode default `"auto"`), `maxToolCalls: int`, `parallelToolCalls: bool`. Chat uses only `store`, `reasoningEffort`.

#### 4.3.3 Body (`OC:804-854`)

Key order: `model, messages, tools, tool_choice, stream, stream_options, tool_stream, max_completion_tokens|max_tokens,
temperature, top_p, frequency_penalty, presence_penalty, seed, stop, store, prompt_cache_key, reasoning_effort`.

* If `compatibility.reasoningField` ∈ {`role`,`content`,`refusal`,`tool_calls`} → InvalidRequest
  `"OpenAI Chat reasoning field conflicts with reserved field <f>"`.
* `tools`: flattened tools → `{type:"function", function:{name, description, parameters: inputSchema, strict:false (only if supportsStrictMode)}, cache_control?}`.
  No active tools but tool history exists (any `tool` message or assistant tool-call) → `"tools": []`; else omitted (`OC:666-675, 833-838`).
* `tool_choice` only with active tools: `"auto"|"none"|"required"` or `{type:"function", function:{name}}`.
* `stream: true`; `stream_options: {include_usage: true}` when `supportsUsageInStreaming` (default true).
* `tool_stream: true` when zaiToolStream (Z.ai providers/hosts, except `glm-4.5`, `glm-4.5-air`, `glm-4.5-flash`, `glm-4.5v`) and tools active (`OC:770-784`).
* Max tokens field: `compatibility.maxTokensField` else `max_tokens` for provider ids `deepseek, moonshotai, togetherai, zai, zai-coding-plan, zhipuai, zhipuai-coding-plan, nvidia, cerebras, chutes, cloudflare-ai-gateway, cloudflare-workers-ai`
  or base URLs containing `deepseek.com, api.moonshot.ai, api.together., api.z.ai, open.bigmodel.cn, integrate.api.nvidia.com, cerebras.ai, llm.chutes.ai, gateway.ai.cloudflare.com, api.cloudflare.com`;
  otherwise `max_completion_tokens` (`OC:682-714`). Value = `gen.maxTokens` (key emitted with undefined → omitted).
* `store`: when supportsStore → `PO.store ?? false`; else omitted. `supportsStore` = `compatibility.supportsStore` else
  false for Gemini base URL (`generativelanguage.googleapis.com`), nvidia, cerebras, xai, together, chutes, deepseek,
  zai/zhipu, moonshot, opencode (`opencode.ai`), cloudflare (both), vercel-ai-gateway (`ai-gateway.vercel.sh`/`vercel.sh`),
  ant-ling (`api.ant-ling.com`) (`OC:716-756`).
* `prompt_cache_key`: only if `compatibility.supportsPromptCacheKey` → `promptCacheKey(request)` (undefined when
  `cache == "none"`; truncated to 64 Unicode code points) (`packages/ai/src/protocols/shared.ts:40-49`).
* `reasoning_effort`: `PO.reasoningEffort` when truthy.
* `strict` support (`OC:760-768`): false for moonshot, together, cloudflare-ai-gateway, nvidia; else true.

#### 4.3.4 Message lowering (`OC:551-664`)

* System: if any system part has a cache hint AND a cacheControl lowering option is present (DigitalOcean,
  OpenRouter, Alibaba chat, Venice) → one `{role:"system", content:[{type:"text", text, cache_control}...]}`; else
  `{role:"system", content: join(texts, "\n")}`; omitted when no system parts.
* `requireReasoning` = `compatibility.requireReasoning ?? (compatibility.reasoningField defined || provider == "deepseek" || baseURL contains "deepseek.com" || modelId contains "deepseek")`.
* Tool call id rewriting (`OC:577-591`): model id (lower) contains any of `mistral, devstral, codestral, pixtral, mixtral`
  → strip non-alphanumerics, first 9 chars, right-pad with `"0"` to 9; else contains `claude` → replace
  `[^a-zA-Z0-9_-]` with `_`; else provider `openai`/`azure` or model id starts with `openai/` → first 40 chars; else unchanged.
* `requireAssistantAfterTool` = compat ?? mistral-family: before a user message (or before flushing attachments), if
  the last message is `role:"tool"`, insert `{role:"assistant", content:"Done."}`.
* system update messages: wrapped text; merged into a pending-attachments user message, or appended to the previous
  user message (string content → `"<prev>\n<text>"`, or array form when a cache marker is present), else a new user message.
* Assistant messages consisting only of blank text parts are skipped.
* user: text → `{type:"text", text, cache_control?}`; media (`OC:365-380`): PDF → `{type:"file", file:{filename: filename ?? "document.pdf", file_data: dataUrl}}` (inline required);
  image → `{type:"image_url", image_url:{url: url-source URL or data URL}}`; other kinds →
  `"OpenAI Chat does not support media type <t>"`. If every part is text without cache_control, content collapses to the
  concatenated string (joined with `""`).
* assistant (`OC:435-498`): text parts; reasoning parts collected; tool calls → `{id, type:"function", function:{name, arguments: JSON(input ?? {})}, extra_content?}`
  where `extra_content` = `providerMetadata[key].extraContent` if it matches `{google:{thought_signature}}`.
  `content` = text-part array if any part has cache_control; `null` if no text and has tool calls; else joined text.
  Reasoning replay field selection:
  - `configuredField` (= `compatibility.reasoningField`) used when requireReasoning or reasoning present or native
    `reasoning_content` exists (`message.native.openaiCompatible.reasoning_content`).
  - else no reasoning → `"reasoning_content"` if requireReasoning else none;
  - else the field recorded on the part (`providerMetadata[key].reasoningField`); else `reasoning_content` if native;
    else `reasoning_content` if not all parts carry `reasoningDetails` or requireReasoning; else `reasoning_content`
    if any detail is Kimi-style (`summary`/`encrypted`).
  - reasoning text = concatenation of reasoning part texts (`""` join), or native / `""` (requireReasoning) when none.
  - `reasoning_details`: from parts' `providerMetadata[key].reasoningDetails` (if any part has them) else
    `native.openaiCompatible.reasoning_details`; only known shapes kept (`reasoning.text|reasoning.summary|reasoning.encrypted`
    OpenRouter entries verbatim; Kimi `summary`/`encrypted` re-emitted without `index`) (`OC:79-115, 390-409`).
* tool: each tool-result → `{role:"tool", tool_call_id, content: text | [{type:"text", text, cache_control}]}`; text =
  `toolResultText` for non-content results; for `content` results text items joined with `"\n"` and `file` items
  lowered as media into a pending user message (attachments) inserted after the tool messages (`OC:500-535, 597-602`).

#### 4.3.5 Stream parsing (`OC:1043-1264`)

Event: `"[DONE]"` or JSON `{choices?: [{delta?, finish_reason?, native_finish_reason?, usage?}], usage?, error?}`.

1. `error` present → fail `classifyProviderFailure({message: error.message, status: error.code if number, rawBody: JSON(event)})`.
2. Only `choices[0]` is considered. `usage = map(event.usage) ?? map(choice.usage) ?? state.usage`.
3. `finish_reason` mapping (`OC:862-896`): `stop|end` → stop; `length` → length; `content_filter` → content-filter;
   `function_call|tool_calls` → tool-calls; `error` → fail UnknownProvider `"Provider reported an error (finish_reason: error)"`;
   `network_error` → fail ProviderInternal `"Provider reported a network error (finish_reason: network_error)"`;
   anything else → fail UnknownProvider `"Provider finish_reason: <reason>"`. `raw = native_finish_reason ?? finish_reason`.
4. After a finish reason has been seen, further chunks may only carry usage; content/refusal/reasoning/tool deltas →
   InvalidProviderOutput `"OpenAI Chat received content after the finish reason"`.
5. Reasoning: first non-empty string among fields `[compatibility.reasoningField, "reasoning_content", "reasoning", "reasoning_text"]`
   (dedup order) is the reasoning delta; the field name is remembered (`reasoningField`). `reasoning_details` arrays are
   accumulated (consecutive same-kind text/summary entries merge unless id/index/format/signature conflict; first
   non-empty signature/format wins). Reasoning text emitted = text from details (`reasoning.text`, `reasoning.summary`,
   Kimi `summary` only if no scalar reasoning text seen) else the scalar field. All reasoning goes to one block
   `reasoning-0` with metadata `{[key]:{reasoningField}}`. If details were seen but no text yet, a reasoning-start is
   emitted when content/refusal/tool deltas arrive.
6. `delta.content` and `delta.refusal` → text-delta on `text-0`.
7. Tool deltas (`OC:1124-1175`): index = `tool.index` ?? index of existing tool with same id ?? (if id differs from the
   fallback tool's id → `nextToolIndex`) ?? fallback (batch position when >1 deltas, else latest index or position).
   Deltas without known id+name are buffered as pending until both are known. `extra_content` captured into
   `providerMetadata[key].extraContent`. Error `"OpenAI Chat tool call delta is missing id or name"` if a finish reason
   arrives while calls are still pending (except length/content-filter).
8. On first finish reason (not length/content-filter): finish all tools (tool-input-end + tool-call/tool-input-error),
   held in `toolCallEvents` until halt.
9. `onHalt` (after `[DONE]` or end): if no finish reason and `requireFinishReason` (default true) → InvalidProviderOutput
   `"OpenAI Chat stream ended without finish_reason"` (incomplete-stream). Else: finish leftover tools when no finish
   reason; normalized reason = `tool-calls` if mapped `stop` and tool calls exist; no reason → `tool-calls`/`stop`;
   emit reasoning-start if only details were observed, then reasoning-end on `reasoning-0` with metadata
   `{reasoningField?, reasoningDetails?: [...]}`, then tool events, then `Lifecycle.finish({reason, usage})`.

Usage (`OC:907-930`): `input = prompt_tokens`, `output = completion_tokens`;
`cached = prompt_tokens_details.cached_tokens ?? prompt_cache_hit_tokens ?? cached_tokens ?? cache_read_input_tokens`;
`cacheWrite = prompt_tokens_details.cache_write_tokens ?? cache_created_input_tokens`;
`reasoning = completion_tokens_details.reasoning_tokens`; `nonCached = input - (cached + cacheWrite)` (clamped);
`total = total_tokens ?? in+out`; raw usage under metadata key.

### 4.4 openai-compatible-chat

`packages/ai/src/protocols/openai-compatible-chat.ts:16-22`: route id `openai-compatible-chat`, metadata key `openai`
(re-keyed to the provider id when `route.with({provider})` changes provider, `packages/ai/src/route/client.ts:400-404`),
protocol = openai-chat unchanged, path `/chat/completions`, framing `sseWithDone`, no default baseURL.
Provider factory (`packages/ai/src/providers/openai-compatible.ts:26-41`): `provider` (default `"openai-compatible"`),
`baseURL` required, auth `AuthOptions.bearer(input, [])` — bearer from `apiKey` only (no env fallback); with no key the
request fails with AuthenticationError `"Missing auth credential: apiKey"` (`Auth.optional(undefined)` fails). Core
replaces the route auth with `Auth.none` for keyless enabled providers (5.6; open question 4).
All provider-specific behavior (max_tokens field, store, strict, reasoning fields, tool id rewriting) is driven by
provider id / base URL detection in 4.3 and by `LanguageModelCompatibility`.

### 4.5 mistral-chat

Source: `packages/ai/src/protocols/mistral-chat.ts` (abbrev. `MC:`). Route id `mistral-chat`, provider/metadata key
`mistral`, base `https://api.mistral.ai/v1`, path `/chat/completions`, SSE with `[DONE]` terminal (`MC:27-30, 771-782`).

PO (`MC:105-113`): `safePrompt`, `documentImageLimit`, `documentPageLimit`, `parallelToolCalls`, `reasoningEffort`,
`promptMode: "reasoning"`, `promptCacheKey`. `reasoningEffort` + `promptMode` together → InvalidRequest
`"Mistral Chat reasoningEffort and promptMode provider options are mutually exclusive"`.

Body (`MC:387-429`): `model, messages, tools, tool_choice, stream:true, max_tokens, random_seed (=gen.seed), temperature,
top_p, frequency_penalty, presence_penalty, stop, prompt_cache_key (= PO.promptCacheKey ?? request.promptCacheKey; omitted when cache=="none"), safe_prompt,
document_image_limit, document_page_limit, parallel_tool_calls (= PO.parallelToolCalls ?? (disableParallelToolUse===true ? false : undefined)), reasoning_effort, prompt_mode`.
* tools: `{type:"function", function:{name, description, parameters, strict:false}}`; tool_choice: auto→`"auto"`,
  none→`"none"`, required→`"any"`, tool→`{type:"function", function:{name}}`.
* system: one system message — string if exactly one part, else array of `{type:"text", text}`.
* system updates → `{role:"user", content: wrapped text}`.
* user: text/`{type:"image_url", image_url: "<url|dataUrl>"}`/`{type:"document_url", document_url}` (PDF);
  all-text collapses to joined string; other media → `"Mistral Chat does not support media type <t>"`.
* assistant: skipped if no tool calls, no native thinking and blank concatenated text. "Structured" when any reasoning
  part has `providerMetadata.mistral.thinking` matching `{type:"thinking", thinking:[...]}`: content array of
  `{type:"text"}` and native thinking blocks (non-native reasoning → text); otherwise content = concatenation of text
  and reasoning texts. `tool_calls` `{id: normalize(id), type:"function", function:{name, arguments: JSON(input)}}`.
  `prefix: true` is set on the **last message of the request** when it is an assistant message without tool calls.
* tool: `{role:"tool", tool_call_id, name, content}`; content results → array with images/docs if any non-text, else joined text.
* Tool id normalization (`packages/ai/src/protocols/utils/mistral-tool-id.ts`): valid = `/^[A-Za-z0-9]{9}$/`; others
  map to `hash(id)` = last 9 chars of `fnv(2166136261).padStart(7,"0") + fnv(2246822519).padStart(7,"0")` where
  `fnv(seed)`: `r = seed; for each UTF-16 code unit c: r = imul(r ^ c, 16777619)`; result `(r >>> 0).toString(36)`.
  Collisions with already-used ids retry with `"<id>:<n>"`. Valid ids found anywhere in history are reserved first.

Streaming (`MC:665-748`): content may be a string or array of `{type:"text", text}` / `{type:"thinking", thinking:[units]|string}`.
Active block switches (text ↔ reasoning) close the previous block; block ids `text-<n>`/`reasoning-<n>` with a
running counter. Reasoning end carries `providerMetadata.mistral.thinking = {type:"thinking", thinking: [all units]}` and
authoritative `text` = concatenated unit texts. Tool deltas: key = `index` ?? `"id:<id>"` ?? `"position:<i>"` (multi)
?? latest key ?? `"missing:<n>"`; id `"null"` string treated as absent; ids normalized to 9-char ids with the same hash;
`arguments` may be a JSON object (encoded). Missing name → buffered; error `"Mistral Chat tool call delta is missing a name"`
at finish. Finish map: `stop`→stop; `length|model_length`→length; `tool_calls`→tool-calls; `content_filter`→content-filter;
`error` → fail UnknownProvider `"Mistral Chat stopped with error"`; `network_error` → ProviderInternal; else unknown.
Late content after finish → `"Mistral Chat received content after the finish reason"`. Halt without finish →
`"Mistral Chat stream ended without finish_reason"` (incomplete-stream). stop + tool calls → tool-calls.
Usage: `cached = clamp(num_cached_tokens ?? prompt_tokens_details.cached_tokens ?? prompt_token_details.cached_tokens, 0, input)`;
`nonCached = input - cached`; metadata key `mistral`. `error` objects in stream → classifyProviderFailure.

### 4.6 cohere-chat

Source: `packages/ai/src/protocols/cohere-chat.ts` (abbrev. `CC:`). Route `cohere-chat`, provider/metadata key `cohere`,
URL `https://api.cohere.com/v2/chat`, plain SSE (`CC:12, 326-333`).

PO: `thinking? {type?: "enabled"|"disabled", tokenBudget?: int>0}`.
Body: `model, messages, stream:true, tools, tool_choice, thinking, max_tokens, temperature, p (=topP), k (=topK), seed,
stop_sequences, frequency_penalty, presence_penalty`.
* system: single system message (string if one part, else text array). System updates → `{role:"user", content: wrapped}`.
* tool results → one `{role:"tool", tool_call_id, content: toolResultText}` per result; `file` items →
  `"Cohere Chat does not support file content in tool results"`.
* user/assistant: content array of `{type:"text"}`, assistant reasoning → `{type:"thinking", thinking}` unless
  `providerMetadata.cohere.toolPlan === true` → joined into `tool_plan`; tool calls → `tool_calls`; user images →
  `{type:"image_url", image_url:{url}}`. `content` omitted when empty.
* tool_choice map: auto→omitted, none→`"NONE"`, required→`"REQUIRED"`, tool→`"REQUIRED"` with tools filtered to the
  named tool (`"Cohere Chat tool choice must name an available tool"` if missing). Tools with `native` →
  `"Cohere Chat does not support provider-defined tools"`.
* `thinking = {type: type ?? "enabled", token_budget?: fitThinkingBudget(tokenBudget, maxTokens)}`.

Stream events (strict union; unknown types fail decoding): `message-start` (step-start), `content-start`/`content-delta`
(`delta.message.content.thinking` → reasoning-delta id `<index>`, else text-delta), `content-end` (close both),
`tool-plan-delta` (reasoning-delta id `tool-plan`, metadata `{cohere:{toolPlan:true}}`), `tool-call-start/delta`
(appendOrStart by index), `tool-call-end`, `message-end` (terminal; finish all tools; finish map `COMPLETE|STOP_SEQUENCE`→stop,
`MAX_TOKENS`→length, `TOOL_CALL`→tool-calls, `ERROR|TIMEOUT`→error, else unknown), `citation-start/end` ignored.
Halt without message-end → `"Cohere stream ended without message-end"`.
Usage from `delta.usage.tokens {input_tokens, output_tokens, reasoning_tokens}` and `cached_tokens`.

### 4.7 venice-chat

Source: `packages/ai/src/protocols/venice-chat.ts` (abbrev. `VC:`). Wraps openai-chat with cache markers
(`cacheControl()` 4-breakpoint helper) and model compatibility `{maxTokensField:"max_completion_tokens", supportsStore:false, supportsPromptCacheKey:true}` (`VC:350-354`).
Body = openai-chat body plus: `max_completion_tokens = PO.maxCompletionTokens ?? PO.maxTokens ?? gen.maxTokens`,
`reasoning_effort` removed, `reasoning = {...PO.reasoning, effort: PO.reasoningEffort}`,
`venice_parameters` = `PO.veniceParameters` with `includeVeniceSystemPrompt` defaulting to `false`, undefined dropped,
keys camelCase→snake_case; `prompt_cache_retention, parallel_tool_calls, min_p, top_k (gen.topK), repetition_penalty,
stop_token_ids, logprobs, top_logprobs, max_temp, min_temp, response_format, user` (`VC:186-243`).
Assistant replay adds `thought_signature` (message-level, from `providerMetadata.venice.messageThoughtSignature`),
`reasoning_content` = `providerMetadata.venice.encryptedReasoningContent` when present, and per-tool-call
`thought_signature` (`providerMetadata.venice.thoughtSignature`).
Stream: `error` as a string → classified with `issues` appended (`"path.a: msg"` joined by `"; "`). Scalar
`reasoning_content` is scanned for the marker `__ENCRYPTED_REASONING__`: text before the marker is visible reasoning;
after the marker everything is treated as encrypted (not shown) and the full raw reasoning string is attached to the
reasoning-end as `venice.encryptedReasoningContent`; partial marker prefixes at chunk ends are held back.
`prompt_tokens_details.cache_creation_input_tokens` used as `cache_write_tokens` fallback. Tool-call/tool-input-end
events get `venice.thoughtSignature`; reasoning/text ends get `venice.messageThoughtSignature`.

### 4.8 zai-chat

Source: `packages/ai/src/protocols/zai-chat.ts`. openai-chat body plus `thinking` (`{type?, clear_thinking?}` verbatim),
`tool_stream` (= `PO.toolStream ?? (tools non-empty && model matches /(?:^|\/)glm-(\d+)(?:\.(\d+))?(?:-|$)/i with version ≥ 4.6 ? true : undefined)`),
`do_sample`, `response_format`, `request_id`, `user_id`. Compatibility: `{maxTokensField:"max_tokens", supportsStore:false,
supportsStrictMode:false, reasoningField:"reasoning_content", zaiToolStream:false}`. Stream = openai-chat.

### 4.9 alibaba-chat

Source: `packages/ai/src/protocols/alibaba-chat.ts`. openai-chat body with cache markers, plus `enable_thinking`,
`thinking_budget` (= `fitThinkingBudget(PO.thinkingBudget, gen.maxTokens)`), `preserve_thinking`, `clear_thinking`,
`thinking`, `tool_stream`, `parallel_tool_calls` (= PO ?? `!disableParallelToolUse`), `repetition_penalty`, `top_k`,
`response_format`, `enable_search`, `search_options`. Compatibility `{maxTokensField:"max_completion_tokens",
supportsStore:false, supportsStrictMode:false, reasoningField:"reasoning_content", zaiToolStream:false}`.
Cache policy for this route: only for model ids starting `qwen` (see 2.10).

### 4.10 open-responses (core Responses implementation)

Source: `packages/ai/src/protocols/open-responses.ts` (abbrev. `OR:`). Provider-neutral Responses API baseline used by
OpenAI, Azure, xAI, Alibaba, Meta, Bedrock Mantle, MiniMax, openai-compatible-responses, etc.
Path `/responses` (`OR:30`). Framing: plain SSE (no `[DONE]`); HTTP transport `sseJson` (`OR:1505`).
Metadata key: `route.providerMetadataKey ?? "openresponses"` (`OR:882`).

#### 4.10.1 Body (`OR:259-302, 768-843`)

```json
{
  "model": "<id>",
  "input": [ ...InputItem ],
  "instructions": "<system parts joined with \n>",
  "stream": true,
  "max_output_tokens": 1024, "temperature": 1, "top_p": 1, "presence_penalty": 0, "frequency_penalty": 0,
  "store": false, "metadata": {...}, "safety_identifier": "...",
  "stream_options": {"include_obfuscation": false}, "top_logprobs": 5,
  "prompt_cache_key": "<≤64 chars>", "include": ["reasoning.encrypted_content"],
  "reasoning": {"effort": "medium", "summary": "auto"}, "text": {"verbosity": "low"},
  "service_tier": "flex", "max_tool_calls": 3, "parallel_tool_calls": true, "truncation": "auto",
  "tools": [{"type":"function","name":"...","description":"...","parameters":{...},"strict":false}],
  "tool_choice": "auto"
}
```
Order: `model, input, instructions?` (from `lowerConversation`), then `lowerGeneration` fields in the order above
(each optional field only when defined/truthy as coded at `OR:780-809`), then `tools`, `tool_choice`.
* `instructions` omitted when the joined system text is empty.
* `reasoning` emitted when `PO.reasoningEffort` or `PO.reasoningSummary` is truthy: `{effort, summary}` (undefined members omitted).
* `prompt_cache_key` = `promptCacheKey(request)` (always sent when available; not gated by compatibility here).
* `parallel_tool_calls` = `PO.parallelToolCalls` ?? `!toolChoice.disableParallelToolUse` (when defined).
* tools: namespaces flattened; tools with `native` → adapter `nativeTool` or InvalidRequest
  `"<Name> does not support provider-native tool <name>"`; function tools always `strict:false`.
* `tool_choice` = `allowedTools` PO → `{type:"allowed_tools", mode, tools:[{type:"function", name}]}`, else auto/none/required strings or `{type:"function", name}`.

#### 4.10.2 Input items (`OR:605-766`)

| Normalized | Responses input item |
|---|---|
| system update (text) | `{type:"message", role:"developer", content:"<joined text>"}` |
| effort update | `{type:"configuration_update", reasoning:{effort: effort ?? "medium"}}` — a consecutive update replaces the previous one (only reaches the body when the route supports effort updates; else stripped earlier) |
| user | `{type:"message", role:"user", content:[input_text/input_image/input_file], id?: metadata.itemId, status?}` (message-level `providerMetadata[key]` validated as `{itemId?, status?, phase?}`) |
| assistant text | grouped consecutive text parts by `(itemId, phase)` → `{type:"message", id?, role:"assistant", status:"completed", content:[{type:"output_text", text}], phase?}`; `itemId` from part metadata (must look like `<prefix>_<suffix>`) or message metadata |
| assistant reasoning | `{type:"reasoning", id?, summary:[{type:"summary_text", text}] (empty if text ""), encrypted_content}` where `encrypted_content = metadata.reasoningEncryptedContent` (string or null); skipped when the part has no `providerMetadata[key]` or has empty text and no encrypted content; parts sharing an id are merged (summaries appended, last string encrypted wins) |
| assistant compaction | `{type:"compaction", id, encrypted_content}` (must match provider and be encrypted, else `"Compaction state must be replayed to its originating provider and API"`) |
| assistant tool-call (not provider-executed) | `{type:"function_call", id?: itemId, call_id: part.id, name, namespace?, arguments: JSON(input ?? {})}` |
| assistant provider-executed tool-result | if `result` is json and is a known hosted item (`web_search_call`, `file_search_call`, `code_interpreter_call`, `mcp_call`, or adapter-restored) whose `id` equals the part's itemId → replay the item verbatim once; else a user message with the result content |
| assistant media | `{type:"message", role:"user", content:[media]}` |
| tool result | `{type:"function_call_output", call_id, output}` — output = `toolResultText` string, or array of `input_text`/`input_image`/`input_file`/`input_video` for `content` results |

Media (`OR:522-551`): adapter hook first; `detail` from `providerMetadata[key].detail`; location = url or data URL
(inline required); images → `{type:"input_image", image_url, detail?}`; tool-result videos →
`{type:"input_video", video_url}` (user video → `"<Name> user messages do not support input_video"`); other →
`{type:"input_file", filename: filename ?? ("document.pdf" for PDF else "file"), detail?, file_url | file_data}`.

#### 4.10.3 Stream events (`OR:870-1468`)

Every event is first `normalize`d: `item_id` remapped through the `output_index → item id` table; items missing `id`
adopt the slot's id or a minted `"<msg|rs|fc|cmp|item>_<uuid without dashes>"`.

| Event `type` | Handling |
|---|---|
| `response.output_item.added` (message) | close any other open text blocks (text-end with `{itemId, phase?}`); remember current message `{id, phase}` |
| `response.output_item.added` (reasoning) | error if another reasoning block is open (`"... started reasoning before the previous item ended"`); reasoning-start id `"<itemId>:0"`, metadata `{itemId, reasoningEncryptedContent: encrypted_content ?? null}` |
| `response.output_item.added` (function_call with call_id) | start tool keyed by item id (initial input = `arguments ?? ""`); step-start; `tool-input-start {id: call_id, name, namespace, providerMetadata:{[key]:{itemId}}}` |
| `response.output_text.delta` | only for the current message item: text-start (metadata `{itemId, phase?}`) + text-delta id = item id |
| `response.output_text.done` | current message: emit `text` as a delta only if nothing streamed; other ids: text-end |
| `response.refusal.delta` / `.done` | same as output_text with `delta`/`refusal` |
| `response.reasoning_summary_part.added` | for index > 0: close earlier non-concluded parts (reasoning-end `<item>:<i>`), start `<item>:<index>` |
| `response.reasoning_summary_text.delta`, `response.reasoning.delta` (+ OpenAI: `response.reasoning_text.delta`) | reasoning-delta on `<item>:<summary_index ?? 0>` (ignored if that part concluded) |
| `response.reasoning_summary_text.done`, `response.reasoning.done`, `response.reasoning_text.done` | emit `text` as delta only if that index never streamed |
| `response.reasoning_summary_part.done` | mark part `can-conclude` |
| `response.function_call_arguments.delta` | append delta to tool |
| `response.function_call_arguments.done` | if `arguments` doesn't start with accumulated input → replace silently; else append the missing suffix |
| `response.output_item.done` compaction | emit `compaction {provider, id, encrypted}` once (`"Compaction output is missing its encrypted content"` if absent) |
| `response.output_item.done` message | text-end with authoritative text = concatenated `output_text`/`refusal` parts (text-start first if never started) |
| `response.output_item.done` function_call | finish tool with final `arguments` (tool-input-start emitted if done-only); marks `hasFunctionCall` |
| `response.output_item.done` reasoning | close open parts with metadata `{itemId, reasoningEncryptedContent}`; authoritative text: summaries joined by `"\n\n"` (or `reasoning_text` content) when one part, else per-index summary; unknown item (never added) → reasoning-start + reasoning-end pair with id = item id |
| `response.completed` | recover `function_call` items still pending and `compaction` items from `response.output` (error `"Cannot recover a compaction checkpoint after output has been emitted"`), finish all pending tools, then finish |
| `response.incomplete` | finish |
| `response.failed` | fail `providerFailure(event, "<Name> response failed")` |
| `error` | fail `providerFailure(event, "<Name> stream error")` |
| other | ignored |

Terminal types: `error, response.completed, response.incomplete, response.failed` (`OR:895-896`).
Finish (`OR:870-880, 1322-1367`): `incomplete_details.reason` absent → `tool-calls` if any function call else
(`response.incomplete` → unknown, else stop); `max_output_tokens` → length; `content_filter` → content-filter; other
→ tool-calls/unknown. `raw = incomplete_details.reason`. Finish providerMetadata `{[key]:{responseId, serviceTier}}`
when either is present. Usage (`OR:852-868`): `cached = input_tokens_details.cached_tokens`,
`cacheWrite = input_tokens_details.cache_write_tokens`, `reasoning = output_tokens_details.reasoning_tokens`,
`nonCached = input - (cached + cacheWrite)`, total = `total_tokens ?? in+out`.

Error frames (`OR:1370-1400`): detail = top-level `message`/`code` (string or number) over nested `error`/`response.error`
(string → message). Message = `"<code>: <message>"` / message / code / raw JSON body (or fallback when body is `{}`).
Status from numeric `status` or `status_code`. A bare `{type:"error"}` with no details → ProviderInternalError;
otherwise `classifyProviderFailure({message, status, rawBody})`.

### 4.11 openai-responses

Source: `packages/ai/src/protocols/openai-responses.ts` (abbrev. `ORs:`). Route id `openai-responses`, provider/metadata
key `openai`, base `https://api.openai.com/v1`, path `/responses`, auth bearer `OPENAI_API_KEY` (provider facade),
route default PO `{store:false, include:["reasoning.encrypted_content"]}` (`ORs:318-331`). Transport = channel
transport (HTTP SSE by default; WebSocket when the caller supplies a WebSocket executor, 4.16).

Differences from 4.10:
* PO `contextManagement: [{type:"compaction", compactThreshold?: int>0}]` → body
  `context_management: [{type:"compaction", compact_threshold}]` (`ORs:26-32, 199-216`).
* Effort updates supported when: no `contextManagement`, `http.body.reasoning.mode != "pro"`, and
  (`compatibility.supportsEffortUpdates` override, else model id matches `/(?:^|\/)gpt-(\d+)(?:\.\d+)?(?:-|$)/i` with major ≥ 6)
  (`ORs:140-149`). Top-level `reasoning.effort` is the frozen effort from `resolveEffortUpdates`.
* Native namespaces: `{type:"namespace", name, description: description ?? "Tools in the <name> namespace.", tools:[flattened function tools]}` (`ORs:170-184`).
* Hosted image generation tool: a `ToolDefinition` whose `native.openai.type == "image_generation"` lowers to the
  validated object `{type:"image_generation", action?, background?, input_fidelity?, output_compression? (0-100), output_format?, partial_images?, quality?, size? ("auto" or WxH)}`;
  invalid → `"OpenAI Responses image generation tool options are invalid"`. Named tool choice of that tool →
  `{type:"image_generation"}`. `tools`/`tool_choice` omitted when `request.tools` is empty.
* Extra stream handling: `response.reasoning_text.delta`; hosted tool items on `output_item.done` (`ORs:264-289`)
  emit a provider-executed `tool-call` + `tool-result` pair (id = item id, metadata `{itemId}`):
  `web_search_call`→`web_search` (input `action ?? {}`), `web_search_preview_call`→`web_search_preview`,
  `file_search_call`→`file_search` (`{queries}`), `code_interpreter_call`→`code_interpreter` (`{code, container_id}`),
  `computer_call`→`computer_use`, `image_generation_call`→`image_generation` (result = content file
  `data:image/<output_format ?? png>;base64,<result>`; invalid base64 → `"OpenAI Responses returned invalid image base64"`),
  `mcp_call`→`mcp` (`{server_label, name, arguments}`). Result = error if `item.error` non-null, else json(item).
* Compaction:
  - `compact.endpoint` (`packages/ai/src/protocols/utils/responses-compaction.ts`): POST `<baseURL>/responses/compact`
    (path of the rendered URL + `/compact`) with body `{model, input, instructions?, service_tier, reasoning, text,
    include, parallel_tool_calls, tools, prompt_cache_key}` deep-merged with `http.body` and validated; effort updates
    stripped; not streamed. Response must be `{object:"response.compaction", output:[compaction|reasoning|message...], usage?}`
    containing at least one compaction item (`"Compaction response did not contain a checkpoint"`); output is converted
    back into replacement `Message`s.
  - `compact.trigger` (`packages/ai/src/protocols/utils/responses-checkpoint.ts`): normal streamed request with
    `{type:"compaction_trigger"}` appended to `input`; rejects `http.body` overrides of `input`,
    `previous_response_id`, `conversation`, or non-true `stream`; requires `response.created` id consistency,
    exactly one compaction checkpoint, `response.completed` with id and status `completed`.

### 4.12 openai-compatible-responses

`packages/ai/src/protocols/openai-compatible-responses.ts:14-21`: route id `openai-compatible-responses`, metadata key
`openresponses` (re-keyed to provider on `.with({provider})`), protocol open-responses, path `/responses`, plain HTTP
SSE transport, default PO `{store:false, include:["reasoning.encrypted_content"]}`. Base URL/auth supplied by the
provider (`packages/ai/src/providers/openai-compatible-responses.ts`).

### 4.13 xai-responses

`packages/ai/src/protocols/xai-responses.ts`: baseline body; `PO.contextManagement` → UnsupportedOperation
(operation `"in-band-compaction"`, message `"xAI requires explicit compaction through LLMClient.compact; automatic context management is not supported"`).
Hosted tools: `web_search_call`, `x_search_call`→`x_search`, `file_search_call`, `code_interpreter_call`
(`{code, container_id}`), `image_generation_call` (json result), `mcp_call`. Restorable replay items: `x_search_call`,
`image_generation_call`. Explicit compaction via `/responses/compact` (same as 4.11).

### 4.14 alibaba-responses

`packages/ai/src/protocols/alibaba-responses.ts`: PO `reasoningEffort, enableThinking, store, previousResponseId,
conversation`; body adds `enable_thinking`, `previous_response_id`, `conversation`; a named function tool choice is
rewritten to `{type:"allowed_tools", mode:"required", tools:[{type:"function", name}]}`. Native tools: `native.alibaba`
must be `{type: "web_search"|"web_extractor"|"code_interpreter"}`. Hosted results: `web_search_call`,
`code_interpreter_call` (`{code}`), `web_extractor_call` → `web_extractor` (`{urls, goal}`; invalid →
`"Alibaba returned an invalid web extraction item"`).

### 4.15 meta-responses

`packages/ai/src/protocols/meta-responses.ts`: native tools `native.meta` = `{type:"web_search", search_context_size?, user_location?{type:"approximate", city?, region?, country?, timezone?}}`
or `{type:"image_generation", size?, output_format?, reasoning_strength?, enable_image_search?, enable_web_search?, enable_shell?}`.
Replay: provider-executed `image_generation` content results whose `providerMetadata[key].itemId == id` are replayed
as the hosted handle `{type:"image_generation_call", id, status:"completed", result:null}`. Stream: image items
decoded (`"Meta returned an invalid image item"`, `"Meta returned an image without data"`,
`"Meta returned invalid image base64"`); mime = `image/<output_format>` or sniffed; message `annotations` attached to
text-end metadata `{[key]:{annotations}}`; `response.completed.output` items not yet seen (by id or by output index)
are replayed as `output_item.done` before finishing (Muse Image delivers results only at completion).

### 4.16 Responses WebSocket channel and continuation

Sources: `packages/ai/src/protocols/open-responses-channel.ts` (`ORC:`), `open-responses-continuation.ts` (`ORK:`),
`packages/ai/src/route/transport/websocket*.ts`, executor in `packages/core/src/session/model-transport.ts` (section 7).
Used by `openai-responses` (and routes built from `OpenResponsesChannel.transport`).

* Prepare (`ORC:138-178`): build the normal HTTP request (URL, headers, body). If the caller passed a WebSocket
  executor and `enabled(url)` (default true): URL converted `https:`→`wss:` / `http:`→`ws:` (other schemes →
  `"Unsupported WebSocket URL protocol <p>"`); headers = route headers hook (OpenAI: set `openai-beta:
  responses_websockets=2026-02-06` unless already present, `ORs:21, 311-316`) minus `content-length`; the
  `response.create` message = body without `stream`, `stream_options`, `background`, plus `"type":"response.create"`.
  `rotateAfterMs` = 55 minutes for OpenAI (`ORs:22`). If WebSocket was requested but not available: log warning
  `"<Name> does not offer WebSocket for this endpoint; using HTTP"` and use HTTP.
* Driver observations (`ORC:51-136`): frames decoded as Responses events (a bare `{error:...}` → type `error`);
  `error`/`response.failed` → provider failure (terminal); `response.created` must appear once with an id; non
  `response.*` events pass through (keepalives); `response.*` before `response.created` → error
  `"<Name> emitted <type> before response.created"`; response id must not change; `response.completed` →
  completed; `response.incomplete` → incomplete; events after terminal → error.
* Continuation (`ORK:134-206`): after a completed response, a checkpoint
  `{protocol:"open-responses.websocket.v1", value:{version:1, responseID, request (without previous_response_id), output}}`
  is stored (`output` = `response.completed.output` with reasoning items replaced by the `output_item.done` versions,
  or the collected done items). Next request on the same socket: if the invariant request fields (everything except
  `type`, `input`, `previous_response_id`; canonical JSON with sorted keys, undefined dropped) are identical and the new
  `input` strictly extends `previousInput + output` (assistant messages compared by `{role, content:[{type,text}], phase}`,
  function calls by `{type, call_id, name, arguments(parsed)}`, reasoning by `{type, summary, encrypted_content}`),
  send `{...fields, input: <new suffix>, previous_response_id}` (mode `incremental`); otherwise send the full request.
  Failures with code `previous_response_not_found` → rejected/`retry-full`; `websocket_connection_limit_reached` →
  `rotate-and-retry-full`; unclassified InvalidRequest during an incremental send → `retry-full` (TransportError
  `transport:"websocket", operation:"read", phase:"receive", delivery:"rejected"`). A request containing a
  `compaction_trigger` input never produces a checkpoint.

### 4.17 gemini (Google Generative Language / Vertex generateContent)

Source: `packages/ai/src/protocols/gemini.ts` (abbrev. `G:`), `utils/gemini-json-schema.ts` (`GJS:`),
`utils/gemini-generate-content.ts` (`GGC:`), `packages/ai/src/providers/google.ts`, `google-vertex.ts`,
`google-vertex-shared.ts` (`GVS:`).

#### 4.17.1 Routes and URLs

| Route id | URL | Auth |
|---|---|---|
| `gemini` (provider `google`, metadata `google`) | `https://generativelanguage.googleapis.com/v1beta/models/<model.id>:streamGenerateContent?alt=sse` (`G:32, 823-834`) | `x-goog-api-key: <key>` from `apiKey` option, else env `GOOGLE_GENERATIVE_AI_API_KEY` (`packages/ai/src/providers/google.ts:38-43`) |
| `google-vertex-gemini` (provider `google-vertex`, metadata `vertex`) | `<base>/<model or endpoints/...>:streamGenerateContent?alt=sse` where path = `/endpoints/...` if model id starts with `endpoints/` else `/models/<id>` (`packages/ai/src/providers/google-vertex.ts:58-73`) | see below |

Vertex base URL (`google-vertex.ts:77-106`): explicit `baseURL`; else with Express API key
`https://aiplatform.googleapis.com/v1/publishers/google`; else
`https://<host(location)>/v1beta1/projects/<project>/locations/<location>` + `/publishers/google` (omitted for
`endpoints/` tuned models). `host(location)` (`GVS:56-61`): `global` → `aiplatform.googleapis.com`; `eu`/`us` →
`aiplatform.<loc>.rep.googleapis.com`; else `<loc>-aiplatform.googleapis.com`.
`project` = option ?? `$GOOGLE_VERTEX_PROJECT` ?? `$GOOGLE_CLOUD_PROJECT` ?? `$GCP_PROJECT` ?? `$GCLOUD_PROJECT`;
missing → `"Google Vertex requires a project when baseURL is not configured"`. `location` = option ??
`$GOOGLE_VERTEX_LOCATION` ?? `$GOOGLE_CLOUD_LOCATION` ?? `$VERTEX_LOCATION` ?? `"global"` (`GVS:42-69`).
Vertex auth (`GVS:71-113`): API key (Express mode) = explicit `apiKey`, else `$GOOGLE_VERTEX_API_KEY` only when no
explicit project/location/accessToken/auth → header `x-goog-api-key`; tuned `endpoints/` models reject API keys
(`"Google Vertex tuned models do not support Express Mode API keys"`). Otherwise OAuth: explicit `auth`, or
`accessToken` → `Authorization: Bearer`, or Google ADC (google-auth-library, scope
`https://www.googleapis.com/auth/cloud-platform`; honors `CLOUDSDK_CONFIG` for
`<dir>/application_default_credentials.json`; failure → MissingCredential "Google Application Default Credentials").
Vertex header: `x-vertex-ai-llm-shared-request-type: <PO.serviceTier>` when set; `serviceTier` removed from the
body; `labels` (PO, string values only) added to the body (`google-vertex.ts:39-48, 68-71`).
Other Vertex routes: `google-vertex-chat` (openai-chat, base `https://aiplatform.googleapis.com/v1/projects/<p>/locations/<l>/endpoints/openapi`, path `/chat/completions`),
`google-vertex-responses` (open-responses, same base, `/responses`, PO defaults store false + encrypted reasoning),
`google-vertex-messages` (4.1.7), `google-vertex-mistral` (mistral-chat body without `prompt_cache_key`, POST
`https://<host>/v1/projects/<p>/locations/<l>/publishers/mistralai/models/<model>:streamRawPredict`),
`google-vertex-interactions` (4.18). All Vertex OpenAI/Mistral/Messages variants reject API keys.

#### 4.17.2 Body (`G:440-491`)

```json
{
  "cachedContent": "cachedContents/...",
  "contents": [{"role":"user","parts":[{"text":"..."}]}, {"role":"model","parts":[...]}],
  "safetySettings": [{"category":"HARM_CATEGORY_...","threshold":"BLOCK_NONE"}],
  "serviceTier": "flex",
  "systemInstruction": {"parts":[{"text":"..."}]},
  "tools": [{"functionDeclarations":[{"name":"...","description":"...","parametersJsonSchema":{...}}]}],
  "toolConfig": {"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["x"]}},
  "generationConfig": {"maxOutputTokens":..., "temperature":..., "topP":..., "topK":..., "frequencyPenalty":..., "presencePenalty":..., "seed":..., "stopSequences":[...], "thinkingConfig":{"thinkingBudget":..., "includeThoughts":true, "thinkingLevel":"high"}}
}
```
* PO (each key lenient): `cachedContent`, `safetySettings`, `serviceTier` (`standard|flex|priority|...`),
  `thinkingConfig {thinkingBudget?, includeThoughts?, thinkingLevel?}`.
* `thinkingConfig`: if present, `includeThoughts` defaults to `true`; `thinkingBudget =
  fitThinkingBudget(budget, maxTokens, 512)`; thinkingLevel passed through (`G:455-469`).
* `generationConfig` omitted if every field undefined. `systemInstruction` = one part per system part (not joined).
* tools: flattened; `toolConfig` only with tools and toolChoice: auto→`AUTO`, none→`NONE`, required→`ANY`,
  tool→`ANY` + `allowedFunctionNames:[name]`.
* Tool schemas: the gemini sanitizer (`protocol.sanitizer = "gemini"`, `G:807`) applies `GeminiJsonSchema.normalize`
  (`GJS:19-75`): keep `const/default/enum/examples/dependentRequired` verbatim; recurse into
  `properties/patternProperties/$defs/definitions/dependentSchemas/dependencies` values; `required` filtered to
  declared properties; draft-04 boolean `exclusiveMinimum/Maximum` → numeric (taking `minimum`/`maximum`, which are
  then dropped) or removed when false; array `items` → `prefixItems`, `additionalItems` (with array items) → `items`.
  Then recursive `$ref` loops are cut: a self `$ref` (to `#` or `#/$defs/<name>`/`#/definitions/<name>`) that is not
  reached through an optional property or a possibly-empty array (`minItems` not > 0) is replaced by `{}`.

Message lowering (`G:305-438`):
* metadata key = `route.providerMetadataKey ?? provider`.
* system update → wrapped text appended to the previous user turn unless that turn contains a `functionResponse`
  (then a new user turn).
* user: text → `{text}`; media → `{fileData:{mimeType, fileUri}}` for Google `ref` sources, else inline
  `{inlineData:{mimeType, data}}` (`GGC:136-149`).
* assistant (`role:"model"`): text → `{text, thoughtSignature?}`; media → inline data (+signature); reasoning →
  `{text, thought:true, thoughtSignature?}`; tool-call → `{functionCall:{id?, name, args: input}, thoughtSignature?}`.
  `id` omitted when model id matches `/^gemini(?:-live)?-(\d+)/i` with major < 3 (`G:50-53`). For models needing the
  fallback (`/(^|\/)gemini-/i` and not gemini-1/2, not `gemini-pro(-vision)`, not `gemini-robotics-er-1.5`), an unsigned
  tool call gets `thoughtSignature: "skip_thought_signature_validator"` unless an earlier call in the same message was
  signed (`G:28-42, 371-383`). `thoughtSignature` read from `providerMetadata[key].thoughtSignature`.
* tool results: `{functionResponse:{id?, name, response:{name, content: toolResultText}}}`; for `content` results
  `content` = text items joined `"\n"` and file items become inline `parts` (Gemini 3) or, for models matching
  `/gemini-2[.-]5(?:[.-]|$)/i`, are deferred into a following user turn `[{text:"Attached media from tool result:"}, ...inlineData]`.
  Consecutive tool results merge into the open function-response user turn.

#### 4.17.3 Streaming (`G:585-821`)

Each SSE data frame: `{candidates?:[{content?:{role, parts}, finishReason?}], promptFeedback?, usageMetadata?, error?}`.
* `error` object → `classifyProviderFailure({message: error.message || error.status || "Gemini provider error", status: error.code if number, rawBody})`.
* Only `candidates[0]` used. Error finish reasons (`MALFORMED_FUNCTION_CALL, UNEXPECTED_TOOL_CALL, NO_IMAGE,
  TOO_MANY_TOOL_CALLS, MISSING_THOUGHT_SIGNATURE, MALFORMED_RESPONSE`) → InvalidProviderOutput `"Gemini stopped with <reason>"`.
* Parts without `text/inlineData/functionCall/functionResponse` keys are skipped. Signature attribution: a signature
  on a `thought` part → reasoning signature; on a text part → text signature.
* `inlineData` → `media` event (base64 asset, metadata `{thoughtSignature}`).
* `text` with `thought:true` → closes open text block, reasoning-delta on `reasoning-<n>` (counter); plain text → closes
  reasoning, text-delta on `text-<n>`. End events carry `{[key]:{thoughtSignature}}`.
* `functionCall` → closes reasoning/text; emits a complete `tool-call` (no input streaming) with `id` = supplied id
  (unless duplicated within the response) else `"tool_" + uuid without dashes`; input = `args ?? {}`; metadata
  `{thoughtSignature}`; sets hasToolCalls.
* `onHalt` (`G:542-583`): only if a finish reason, prompt block reason, or usage was seen: close reasoning/text, then
  finish. If no candidate finish reason but `promptFeedback.blockReason` → normalized `content-filter`, raw = block reason.
  Finish map (`G:525-540`): undefined → tool-calls/unknown; `STOP` → tool-calls if tool calls else stop; `MAX_TOKENS` →
  length; content-filter set `IMAGE_SAFETY, RECITATION, SAFETY, BLOCKLIST, PROHIBITED_CONTENT, SPII, MODEL_ARMOR,
  IMAGE_PROHIBITED_CONTENT, IMAGE_RECITATION, LANGUAGE` (`GGC:75-90`) → content-filter; else unknown. Finish metadata
  `{[key]:{promptFeedback}}` when present.
* Usage (`G:500-523`): `input = promptTokenCount`, `cached = cachedContentTokenCount`, `reasoning = thoughtsTokenCount`,
  `output = candidatesTokenCount + thoughts` (only when candidates count present), `nonCached = input - cached`,
  `total = totalTokenCount ?? in+out`. Explicit nulls treated as absent.

### 4.18 google-interactions

Source: `packages/ai/src/protocols/google-interactions.ts` (abbrev. `GI:`). Route `google-interactions`, provider/metadata
`google`, URL `https://generativelanguage.googleapis.com/v1beta/interactions`, SSE (`GI:551-559`); Vertex variant
`google-vertex-interactions` at `https://<host>/v1beta1/[projects/<p>/]locations/<l>/interactions?alt=sse`
(`packages/ai/src/providers/google-vertex-interactions.ts:33-71`). Sanitizer `gemini`.

PO (lenient): `previousInteractionId`, `store`, `thinkingLevel`, `thinkingSummaries` (`auto|none`), `serviceTier`
(`standard|flex|priority|deferred`). Hosted (`native`) tools → `"Google Interactions hosted tools are not supported"`;
topK/frequencyPenalty/presencePenalty → `"Google Interactions does not support topK, frequencyPenalty, or presencePenalty"`.

Body: `{model, input:[steps], stream:true, store: PO.store ?? false, previous_interaction_id?, system_instruction? (joined), service_tier?,
tools?:[{type:"function", name, description, parameters}], generation_config:{max_output_tokens, temperature, top_p, seed, stop_sequences, thinking_level, thinking_summaries, tool_choice}}`.
tool_choice: auto→`"auto"`, none→`"none"`, required→`"any"`, tool→`{allowed_tools:{mode:"any", tools:[name]}}`.
Steps: system update → `{type:"user_input", content:[{type:"text", text}]}`; consecutive text/media of a message
merge into one `user_input`/`model_output` step; media `{type: image|audio|video|document, uri | data, mime_type}`
(documents only `application/pdf`/`text/csv`, else `"Google Interactions does not support <t> document input"`);
reasoning → `{type:"thought", signature?, summary?:[{type:"text", text}]}`; tool-call →
`{type:"function_call", id, name, arguments, signature?}`; tool result → `{type:"function_result", call_id, name,
result (json object verbatim | text | content array), is_error?: true}`. Signatures stored at
`providerMetadata[key].interactionSignature`.

Stream events (`event_type`): `step.start` (`thought` → reasoning-start + summaries; `model_output` → text-start +
text parts (non-text → `"Unsupported Interactions output: <t>"`); `function_call` → tool start (needs id/name);
others → `"Unsupported Interactions step: <t>"`), `step.delta` (`text`, `thought_summary`, `thought_signature`,
`arguments_delta`; unknown → `"Unsupported Interactions delta: <t>"`), `step.stop`, `interaction.created`/
`interaction.status_update` (ignored), `error` (classified), `interaction.completed` (terminal; status `failed|cancelled`
→ classified `"Google Interactions <status>"`; `completed`→stop, `requires_action`→tool-calls, `incomplete`→length;
other → `"Unexpected terminal Interactions status: <s>"`; finish metadata `{interactionId}`).
Usage: `input = total_input_tokens`, `cached = total_cached_tokens`, `reasoning = total_thought_tokens`,
`output = total_output_tokens + reasoning`, `contextTokens = raw_prompt_token ?? input`, `total = total_tokens`.
Halt before completion → `"Google Interactions stream ended before interaction.completed"`.

---

## 5. Provider registry, model catalog, custom providers

### 5.1 Layers of "provider"

1. **`@opencode/ai` provider packages** (`packages/ai/src/providers/*.ts`): factories exporting
   `model(modelID, settings) -> LanguageModel` (`packages/ai/src/provider-package.ts:5-25`). `settings` is flat:
   connection keys (`apiKey`, `baseURL`, `headers`, `body`, plus provider-specific ones like `region`, `project`) and
   **every other key becomes `providerOptions`** for the route's protocol (e.g. `reasoningEffort`, `thinking`).
   `headers` → route default headers; `body` → `http.body` overlay.
2. **Core catalog** (`packages/core/src/provider.ts`, `model.ts`): `Provider.Info` / `Model.Info` records (5.4) whose
   `package` field names either a native entrypoint (`@opencode/ai/providers/...`) or an AI-SDK package prefixed
   `aisdk:` (`packages/core/src/provider.ts:24-35`) served by the AI-SDK bridge (`packages/core/src/aisdk.ts`).
3. **Core plugins** (`packages/core/src/plugin/provider/*.ts`, `packages/core/src/plugin/models-dev.ts`,
   `packages/core/src/config/plugin/provider.ts`): populate/modify the catalog, integrations (auth methods) and
   request hooks.

### 5.2 Native provider packages (`packages/ai/src/providers/`)

All use bearer auth via `AuthOptions.bearer(options, ENV...)` unless noted (explicit `apiKey` setting wins; env vars
tried in order).

| Provider id | Route id(s) → protocol | Default base URL (verbatim) | Env vars | Notes / compatibility |
|---|---|---|---|---|
| `openai` | `openai-responses` (default), `openai-chat` | `https://api.openai.com/v1` | `OPENAI_API_KEY` | headers `OpenAI-Organization`, `OpenAI-Project`; default PO `store:false` (+gpt-5 defaults); chat: `supportsPromptCacheKey` |
| `azure` | `azure-openai-responses` (default), `azure-openai-chat` | `https://<resourceName>.openai.azure.com/openai` + `/v1` (or `/deployments/<model>` when `useDeploymentBasedUrls`); custom non-azure baseURL used verbatim | `AZURE_OPENAI_API_KEY` | header `api-key: <key>` (removes `authorization`); query `api-version=<apiVersion ?? "v1">` (+`queryParams`; not added for custom non-`.openai.azure.com` base URLs); WebSocket only for `https://*.openai.azure.com/openai/v1/responses?api-version=v1` (api-version stripped) (`packages/ai/src/providers/azure.ts:38-119`); `"Azure requires resourceName or baseURL"` |
| `anthropic` | `anthropic-messages` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY` | `x-api-key`; `authToken` → Bearer |
| `anthropic-compatible` | `anthropic-compatible-messages` | required | — | `x-api-key` |
| `amazon-bedrock` | `bedrock-converse` | `https://bedrock-runtime.<region>.amazonaws.com` | `AWS_BEARER_TOKEN_BEDROCK`, AWS chain, `AWS_REGION`, `AWS_DEFAULT_REGION` | 4.2 |
| `amazon-bedrock` (mantle) | `bedrock-mantle-{responses,chat,messages}` | `https://bedrock-mantle.<region>.api.aws/v1`, `/anthropic/v1` | same | 4.2.6 |
| `google` | `gemini`, `google-interactions` | `https://generativelanguage.googleapis.com/v1beta` | `GOOGLE_GENERATIVE_AI_API_KEY` | `x-goog-api-key` |
| `google-vertex` | `google-vertex-gemini`, `-chat`, `-responses`, `-messages`, `-mistral`, `-interactions` | 4.17.1 | `GOOGLE_VERTEX_PROJECT`/`GOOGLE_CLOUD_PROJECT`/`GCP_PROJECT`/`GCLOUD_PROJECT`, `GOOGLE_VERTEX_LOCATION`/`GOOGLE_CLOUD_LOCATION`/`VERTEX_LOCATION`, `GOOGLE_VERTEX_API_KEY`, ADC | OAuth bearer |
| `mistral` | `mistral-chat` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` | |
| `cohere` | `cohere-chat` (native v2) and `cohere-chat-completions` (openai-chat at `https://api.cohere.ai/compatibility/v1`) | `https://api.cohere.com/v2` | `COHERE_API_KEY` | compat chat: `{maxTokensField:"max_tokens", supportsStore:false, supportsUsageInStreaming:true, reasoningField:"reasoning_content", supportsStrictMode:false}` |
| `xai` | `xai-responses` (default; WebSocket rotate 24 min; continuation only when `store !== false`, `instructions` removed from incremental sends), `xai-chat` (header `x-grok-conv-id: <promptCacheKey>`) | `https://api.x.ai/v1` | `XAI_API_KEY` | default PO `store:false, include:["reasoning.encrypted_content"]` |
| `groq` | `groq-chat` (openai-chat + `reasoning_format:"parsed"` except `openai/gpt-oss-*` which get `include_reasoning`; `parallel_tool_calls`, `service_tier`, `user`) | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` | `{maxTokensField:"max_completion_tokens", reasoningField:"reasoning", requireReasoning:false, supportsStore:false, supportsStrictMode:false}` |
| `deepseek` | `deepseek-chat` | `https://api.deepseek.com/v1` | `DEEPSEEK_API_KEY` | `{maxTokensField:"max_tokens", supportsStore:false}` |
| `cerebras` | `cerebras-chat` | `https://api.cerebras.ai/v1` | `CEREBRAS_API_KEY` | `{maxTokensField:"max_tokens", reasoningField:"reasoning", supportsStore:false, supportsPromptCacheKey:true}` |
| `deepinfra` | `deepinfra-chat` | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA_API_KEY` | `{maxTokensField:"max_tokens", reasoningField:"reasoning_content", supportsStore:false, supportsPromptCacheKey:true}` |
| `togetherai` | `togetherai-chat` | `https://api.together.xyz/v1` | `TOGETHER_API_KEY`, `TOGETHER_AI_API_KEY` | `{maxTokensField:"max_tokens", supportsStore:false, supportsStrictMode:false}` |
| `fireworks` | `fireworks-chat` | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` | `supportsPromptCacheKey` |
| `baseten` | `baseten-chat` | `https://inference.baseten.co/v1` | `BASETEN_API_KEY` | |
| `digitalocean` | `digitalocean` (openai-chat with cache markers) | `https://inference.do-ai.run/v1` | `DIGITALOCEAN_ACCESS_TOKEN`, `DIGITALOCEAN_API_KEY`, `DO_INFERENCE_API_KEY` | `supportsPromptCacheKey` |
| `openrouter` | `openrouter` (openai-chat with cache markers + body extras) | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | see below |
| `vercel-ai-gateway` | `vercel-ai-gateway-messages` (default), `-responses` (model ids `openai/gpt-*`, `spacexai/grok-*`), `-chat` (`meta/muse-*`) | `https://ai-gateway.vercel.sh/v1` | `AI_GATEWAY_API_KEY`, `VERCEL_OIDC_TOKEN` | header `x-session-affinity: <promptCacheKey>`; PO `gateway`/`upstream` sent as body `providerOptions`; messages: `requireSignature` only for `anthropic/*`; chat: `reasoningField:"reasoning"` |
| `cloudflare-ai-gateway` | `cloudflare-ai-gateway-responses` (`openai/*`), `-messages` (`anthropic/*`), `-chat` (others; `workers-ai/` prefix stripped) | `https://api.cloudflare.com/client/v4/accounts/<accountId>/ai/v1`; native passthrough `https://gateway.ai.cloudflare.com/v1/<accountId>/<gatewayId or "default">/{openai,anthropic/v1}` | `CLOUDFLARE_API_TOKEN`, `CF_AIG_TOKEN` | headers `cf-aig-gateway-id`, `cf-aig-metadata`, `cf-aig-cache-ttl`, `cf-aig-cache-key`, `cf-aig-skip-cache`, `cf-aig-collect-log`; passthrough auth header `cf-aig-authorization: Bearer <token>`; anthropic model ids: prefix stripped and `.`→`-` |
| `cloudflare-workers-ai` | `cloudflare-workers-ai` (openai-chat) | `https://api.cloudflare.com/client/v4/accounts/<accountId>/ai/v1` | `CLOUDFLARE_API_KEY`, `CLOUDFLARE_WORKERS_AI_TOKEN`, `CLOUDFLARE_API_TOKEN` | accountId required unless baseURL |
| `alibaba` | `alibaba-chat` (default), `alibaba-messages`, `alibaba-responses` | `https://<host>/compatible-mode/v1` (chat/responses), `https://<host>/apps/anthropic/v1` (messages); host by region: `ap-southeast-1`→`dashscope-intl.aliyuncs.com`, `cn-beijing`→`dashscope.aliyuncs.com`, `cn-hongkong`→`cn-hongkong.dashscope.aliyuncs.com`, `us-east-1`→`dashscope-us.aliyuncs.com`, or `<workspaceID>.<region>.maas.aliyuncs.com` | `DASHSCOPE_API_KEY`, `ALIBABA_API_KEY` | messages: `requireSignature:false`; errors `"Alibaba requires region or baseURL"`, `"Alibaba region <r> requires workspaceID or baseURL"` |
| `moonshotai` | `moonshot-chat` (adds `thinking {type, keep?}`), `moonshot-messages`, `moonshot-responses` | `https://api.moonshot.ai/v1`, `https://api.moonshot.ai/anthropic/v1` | `MOONSHOT_API_KEY`, `MOONSHOTAI_API_KEY` | chat `{maxTokensField:"max_tokens", supportsStore:false, supportsStrictMode:false, supportsPromptCacheKey:true, sanitizer:"moonshot", reasoningField:"reasoning_content"}`; messages `{requireSignature:false, sanitizer:"moonshot"}` |
| `minimax` | `minimax-messages` (default), `minimax-chat` (adds `thinking`, `reasoning_split` default true), `minimax-responses` | `https://api.minimax.io/anthropic/v1`, `https://api.minimax.io/v1` | `MINIMAX_API_KEY` | chat `{supportsStore:false, supportsStrictMode:false}` |
| `meta` | `meta-responses` (default, HTTP only), `meta-chat`, `meta-messages` (default PO `thinking:{type:"adaptive", display:"omitted"}`) | `https://api.meta.ai/v1` | `META_API_KEY` | chat `{maxTokensField:"max_completion_tokens", supportsStore:false, supportsPromptCacheKey:true}` |
| `zai` | `zai-chat` | `https://api.z.ai/api/paas/v4` | `ZAI_API_KEY` | 4.8 |
| `zai-coding-plan` | `zai-coding-chat`, `zai-coding-messages`, `zai-coding-responses` | `https://api.z.ai/api/coding/paas/v4`, `https://api.z.ai/api/anthropic/v1`, `https://api.z.ai/api/v1` | `ZAI_API_KEY` | messages `requireSignature:false` |
| `venice` | `venice-chat` | `https://api.venice.ai/api/v1` | `VENICE_API_KEY` | 4.7 |
| `openai-compatible` | `openai-compatible-chat`, `openai-compatible-responses` | required | none (apiKey only) | `provider` setting renames the provider id |
| `opencode` (Zen), `typesafe-ai` | experimental evaluation only | `https://opencode.ai/zen/v1`, `https://api.typesafe.ai/v1` | `OPENCODE_API_KEY`, `TYPESAFE_API_KEY` | not chat |

OpenRouter body extras (`packages/ai/src/providers/openrouter.ts:96-162`): assistant messages drop
`reasoning_content`/`reasoning_text`; `reasoning` = concatenated reasoning text only when `reasoning_details` exist;
**all remaining PO keys are spread into the body** except `usage, models, provider, plugins, web_search_options, debug,
user, reasoning, promptCacheKey` which are handled explicitly: `usage` → `{include:true}` by default
(`false` → `{include:false}`, object passthrough); `models` (array), `provider` (object), `plugins` (array),
`web_search_options`, `debug` (objects), `user` (string); `reasoning` object with `max_tokens` fitted by
`fitThinkingBudget(max_tokens, maxTokens, 1024)`. Model compat `supportsPromptCacheKey:true`.

### 5.3 npm package → native entrypoint mapping (`packages/core/src/aisdk-native.ts`)

models.dev and v1 configs name AI SDK npm packages. `AISDKNative.native(npm, {providerID, modelID, shape, settings})`
(`aisdk-native.ts:134-148`) resolves in order: per-provider host table, Bedrock Mantle, Azure chat, then the package
table. Unmapped packages stay `aisdk:<npm>` and run through the AI SDK bridge.

| npm | Native entrypoint |
|---|---|
| `@ai-sdk/amazon-bedrock` | `@opencode/ai/providers/amazon-bedrock` |
| `@ai-sdk/amazon-bedrock/mantle` | `.../amazon-bedrock/mantle/chat` if model id contains `gpt-oss` else `.../mantle/responses` (`.../mantle` without model) |
| `@ai-sdk/alibaba` | `.../alibaba/chat` |
| `@ai-sdk/anthropic` | `.../anthropic` |
| `@ai-sdk/azure` | `.../azure/responses`, or `.../azure/chat` when model `shape == "completions"` or setting `useCompletionUrls === true` |
| `@ai-sdk/cerebras`, `@ai-sdk/cohere`, `@ai-sdk/deepinfra`, `@ai-sdk/google`, `@ai-sdk/google-vertex`, `@ai-sdk/groq`, `@ai-sdk/mistral`, `@ai-sdk/openai`, `@ai-sdk/openai-compatible`, `@ai-sdk/togetherai`, `@ai-sdk/xai` | `.../<same name>` |
| `@ai-sdk/google-vertex/anthropic` | `.../google-vertex/messages` |
| `@ai-sdk/gateway` | `.../vercel-ai-gateway` |
| `@openrouter/ai-sdk-provider` | `.../openrouter` |
| `ai-gateway-provider` | `.../cloudflare-ai-gateway` |
| `venice-ai-sdk-provider` | `.../venice` |

Host overrides (`aisdk-native.ts:82-123`) keyed by provider id: `alibaba`, `alibaba-cn`, `alibaba-coding-plan(-cn)`,
`alibaba-token-plan(-cn)` → `@ai-sdk/openai-compatible`→`.../alibaba/chat`, `@ai-sdk/anthropic`→`.../alibaba/messages`,
`@ai-sdk/openai`→`.../alibaba/responses`; same triple for `meta`, `minimax`, `minimax-cn`, `minimax-coding-plan`,
`minimax-cn-coding-plan` (→ minimax), `moonshotai`, `moonshotai-cn`, `kimi-for-coding` (→ moonshot),
`zai-coding-plan`, `zhipuai-coding-plan` (→ zai-coding-plan); `baseten`→`.../baseten`; `cloudflare-ai-gateway` (anthropic,
openai, openai-compatible, ai-gateway-provider → `.../cloudflare-ai-gateway`); `cohere` + openai-compatible →
`.../cohere/chat`; `cloudflare-workers-ai`, `deepseek`, `digitalocean`, `fireworks-ai` (→ `.../fireworks`) for
openai-compatible; `google-vertex`: openai-compatible → `.../google-vertex/chat`, `@ai-sdk/mistral` →
`.../google-vertex/mistral`; `zai`, `zhipuai` + openai-compatible → `.../zai/chat`.

Settings translation on rewrite (`aisdk-native.ts:165-280`): drops `headers` (→ request headers), `extraBody`
(→ body overlay), `useCompletionUrls`, and OpenRouter `appName` (→ header `X-OpenRouter-Title`), `appUrl`
(→ `HTTP-Referer`), `api_keys` (→ `X-Provider-API-Keys: <JSON>`), `compatibility`. Bedrock: AI SDK keys
`bearerToken` → `apiKey`, `endpoint` → `baseURL` (`${AWS_REGION}` replaced with region), `accessKeyId/secretAccessKey/sessionToken/region` → `credentials`;
for Converse models, `reasoningConfig`/`additionalModelRequestFields`/`anthropicBeta`/`serviceTier` become body
`additionalModelRequestFields` (Claude: adaptive thinking/`output_config.effort`; gpt-oss: `reasoning_effort`;
other openai: `reasoning.effort`; others: `reasoningConfig {type?, budgetTokens?, maxReasoningEffort}`) and
`serviceTier: {type}`; Claude `reasoningConfig {type:"enabled", budgetTokens}` → setting `thinking {type:"enabled", budgetTokens}`.
When the replacement is `@opencode/ai/providers/openai-compatible` for a non-native specifier, setting `provider` =
canonical provider id.

### 5.4 Catalog data model (`packages/schema/src/provider.ts`, `packages/schema/src/model.ts`)

`Provider.Info = { id, canonical?, integrationID?, name, activation: "auto"|"enabled"|"disabled", package, settings?, headers?, body? }`;
`Provider.Settings` = `{timeout?, headerTimeout?, chunkTimeout? (number|false), compaction?: {type:"summary"|"native"}, transport?: "http"|"websocket", ...any}`.
`Model.Info = { id, modelID (API id), providerID, canonical?, family?, name, compatibility?, package?, settings?, headers?, body?, capabilities:{tools, input:string[], output:string[]}, variants:[{id, settings?, headers?, body?}], time:{released: ms}, cost:[{tier?:{type:"context", size}, input, output, cache:{read, write}}], status: "alpha"|"beta"|"deprecated"|"active", enabled, limit:{context, input?, output} }`.
`Model.Compatibility` (catalog-level) = `{reasoningField?, requireReasoning?, maxTokensField?, requireFinishReason?, requireAssistantAfterTool?, supportsPromptCacheKey?, supportsThinkingBlockBinding?, supportsEffortUpdates?}`.
Defaults for unknown models (`Model.Info.default`): capabilities `{tools:true, input:["text","image"], output:["text"]}`,
`limit {context:200000, output:32000}`, status active, enabled, cost `[]`.
Model ref string: `"<providerID>/<modelID>[#<variant>]"` parsed at the first `/` and the first `#` after it
(`packages/schema/src/model.ts:18-42`); `Model.parse` in core splits at the first `/` only (`packages/core/src/model.ts:298-304`).

### 5.5 models.dev catalog (`packages/core/src/models-dev.ts`)

* Source URL `https://models.opencode.ai` (override `OPENCODE_MODELS_URL`); fetch `GET <source>/api.json` with header
  `User-Agent: opencode/<channel>/<version>/<name>` (`packages/core/src/app.ts:24-26`), 10 s timeout, retried twice on
  transient errors/non-2xx with jittered exponential backoff starting at 200 ms (`models-dev.ts:316-352`).
* Local file override `OPENCODE_MODELS_PATH` (read as JSON, wins over everything); `OPENCODE_DISABLE_MODELS_FETCH`
  (truthy) disables network fetch (`packages/cli/src/server-process.ts:110-114`).
* Cache: SQLite KV table `kv` (columns `key` PK, `value` JSON text, timestamps) in the main DB
  (`$OPENCODE_DB` or `<XDG_DATA_HOME|~/.local/share>/opencode/opencode.db`, channel-specific name
  `opencode-<channel>.db` for non-release channels). Key `"models-dev:catalog"` for the default source, else
  `"models-dev:catalog:<Hash.fast(source)>"`. Value `{updatedAt: ms, digest: sha256(body), body: <raw JSON text>}`.
  Cache write failures are logged (`"Failed to cache models.dev catalog"`) and ignored.
* Populate order (`models-dev.ts:385-401`): file → KV cache (any age) → bundled snapshot
  (`packages/core/src/models-dev/snapshot.txt`, the raw `api.json`, embedded at build; refreshed by
  `packages/core/script/update-models-snapshot.ts`, which refuses bodies with < 100 providers) → network fetch (if
  enabled) → `[]`. The result is memoized forever until invalidated.
* Refresh loop: if fetch enabled, `refresh()` runs immediately and then every 5 minutes (`Schedule.spaced(5 min)`).
  `refresh(force=false)`: skip if cache younger than 5 min; fetch; skip if digest unchanged; validate JSON; write
  cache; invalidate memo; publish `ModelsDev.Event.Refreshed`. Errors logged `"Failed to fetch models.dev"` and ignored.
* Catalog JSON (`models-dev.ts:21-77`): `{ <providerID>: { id, name, env: string[], npm, api?, models: { <id>: {
  id, name, family?, release_date, attachment, reasoning, reasoning_options?: [{type:"effort", values:[string|null]} | {type:"toggle"} | {type:"budget_tokens", min?, max?}],
  temperature?, tool_call, interleaved?: bool|string|{field}, cost?: {input, output, cache_read?, cache_write?, tiers?: [{...cost, tier:{type:"context", size}}]},
  limit:{context, input?, output}, modalities?:{input, output}, experimental?:{modes?:{<mode>:{cost?, provider?:{body?, headers?}}}},
  status?: "alpha"|"beta"|"deprecated", provider?:{npm?, api?, shape?: "responses"|"completions"} } } } }`.
* Normalization (`models-dev.ts:85-250`):
  - Provider `package` = native mapping of `npm` (5.3) else `aisdk:<npm>`; `settings.baseURL = api` when present
    (except for the cloudflare-workers-ai package); `activation:"auto"`.
  - Model: `modelID = id`; `package` set only if the model overrides npm or maps differently; `settings.baseURL =
    provider.api` of the model; `settings.provider = providerID` when package is `openai-compatible`; compatibility
    from `interleaved` (string or `{field}` → `reasoningField`); `capabilities = {tools: tool_call, input/output modalities}`;
    `time.released = Date.parse(release_date) || 0`; cost list = base + tiers (missing values = 0); `status` default
    `active`; `limit` copied; variants from `Variant.resolve` (5.7) with supports = `reasoning_options` (effort values
    with `null`/`"null"` removed; if none, `[]` → no variants; note `resolve` defaults to `[{type:"effort"}]` only when
    the argument is omitted).
  - Each `experimental.modes.<mode>` adds a model `"<id>-<mode>"` named `"<name> <Mode>"` with merged cost and request
    headers/body.
* Plugin filtering (`packages/core/src/plugin/models-dev.ts:9-102`): drop providers `azure-cognitive-services`,
  `google-vertex-anthropic`; drop deprecated models, non-text models, and Bedrock inference-profile-only ids
  (`amazon.nova-2-lite-v1:0, anthropic.claude-fable-5, anthropic.claude-fable-5-1, anthropic.claude-haiku-4-5-20251001-v1:0,
  anthropic.claude-opus-4-1-20250805-v1:0, anthropic.claude-opus-4-5-20251101-v1:0, anthropic.claude-opus-4-7,
  anthropic.claude-opus-4-8, anthropic.claude-opus-5, anthropic.claude-sonnet-4-5-20250929-v1:0, anthropic.claude-sonnet-5,
  deepseek.r1-v1:0, mistral.pixtral-large-2502-v1:0`). For each provider with env vars an integration is registered
  with methods `{type:"key"}` and `{type:"env", names: env}`; providers added with `integrationID = provider id`.

### 5.6 Availability, defaults, resolution

* Provider availability (`packages/core/src/provider.ts:398-432`): `disabled` → hidden; providers whose discovered
  models came from a specific integration connection are hidden when the active connection differs; `enabled` →
  available; else available if its integration has ≥1 connection, or if it has no integration at all.
* Effective model = model overlaid on provider: `package = model.package ?? provider.package`,
  `settings = merge(provider.settings minus {chunkTimeout, headerTimeout, timeout, transport}, model.settings)`,
  `headers`/`body` merged (`packages/core/src/model.ts:191-215`); header merge is case-insensitive, later wins with
  later key spelling (`provider.ts:205-219`); `mergeOverlay` deep-merges records.
* Model list sorted by `time.released` descending. Default model: configured `model` if enabled, else first available
  model that supports text (`model.ts:260-265`). Small model for a provider: first active text model whose `family`
  is, in priority order, `gpt-luna`, `gemini-flash-lite`, `gemini-flash`, `claude-haiku` (`model.ts:266-278`).
* `ModelResolver.resolve` (`packages/core/src/model-resolver.ts:363-429`): look up the selected model (or default with
  a package, else first available text model with a package); apply variant overlay (`withVariant`, `"default"` = no
  variant; unknown → `"Variant unavailable for <p>/<m>: <variant>"`); fetch the provider's active integration
  connection and resolve its credential; settings = `merge(provider.settings, variant model settings)`.
* `fromCatalogModel` (`model-resolver.ts:168-257`):
  - `apiKey: ""` setting is removed.
  - `aisdk:` packages → AI SDK bridge.
  - Native: settings = provider/model settings + credential metadata + key credential `configuration`, minus core keys
    `chunkTimeout, compaction, fetch, headerTimeout, timeout, transport`; `${VAR}` placeholders in `baseURL` replaced
    from env (unresolved → `"Cannot initialize <p>/<m>: VAR is required to resolve the provider endpoint"`);
    credential injection: key → `apiKey`; oauth → `authToken` (anthropic/anthropic-compatible), `accessToken`
    (google-vertex*), else `apiKey`; Bedrock without key/oauth → `auth:"sigv4"`; Azure → `apiKey:"azure-cli"`
    placeholder (replaced by the plugin's Entra token hook); when a credential exists, `accessToken/apiKey/authToken`
    from settings are dropped first. `provider` setting = canonical id when set; `headers`, `body` from the model.
  - Calls `package.model(model.modelID ?? model.id, settings)`, then sets `provider = canonical ?? providerID`, merges
    catalog `compatibility` over the package's, and sets route HTTP defaults
    `{timeout?, headerTimeout: ?? 300000, chunkTimeout: ?? 300000}` from settings (`packages/core/src/provider.ts:170-184`).
  - `ProviderConfigurationError` → `ModelConfigurationError` `"Cannot initialize <p>/<m>: <detail>"`; other failures →
    `ModelInitializationError` with phase `load|init|construct`.
  - `compaction.type == "native"` on a route without compaction → `"Provider compaction is not supported by <p>/<m> (<route>)"`.
  - If provider `activation == "enabled"`, no credential, no `apiKey/authToken/accessToken` setting, and the package is
    an API-key package (list at `model-resolver.ts:437-475`), the route auth is replaced by `Auth.none` (keyless
    local/custom endpoints).

### 5.7 Variants (`packages/core/src/variant.ts`)

`Variant.resolve(model, supports = [{type:"effort"}])`: protocol chosen by `model.package` (`variant.ts:577-633`);
output = toggle `"none"` variant (when toggle supported) + main variants (effort, else budget, else toggle), deduped by
id. Default effort values `EFFORTS = ["low","medium","high"]`. Each variant is an overlay of `settings`
(→ provider options), `headers`, `body`.

| Package(s) | effort variant overlay | toggle (`none`/`thinking`) | budget (`high`/`max`) |
|---|---|---|---|
| openai, azure/responses, mantle, alibaba/meta/minimax/moonshot/zai-coding-plan responses | values default `none, minimal, low, medium, high, xhigh`; `{reasoningEffort, reasoningSummary:"auto", include:["reasoning.encrypted_content"]}` | — | — |
| xai | default `low, medium, high`; same as responses | — | — |
| openai-chat family (azure/chat, cerebras, cohere/chat, digitalocean, fireworks, groq, meta/chat, minimax/chat, mistral, togetherai, google-vertex/mistral, openai-compatible non-nvidia) | `{reasoningEffort}` | — | — |
| google-vertex/chat | as openai chat | `body.chat_template_kwargs.{thinking (deepseek) or enable_thinking}: false/true` | — |
| anthropic, google-vertex/messages, meta/messages | Claude parsing (`variant.ts:55-69`): manual-thinking models (3.7, 4.0-4.5 except opus 4.5) use budget variants (min 1024); opus 4.5 → `{thinking:{type:"enabled", budgetTokens: min(16000, output-1)}, effort}`; else `{thinking:{type:"adaptive", display:"summarized"}, effort}`; defaults `low, medium, high, max` for 4.6, else `low, medium, high, xhigh, max`; `none` → `{thinking:{type:"disabled"}}` (omitted for fable/mythos) | disabled / adaptive (or manual budget) | `{thinking:{type:"enabled", budgetTokens}}`, ceiling 32000 |
| google, google-vertex | `{thinkingConfig:{includeThoughts:true, thinkingLevel: effort}}` | `{includeThoughts:false, thinkingBudget:0}` / `{includeThoughts:true, thinkingBudget:-1}` | `{thinkingConfig:{includeThoughts:true, thinkingBudget}}` |
| amazon-bedrock | Claude: `body.additionalModelRequestFields = {thinking: adaptive (non-manual), output_config:{effort}}`; gpt-oss: `{reasoning_effort}`; other openai/xai: `{reasoning:{effort}}`; others `{reasoningConfig:{type:"enabled", maxReasoningEffort}}` | Claude thinking disabled/adaptive; others reasoningConfig disabled/enabled | Claude: setting `thinking.budgetTokens` (ceiling 32000); others `reasoningConfig.budgetTokens` |
| openrouter | `{reasoning:{effort}}` | `{reasoning:{enabled:false/true}}` | `{reasoning:{max_tokens}}` |
| deepseek | default `low, high, max`; `none` → `body.thinking.type:"disabled"`; else `{reasoningEffort}` + `body.thinking.type:"enabled"` | body thinking disabled/enabled | — |
| alibaba/chat | default `high, max` for glm/deepseek ids else `low, medium, xhigh`; fixed-thinking ids (`-thinking`, `qwq`, `deepseek-r1`, `minimax-m2`) have none; `{enableThinking:true, reasoningEffort}` / `none` → `{enableThinking:false}` | `enableThinking` | `{enableThinking:true, thinkingBudget}`, ceiling 64000 |
| alibaba/messages | same values; `{thinking:{type:"enabled"}, effort}` | thinking disabled/enabled | budget, ceiling 64000 |
| zai/chat, zai-coding-plan/chat | GLM version parse `/glm-?(\d+)(?:(?:[.-]|p)(\d+))?/`; "latest" (coding plan, unparsable, or ≥5.3) default `low, high, max`, 5.2 default `high, max`; `{thinking:{type:"enabled", clear_thinking:false}, reasoningEffort}`, none/minimal → disabled; older GLM → toggle only | `thinking` disabled / `{type:"enabled", clear_thinking:false}` (not for latest) | — |
| zai-coding-plan/messages | default `high, max` (glm-5.3: `low, high, max`); `{thinking:{type}, effort}` | toggle (not glm-5.3) | — |
| moonshot/chat | k2.5/k2.6: toggle `thinking.type`; k2-thinking/k2.7-code: none; else effort default `low, high, max` | | |
| moonshot/messages | effort `{effort}` (not kimi-k2*) | | |
| minimax/messages | not m2: toggle disabled/adaptive | | |
| cloudflare-workers-ai | `{reasoningEffort}`; `none` → `body.chat_template_kwargs {enable_thinking:false, thinking:false}` | template kwargs | — |
| baseten, deepinfra, venice, nvidia (openai-compatible with provider `nvidia`), cohere (native), vercel gateway, cloudflare gateway, sap-ai-core, gitlab | see `variant.ts:128-575` (exact overlays listed there) | | |

Budget helper (`variant.ts:41-51`): `maximum = min(support.max ?? ceiling-1, limit.output-1, ceiling-1)`;
`high = min(max(support.min ?? 0, floor((maximum+1)/2)), maximum)`; none if `maximum <= 0`.

### 5.8 Custom providers from config (`packages/core/src/config/plugin/provider.ts`, `packages/schema/src/config/provider.ts`)

Config key `providers: { <id>: { canonical?, name?, env?: string[], package?, settings?, headers?, body?, models?: { <id>: { modelID?, family?, name?, compatibility?, package?, settings?, headers?, body?, capabilities? (partial), variants?: [{id, settings?, headers?, body?}], cost?: Cost|Cost[], disabled?, limit?: {context?, input?, output?} } } } }`.
* Each configured provider: integration gets method `{type:"key", label:"Manually enter API Key"}` (if new) and
  `{type:"env", names: env}` when `env` given; provider `activation = "enabled"`; `canonical` copies the canonical
  provider's info (keeping own id/integrationID) when it changes; `name/package` replaced; `settings`/`body` deep-merged;
  `headers` merged.
* Models: a configured model id inherits the base model from the source provider (`modelID ?? id`); when the config
  sets a provider `package`, inherited model packages are cleared; a provider `settings.baseURL` clears inherited model
  baseURLs. Model fields are then overlaid (compatibility shallow-merged, capabilities merged with defaults, variants
  merged by id, cost list replaced, `disabled` → `enabled=false`, limit shallow-merged). Brand-new models without
  configured variants get `Variant.resolve(model)` (effort support by default).
* Top-level `model` (`{providerID, model}` selection) sets the default model.
* v1 config migration (`packages/core/src/v1/config/migrate.ts:106-203`): `provider.<id>` → `providers.<id>` with
  `npm` → `package: "aisdk:<npm>"`, `api` → `settings.baseURL`, `options` → settings/headers/body, model `options` →
  model settings, `interleaved` → compatibility, `cost.context_over_200k` → tier `{type:"context", size:200000}`;
  `azure-cognitive-services` → `azure` (baseURL `https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai`
  for openai-compatible), `google-vertex-anthropic` → `google-vertex`; `enabled_providers` / `disabled_providers`
  become permission rules on action `"provider.use"` (`packages/core/src/config/normalize.ts:388-405`);
  `small_model`, `whitelist`, `blacklist`, `id` are unsupported/diagnosed.

### 5.9 Core provider plugins (`packages/core/src/plugin/provider/*.ts`)

Registered in order (`packages/core/src/plugin/provider.ts:35-68`): AmazonBedrock, Azure, Cerebras, ChatGPT, CloudflareAIGateway,
CloudflareWorkersAI, Cohere, Databricks, DigitalOcean, GithubCopilot, GitLab, GoogleVertex, Kilo, LLMGateway,
LMStudio, Modal, Neon, Nvidia, Ollama, Opencode, SnowflakeCortex, OpenAI, OpenRouter, Perplexity, Poe,
PromptCacheKey, SapAICore, Vercel, VLLM, XAI, Zenmux, DynamicProvider (last). Plugins edit three registries through
transforms: integrations (auth methods), providers (`Provider.Info` + model maps), models; plus hooks
(`session.hook("model.request" | "http.request" | "http.response" | "retry" | "context" | "compaction" | "title" | "experimental.ws.handshake")`,
`aisdk.hook("sdk" | "language")`).

| Plugin | Behavior |
|---|---|
| openai | OAuth methods `chatgpt-browser` (label `"Codex browser (legacy)"`) and `chatgpt-headless` (`"Codex device code (legacy)"`) (6.4.2). Sets provider `settings.transport` default `"websocket"`. With a Codex OAuth credential: `baseURL = "https://chatgpt.com/backend-api/codex"`, headers `originator: opencode`, `x-codex-beta-features: remote_compaction_v2`, `chatgpt-account-id: <accountID>`; models allowed only if API id ∈ {`gpt-5.5`, `gpt-5.3-codex-spark`} or (not in {`gpt-5.5-pro`, `gpt-5.6`} and version > 5.4); `reasoning.mode == "pro"` body models disabled; cost zeroed; limits `{context:400000, input:272000}`; request hook rewrites `https://api.openai.com` base URLs to Codex, sets `originator` and `session-id` (session affinity id); `maxTokens` removed from context/compaction requests (`packages/core/src/plugin/provider/openai.ts:232-330`) |
| chatgpt | OAuth method `chatgpt-token-sharing` ("Sign in with ChatGPT", 6.4.1). With it: provider settings `{baseURL: "https://api.openai.com/v1", transport:"http", compaction:{type:"summary"}}`; only models in a fixed allow-list (`gpt-5.5`, `gpt-5.5-fast`, `gpt-5.6-luna(-fast)`, `gpt-5.6-sol(-fast)`, `gpt-5.6-terra(-fast)`, `gpt-6-astra(-fast)`, `gpt-6-luna(-fast)`, `gpt-6-sol(-fast)`, `gpt-6.1-sol(-fast)`); `supportsEffortUpdates:false`; cost zeroed; headers `session-id`, `thread-id`, `x-client-request-id`; retry hook disables retries when the error body contains any of `subscription_sharing_usage_limit_exceeded, subscription_sharing_user_not_eligible, subscription_sharing_unsupported_capability, subscription_sharing_route_not_supported, subscription_sharing_invalid_user, chatpass_v2_scope_not_authorized, chatpass_v2_invalid_authorization_context` |
| github-copilot | removes the key method; OAuth device method (6.4.3); live model list from `<base>/models` (5 s timeout) with headers `Authorization: Bearer <github token>`, `User-Agent`, `X-GitHub-Api-Version: 2026-08-01`; models built by `CopilotModels.derive` (`packages/core/src/github-copilot/models.ts`): endpoint from `supported_endpoints` (`/v1/messages` → native anthropic package with `baseURL <base>/v1`; else `aisdk:@ai-sdk/github-copilot` with `settings.endpoint` `responses`/`chat`), costs from `billing.token_prices` × `10000/batch_size`, limits from capabilities, variants from `reasoning_effort` / `adaptive_thinking` / `max_thinking_budget`; base URL `https://api.githubcopilot.com` or `https://copilot-api.<enterprise domain>` or user `endpoints.api`. Request headers: `X-Interaction-Type` (`conversation-agent`, `conversation-subagent` for child sessions, `conversation-background` for title, `conversation-compaction`), `X-Interaction-Id`, `x-initiator: agent` for non-agent interactions; every HTTP request: `Authorization: Bearer <token>` (removing `x-api-key`), `User-Agent`, `Openai-Intent: conversation-edits`, `X-GitHub-Api-Version: 2026-08-01`, `x-initiator: agent` if the last message is not a user message (else `user` unless already set), `Copilot-Vision-Request: true` when images present, `anthropic-beta: interleaved-thinking-2025-05-14` on messages requests. Language selection without endpoint metadata: Responses for `gpt-N` (N≥5, not `gpt-5-mini*`), `grok-*`, `mai-code-*`, else chat. Title hook uses `gpt-4o-mini` via a direct non-streaming `/chat/completions` call (`X-Interaction-Type: agent-session-name-generation`). `gpt-5-chat-latest` disabled |
| opencode (Console) | OAuth device method + `{type:"key", label:"API key (service account)"}` (6.4.4); fetches `GET <server>/api/v2/config` (Bearer + `x-org-id`) every minute → `{providers: <config providers>, websearch?, mcp?, experimental.policies?}` merged into the catalog (credentials stripped from settings) under integration `opencode`; without any key (`OPENCODE_API_KEY`, connection, or setting) the `opencode` provider is enabled with `apiKey: "public"` and paid models (any cost input > 0) disabled; 401/403 responses re-check config and are replaced by `{"error":{"type":"authentication_error","message":"<status message>."}}` |
| xai | OAuth device method `device` ("SuperGrok Subscription", 6.4.5) + key method; transport default `"websocket"` |
| azure | env `AZURE_API_KEY`, `AZURE_COGNITIVE_SERVICES_API_KEY`; key method with `resourceName` form (unless configured); external method `azure-cli` ("Microsoft Entra ID (Azure CLI)") when `az` is on PATH; resource from settings/credential/`AZURE_RESOURCE_NAME`/`AZURE_COGNITIVE_SERVICES_RESOURCE_NAME`, `${AZURE_RESOURCE_NAME}` placeholders expanded; deployments discovered via Azure CLI/management API narrow the model list (deployment name → model id lowercased); Entra tokens from `az account get-access-token --scope <scope> --output json` (cached until 5 min before expiry), scope `https://ai.azure.com/.default` for `*.services.ai.azure.com` non-`/models` paths, else `https://cognitiveservices.azure.com/.default`; injected per HTTP request / WebSocket handshake (removes `api-key`, `x-api-key`); transport `websocket` for v1 resource URLs |
| google-vertex | env `GOOGLE_VERTEX_API_KEY`; external method `google-adc` (project + location form, options from gcloud config); expands `${GOOGLE_VERTEX_PROJECT}`, `${GOOGLE_VERTEX_LOCATION}`, `${GOOGLE_VERTEX_ENDPOINT}` (`aiplatform.googleapis.com` for global else `<loc>-aiplatform.googleapis.com`) in base URLs |
| amazon-bedrock | env `AWS_BEARER_TOKEN_BEDROCK`; key method "Bedrock API key"; external `aws-profile` (profiles parsed from `$AWS_CONFIG_FILE ?? ~/.aws/config` and `$AWS_SHARED_CREDENTIALS_FILE ?? ~/.aws/credentials`); provider usable via default chain when profile set or any of `AWS_PROFILE, AWS_ACCESS_KEY_ID, AWS_WEB_IDENTITY_TOKEN_FILE, AWS_CONTAINER_CREDENTIALS_RELATIVE_URI, AWS_CONTAINER_CREDENTIALS_FULL_URI` present; region default `AWS_REGION ?? AWS_DEFAULT_REGION ?? "us-east-1"` |
| gitlab | OAuth PKCE (6.4.6); `aisdk:gitlab-ai-provider` SDK with `instanceUrl` (`GITLAB_INSTANCE_URL` ?? `https://gitlab.com`), `apiKey` (`GITLAB_TOKEN`), gateway headers `User-Agent: <ua> gitlab-ai-provider/<v> (<platform> <release>; <arch>)`, `anthropic-beta: context-1m-2025-08-07`; workflow model discovery |
| snowflake-cortex | key method (PAT, account form), env `SNOWFLAKE_CORTEX_TOKEN`, `SNOWFLAKE_CORTEX_PAT`, OAuth browser (6.4.7); base URL `https://<account>.snowflakecomputing.com/api/v2/cortex/v1` (account from `SNOWFLAKE_ACCOUNT`, credential, or settings); models get `maxTokensField: "max_completion_tokens"` |
| digitalocean | OAuth implicit grant (6.4.8); model routers from `https://api.digitalocean.com/v2/gen-ai/models/routers` added as models `router:<name>` |
| poe | OAuth PKCE yielding an API key (6.4.9) |
| cloudflare-ai-gateway / -workers-ai | key methods with accountId/gatewayId forms; env `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`; workers base URL `https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1` |
| ollama / lmstudio / vllm (`local.ts`) | local discovery every 30 s (1 s request timeout) at config `providers.<id>.settings.baseURL` or default origin `http://127.0.0.1:11434` (ollama: `GET /api/tags` + `POST /api/show {model}`; keeps models with capability `completion`), `http://127.0.0.1:1234` (LM Studio: `GET /api/v1/models`, `type=="llm"`), `http://127.0.0.1:8000` (vLLM: `GET /health`, `GET /v1/models`, `owned_by=="vllm"`); when models exist the provider becomes `{activation:"enabled", package:"@opencode/ai/providers/openai-compatible", settings:{baseURL, provider:<id>, apiKey: <configured apiKey> ?? ""}}` with discovered models (tools/vision/context); default origins are skipped in workspaces/remote environments |
| modal | models from `ModalModels.load(baseURL, key)` (key from credential or `MODAL_PROXY_TOKEN`) |
| cerebras | header `X-Cerebras-3rd-Party-Integration: opencode` |
| openrouter | headers `HTTP-Referer: https://opencode.ai/`, `X-Title: opencode`; disables `gpt-5-chat-latest`, `openai/gpt-5-chat` |
| kilo (`https://api.kilo.ai/api/gateway`), zenmux (`https://zenmux.ai/api/v1`), nvidia (`https://integrate.api.nvidia.com/v1`), llmgateway (`https://api.llmgateway.io/v1`), vercel (`aisdk:@ai-sdk/vercel`) | attribution headers `HTTP-Referer: https://opencode.ai/`, `X-Title: opencode` (+ nvidia `X-BILLING-INVOKE-ORIGIN: OpenCode`, llmgateway `X-Source: opencode`) |
| databricks, neon | env methods `DATABRICKS_TOKEN`, `NEON_AI_GATEWAY_TOKEN` only |
| prompt-cache-key | `supportsPromptCacheKey:true` for providers `abliteration-ai, aihubmix, anyapi, auriko, edenai, jalapeno, llama, morph, snowflake-cortex, tencent-tokenhub, upstage, venice` |
| cohere, perplexity, sap-ai-core, dynamic | AI-SDK factories (`@ai-sdk/cohere`, `@ai-sdk/perplexity`, SAP with `AICORE_SERVICE_KEY`, `AICORE_DEPLOYMENT_ID`, `AICORE_RESOURCE_GROUP`); dynamic: install any `aisdk:` package via npm (`@ai-sdk/<name>` unversioned → `@ai-sdk/<name>@ai-v6`) and call its first `create*` export |

### 5.10 AI-SDK bridge (`packages/core/src/aisdk.ts`) and Copilot SDK

Models whose package is `aisdk:<npm>` are served by a Vercel AI SDK v6 `LanguageModelV3` wrapped as an
`@opencode/ai` route (see section 9 open question: the Rust port must decide whether to re-implement these packages
natively). The GitHub Copilot SDK (`packages/core/src/github-copilot/**`) is a vendored fork of
`@ai-sdk/openai-compatible` chat and `@ai-sdk/openai` Responses with Copilot adaptations: `copilot` options/metadata
namespace, `thinking_budget`, reasoning fields `reasoning_text` / `reasoning_opaque` (round-tripped as
`providerMetadata.copilot.reasoningOpaque`), stateless Responses with encrypted reasoning, rotating item ids; chat
URL `<baseURL>/chat/completions`, responses `<baseURL>/responses`; default headers `Authorization: Bearer <apiKey>`
plus user-agent suffix `ai-sdk/openai-compatible/0.1.0` (`packages/core/src/github-copilot/copilot-provider.ts`).

---

## 6. Credentials, OAuth and provider login

### 6.1 Model

* **Integration** (`packages/schema/src/integration.ts`, `packages/core/src/integration.ts`): `{id, name, metadata?, methods: Method[], connections: Connection[]}`.
  Integration id usually equals the provider id. Methods:
  `{type:"oauth", id, label, form?}`, `{type:"command", id, label, command: string[]}`, `{type:"key", label?, form?}`,
  `{type:"external", id, label, form?}`, `{type:"env", names: string[]}`. Methods are keyed by `type` (+`id` for
  oauth/command/external); re-registering replaces.
* **Credential value** (`packages/schema/src/credential.ts:32-61`):
  `{type:"oauth", methodID, refresh, access, expires: epoch ms (non-negative int), metadata?}` |
  `{type:"key", key, metadata?, configuration?: <form answer>}` | `{type:"external", methodID, metadata?}`.
* **Connection**: `{type:"credential", id, label, method}` or `{type:"env", name}` plus runtime `status?: {status:"needs_auth", message, url?}`.
  Connection identity key: `"credential:<id>"` / `"env:<name>"` (`packages/core/src/integration/connection.ts:18-26`).

### 6.2 Storage

* SQLite table `credential` in the main DB (`opencode.db` under `$XDG_DATA_HOME/opencode` or
  `~/.local/share/opencode`, overridable by `OPENCODE_DB`; see 5.5): columns `id TEXT PK` (`"cred_" + ascending id`),
  `integration_id TEXT`, `label TEXT NOT NULL`, `value TEXT (JSON) NOT NULL`, `connector_id`, `method_id`,
  `active INTEGER(bool)`, `time_created`, `time_updated` (`packages/core/src/credential/sql.ts:5-14`). Secrets are stored
  in plaintext JSON (no encryption).
* Ordering & selection (`packages/core/src/credential.ts:87-207`): list order `active ASC, time_created ASC, id ASC`;
  the integration's active credential = first by `active DESC, time_created DESC, id DESC` (legacy rows without the
  flag → newest wins). `create` activates the new credential unless `activate === false` and another exists; default
  label `"default"`, but the integration layer picks `"<name>"`, `"<name> 2"`, ... unique labels. Removing the active
  credential activates the newest remaining. Events `credential.updated` and `credential.switched {integrationID, credentialID|null}`.
* Active connection for an integration (`packages/core/src/integration.ts:396-410, 719-722`): stored credentials
  (active first, then newest) take precedence over env connections; env connections are the env-method names whose
  variable is set (in listed order). An env connection resolves to `{type:"key", key: $NAME}`.
* OAuth refresh on resolve (`integration.ts:723-741`): if the method has `refresh` and `expires <= now + 5 min`, call it,
  store the new value, return it. Refresh failures surface as `Integration.AuthorizationError` (message = cause
  message or `"Authorization failed"`).
* Legacy import (`packages/core/src/database/migration/20260805200742_import_legacy_credentials.ts`): reads
  `<data>/auth.json` = `{ "<providerID or origin>": {type:"api", key, metadata?} | {type:"oauth", refresh, access, expires, accountId?, enterpriseUrl?} | {type:"wellknown", key, token} }`.
  For each id (trailing `/` stripped) without an existing credential: api → key credential; wellknown → key credential
  `{key: token}` and origin appended to KV `wellknown:sources`; oauth → OAuth credential with methodID `chatgpt-browser`
  (openai), `device` (github-copilot, opencode, xai) or `oauth` (others), metadata `{accountID?, enterpriseUrl?}`;
  label `"OAuth"`/`"API key"`.
* `account` / `account_state` tables (`packages/core/src/account/sql.ts`) hold OpenCode Console accounts
  (`id, email, url, access_token, refresh_token, token_expiry`) and the active account/org — legacy; Console auth in
  v2 is the `opencode` integration (6.4.4).

### 6.3 Generic OAuth attempt lifecycle (`packages/core/src/integration.ts:585-848`)

1. `oauth.connect {integrationID, methodID, answer?, label?}`: validate the method form; run `authorize(answer)` in a
   new scope → `{url, instructions, mode: "auto"|"code", expiresAt?, callback}`; create attempt id; expiry =
   `expiresAt ?? now + 10 min`. `auto`: callback runs in background immediately.
2. Client polls `oauth.status` → `pending|complete|failed (message)|expired`; terminal attempts retained 1 min;
   scrubber every 30 s.
3. `code` mode: `oauth.complete {code}` runs `callback(code)` (missing code → `CodeRequiredError`).
4. On success the credential is created (label = given ?? method `label(credential)` ?? unique name) and activated.
5. `oauth.cancel` closes the scope (stops loopback servers).
Command methods (`command.connect`) spawn the command (stdin ignored, env inherited), collect stderr as status
message, require exit 0 and non-empty trimmed stdout which becomes a key credential (errors
`"Authentication command exited <code>"`, `"Authentication command returned no credential"`).
Key methods: `connection.key {integrationID, key, answer?, label?}` stores `{type:"key", key, configuration?: answer}`
(answer rejected when the method has no form: `"Key method does not accept a form answer"`).
HTTP API (`packages/protocol/src/groups/integration.ts`): `GET /api/integration`, `GET /api/integration/:id`,
`POST /api/experimental/integration/wellknown`, `POST /api/integration/:id/connect/key`, `POST .../connect/external`,
`POST .../connect/oauth`, `GET .../connect/oauth/:attemptID`, `POST .../connect/oauth/:attemptID/complete {code?}`,
`DELETE .../connect/oauth/:attemptID`, `POST .../connect/command`, `GET|DELETE .../connect/command/:attemptID`;
credentials: `GET|POST /api/credential`, `PATCH|DELETE /api/credential/:id`, `POST /api/credential/:id/activate`.

Loopback callback pages (`packages/core/src/oauth/page.ts`): self-contained HTML; `success` title
`"Authorization successful"`, message `"OpenCode is now connected to <provider>."`; `error` title
`"Authorization failed"`; `bootstrap` page reads the URL fragment and POSTs it to `tokenPath` (implicit grant).

### 6.4 Provider OAuth flows

PKCE (where used): verifier = 43 chars drawn from `A-Za-z0-9-._~` via `byte % 66` (OpenAI/ChatGPT) or base64url of
32 random bytes (GitLab, Poe) / 48 bytes (Snowflake); challenge = base64url(SHA-256(verifier)), method `S256`.
State = base64url(32 random bytes). All token requests send `User-Agent: opencode/<channel>/<version>/<name>`.

#### 6.4.1 ChatGPT token sharing (`packages/core/src/plugin/provider/chatgpt.ts`)

* Integration `openai`, method `chatgpt-token-sharing`, label `"Sign in with ChatGPT"`.
* Loopback server on `127.0.0.1:<random port>`, path `/auth/callback`; redirect `http://127.0.0.1:<port>/auth/callback`.
* Authorize URL: `https://auth.openai.com/api/accounts/authorize?` with params in order `client_id` (saved client id
  or `"dynamic_agent_client"`), `agent_name_hint=OpenCode` (first sign-in only), `ext_agent_host_id` (persistent
  `urn:uuid:<uuid>` in plugin storage key `chatgpt-agent-host-id`), `response_type=code`, `redirect_uri`,
  `scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`,
  `resource=https://api.openai.com/v1`, `state`, `nonce` (base64url 32 bytes), `code_challenge_method=S256`, `code_challenge`.
* Callback: `error_description`/`error` → fail; state mismatch → `"Invalid OAuth state"`; client id from callback
  `client_id` (must equal saved one if present).
* Token: POST `https://auth.openai.com/api/accounts/oauth/token`, form `grant_type=authorization_code, client_id, code,
  code_verifier, redirect_uri, resource=https://api.openai.com/v1`. Must include scope `chatgpt.tokens.use.direct` and
  `id_token`; ID token verified against JWKS `https://auth.openai.com/.well-known/jwks.json` (issuer
  `https://auth.openai.com`, audience client id, RS256, required `exp, nonce, sub`, nonce match).
* Credential: `{access, refresh, expires: now + (expires_in ?? 3600)*1000, metadata:{clientID, scopes}}`.
  Refresh: same URL, `grant_type=refresh_token, client_id, refresh_token, resource`.
* Use: API base `https://api.openai.com/v1` with the access token as bearer.

#### 6.4.2 Codex (legacy) (`packages/core/src/plugin/provider/openai.ts`)

* Client id `app_EMoamEEZ73f0CkXaXp7hrann`, issuer `https://auth.openai.com`.
* Browser (`chatgpt-browser`): loopback `localhost:1455` (fallback `1457`; on EADDRINUSE first `GET http://localhost:1455/cancel`
  (2 s), retry bind 10× every 200 ms; error message mentions both ports). Authorize
  `https://auth.openai.com/oauth/authorize?response_type=code&client_id=...&redirect_uri=http://localhost:<port>/auth/callback&scope=openid profile email offline_access&code_challenge=...&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=...&originator=opencode`.
  Token POST `https://auth.openai.com/oauth/token` form `grant_type=authorization_code, code, redirect_uri, client_id, code_verifier`.
* Device (`chatgpt-headless`): POST `https://auth.openai.com/api/accounts/deviceauth/usercode` JSON `{client_id}` →
  `{device_auth_id, user_code, interval}`; user URL `https://auth.openai.com/codex/device`, instructions
  `"Enter code: <user_code>"`; poll POST `.../api/accounts/deviceauth/token` JSON `{device_auth_id, user_code}` every
  `max(interval,1)s + 3 s`; 403/404 = pending; success `{authorization_code, code_verifier}` exchanged with redirect
  `https://auth.openai.com/deviceauth/callback`.
* Account id: JWT claims (id_token then access_token) `chatgpt_account_id` ?? `["https://api.openai.com/auth"].chatgpt_account_id`
  ?? `organizations[0].id` → `metadata.accountID` → header `chatgpt-account-id`.
* Refresh: `grant_type=refresh_token, refresh_token, client_id` (metadata preserved when not re-derived).

#### 6.4.3 GitHub Copilot device flow (`packages/core/src/plugin/provider/github-copilot.ts:17-157`)

* Client id `Ov23li8tweQw6odWQebz`, method `device` ("Login with GitHub Copilot") with form `deploymentType`
  (`github.com` | `enterprise`) and `enterpriseUrl` (when enterprise; normalized by stripping `http(s)://` and trailing `/`).
* POST `https://<domain>/login/device/code` JSON `{client_id, scope:"read:user"}` (headers `Accept: application/json`,
  `Content-Type: application/json`, UA) → `{verification_uri, user_code, device_code, interval}`.
* Poll POST `https://<domain>/login/oauth/access_token` JSON `{client_id, device_code, grant_type:"urn:ietf:params:oauth:grant-type:device_code"}`
  waiting `interval + 3 s`; `authorization_pending` → wait; `slow_down` → new interval = `token.interval*1000` or `+5 s`; other error → `"Device authorization failed: <error>"`.
* Entitlement check: GET `https://api.github.com/copilot_internal/user` (enterprise: `https://api.<domain>/copilot_internal/user`)
  with `Authorization: Bearer <token>`, `X-GitHub-Api-Version: 2025-04-01`. `chat_enabled === false` → error
  (sign-up message if `can_signup_for_limited`). `endpoints.api` stored as `metadata.apiEndpoint`.
* Credential: `{access: token, refresh: token, expires: 0, metadata:{enterpriseUrl?, apiEndpoint?}}` (no refresh
  method; the GitHub OAuth token is used directly as the Copilot API bearer — there is no `copilot_internal/v2/token` exchange in v2).

#### 6.4.4 OpenCode Console device flow (`packages/core/src/plugin/provider/opencode.ts:101-178, 577-633`)

* Server default `https://opencode.ai/console` (hidden form field `server`), client id `opencode-cli`.
* POST `<server>/auth/device/code` JSON `{client_id:"opencode-cli", supports_org_scope:true}` →
  `{device_code, user_code, verification_uri_complete, expires_in, interval}`.
* Poll (after `interval` s) POST `<server>/auth/device/token` `{grant_type:"urn:ietf:params:oauth:grant-type:device_code", device_code, client_id}`;
  `authorization_pending` → same interval, `slow_down` → +5 s.
* Then GET `<server>/api/user` and `<server>/api/orgs` (Bearer); org = token `org_id` match or first by name.
  Credential metadata `{server, accountID, email, orgID, orgName}`; label = orgName.
* Refresh POST `<server>/auth/device/token` `{grant_type:"refresh_token", refresh_token, client_id}`; 400
  `invalid_grant` → `"Reconnect OpenCode Console to continue."`.
* Requests carry `authorization: Bearer <token>` and `x-org-id: <orgID>`.

#### 6.4.5 xAI device flow (`packages/core/src/plugin/provider/xai.ts`)

Client id `b1a00492-073a-47ea-816f-4c329264a828`, issuer `https://auth.x.ai/oauth2`, scope
`openid profile email offline_access grok-cli:access api:access`. POST `<issuer>/device/code` form
`client_id, scope, referrer=opencode`; open `verification_uri_complete ?? verification_uri`; poll POST `<issuer>/token`
form `grant_type=urn:ietf:params:oauth:grant-type:device_code, client_id, device_code` (interval default 5 s,
+3 s margin, `slow_down` +5 s, expiry default 300 s; errors `access_denied|authorization_denied`, `expired_token`).
Expiry = `expires_in` or JWT `exp` or +1 h. Refresh `grant_type=refresh_token`. Legacy method id `browser` is migrated to `device`.

#### 6.4.6 GitLab PKCE (`packages/core/src/plugin/provider/gitlab.ts`)

Bundled client id `fd180700a8f9c5d5557aca231632dd0611a1135a3bb510a741d5a988a3394fa7` (gitlab.com only; override
`GITLAB_OAUTH_CLIENT_ID`; legacy `1d89f9fdb23ee96d4e603201f6861dab6e143c5c3c00469a018a2d94bdc03d4e` for refreshing
old credentials), scope `api`, fixed redirect `http://127.0.0.1:8080/callback`; authorize
`<instance>/oauth/authorize?client_id&redirect_uri&response_type=code&state&scope=api&code_challenge&code_challenge_method=S256`;
token exchange at the instance (`grant_type=authorization_code, code, redirect_uri, code_verifier`); refresh rotates
the refresh token and is de-duplicated across concurrent callers (result cached 60 s).

#### 6.4.7 Snowflake (`packages/core/src/plugin/provider/snowflake-cortex.ts`)

Client id `LOCAL_APPLICATION` with `Authorization: Basic base64("LOCAL_APPLICATION:LOCAL_APPLICATION")`; authorize
`https://<account>.snowflakecomputing.com/oauth/authorize` (scope `refresh_token` + `session:role:<role>` or
`session:role-encoded:<urlencoded>`), redirect `http://127.0.0.1:<port>/`; token POST `.../oauth/token-request`
(form + `client_id`); default expiry 600 s; `metadata.account`.

#### 6.4.8 DigitalOcean implicit grant (`packages/core/src/plugin/provider/digitalocean.ts`)

Client id `b1a6c5158156caac821fd1b30253ca8acb52454a48fa744420e41889cb589f82`, authorize
`https://cloud.digitalocean.com/v1/oauth/authorize?response_type=token&client_id&redirect_uri=http://localhost:1456/auth/callback&scope=genai:read inference:query&state`;
the callback page posts the fragment to `/auth/token`; credential `{access, refresh:"", expires: now + expires_in (default 30 days)}`.

#### 6.4.9 Poe (`packages/core/src/plugin/provider/poe.ts`)

Client id `client_728290227fc048cc9262091a1ea197ea`, authorize `https://poe.com/oauth/authorize` (scope
`apikey:create`, PKCE), redirect `http://127.0.0.1:<port>/callback`, optional `iss` must equal `https://poe.com`; token
POST `https://api.poe.com/token`; credential access = returned `api_key`, no refresh (expired → `"Poe API key expired. Log in with Poe again."`).

#### 6.4.10 Anthropic

There is **no Anthropic OAuth flow in v2** (no Claude Pro/Max login); Anthropic uses API keys (`ANTHROPIC_API_KEY`,
`x-api-key`) or a stored key credential. An OAuth credential for an Anthropic package would be passed as `authToken`
(Bearer) by `ModelResolver` (5.6).

### 6.5 Well-known discovery (`packages/core/src/wellknown.ts`, `packages/core/src/wellknown/plugin.ts`)

* Manifest URL: `<origin without trailing slashes>/.well-known/opencode` (GET, Accept JSON, 15 s timeout). Schema:
  `{auth?: {command: string[], env: string}, config?: object|null, remote_config?: {url, headers?}}`.
* `add(origin)`: requires `auth` (`"No authentication method found at <origin>"`); origins stored in KV
  `wellknown:sources` (array), manifest cached at `wellknown:manifest:<origin>` (used when the origin is unreachable),
  resolved configs at `wellknown:config:<origin>`.
* Integration per origin (id = origin) named by hostname, with method `{id:"login", type:"command", label:"Log in", command: auth.command}`;
  the command's stdout becomes a key credential.
* Config resolution: `config` (if any) plus `remote_config` fetched from `url` with `{env:NAME}` placeholders
  substituted from variables (`{[auth.env]: <stored key>}`) or the process env; response `{config:{...}}` or a bare
  config object; configs feed the config system (`packages/core/src/config.ts:148-152`).

---

## 7. Retries, timeouts, rate limits, error classification

### 7.1 Error taxonomy (`packages/ai/src/schema/errors.ts`)

All request failures are `AIError { reason }` (tag `"AI.Error"`); `message` = `reason.message`. Every reason carries
`message`, `body?` (raw response or triggering event), `http?: {url, status, headers}`, `cause?`.

| `_tag` | Extra fields | Retryable (`isRetryable`) |
|---|---|---|
| `InvalidRequest` | `parameter?`, `classification?: "context-overflow"|"payload-too-large"` | no |
| `UnsupportedOperation` | `operation`, `provider?`, `route?` | no |
| `NoRoute` | `route, provider, model` | no |
| `Authentication` | — | no |
| `RateLimit` | `retryAfterMs?`, `rateLimit?: {retryAfterMs?, limit?, remaining?, reset?}` | yes |
| `QuotaExceeded` | — | no |
| `ContentPolicy` | — | no |
| `ProviderInternal` | `retryAfterMs?` | yes |
| `Transport` | `transport: "http"|"websocket"`, `operation: "request"|"read"|"write"`, `code?`, `url?`, `phase?`, `delivery?: "not-sent"|"rejected"|"ambiguous"|"accepted"`, `recovery?: "retry-connect"|"retry-full"|"rotate-and-retry-full"|"fallback-http"|"fail"` | yes unless `delivery == "rejected"`, or `delivery == "accepted"` with operation ≠ `read` |
| `InvalidProviderOutput` | `classification?: "incomplete-stream"`, `route?` | only when `incomplete-stream` |
| `UnknownProvider` | — | yes |
| `Timeout` | `timeoutMs?` | no |

Response header `x-should-retry: true|false` overrides the table (`packages/ai/src/provider-error.ts:81-116`).
`ProviderConfigurationError {provider, message}` is thrown synchronously during model construction (not an AIError).

### 7.2 HTTP error classification (`packages/ai/src/route/executor.ts`, `packages/ai/src/provider-error.ts`)

On status ≥ 400 the body is read as text and passed to `httpFailure` (`executor.ts:100-142`):

* Message: `providerErrorMessage(body)` = first decodable of `{error:{message}}`, `{error:"<string>"}`, `{message}`,
  `{Message}` (AWS), `{detail}` (RFC 9457), `{errors:[{message}, ...]}` (non-blank strings only); else if the body is
  empty or starts with `<!doctype`/`<html` (case-insensitive) → `"Provider request failed with HTTP <status>"`; else
  `"Provider request failed with HTTP <status>: <body>"` truncated to 2000 chars + `"…"` (`executor.ts:87-98`).
* Headers lower-cased. `retryAfterMs`: `retry-after-ms` numeric (≥0) wins; else `retry-after` as seconds (×1000) or
  HTTP date (`date - now`, ≥0) (`executor.ts:23-36`). Rate-limit details collect `x-ratelimit-limit-<k>`,
  `x-ratelimit-remaining-<k>`, `x-ratelimit-reset-<k>` and `anthropic-ratelimit-<k>-(limit|remaining|reset)`.
* `classifyProviderFailure({message, status, rawBody, data, retryAfterMs, rateLimit, http})` (`provider-error.ts:235-305`), in order:
  1. Codes collected (lower-cased) from parsed `data`, parsed body, parsed message: `code`, `name` (when `errors` is an
     array), `error_type`, `error.code`, `error.type`, `error.status`, `error.error_type`, `error.details[].reason`,
     `error.innererror.code`, `error.metadata.error_type`, `response.error.code`, `response.error_type`,
     `exception.type`; plus a gateway label `^[^:\n]+: \[([A-Za-z0-9_.-]+)\]` in the message. Text = message + "\n" + body.
     `clientScoped` = status undefined or 4xx.
  2. clientScoped and (code ∈ {`context_length_exceeded`, `model_context_window_exceeded`, `request_too_large`} or
     `isContextOverflow(text)`) → `InvalidRequest` classification `context-overflow`.
  3. status 413 or payload pattern (`/request entity too large/i`, `/payload too large/i`, `/request too large/i`) →
     `InvalidRequest` `payload-too-large`.
  4. code ∈ {`content_filter`, `content_moderation`, `responsibleaipolicyviolation`, `content_policy_violation`,
     `image_content_policy_violation`, `refusal`} or (clientScoped and message matches
     `/violating our usage policy|blocked by content filtering policy|content[-_\s]?policy|rejected as a result of our safety system|detected potentially unsafe or sensitive content/i`) → `ContentPolicy`.
  5. status 402, or code ∈ {`insufficient_quota`, `usage_not_included`, `billing_error`, `gousagelimiterror`,
     `freeusagelimiterror`, `creditlimitexceeded`}, or (status 429 and text matches
     `/insufficient[-_\s]?(?:quota|balance)|quota[-_\s]?exceeded|budget exceeded|usage limit|limit exhausted|package has expired|plan does not yet include/i`) → `QuotaExceeded`.
  6. status 401/403, or code ∈ {`authentication_error`, `permission_error`, `permission_denied`, `api_key_invalid`},
     or (status 400 and `/incorrect api key provided/i`) → `Authentication`.
  7. status 429, code containing `rate_limit` or equal `too_many_requests`/`throttlingexception`, or text matches
     `/rate increased too quickly|rate[-_\s]?limit|too[_\s]?many[_\s]?requests/i` → `RateLimit` (with retryAfter/rateLimit).
  8. status 408/409/≥500, or (status undefined or <400) and ((no invalid-request code and text matches the server
     phrase regex at `provider-error.ts:177-178`) or code ∈ {`api_error`, `internal_error`, `internalserverexception`,
     `modelstreamerrorexception`, `overloaded_error`, `server_error`, `server_is_overloaded`, `slow_down`,
     `serviceunavailableexception`} or code contains `exhausted`/`unavailable`) → `ProviderInternal` (retryAfterMs).
  9. code ∈ {`model_not_found`, `invalid_prompt`, `invalid_request`, `invalid_request_error`, `validationexception`} → `InvalidRequest`.
  10. any other 4xx → `InvalidRequest`; else `UnknownProvider`.
* Context overflow text patterns (`provider-error.ts:16-68`), case-insensitive, unless an exclusion matches
  (`/^(throttling error|service unavailable):/i`, `/rate limit/i`, `/too many requests/i`, `/max[_ ]tokens must be less than/i`):
  `prompt is too long`; `input is too long for requested model`; `exceeded this model context window limit`;
  `exceeds the context window`; `exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))`;
  `input token count.*exceeds the maximum`; `number of input tokens exceeds maximum length`;
  `tokens in request more than max tokens allowed`; `maximum prompt length is \d+`; `reduce the length of the messages`;
  `requested input length \d+ exceeds maximum input length`; `maximum context length is \d+ tokens`;
  `exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?`; `input(?: \(\d+ tokens\))? is longer than the model'?s context length`;
  `exceeds the limit of \d+`; `exceeds the available context size`; `greater than the context length`;
  `` `inputs` tokens \+ `max_new_tokens` must be <= \d+ ``; `context window exceeds limit`; `exceeded model token limit`;
  `context[_ ]length[_ ]exceeded`; `context length is only \d+ tokens`; `input length.*exceeds.*context length`;
  `prompt (?:too long|超长)`; `too large for model with \d+ maximum context length`;
  `prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?`; `model_context_window_exceeded`;
  `range of input length should be`; `too many tokens`; `token limit exceeded`; `request_too_large`; or the whole
  message matching `/^4(?:00|13)\s*(status code)?\s*\(no body\)/i`.
* In-stream provider errors (Anthropic `error` event, OpenAI chat `error`, Responses `error`/`response.failed`,
  Bedrock exceptions, Gemini `error`) go through the same classifier with the event JSON as `rawBody` (sections 4.x).

### 7.3 Timeouts

| Timeout | Default | Source |
|---|---|---|
| Header timeout (time to response headers) | 300 000 ms | `HttpOptions.headerTimeout`; core sets from provider `settings.headerTimeout` (`packages/core/src/provider.ts:170-184`); `false` disables |
| Chunk/idle timeout between body chunks | 300 000 ms | `chunkTimeout` |
| Whole request | none | `timeout` |
| WebSocket connect | 15 s | `packages/core/src/session/model-transport.ts:23` (code `connect-timeout`) |
| WebSocket idle (no frame) | 30 min, or provider `chunkTimeout` | `model-transport.ts:25, 416-427` (code `idle-timeout`) |
| models.dev fetch | 10 s | 5.5 |
| Well-known fetch | 15 s | 6.5 |
| AI-SDK path | header timer aborts with `"Response headers timed out"`; SSE bodies wrapped so no SSE event within `chunkTimeout` aborts with `"SSE read timed out"`; `timeout` via `AbortSignal.timeout` (`packages/core/src/aisdk.ts:64-178`) | |

HTTP timeout errors: TransportError code `"Timeout"` with messages in 3.6. A Transport `Timeout` error counts toward
the timeout-retry cap (7.4).

### 7.4 Session retry policy (`packages/core/src/session/runner/retry.ts`, `step.ts`, `llm.ts`)

* Retry is decided per logical step (`runStep`). Eligible when `isRetryable(error)` (7.1), or output already started
  and the failure is an interrupted stream (InvalidProviderOutput `incomplete-stream`, or Transport `operation == "read"`).
  Context-overflow failures are never retried; they trigger compaction instead (7.5).
* Schedule (`retry.ts:42-56`): delay = `min(2 s × 2^(n-1), 10 s)` with Effect `jittered` (uniform factor 0.8-1.2),
  at most 10 retries (delays ≈ 2, 4, 8, 10, 10, ...). If the error is `RateLimit`/`ProviderInternal` with
  `retryAfterMs`, delay = `max(delay, min(retryAfterMs, 15 min))`; rounded up to whole ms.
* At most 3 retries for Transport `Timeout` errors (`MAX_TIMEOUT_RETRIES`).
* Plugin hook `session.retry` receives `{sessionID, agent, model, error, attempt, decision}` and may change the
  decision (e.g. ChatGPT plugin forbids retry for token-sharing errors); a non-finite/negative hook delay falls back
  to the computed delay.
* Before waiting, publish `SessionEvent.RetryScheduled {sessionID, assistantMessageID, attempt, at, error}`; then sleep
  until `at`.
* Outcomes (`step.ts:145-288`): failure before any output + retry → same assistant message is retried
  (`Outcome.Retry`); failure after output started + retry → `Outcome.Continue`: wait, publish a synthetic user text
  `"The previous response was interrupted. Continue from where you left off without repeating completed content."`
  (`llm.ts:35-36`), and continue with a new assistant message; WebSocket `retry-full`/`rotate-and-retry-full`
  rejections before output → `Outcome.RecoverFull` (resend in full once, no continuation checkpoint).
* A stream that completes with finish reason `unknown` is treated as InvalidProviderOutput
  `"The provider response ended with an unknown finish reason."` (incomplete-stream) → retry/continue.
* Unrecoverable failures are converted with `toSessionError` (`packages/core/src/session/to-session-error.ts`):
  `RateLimit→provider.rate-limit`, `Authentication→provider.auth`, `QuotaExceeded→provider.quota`,
  `ContentPolicy→provider.content-filter`, `Transport→provider.transport`, `ProviderInternal→provider.internal`,
  `InvalidProviderOutput→provider.invalid-output`, `InvalidRequest→provider.invalid-request`,
  `UnsupportedOperation→provider.unsupported-operation`, `NoRoute→provider.no-route`, `UnknownProvider→provider.unknown`,
  `Timeout→provider.timeout`; payload `{type, message, status?, response?: {body}}`; ChatGPT token-sharing error codes
  in the body are replaced with friendly messages (`to-session-error.ts:10-26`). Model resolution errors map to
  `provider.no-route`; integration authorization errors to `provider.auth`.
* No retries happen inside `packages/ai` itself (single attempt per call); AI-SDK `LanguageModelV3.doStream` is called
  directly so SDK-level retries are not used.

### 7.5 Context overflow handling

* During streaming, a `provider-error` event or failure classified `context-overflow` before any output is held back;
  if the runner's `recoverOverflow` (once per step) compacts successfully (`compaction.compact({reason:"overflow"})`),
  the step restarts with a new assistant message (`step.ts:107-171`, `llm.ts:262-289`); otherwise the error is
  published. Compaction treats both `context-overflow` and `payload-too-large` from its own summary request as
  shrink triggers (`packages/core/src/session/compaction.ts:428-435`, shrink factors `[0.7, 0.5, 0.35]`). (Compaction
  policy itself is specified in the session spec.)
* Default output budget per request (`packages/core/src/session/model-request.ts:43-98`): `maxTokens = min(limit.output || 32000, 256000)`
  (compaction requests additionally ≤ 32000), reduced to `max(1024, context − measured − ceil(estimated × 1.15))` when
  prompt size is known. Title/generate requests leave max tokens unset.

### 7.6 Session-level request decoration (`packages/core/src/session/model-request.ts:236-417`)

Every session model request adds HTTP headers: `x-opencode-session-id: <session id>`,
`x-opencode-parent-session-id` (child sessions), `x-session-affinity: <affinity>`, `X-Session-Id: <affinity>`,
`x-parent-session-id` (child), `User-Agent: opencode/<channel>/<version>/<name>`, `x-opencode-project: <project id>`,
`x-opencode-session: <affinity>`, `x-opencode-client: <app name>`. `promptCacheKey` = affinity with leading `ses_`
removed when it matches `/^ses_[0-9a-f]{64}$/`, else the affinity. Hook option keys that are `GenerationOptions`
fields become `generation`, all others `providerOptions`. Media the model cannot read (by `capabilities.input`
modality, or non png/jpeg/webp images for `xai`) is replaced with the text
`ERROR: Cannot read "<name>" (this model does not support <modality> input). Inform the user.`; if inline image bytes
exceed 25 MiB, images are replaced (oldest first) with the "image was removed" notice until ≤ 15 MiB. Hooks:
`session.model.request` (may change `baseURL` and headers), `http.request`/`http.response` (wrap every HTTP call),
`experimental.ws.handshake|send|receive` for WebSocket. WebSocket transport is used only for the durable runner's
primary requests when provider `settings.transport == "websocket"`; per session one socket is reused (rotated on
affinity change or age ≥ `rotateAfterMs`, default 55 min); any connect failure, 5 consecutive mid-stream failures, or a
`1009` (message too big) rejection pins the session to HTTP.

---

## 8. Non-chat modalities (brief)

`packages/ai` also contains media clients built on `route/media.ts` + `route/media-protocol.ts` and the polling
`Generation` helper (`packages/ai/src/generation.ts`: statuses `queued|running|completed|failed|cancelled|expired`,
default poll interval 5 s, poll timeout 10 min, `TimeoutError "Generation <id> did not finish within <duration>"`).
Usage for media is `MediaUsage` (`tokens|seconds|characters|credits|compute`). Protocols (not specified further here):

* Image: `openai-images`, `google-images`, `xai-images`, `meta-images`, `zai-images`, `bfl-images` (Black Forest Labs),
  `fal-images`, `replicate-images`, `stability-images`.
* Video: `google-video`, `xai-video`, `fal-video`, `runway-video`.
* Speech (TTS): `openai-speech`, `google-speech`, `elevenlabs-speech`, `deepgram-speech`, `cartesia-speech`.
* Transcription (STT): `openai-transcription`, `google-transcription`, `elevenlabs-transcription`,
  `deepgram-transcription`, `assemblyai-transcription`.
* Experimental: `experimental/evaluation*.ts`, `experimental/system-one.ts` (OpenRouter / OpenCode Zen evaluation
  endpoints), not used by chat.

---

## 9. Open questions

1. **AI-SDK bridge scope.** Providers whose package stays `aisdk:<npm>` (e.g. GitHub Copilot non-Messages models,
   GitLab, Perplexity, SAP AI Core, `@ai-sdk/vercel`, arbitrary user npm packages installed at runtime) execute through
   Vercel AI SDK v6 JS packages (`packages/core/src/aisdk.ts`, `packages/core/src/plugin/provider/dynamic.ts`). A Rust
   port cannot load npm packages; decide whether to (a) re-implement the commonly used ones natively (Copilot
   chat/responses fork with `reasoning_text`/`reasoning_opaque`, GitLab Duo), (b) drop dynamic npm providers, or (c)
   map them to openai-compatible. The Copilot fork (≈4.7k lines under `packages/core/src/github-copilot/chat`,
   `responses`) was only summarized here.
2. **Effect runtime details.** SSE parsing relies on Effect's `makeParser` (WHATWG semantics assumed: BOM handling,
   `id`, `retry` fields ignored); AWS event-stream decoding relies on `@smithy/eventstream-codec` header types; SigV4
   signing relies on `aws4fetch` (exact set of signed headers, e.g. whether `x-amz-content-sha256` is added). These
   should be verified against recorded fixtures (`packages/http-recorder`).
3. **Jitter.** The session retry uses Effect `Schedule.jittered` (assumed uniform 0.8×–1.2×, consistent with the
   comment "67–101s with jitter" at `packages/core/src/session/runner/retry.ts:42-44`).
4. **`openai-compatible` without an API key.** `AuthOptions.bearer(input, [])` fails with
   `"Missing auth credential: apiKey"` when no key exists; core avoids this only for `activation: "enabled"` providers
   via `Auth.none` (5.6) and for local providers by setting `apiKey: ""`. Confirm whether an empty-string key yields
   `Auth.none` or an error in practice (`Auth.optional("")` → MissingCredential; then `model-resolver` drops `apiKey: ""`
   (`model-resolver.ts:259-262`) and replaces auth with `Auth.none` only when the provider is `enabled`).
5. **OpenRouter PO spreading.** `bodyOptions` spreads every unrecognized provider option (e.g. `reasoningEffort`,
   `store`) verbatim into the OpenRouter body (`packages/ai/src/providers/openrouter.ts:138-162`). Intended? Replicate
   as-is for fixture parity.
6. **Partial JSON parser.** `tool-input-delta.input` uses a port of promplate partial-json plus a repair pass; exact
   parity for malformed partial input is likely untestable — confirm it is advisory only (UI preview) and not
   persisted.
7. **`ToolSchemaProjection.objectRoot` key order.** The projected schema places `type` first (`{type:"object", ...rest}`);
   JSON key order affects prompt-cache hits and recorded fixtures — the Rust port must preserve insertion order
   (use an order-preserving map such as `serde_json` with `preserve_order`).
8. **Models.dev snapshot.** `packages/core/src/models-dev/snapshot.txt` is empty in this checkout (0 bytes; generated
   artifact). The port needs a build step equivalent to `packages/core/script/update-models-snapshot.ts`.
9. **Gemini 2.5 and 3 detection** relies on model-id regexes (`/gemini-2[.-]5(?:[.-]|$)/i`, `/^gemini(?:-live)?-(\d+)/i`);
   behavior for vendor-prefixed ids (e.g. `google/gemini-3-pro` via gateways) differs between the two regexes
   (one anchored at `^`). Replicate exactly.
10. **Account tables.** `account`/`account_state`/`control_account` tables exist but no v2 code path in scope reads
    them for provider auth; confirm they are legacy-only.
11. **Anthropic route path `?beta=true`.** The native Anthropic route appends `?beta=true` to `/messages` only when
    `model.provider === "anthropic"` (`packages/ai/src/protocols/anthropic-messages.ts:1668`); custom providers
    reusing the Anthropic package with a different provider id (`anthropic-compatible`, Copilot Messages models)
    omit it. Confirm intended.
12. **Credential encryption.** Secrets are stored as plaintext JSON in SQLite; confirm no OS keychain integration is
    expected for the port.
