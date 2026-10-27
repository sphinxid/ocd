# oc-tunnel

A Rust OpenTunnel client, wire-compatible with `@opentunnel/client` 0.4.1 and `@opentunnel/protocol` 0.4.1 (MIT,
anomalyco/opentunnel), which upstream opencode 2.1.4 uses for `pair --remote` and `service set remote true`. ocd's
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
`{"_tag", "message"}` bodies. API calls carry `user-agent: opentunnel-sdk/0.4.1` and, as calls through Effect's HTTP
client, the `b3`/`traceparent` headers; the running tunnel's certificate check is a plain `fetch` (Bun's user agent,
no trace headers).

**Certificate.** An ECDSA P-256 key generated on the device and a PKCS#10 request with CN = hostname and the SANs
hostname and `*.hostname`, signed with ecdsa-with-SHA256 (`csr.rs`, the same DER as the SDK's hand-written encoder).
The client polls every 2 s until `ready` (`Certificate issuance failed: <reason>` on `failed`). `ensure` re-reads the
certificate on every connect: a different `ready` certificate (renewed while offline) is saved, `failed` provisions a
new key for the same tunnel. A running tunnel checks again at start and every 12 h, or every minute within 30 days of
expiry, and uses a renewed certificate for new connections.

**Bridge** (`bridge.rs`, `tunnel.rs`, `ws.rs`). A WebSocket to `/api/tunnel/:id/connect` (wss for an https API) with
subprotocol `opentunnel` and the SDK user agent. Text frames carry JSON control messages, binary frames carry data as
a big-endian `u32` connection id followed by at most 32 KiB of payload. The client sends
`{"type":"attach","token","transport":"ws","routes":[sorted names],"client":{"version":"0.1.0","max_conns":256}}`
and the relay answers `attached {session, routes, heartbeat_ms, idle_timeout_ms}` or `attach_error {code}`. Then the
relay sends `open {conn, peer, sni, alpn}` for each public connection, `end {conn}` (half-close), `reset {conn, code}`,
`ping {time_sent}` (answered with `pong`) and `drain {reason}`; the client sends data, `end`, `reset`, and a `ping`
every `max(heartbeat_ms, 1 s)`, giving up when nothing arrived for `max(idle_timeout_ms, 5 s)`. Messages that do not
decode (including an `open` without `alpn`) are ignored.

**Connections.** The route is the SNI label left of the tunnel hostname (`@` for the hostname itself); an unknown
route or a full table (256) is reset before any TLS state is created. Otherwise TLS terminates here with the tunnel's
certificate (rustls), and the plaintext goes to the route's `host:port` target; failures reset with
`upstream_connect_failed` or `upstream_io_error`. Reading a local socket pauses while more than 1 MiB waits on the
WebSocket (until it drops to 256 KiB), and a connection whose local side stops reading for 8 MiB is reset.

**Reconnects.** Any end other than a fatal attach error (`bad_token`, `cert_not_ready`) or shutdown reconnects after
`round(b/2 + random * b/2)` ms with `b = min(250 * 2^min(attempt, 16), 30000)`; an attached session resets the
attempt count. The reasons are the SDK's: `bridge closed: <code> <reason>` (1006 for a failed upgrade or a dropped
connection), `bridge attach timed out` (10 s), `bridge idle timeout`, `attach failed: <code>`,
`server draining: <reason>`. `connect` resolves on the first attach and fails only on a fatal attach error.

The SDK depends on nothing a native client cannot do, so this is a full port rather than the SDK running in the Bun
sidecar.

## Tests

`cargo test -p oc-tunnel` runs everything against `fake.rs` (feature `fake`, compiled for tests), a local stand-in
for the service: the HTTP API, a CA that checks and signs the request, the bridge, and a public TLS listener on
127.0.0.1 that routes by SNI. The `fake_relay` and `tunnel_probe` examples run the same fake and client from the
command line, which is how the client was compared with the real SDK under Bun 1.4.4 (same requests, messages,
events and files, and identities usable by either side).
