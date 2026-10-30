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
| `gemini` | `gemini`, `google-vertex-gemini`; Vertex helpers; `gemini::adc`: Google ADC as google-auth-library 10.5.0 resolves it (see below). |
| `google_interactions` | `google-interactions`, `google-vertex-interactions`. |
| `bedrock_converse` | `bedrock-converse`, hand-written SigV4 (`sigv4`), the AWS default credential chain (`aws`, see below). |
| `cloud_env` | The environment and SDK module state the AWS and Google chains read (`Env::process()`, or `Env::from_map` in tests). |

## Cloud credentials

**AWS** (`bedrock_converse::aws`): `default_chain` is `fromNodeProviderChain({ ignoreCache: true, profile })` of
`@aws-sdk/credential-providers` 3.1057.0, run afresh for every request, in the SDK's order: environment variables
(skipped when a profile is selected), the shared files for the profile (`ini`: smithy's INI parser, the credentials
file merged over the config file), `credential_process`, the web identity variables, then the container endpoint
(`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` / `_FULL_URI`, authorization token or token file) or EC2 instance metadata
(IMDSv2 token with the v1 fallback, `AWS_EC2_METADATA_DISABLED`, `AWS_EC2_METADATA_V1_DISABLED`,
`AWS_EC2_METADATA_SERVICE_ENDPOINT[_MODE]` and their config keys, 1 s timeouts, expired credentials extended). A
profile can hold static keys, `role_arn` with `source_profile` (chained) or `credential_source`
(`Environment` / `Ec2InstanceMetadata` / `EcsContainer`) for STS `AssumeRole` (`external_id`,
`role_session_name`, `duration_seconds`; `mfa_serial` fails as the SDK does without an MFA callback),
`web_identity_token_file` for `AssumeRoleWithWebIdentity`, `credential_process`, IAM Identity Center SSO
(`sso_session` with the `~/.aws/sso/cache` token refreshed through SSO-OIDC `CreateToken` inside 5 minutes of
expiry, at most every 30 s, or the legacy `sso_*` keys) and `login_session` (`aws login`, refreshed through the
sign-in service with a DPoP proof). STS, SSO, SSO-OIDC and sign-in calls follow the nested clients: endpoint rule
sets and partitions, `AWS_ENDPOINT_URL_<SERVICE>` / `AWS_ENDPOINT_URL` / `endpoint_url` / `[services]` overrides
(the tests point them at local mocks), FIPS and dual-stack switches, `AWS_MAX_ATTEMPTS` and standard retries, no
proxy. A provider error moves to the next provider; any other error ends the chain and becomes
`AWS default credential chain failed: <message>`.

**Google** (`gemini::adc`): `GOOGLE_APPLICATION_CREDENTIALS`, the well-known file (`$CLOUDSDK_CONFIG` as upstream's
`CloudSDKAuth` reads it, else `$HOME/.config/gcloud`), then the GCE metadata server (residency, `METADATA_SERVER_DETECTION`,
`GCE_METADATA_HOST`/`_IP`, cached ping). Types: `authorized_user`, `service_account` (gtoken's assertion to
`https://oauth2.googleapis.com/token`), `impersonated_service_account` (any source type, `delegates`, the source's
quota project), `external_account_authorized_user`, and `external_account` (`gemini::adc::external`): identity pool
file / URL (text or JSON, headers) / X.509 certificate (mTLS, `GOOGLE_API_CERTIFICATE_CONFIG`, trust chain), AWS
(region and role from the metadata URLs or the environment, signed `GetCallerIdentity` subject token) and executables
(`GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES`, output-file cache, timeouts), the STS exchange at `token_url` with the
library's metrics header, `workforce_pool_user_project`, client id/secret, `service_account_impersonation_url`
and `token_lifetime_seconds`. Tokens refresh 5 minutes before expiry; requests use gaxios' retries. `Adc::new(env,
Endpoints)` lets tests point the fixed Google URLs at local servers.
| `mistral_chat` | `mistral-chat`, Vertex Mistral. |
| `cohere_chat` | `cohere-chat` (native v2). |

## Traits that core implements

- `catalog::CatalogCache`: `get`/`set`/`remove` of the models.dev KV values: the body row `{updatedAt, digest, body}`
  under `models-dev:v2:catalog` (or `models-dev:v2:catalog:<hash>` for a non-default source) and the check row
  `{updatedAt, digest?, etag?}` under `<key>:meta`. The unversioned v1 rows (`models-dev:catalog[:<hash>][:meta]`)
  are removed before the service is first used, as 2.2 does at startup. `MemoryCache` is provided.
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

## models.dev catalog (2.2)

`catalog::models_dev` reads models.dev's v2 API (`<source>/experimental/v2.0/api.json?type=chat,decision,image`,
bundled as `assets/models-dev/snapshot.txt`): providers without `npm`, models with a `type`, MIME modalities, a
`capabilities.reasoning` object and an ordered `api` map. Each model is decoded on its own (a failure is skipped and
logged), routed to its first API with a loadable package (`CATALOG_PACKAGES`, upstream's full builtin set including
the image and decision entrypoints, for catalog listings only), priced from per-token text prices with context
overrides as tiers and service-tier multipliers for modes. The provider package and base URL are the most common ones
of its models. v1 catalogs (`OPENCODE_MODELS_PATH`, or a custom `OPENCODE_MODELS_URL` mirror that answers 404 for the
v2 path) are converted first (`models_dev_v1`). Refreshes revalidate with the stored `ETag` and treat a 304 or an
identical digest as unchanged. `tests/catalog_models_dev_upstream.rs` compares the normalization of the bundled
snapshot, the v1 fixture and edge cases with upstream's own `decodeSnapshot` (run with Bun on the 2.2.2 checkout).

`catalog::transforms::ProviderTransforms` holds the provider transforms of the Amazon Bedrock (ambient AWS
configuration enables the provider; `region` and `endpoint`), Google Vertex (project, location, `${GOOGLE_VERTEX_*}`
templates on providers and models, `eu`/`us` multi-region hosts), Cloudflare AI Gateway and Workers AI (account and
gateway IDs), Databricks (`DATABRICKS_HOST`) and GitLab (OAuth `instanceUrl`) plugins. `CredentialLogins` carries the
settings these plugins take from the active connection's stored credential; credential metadata no longer reaches
packages otherwise. `${AWS_REGION}` in a base URL takes a string `region` setting before the environment.

Model types: `ModelInfo.kind` (`type`), set from the catalog or config `type`. `Catalog` keeps non-chat models out of
every session-facing lookup and lists them only through `available_models_of(types)`.

## Wire changes of 2.2

- **Errors**: Bedrock's "Too many tokens[, per day], please wait" and any 429 or `ThrottlingException` never read as
  context overflow; AWS restJson1 error types come from `x-amzn-errortype`, `__type` and `code` (`ns#Name:uri` →
  `Name`); `budgetlimiterror` is a quota code, `datainspectionfailed` and `moderation_blocked` content-policy codes;
  `token_quota_exceeded` is a rate limit (the look-behind of upstream's quota regex is checked by hand).
- **OpenAI Chat**: the shared options `verbosity`, `service_tier`, `parallel_tool_calls` (only with tools), `metadata`
  and `safety_identifier` on every chat-family route (Groq no longer adds its own); unknown finish reasons map to
  `unknown`; text, refusal and reasoning after the finish reason keep streaming and the first finish reason sticks.
- **Reported costs**: `Usage.cost` from a finite, non-negative `usage.cost` (OpenAI Chat, Open Responses, Anthropic,
  Gemini; summed over Gemini continuations).
- **Video input**: Anthropic `video` blocks (base64, file id or URL), OpenAI Chat `video_url`, Open Responses
  `input_video` in user messages too, xAI `input_file`, Meta Messages/Responses move tool-result videos after the
  results, Gemini and Interactions send `http(s)` videos and `gs://` tool videos as file URIs. Tool-result files with any
  `scheme://` URI are URL assets; `file:` references fail with "Local file references must be read with
  Media.file(path) before sending". `detect_media_type` tells AVIF, HEIC, QuickTime and M4A apart by brand.
- **Options**: Anthropic `speed` (+ the `fast-mode-2026-02-01` beta); OpenAI Responses `reasoningMode` /
  `reasoningContext` (`pro` disables effort updates); xAI `top_k` and `min_p` without penalties; Bedrock Converse
  `effort` (per model family, adaptive thinking on Claude 4.6+), adaptive/disabled thinking, `serviceTier`,
  `guardrail`, `outputFormat` and `requestMetadata`, with every non-connection setting passed as a provider option;
  Kimi tool schemas keep literal values and schema maps verbatim and fold `prefixItems`/`items` tuples.
- **Routes**: `openrouter/responses`, `digitalocean/responses`, `venice/responses` (Open Responses, stateless
  encrypted reasoning by default).
- Google Interactions failures read their `errors[]` messages and classify by the first coded error.

`tests/wire_2_2.rs` ports upstream's tests for these and cross-checks with the 2.2.2 checkout when `OCD_SCRATCH` and
`OCD_BUN_PATH` are set: every recorded response of the corpus decodes to the same events as upstream's route, and a
set of requests with the new options compiles to the same URL, body text and headers.

## Not ported / gaps

- **Freeform (custom) tools** (`ToolDefinition.freeform`, `custom` tool calls on Chat and Responses): core never
  declares them, so they are not ported; `custom_tool_call` items in a stream are ignored.
- **Provider option validation messages**: strict provider options fail with "Invalid <route> provider option
  `<key>`" (xAI's `minP` uses upstream's Effect Schema text); upstream reports the Effect Schema message.

- **WebSocket client details**: a proxy's own TLS settings (`proxy.tls`, upstream never passes them), Bun's
  handshake timeout (core's 15 s connect timeout applies) and the exact compressed bytes (zlib-rs vs Bun's zlib,
  same format and parameters) are not replicated.
- **Explicit compaction endpoints** (`/responses/compact`, compaction trigger): `compaction` and `transport` are
  surfaced on `ResolvedModel` for core to decide.
- **Non-chat modalities**: image, video, speech and transcription (see `modalities`). Image and decision models are
  listed by the catalog but cannot be resolved yet (their entrypoints are not in `BUILTIN_PACKAGES`).
- **`fetch` in native settings**: 2.2's `Provider.nativeSettings` no longer omits `fetch`; it is never a JSON config
  value, so ocd ignores it.
- **AWS SDK details**: the standard retry strategy's retry quota and clock-skew correction, SDK user-agent
  headers, region validation (`Region not accepted`) and the FIPS / custom-endpoint conflict errors are not
  replicated. JavaScriptCore `JSON.parse` messages are reproduced for the common cases only. With `mfa_serial` the
  source credentials are not resolved before the MFA error (the SDK starts resolving them in the background).
- **Google ADC**: project discovery when no project is configured (`gcloud config config-helper`, the GCE project,
  an external account's Cloud Resource Manager lookup) is not run. GCE residency reads MAC addresses from
  `/sys/class/net` (Linux only). An executable's stdout and stderr are joined in that order, not interleaved.
