# oc-ai

`oc-ai` is the Rust port of opencode v2's provider-neutral LLM client (`packages/ai`). It also includes the
provider, catalog and credential parts of `packages/core`. The spec is `docs/spec/05-ai-providers.md`.

It covers:
- the normalized request, message and event model;
- protocol adapters, with exact request bodies and stream parsing;
- HTTP transport, with SSE and AWS event-stream framing, and the Open Responses WebSocket channel;
- provider factories;
- the models.dev catalog and model resolution;
- credentials and OAuth flows;
- error classification and retry helpers.

Retries are not performed inside a stream call; the session runner (core) drives them with
`oc_ai::retry`.

## Quick start (session runner)

```rust
use oc_ai::{catalog::*, Client, CancellationToken, LLMRequest, Message, LLMEvent};
use futures::StreamExt;

// 1. Build the catalog once (and again whenever config, credentials or models.dev change).
let models_dev = std::sync::Arc::new(ModelsDev::with_reqwest(
    ModelsDevOptions::from_env(oc_ai::credentials::user_agent("latest", "2.1.4", "opencode")),
    cache,                        // Arc<dyn CatalogCache> backed by the SQLite `kv` table
));
let snapshots = models_dev.get().await?;               // file → KV cache → embedded snapshot → network
let mut plugins = standard_plugins(&snapshots, configured_integrations, ConfigProviders::from_value(&cfg["providers"]));
let catalog = Catalog::build(&mut plugins, &access);   // Access: integration id → connection keys
// A built `Catalog` is immutable and shares its data: `clone()` is cheap and `ptr_eq` tells two
// handles of one build apart from a rebuild. Keep it and hand it out instead of rebuilding per call.

// 2. Resolve `providerID/modelID[#variant]` into a runnable model.
let resolved: ResolvedModel = catalog.resolve("anthropic", "claude-sonnet-4-5", Some("high"), credential.as_ref())?;
resolved.context_limit(); resolved.output_limit(); resolved.cost(); resolved.capabilities();

// 3. Stream.
let request = LLMRequest::new(resolved.language_model.clone())
    .system("You are opencode.")
    .message(Message::user_text("hello"));
let mut stream = Client::new().stream(request, CancellationToken::new()).await?;   // Err = HTTP/compile failure
while let Some(event) = stream.next().await {
    match event? {                       // Err(AIError) = mid-stream failure (classified)
        LLMEvent::TextDelta { text, .. } => print!("{text}"),
        LLMEvent::Finish { reason, usage, .. } => { /* exactly one terminal event */ }
        _ => {}
    }
}
```

`Client::generate(request, cancel)` folds the stream into an `LLMResponse`, which holds the assistant
`message`, the `events`, the `usage` and the `finish_reason`.

Per-call options are `StreamOptions { http, web_socket }` (`Client::stream_opts` / `generate_opts`;
`stream_with` / `generate_with` take only the HTTP hook).

## Modules

| Module | Contents |
|---|---|
| `types` (re-exported at the root) | `LLMRequest`, `Message`/`Role`, `ContentPart` (`Text`, `Media`, `ToolCall`, `ToolResult`, `Reasoning`, `Compaction`, `Effort`), `ToolDefinition`/`ToolNamespace`/`ToolEntry`, `ToolChoice`, `GenerationOptions`, `HttpOptions`/`HttpTimeout`, `CacheHint`/`CachePolicy`, `Compatibility`, `Usage`, `FinishReason`/`FinishReasonDetails`, `LLMEvent`. All types use serde with upstream JSON shapes (camelCase, `type` tags). |
| `media` | `MediaAsset`/`MediaSource` (bytes, base64, url, ref), `parse_data_url`, `detect_media_type`. |
| `error` | `AIError` (`kind: ErrorKind` with the upstream `_tag`s: `message`, `body`, `http`), `classify_provider_failure`, `is_context_overflow`, `http_status_message`, `retry_after_ms`, `rate_limit_details`, `ProviderConfigurationError`. `AIError::is_retryable()` is upstream `isRetryable`. |
| `compile` | Request pipeline: tool-history normalization, tool dedupe, schema projection (gemini/moonshot sanitizers), effort updates, cache policy, route headers. |
| `route` | `Route` (protocol × endpoint × auth × framing + defaults + optional WebSocket `channel`), `LanguageModel`, `Endpoint`, `auth` combinators (`bearer`, `header`, `bearer_header`, `scheme`, `remove`, `and_then`, `or_else`, `custom`, `bearer_options`), `Credential` (value, env, dynamic), `framing` (SSE, SSE+`[DONE]`, SSE event filter, NDJSON, document, AWS event stream with CRC check), `transport` (JSON POST, body overlay, header/chunk/total timeouts, `HttpMiddleware` hook), `websocket` (channel exchange types: `ChannelExecutor`, `ChannelExchange`, `ChannelDriver`, `ChannelObservation`, `ChannelCheckpoint`; the connector `open` / `open_with_env` / `DirectConnector`, `to_websocket_url`, WebSocket transport errors), `websocket::client` (the WebSocket client of Bun 1.4.4: CONNECT tunnel, upgrade request and validation, frames, permessage-deflate), `websocket::proxy` (proxy selection: upstream's constructor layer and Bun's own `NO_PROXY` matcher). |
| `protocols` | `Protocol` + `StreamParser` traits and the adapters listed below. |
| `providers` | `language_model(package, model_id, settings)` for every `@opencode/ai/providers/*` entrypoint, plus provider wrappers (Groq, OpenRouter, Moonshot/MiniMax chat, Vercel gateway, Bedrock Mantle, DigitalOcean). |
| `catalog` | models.dev fetch/cache/normalization, npm → native mapping, variants, custom providers from config, registry/availability/defaults, and `Catalog::resolve` → `ResolvedModel`. `catalog::copilot`: the GitHub Copilot `/models` decoding, `derive`/`build` and the `GithubCopilot` catalog plugin (core supplies the loaded state). |
| `js_date` | `parse`: JavaScript `Date.parse` (ES date-time format, then the legacy format as Bun/V8 parse it), for release dates parsed from free-form strings. |
| `aisdk` | The AI SDK route for `aisdk:` packages (`core/src/aisdk.ts`): `language_model` (route `ai-sdk:<package>`, metadata key, defaults), `call_options` (request → `LanguageModelV3CallOptions`), stream-part raising, `error_from_shape` (`llmError`), and the `AiSdkBridge` trait the JS side implements. |
| `credentials` | `CredentialStore` trait (+ `MemoryCredentialStore`), `Integrations` (methods, connections, refresh-on-resolve, OAuth/command attempt lifecycle), the OAuth flows, well-known discovery (`KvStore`), and the Azure CLI token helper. |
| `retry` | Session retry schedule and decision (`RetryPolicy`), `CONTINUE_TEXT`, `session_error_type`/`session_error`, ChatGPT token-sharing non-retry codes. |
| `modalities` | TODO list of the non-chat media protocols (not ported). |
| `testing` | Recorded-cassette loader and offline stream decoding helpers, used by the tests. |

## Protocols

| Protocol module | Routes / variants |
|---|---|
| `anthropic_messages` | `anthropic-messages`, `anthropic-compatible-messages`, Vertex messages (`vertex_protocol`), `alibaba_protocol`, `zai_protocol`, `meta_protocol`, Bedrock Mantle messages. The `anthropic-beta` header is merged by `beta_prepare_headers`. |
| `openai_chat` | `openai-chat`, `openai-compatible-chat`, and every chat-family provider (`protocol_with(ChatOptions { cache_control, assistant })`). |
| `openai_chat_variants` | `zai-chat`, `alibaba-chat`, `venice-chat` (encrypted-reasoning marker). |
| `open_responses` | Open Responses baseline + `openai-compatible-responses`; `Adapter` hooks (native tools, hosted tools, Meta). |
| `open_responses_channel` | The Responses WebSocket channel (`open-responses-channel.ts` + `open-responses-continuation.ts`): `ChannelOptions` (id, name, rotation age, `enabled`/`url`/`headers` endpoint hooks, continuation shape), the `response.create` message, the base driver (event order checks, error/failed frames → provider failures with the frame as body) and the continuation driver (checkpoint, incremental `previous_response_id` frames, `retry-full` / `rotate-and-retry-full` rejections), `xai_continuation`. |
| `openai_responses` | `openai-responses`: namespaces, image generation, hosted tools, effort updates, `context_management`. |
| `responses_variants` | `xai-responses`, `alibaba-responses`, `meta-responses`. |
| `gemini` | `gemini`, `google-vertex-gemini`; Vertex helpers; `gemini::adc` (Google ADC: authorized_user, service_account, metadata server). |
| `google_interactions` | `google-interactions`, `google-vertex-interactions`. |
| `bedrock_converse` | `bedrock-converse`, hand-written SigV4 (`sigv4`), AWS credential chain subset (`aws`). |
| `mistral_chat` | `mistral-chat`, Vertex Mistral. |
| `cohere_chat` | `cohere-chat` (native v2). |

## Traits that core implements

- `catalog::CatalogCache`: `get`/`set`/`remove` of the models.dev KV value `{updatedAt, digest, body}`. The key is
  `models-dev:catalog` (or `models-dev:catalog:<hash>` for a non-default source). `MemoryCache` is provided.
- `credentials::CredentialStore`: rows of `Credential { id, integration_id, label, value: CredentialValue, active }`
  in the `credential` table. The ordering and selection rules are exported as pure functions (`store::selected`,
  `store::list_order`, `store::plan_create`, `store::unique_label`).
- `credentials::KvStore`: `wellknown:*` keys.
- `route::transport::HttpMiddleware`: per-request header injection. Register it with `Client::with_middleware`.
  (Core's Copilot and Azure Entra hooks run as `HttpHook`s and WebSocket interceptors instead.)
- `route::transport::HttpHook`: per-call middleware around the HTTP exchange (upstream `StreamOptions.http`, which
  core builds from the `session.http.request` / `session.http.response` plugin hooks). Pass it to
  `Client::stream_with` / `generate_with`. The request hook sees a `WireRequest` (prepared headers incl.
  `content-length`, body); the response hook a `WireResponse` (status, empty status text, headers, body read in
  full, only when `wants_response()`), and the status check runs on its result.
- Every request carries Effect's trace propagation headers (`b3`, `traceparent`) from `oc_util::trace`: one trace
  ID per process, a fresh span and parent span per request, set after the `HttpHook` request hook.
- `catalog::PackageLoader`: optional override of the native package loader.
- `aisdk::AiSdkBridge`: the JS runtime behind `aisdk:` packages (`load(model)` = `AISDK.language`,
  `stream(call)` = `doStream`, parts as JSON). Pass it in `ResolveOptions.aisdk` (core: the oc-plugin
  sidecar, `oc_core::aisdk::LocationBridge`) and call `ResolvedModel::initialize().await` after
  resolving, which loads the package like upstream's resolver (`ModelInitializationError` phase
  `init` on failure). Without a bridge, `aisdk:` packages fail with `ResolveError::AisdkPackage`.
- `route::PreparedStream`: a route that streams without the HTTP transport (upstream
  `streamPrepared`); `Client::stream_with` compiles the request and hands it the protocol body.
- `route::websocket::ChannelExecutor`: carries a route's WebSocket exchange (upstream
  `StreamOptions.webSocket`, core's `SessionModelTransport.bind`, `oc_core::model_transport`).

## Responses WebSocket transport

Routes with a `channel`: `openai-responses` (also inherited by `azure-openai-responses` with Azure's own
options and by `cloudflare-ai-gateway-responses`) and `xai-responses`; Meta Responses stays on HTTP.
`Client::stream_opts` with `StreamOptions.web_socket` set and a route channel that offers the endpoint:

- the request is compiled as for HTTP; the WebSocket message is the JSON body (after the `http.body` overlay)
  without `stream`, `stream_options` and `background`, tagged `"type": "response.create"` (first key);
- the socket URL is the HTTP URL with `https:`→`wss:` / `http:`→`ws:` (Azure: only `https://*.openai.azure.com/.../openai/v1/responses?api-version=v1`, connected without `api-version`);
  handshake headers are the request headers after auth, without `content-type`/`content-length`, no tracing
  headers, plus `openai-beta: responses_websockets=2026-02-06` for OpenAI (not Azure/xAI);
- rotation age 55 min (OpenAI, Azure) / 24 min (xAI); xAI continues only stored responses
  (`store: false` → full frames) and without `instructions`;
- the exchange carries the same request as an HTTP fallback (with the `HttpHook` and trace headers);
  frames are parsed like SSE data; a successful end calls the execution's `complete` (commits the
  continuation checkpoint). A channel that does not offer the endpoint logs
  "<name> does not offer WebSocket for this endpoint; using HTTP" and uses HTTP.

Errors on the socket are `Transport` errors with `transport: "websocket"` and upstream's `code`, `phase`,
`delivery` and `recovery` (e.g. `WebSocket closed with code 1006` with an empty body, like Bun 1.4.4, for a socket
lost without a close frame; `Timed out waiting for WebSocket data`). Error frames classify like SSE error
events, with the raw frame as the body.

### The socket (`route::websocket::client`, `route::websocket::proxy`)

The connector replays what upstream's `WebSocketConstructor` layer and Bun 1.4.4's global `WebSocket` do
(compared against the upstream binary with a logging CONNECT proxy and a deflate-capable mock):

- **Proxy selection, upstream's layer** (`core/src/effect/websocket-constructor.ts`): none for the hosts
  `127.0.0.1` and `localhost` (`::1` never matches: the hostname is `[::1]`); none when `NO_PROXY` (else
  `no_proxy`) lists the host (comma/whitespace list; `*`; `host:port` only for that port, 443/80 by default; a
  leading `*` dropped; `.suffix` matches the end of the host name, anything else only the exact name); otherwise
  `WSS_PROXY` (`wss:`) / `WS_PROXY` (`ws:`), then `HTTPS_PROXY` / `HTTP_PROXY`, then `ALL_PROXY`, each upper case
  first, then lower case. A set but empty variable counts (an empty `WS_PROXY` means no proxy).
- **Bun's layer**: the proxy URL must be `http:` or `https:` (`Unsupported proxy protocol "socks5" (expected "http"
  or "https")`, `Invalid proxy URL: ...`: the constructor throws, the session falls back to HTTP). Bun then applies
  its own `NO_PROXY` matcher to the target (`no_proxy` first, then `NO_PROXY`, empty-ish values skipped; domains
  match themselves and subdomains, IP addresses and CIDR blocks, optional `:port`), so either layer can bypass.
  That matcher lives in `oc_util::proxy`, which also gives every HTTP client from `oc_util::http::client_builder`
  (including the HTTP fallback) the proxy rules of Bun's `fetch`: `http_proxy`/`HTTP_PROXY` or
  `https_proxy`/`HTTPS_PROXY` (lower case first), else an HTTP `ALL_PROXY`, unless that `NO_PROXY` matches.
- **Tunnel**: `CONNECT host:port HTTP/1.1`, `Host: host:port`, `Proxy-Connection: Keep-Alive`, and
  `Proxy-Authorization: Basic ...` from the proxy URL's percent-decoded credentials; any 2xx opens it (407 →
  `Proxy authentication required: 407 ...`, others `Proxy connection failed: ...`). An `https:` proxy is reached
  over TLS (same trust as the target); a `wss:` target gets its TLS inside the tunnel. Both `ws:` and `wss:` go
  through `CONNECT`.
- **Upgrade request**: `Host` (port only when not the default), `Connection: Upgrade`, `Upgrade: websocket`,
  `Sec-WebSocket-Version: 13`, `Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits`,
  `Sec-WebSocket-Key`, an `Authorization: Basic` from URL credentials, then the handshake headers the way Bun
  iterates a `Headers` object: WebKit's known names in their default case (`Authorization`, `User-Agent`, ...),
  the others as given, sorted by name in code point order.
- **Upgrade response**: 101 (else `Expected 101 status code: <status>`, with no `code`), `Connection` listing
  `upgrade`, `Upgrade: websocket`, the matching accept key, no `Sec-WebSocket-Protocol`, at most one valid
  `permessage-deflate` answer (`Unsupported websocket extension` otherwise). Connect failures read
  `Failed to open WebSocket: WebSocket connection to '<url>' failed: <reason>`.
- **permessage-deflate**: inbound RSV1 messages are inflated (context takeover unless
  `server_no_context_takeover`, reset after a final block); outbound text of 860 bytes or more is compressed (zlib
  level 6, the negotiated `client_max_window_bits` (at least 9), reset per message with
  `client_no_context_takeover`). Messages up to 128 MiB after inflation; pings are answered.
- Protocol errors (bad RSV bits, invalid compressed data, invalid UTF-8, oversized messages) close the socket
  with Bun's close code and fail the exchange with `WebSocket error: WebSocket connection to '<url>' failed:
  <reason>` (code `message`); a text frame over 16 MiB closes with 1009 `Message too large`
  (`message-too-large`).

## Error handling and retries

- Compile and HTTP-status failures are returned by `Client::stream`. Mid-stream failures arrive as the stream's
  `Err` item. Every failure is an `AIError`.
- The stream ends with exactly one terminal event, `Finish` or `ProviderError`. If it ends without one, the stream
  yields `InvalidProviderOutput` with message "The provider response ended unexpectedly." and classification
  `incomplete-stream`.
- Cancelling the token ends the stream with a `Transport` error whose code is `"Aborted"`.
- `retry::RetryPolicy::decide(&err, output_started)` gives the delay: `2s·2^(n-1)` capped at 10 s, ±20 % jitter,
  `Retry-After` honored up to 15 min. It retries at most 10 times, with at most 3 Transport-timeout retries.
- `err.is_context_overflow()` triggers compaction instead of a retry.

## Not ported / gaps

- **WebSocket client details**: a proxy's own TLS settings (`proxy.tls`, upstream never passes them), Bun's
  handshake timeout (core's 15 s connect timeout applies) and the exact compressed bytes (zlib-rs vs Bun's zlib,
  same format and parameters) are not replicated.
- **Explicit compaction endpoints** (`/responses/compact`, compaction trigger): `compaction` and `transport` are
  surfaced on `ResolvedModel` for core to decide.
- **Non-chat modalities**: image, video, speech and transcription (see `modalities`).
- **AWS credential sources**: SSO, web-identity and IMDS are not implemented. Supported: env, shared files /
  `credential_process`, and ECS container credentials.
- **Google ADC**: no `external_account` (workload identity federation).
