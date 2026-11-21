//! Experimental client-hosted MCP (`server/handlers/mcp.ts` `mcp.clientToken` and `mcp.client`): a client opens a
//! WebSocket to `/api/experimental/client-mcp?name=<name>` and hosts an MCP server over it, one JSON-RPC message per
//! text frame. The server is the MCP client; the attached server's tools are offered to the location's sessions as
//! `<name>_<tool>` for as long as the socket stays open.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use futures::{SinkExt, StreamExt};

use crate::app::App;
use crate::auth::{TICKET_TTL_SECONDS, TicketScope};
use crate::backend::{McpClientAttachRequest, McpClientAttachment, McpClientControl, McpClientOutbound};
use crate::cors::is_allowed_request_origin;
use crate::error::ApiError;
use crate::generated::PtyTicketConnectToken;
use crate::pty::{close, ticket};
use crate::router::{Call, pattern_message, respond};

/// The client name pattern (`McpClientQuery.name`). It is not in the API document, so it is checked here.
const NAME_PATTERN: &str = "^[a-zA-Z0-9_-]+$";

fn query_issue(name: &str, detail: &str) -> ApiError {
    ApiError::schema("Query", format!("{detail}\n  at [\"{name}\"]"))
}

/// One value of a `Schema.String` query field: a repeated key decodes as an array, which is not a string.
fn single<'a>(call: &'a Call, key: &str) -> Result<Option<&'a str>, ApiError> {
    let mut values = call.query.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let first = values.next();
    if values.next().is_some() {
        return Err(query_issue(key, "Expected string"));
    }
    Ok(first)
}

/// `name`: required, `^[a-zA-Z0-9_-]+$`.
fn client_name(call: &Call) -> Result<String, ApiError> {
    let Some(name) = single(call, "name")? else { return Err(query_issue("name", "Missing key")) };
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return Err(query_issue("name", &pattern_message(NAME_PATTERN)));
    }
    Ok(name.to_string())
}

/// `POST /api/experimental/client-mcp/token`: a short-lived single-use ticket for opening a client MCP WebSocket from
/// a browser, which cannot set headers on the upgrade. The custom `x-opencode-ticket: 1` header forces a CORS
/// preflight, so cross-origin pages cannot mint tickets.
pub(crate) fn token(app: &App, call: &Call) -> Result<Response, ApiError> {
    let name = client_name(call)?;
    crate::handlers::check_ticket_request(app, call, "Invalid client MCP connect token request")?;
    let location = &call.ctx.location;
    let ticket = app.tickets.issue(TicketScope::mcp_client(
        &name,
        Some(location.directory.clone()),
        location.workspace_id.clone(),
    ));
    Ok(respond::location(&call.ctx, &PtyTicketConnectToken { ticket, expires_in: TICKET_TTL_SECONDS }))
}

/// Ends the attachment when the socket closes, or when the upgrade never completes.
struct Ended(Arc<dyn McpClientControl>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.ended();
    }
}

/// `GET /api/experimental/client-mcp`: checks the query, the origin and the ticket before it resolves the location
/// itself; a request without an upgrade then answers 400.
pub(crate) async fn connect(app: &App, call: Call, ws: Option<WebSocketUpgrade>) -> Result<Response, ApiError> {
    let name = client_name(&call)?;
    single(&call, "ticket")?;
    if !is_allowed_request_origin(&call.headers, &app.cors) {
        return Err(ApiError::Empty(StatusCode::FORBIDDEN));
    }
    let location = call.ctx.location.clone();
    if let Some(t) = ticket(&call) {
        let scope = TicketScope::mcp_client(&name, Some(location.directory.clone()), location.workspace_id.clone());
        if !app.tickets.consume(t, &scope) {
            return Err(ApiError::Empty(StatusCode::FORBIDDEN));
        }
    }
    app.check_location(&location)?;
    let Some(ws) = ws else { return Err(ApiError::Empty(StatusCode::BAD_REQUEST)) };
    let attachment = app.backend.experimental_mcp_client(&call.ctx, McpClientAttachRequest { name }).await?;
    let ended = Ended(attachment.control.clone());
    Ok(ws.on_upgrade(move |socket| run(socket, attachment, ended)))
}

/// Outbound frames flow through one stream drained by a single writer so the handshake, responses and the close frame
/// keep their order. Either side closing ends the connection, and every exit ends the attachment.
async fn run(socket: WebSocket, attachment: McpClientAttachment, ended: Ended) {
    let (mut sink, mut stream) = socket.split();
    let McpClientAttachment { mut output, control } = attachment;
    control.open();
    loop {
        tokio::select! {
            out = output.next() => match out {
                Some(McpClientOutbound::Text(text)) => if sink.send(Message::Text(text.into())).await.is_err() { break },
                Some(McpClientOutbound::Close { code, reason }) => { let _ = sink.send(close(code, &reason)).await; break }
                None => { let _ = sink.send(close(1000, "")).await; break }
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => control.deliver(text.as_str()),
                // Frames should be text, but binary frames are accepted and decoded as UTF-8.
                Some(Ok(Message::Binary(data))) => control.deliver(&String::from_utf8_lossy(&data)),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Send the queued close reply so the client sees its own code echoed.
                Some(Ok(Message::Close(_))) => { let _ = sink.flush().await; break }
                _ => break,
            },
        }
    }
    drop(ended);
}
