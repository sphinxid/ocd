# oc-tunnel

A Rust OpenTunnel client, wire-compatible with `@opentunnel/client` 0.6.1 and `@opentunnel/protocol` 0.6.1 (MIT,
anomalyco/opentunnel), which upstream opencode 2.2.2 uses for `pair --remote` and `service set remote true`. ocd's
side of that feature is in `crates/ocd/src/remote_tunnel.rs`.

OpenTunnel is a "blind TLS" tunnel: the device holds the private key and certificate for its tunnel hostname, the
relay routes public TLS connections by SNI and forwards the encrypted bytes over one WebSocket, and TLS terminates on
the device. The relay never sees plaintext or the key.

## Protocol (design note)

**Identity and storage.** One tunnel per profile (`default`), kept in `$XDG_DATA_HOME/opentunnel/<profile>/`
(`~/.local/share/opentunnel/...` when the variable is unset; on Windows too) and shared by the opentunnel CLI, the SDK
and every app using it: `tunnel.json` (`{"id", "hostname", "certificateExpiry"}`, two-space JSON, no trailing newline),
`token` (with a newline), `private-key.pem` (PKCS#8), `certificate.pem`, `chain.pem`. While the certificate is being
issued, `pending.json` (`{"id", "hostname", "csr"}`) replaces `tunnel.json`; saving the identity removes it. Files
are created 0600, directories 0700. `storage.rs`.

**HTTP API** (`api.rs`). `POST /api/tunnel` `{}` creates a tunnel (201 `{tunnel: {id, hostname, state}, token}`); the
hostname is assigned by the service. `POST /api/tunnel/:id/certificate` `{csr}` (bearer token) starts issuance (202;
409 `CertificateInProgressError` when one is already running, which a resumed issuance ignores);
`GET /api/tunnel/:id/certificate` returns `{id, state}` with `state` one of `challenge`, `issuing`,
`ready {certificate, chain, expiry}` or `failed {reason}`; `DELETE /api/tunnel/:id` removes it. Errors are
`{"_tag", "message"}` bodies; only the errors an endpoint declares for that status decode (with their required
fields), anything else is an HTTP client error described by its status. API calls carry
`user-agent: opentunnel-sdk/0.6.1` and, as calls through Effect's HTTP client, the `b3`/`traceparent` headers; the
running tunnel's certificate check is a plain `fetch` (Bun's user agent, no trace headers, no retries).

**Retries** (`retry.rs`, `@opentunnel/protocol/api-retry`). A failed API call is sent again up to 6 times after
`min + random * (max - min)` ms, with `[d/2, d]` and `d = min(250 * 2^attempt, 8000)`, or exactly a `Retry-After` in
seconds (at most 8 s). Reads and the CSR bind (`GET`, `POST .../certificate`) retry on any network error, 429 and 5xx;
other calls only on a connection failure that never sent the request and on 429/503/521/522/523/525.

**Errors.** A failure's message is the step plus what went wrong (`describeApiError`): `Failed to create tunnel:
ServiceUnavailableError: <message>`, `Failed to read certificate: HTTP 409`, `Failed to create tunnel: Cloudflare 522:
the connection to the server timed out (HTTP 522)`, `... : Transport error (POST <url>)`, `Failed to connect tunnel:
Bridge attach failed: bad_token` (with the attach `code`), `Tunnel stopped: ...`. Invalid routes repeat the message
(`Invalid route name 'X': Invalid route name 'X'`), as the SDK does. `ensure` reports `checking-certificate` and
`ready`, and keeps a stored certificate that has not expired when the API is unreachable, rate limited or failing on
the server side (after the retries).

**Certificate.** An ECDSA P-256 key generated on the device and a PKCS#10 request with CN = hostname and the SANs
hostname and `*.hostname`, signed with ecdsa-with-SHA256 (`csr.rs`, the same DER as the SDK's hand-written encoder).
The client polls every 2 s until `ready` (`Certificate issuance failed: <reason>` on `failed`). `ensure` re-reads the
certificate on every connect: a different `ready` certificate (renewed while offline) is saved, `failed` provisions a
new key for the same tunnel. A running tunnel checks again at start and every 12 h, or every minute within 30 days of
expiry, and uses a renewed certificate for new connections; closing it aborts a check in flight.

**Bridge** (`bridge.rs`, `tunnel.rs`, `ws.rs`). A WebSocket to `/api/tunnel/:id/connect` (wss for an https API) with
subprotocol `opentunnel` and the SDK user agent. Text frames carry JSON control messages, binary frames carry data as
a big-endian `u32` connection id followed by at most 32 KiB of payload. The client sends
`{"type":"attach","token","transport":"ws","routes":[sorted names],"client":{"version":"0.1.0","max_conns":256}}`
and the relay answers `attached {session, routes, heartbeat_ms, idle_timeout_ms}` or `attach_error {code}`. Then the
relay sends `open {conn, peer, sni, alpn}` for each public connection, `end {conn}` (half-close), `reset {conn, code}`,
`ping {time_sent}` (answered with `pong`) and `drain {reason}`; the client sends data, `end`, `reset`, and a `ping`
every `max(heartbeat_ms, 1 s)`, giving up when nothing arrived for `max(idle_timeout_ms, 5 s)`. Messages that do not
decode (including an `open` without `alpn`) are ignored.

**Routes and connections** (`routes.rs`, `proxy_protocol.rs`). A route maps a name (`@` or a DNS label) to a
`host:port` target, optionally with `proxy_protocol` `"v1"` or `"v2"`; `Connection::urls` gives each route's
`https://<route>.<hostname>`. The route of a connection is the SNI label left of the tunnel hostname (`@` for the
hostname itself); an unknown route or a full table (256) is reset before any TLS state is created. Otherwise TLS
terminates here with the tunnel's certificate (rustls), and the plaintext goes to the route's target, after a PROXY
protocol header with the visitor's address (destination the unspecified address, port 443; v2 carries the SNI in a
`PP2_TYPE_AUTHORITY` TLV) when the route asks for one; failures reset with `upstream_connect_failed` or
`upstream_io_error`. Reading a local socket pauses while more than 1 MiB waits on the WebSocket (until it drops to
256 KiB), and a connection whose local side stops reading for 8 MiB is reset.

**Reconnects.** Any end other than a fatal error or shutdown reconnects after `round(b/2 + random * b/2)` ms with
`b = min(250 * 2^min(attempt, 16), 30000)`; an attached session resets the attempt count. Fatal: the attach errors
`bad_token` and `cert_not_ready`, `route_conflict` when connected with `OnRouteConflict::Fail` (a `route-conflict`
event with the claimed routes comes first either way; the message adds `(another client holds a route)`), and a proxy
the bridge cannot use. The retry reasons are the SDK's: `bridge closed: <code> <reason>` (1006 for a failed upgrade
or a dropped connection), `bridge attach timed out` (10 s), `bridge idle timeout`, `attach failed: <code>`,
`server draining: <reason>`. `connect` resolves on the first attach and fails only on a fatal error.

## Proxies (`proxy.rs`)

As the SDK behaves in upstream's runtime (Bun 1.4.4), with no `proxy` option (opencode passes none):

- **API calls and the certificate check** go through `fetch`, which picks the proxy with Bun's rules
  (`oc_util::proxy`: `http_proxy`/`HTTP_PROXY` or `https_proxy`/`HTTPS_PROXY` by the API's scheme, lower case first,
  then an HTTP `ALL_PROXY`; Bun's `NO_PROXY` matching, `no_proxy` first). An `http:` API is reached with
  absolute-form requests carrying `Proxy-Connection: Keep-Alive` (as Bun's), an `https:` one through reqwest's
  `CONNECT`. A proxy `fetch` cannot use (`socks5://`) or a value that
  is not a URL fails the request as a network error (`Transport error (...)`), which is retried as above and is not
  fatal: without a stored tunnel `connect` fails with `Failed to create tunnel: Transport error (POST <api>/api/tunnel)`.
- **The bridge WebSocket**: the SDK picks the proxy itself (`proxyFromEnv`): `HTTPS_PROXY`, `https_proxy`,
  `ALL_PROXY`, `all_proxy` for `wss:` (the `HTTP_` ones for `ws:`), the first non-blank one, upper case first; none
  when `NO_PROXY` (else `no_proxy`) exempts the host with the SDK's own rules (`bypassesProxy`: comma or space
  separated `*`, domains covering their subdomains with an optional leading `.` or `*.`, IP addresses and CIDR ranges
  compared within the same family, each optionally `:port` or `[ipv6]:port`; a CIDR prefix out of range fails with
  Node's `ERR_OUT_OF_RANGE` text). The value is parsed with `parseProxy` (no scheme means `http://`), and then Bun's
  WebSocket skips the proxy when Bun's `NO_PROXY` rules exempt the host. It connects with
  `CONNECT host:port` (`Host`, `Proxy-Connection: Keep-Alive`, `Proxy-Authorization: Basic` with the decoded
  credentials; TLS to an `https://` proxy) and runs the TLS and WebSocket handshakes through the tunnel; a refused
  `CONNECT` is a `bridge closed: 1006`.
- **Unusable proxies stop the tunnel.** A selected proxy that is not `http://`/`https://` or not a URL fails the
  session before connecting (`connecting` then `stopped {error}`), and `connect` fails with
  `Failed to connect tunnel: HTTPS_PROXY uses an unsupported proxy scheme 'socks5': OpenTunnel connects through
  http:// and https:// proxies` or `Failed to connect tunnel: HTTPS_PROXY is not a valid proxy URL` (the variable's
  name; never the URL). `Error::is_fatal` matches `/unsupported proxy scheme|not a valid proxy URL/` as opencode's
  `services/remote-tunnel.ts` does, so the remote tunnel stops instead of retrying. With a stored certificate that has
  not expired, the unreachable API is skipped first (after its retries), so this is how a bad proxy shows up.
- **The `proxy` option** (`Client::with_options`, `ClientOptions::proxy`) replaces the environment's proxy for all
  three; Bun's `NO_PROXY` still exempts hosts from it. An unusable one fails when the client is made
  (`The proxy option uses an unsupported proxy scheme ...`). `ClientOptions::env` replaces the process environment
  (tests).

The SDK depends on nothing a native client cannot do, so this is a full port rather than the SDK running in the Bun
sidecar.

## Tests

`cargo test -p oc-tunnel` runs everything against `fake.rs` (feature `fake`, compiled for tests), a local stand-in
for the service: the HTTP API, a CA that checks and signs the request, the bridge, and a public TLS listener on
127.0.0.1 that routes by SNI; proxy tests add a recording HTTP proxy on 127.0.0.1 and pass the proxy variables to the
client explicitly (never through the process environment). `proxy/tests.rs` holds 106 vectors of `bypassesProxy`,
`parseProxy` and `proxyFromEnv` as the 0.6.1 SDK's `proxy.js` returned them under Bun 1.4.4. The `fake_relay` and
`tunnel_probe` examples run the same fake and client from the command line, which is how the client was compared with
the real 0.6.1 SDK under Bun 1.4.4 through a local proxy: the same events, messages, progress, request counts
(retries) and proxy requests, and identities usable by either side.

## Differences

- Header names and order on the wire come from reqwest/hyper (`user-agent`, lower case) rather than Bun's (`User-Agent`
  first); values are the same. A certificate check and the bridge upgrade start together, so their order varies.
- `ApiError` for an undecodable success body is a `SchemaError` with serde's message rather than Effect's schema
  issue text.
- Routes are typed (`Route { target, proxy_protocol }`): the SDK's checks for unknown route keys and a missing
  target cannot occur; a route without a PROXY protocol serializes as its `host:port` string.
- The SDK's runtime switches for Node (undici) and Deno do not apply.
