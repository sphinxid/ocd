# oc-ai

`oc-ai` is the Rust port of opencode v2's provider-neutral LLM client (`packages/ai`). It also includes the
provider, catalog and credential parts of `packages/core`. The spec is `docs/spec/05-ai-providers.md`.

It covers:
- the normalized request, message and event model;
- protocol adapters, with exact request bodies and stream parsing;
- HTTP transport, with SSE and AWS event-stream framing;
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
    ModelsDevOptions::from_env(oc_ai::credentials::user_agent("latest", "2.0.26", "opencode")),
    cache,                        // Arc<dyn CatalogCache> backed by the SQLite `kv` table
));
let snapshots = models_dev.get().await?;               // file → KV cache → embedded snapshot → network
let mut plugins = standard_plugins(&snapshots, configured_integrations, ConfigProviders::from_value(&cfg["providers"]));
let catalog = Catalog::build(&mut plugins, &access);   // Access: integration id → connection keys

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

## Modules

| Module | Contents |
|---|---|
| `types` (re-exported at the root) | `LLMRequest`, `Message`/`Role`, `ContentPart` (`Text`, `Media`, `ToolCall`, `ToolResult`, `Reasoning`, `Compaction`, `Effort`), `ToolDefinition`/`ToolNamespace`/`ToolEntry`, `ToolChoice`, `GenerationOptions`, `HttpOptions`/`HttpTimeout`, `CacheHint`/`CachePolicy`, `Compatibility`, `Usage`, `FinishReason`/`FinishReasonDetails`, `LLMEvent`. All types use serde with upstream JSON shapes (camelCase, `type` tags). |
| `media` | `MediaAsset`/`MediaSource` (bytes, base64, url, ref), `parse_data_url`, `detect_media_type`. |
| `error` | `AIError` (`kind: ErrorKind` with the upstream `_tag`s: `message`, `body`, `http`), `classify_provider_failure`, `is_context_overflow`, `http_status_message`, `retry_after_ms`, `rate_limit_details`, `ProviderConfigurationError`. `AIError::is_retryable()` is upstream `isRetryable`. |
| `compile` | Request pipeline: tool-history normalization, tool dedupe, schema projection (gemini/moonshot sanitizers), effort updates, cache policy, route headers. |
| `route` | `Route` (protocol × endpoint × auth × framing + defaults), `LanguageModel`, `Endpoint`, `auth` combinators (`bearer`, `header`, `bearer_header`, `scheme`, `remove`, `and_then`, `or_else`, `custom`, `bearer_options`), `Credential` (value, env, dynamic), `framing` (SSE, SSE+`[DONE]`, SSE event filter, NDJSON, document, AWS event stream with CRC check), `transport` (JSON POST, body overlay, header/chunk/total timeouts, `HttpMiddleware` hook). |
| `protocols` | `Protocol` + `StreamParser` traits and the adapters listed below. |
| `providers` | `language_model(package, model_id, settings)` for every `@opencode/ai/providers/*` entrypoint, plus provider wrappers (Groq, OpenRouter, Moonshot/MiniMax chat, Vercel gateway, Bedrock Mantle, DigitalOcean). |
| `catalog` | models.dev fetch/cache/normalization, npm → native mapping, variants, custom providers from config, registry/availability/defaults, and `Catalog::resolve` → `ResolvedModel`. |
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
- `route::transport::HttpMiddleware`: per-request header injection (Copilot, Azure Entra).
  Register it with `Client::with_middleware`.
- `route::transport::HttpHook`: per-call middleware around the HTTP exchange (upstream `StreamOptions.http`, which
  core builds from the `session.http.request` / `session.http.response` plugin hooks). Pass it to
  `Client::stream_with` / `generate_with`. The request hook sees a `WireRequest` (prepared headers incl.
  `content-length`, body); the response hook a `WireResponse` (status, empty status text, headers, body read in
  full, only when `wants_response()`), and the status check runs on its result.
- Every request carries Effect's trace propagation headers (`b3`, `traceparent`) from `oc_util::trace`: one trace
  ID per process, a fresh span and parent span per request, set after the `HttpHook` request hook.
- `catalog::PackageLoader`: optional override of the native package loader.

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

- **Responses WebSocket transport and continuation** (`open-responses-channel`/`continuation`): routes always use
  HTTP SSE, which is upstream's own fallback.
- **Explicit compaction endpoints** (`/responses/compact`, compaction trigger): `compaction` and `transport` are
  surfaced on `ResolvedModel` for core to decide.
- **AI-SDK bridge (`aisdk:*` packages)**: resolution returns `ResolveError::AisdkPackage`.
- **Non-chat modalities**: image, video, speech and transcription (see `modalities`).
- **AWS credential sources**: SSO, web-identity and IMDS are not implemented. Supported: env, shared files /
  `credential_process`, and ECS container credentials.
- **Google ADC**: no `external_account` (workload identity federation).
