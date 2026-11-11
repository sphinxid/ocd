//! `machine.ts` (2.2.7): the browser plugin's attachments as a pure state machine. The driver in
//! [`super::Browsers`] feeds it one event at a time and performs the effects it returns, in order.
//!
//! Each session has at most one attachment. Attaching first grants the session the browser rules
//! (a permission sync); only once that write settled does the desktop receive `attached` and may commands
//! run. Detaching (or a move, a deletion, the plugin's disposal) revokes them. A sync is never interleaved
//! with another one of the same session: a grant waits for a revoke still in flight, and a grant that
//! settles after its attachment closed is revoked again.

use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::BrowserResult;

/// How long one permission sync (`session.get` and, when needed, `session.update`) may take.
pub const PERMISSION_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// The one permission sync in flight for a session: grant (`allow`) or revoke the browser rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionSync {
    pub attempt: u64,
    pub allow: bool,
}

/// One session's attachment.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    /// The desktop called `attach` as `attempt`; its grant runs after any revoke in flight (`sync`).
    Attaching { attempt: u64, connection_id: String, sync: PermissionSync },
    /// The session allows the browser and the desktop received `attached`; commands run.
    Attached { attempt: u64, connection_id: String, browser_state: Value, pending: Vec<(String, Value)> },
    /// No attachment; `sync` settles, then a revoke follows unless the session was deleted.
    Revoking { sync: PermissionSync, deleted: bool },
}

/// Every attachment of the plugin. Once `disposed`, only revokes still draining remain.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionState {
    pub disposed: bool,
    pub next_attempt: u64,
    pub sessions: IndexMap<String, SessionState>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        ConnectionState { disposed: false, next_attempt: 1, sessions: IndexMap::new() }
    }
}

/// How an `attach` call ends: its result, or `unavailable` when the grant failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Closed,
    Replaced,
    Unavailable,
}

/// How a permission sync ended.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncResult {
    Synced,
    Failed(String),
    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `attempt` is the state's `next_attempt`, read when the driver registered the attachment's handle.
    AttachStarted {
        session: String,
        attempt: u64,
        connection: String,
    },
    /// The `attach` call ended on the desktop's side (disconnect or interrupt).
    AttachClosed {
        session: String,
        attempt: u64,
    },
    Permissions {
        session: String,
        attempt: u64,
        result: SyncResult,
    },
    StateUpdated {
        session: String,
        connection: String,
        state: Value,
    },
    RequestStarted {
        session: String,
        connection: String,
        request: String,
        command: Value,
    },
    RequestSettled {
        session: String,
        connection: String,
        request: String,
        outcome: Result<BrowserResult, String>,
    },
    RequestTimedOut {
        session: String,
        connection: String,
        request: String,
    },
    /// The tool call that started the request was interrupted.
    RequestCancelled {
        session: String,
        connection: String,
        request: String,
    },
    SessionMoved {
        session: String,
    },
    SessionDeleted {
        session: String,
    },
    Disposed,
}

impl Event {
    fn session(&self) -> &str {
        match self {
            Event::AttachStarted { session, .. }
            | Event::AttachClosed { session, .. }
            | Event::Permissions { session, .. }
            | Event::StateUpdated { session, .. }
            | Event::RequestStarted { session, .. }
            | Event::RequestSettled { session, .. }
            | Event::RequestTimedOut { session, .. }
            | Event::RequestCancelled { session, .. }
            | Event::SessionMoved { session }
            | Event::SessionDeleted { session } => session,
            Event::Disposed => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    SyncPermissions {
        session: String,
        attempt: u64,
        allow: bool,
    },
    /// Disposes the attachment's tunnels and ends its `attach` call with `outcome`.
    CloseAttachment {
        attempt: u64,
        outcome: Outcome,
    },
    /// A `rpc.experimental.browser.control` event.
    EmitControl(Value),
    SettleRequest {
        request: String,
        settlement: Result<BrowserResult, String>,
    },
    Warn {
        message: String,
        error: Option<String>,
    },
    /// The disposed plugin has no revoke left in flight.
    NotifyDrained,
}

struct Step {
    entry: Option<SessionState>,
    next_attempt: u64,
    effects: Vec<Effect>,
}

/// `transitionConnection`: applies `event` to `state` and returns the effects to perform, in order.
pub fn transition(state: &mut ConnectionState, event: Event) -> Vec<Effect> {
    if event == Event::Disposed {
        return dispose(state);
    }
    // A request whose attachment closed or was replaced after the tool picked it was never sent.
    if let Event::RequestStarted { session, connection, request, .. } = &event
        && !is_attached_connection(state, session, connection)
    {
        return vec![Effect::SettleRequest { request: request.clone(), settlement: Err(disconnected_before_dispatch()) }];
    }
    let session = event.session().to_string();
    let entry = state.sessions.get(&session).cloned();
    let step = if state.disposed {
        match entry {
            Some(SessionState::Revoking { sync, deleted }) => transition_revoking(sync, deleted, &event, state.next_attempt),
            _ => None,
        }
    } else {
        transition_session(entry, &event, state.next_attempt)
    };
    let Some(step) = step else { return vec![] };
    match step.entry {
        Some(entry) => {
            state.sessions.insert(session, entry);
        }
        None => {
            state.sessions.shift_remove(&session);
        }
    }
    state.next_attempt = step.next_attempt;
    let mut effects = step.effects;
    if state.disposed && state.sessions.is_empty() {
        effects.push(Effect::NotifyDrained);
    }
    effects
}

fn transition_session(entry: Option<SessionState>, event: &Event, next: u64) -> Option<Step> {
    if let Event::AttachStarted { session, attempt, connection } = event {
        return attach(entry, session, *attempt, connection, next);
    }
    match entry? {
        SessionState::Revoking { sync, deleted } => transition_revoking(sync, deleted, event, next),
        SessionState::Attaching { attempt, connection_id, sync } => transition_attaching(attempt, connection_id, sync, event, next),
        SessionState::Attached { attempt, connection_id, browser_state, pending } => {
            transition_attached(attempt, connection_id, browser_state, pending, event, next)
        }
    }
}

fn attach(entry: Option<SessionState>, session: &str, attempt: u64, connection: &str, next: u64) -> Option<Step> {
    if attempt != next {
        return None;
    }
    let attaching = |sync| SessionState::Attaching { attempt, connection_id: connection.to_string(), sync };
    match entry {
        // A sync already in flight finishes first; the grant waits for it, so two writes never interleave.
        Some(SessionState::Attaching { attempt: previous, sync, .. }) => Some(Step {
            entry: Some(attaching(sync)),
            next_attempt: next + 1,
            effects: vec![Effect::CloseAttachment { attempt: previous, outcome: Outcome::Replaced }],
        }),
        Some(SessionState::Revoking { sync, .. }) => Some(Step { entry: Some(attaching(sync)), next_attempt: next + 1, effects: vec![] }),
        other => {
            let sync = PermissionSync { attempt: next + 1, allow: true };
            let mut effects = Vec::new();
            if let Some(SessionState::Attached { attempt: previous, pending, .. }) = &other {
                effects.extend(fail_pending(pending));
                effects.push(Effect::CloseAttachment { attempt: *previous, outcome: Outcome::Replaced });
            }
            effects.push(Effect::SyncPermissions { session: session.to_string(), attempt: sync.attempt, allow: true });
            Some(Step { entry: Some(attaching(sync)), next_attempt: next + 2, effects })
        }
    }
}

fn transition_attaching(attempt: u64, connection_id: String, sync: PermissionSync, event: &Event, next: u64) -> Option<Step> {
    match event {
        Event::Permissions { session, attempt: synced, result } => {
            if *synced != sync.attempt {
                return None;
            }
            let mut effects = sync_warning(result, sync);
            // The revoke that ran before this attachment settled; now grant.
            if !sync.allow {
                let sync = PermissionSync { attempt: next, allow: true };
                effects.push(Effect::SyncPermissions { session: session.clone(), attempt: sync.attempt, allow: true });
                return Some(Step {
                    entry: Some(SessionState::Attaching { attempt, connection_id, sync }),
                    next_attempt: next + 1,
                    effects,
                });
            }
            if *result == SyncResult::Synced {
                let control = json!({ "type": "attached", "connectionID": connection_id, "version": super::VERSION });
                return Some(Step {
                    entry: Some(SessionState::Attached {
                        attempt,
                        connection_id,
                        browser_state: json!({ "tabs": [], "focusedTabID": null }),
                        pending: Vec::new(),
                    }),
                    next_attempt: next,
                    effects: vec![Effect::EmitControl(control)],
                });
            }
            // A grant that timed out may still have written its rules; revoke them.
            let revoke = PermissionSync { attempt: next, allow: false };
            effects.push(Effect::CloseAttachment { attempt, outcome: Outcome::Unavailable });
            effects.push(Effect::SyncPermissions { session: session.clone(), attempt: revoke.attempt, allow: false });
            Some(Step { entry: Some(SessionState::Revoking { sync: revoke, deleted: false }), next_attempt: next + 1, effects })
        }
        Event::AttachClosed { attempt: closed, .. } if *closed == attempt => closing(attempt, sync, false, next),
        Event::SessionMoved { .. } => closing(attempt, sync, false, next),
        Event::SessionDeleted { .. } => closing(attempt, sync, true, next),
        _ => None,
    }
}

fn closing(attempt: u64, sync: PermissionSync, deleted: bool, next: u64) -> Option<Step> {
    Some(Step {
        entry: Some(SessionState::Revoking { sync, deleted }),
        next_attempt: next,
        effects: vec![Effect::CloseAttachment { attempt, outcome: Outcome::Closed }],
    })
}

fn transition_attached(
    attempt: u64,
    connection_id: String,
    browser_state: Value,
    mut pending: Vec<(String, Value)>,
    event: &Event,
    next: u64,
) -> Option<Step> {
    let session_of = |s: &str| s.to_string();
    match event {
        Event::StateUpdated { connection, state, .. } => {
            if *connection != connection_id || *state == browser_state {
                return None;
            }
            Some(Step {
                entry: Some(SessionState::Attached { attempt, connection_id, browser_state: state.clone(), pending }),
                next_attempt: next,
                effects: vec![],
            })
        }
        Event::RequestStarted { request, command, .. } => {
            if pending.iter().any(|(id, _)| id == request) {
                return None;
            }
            pending.push((request.clone(), command.clone()));
            let control = json!({ "type": "command", "connectionID": connection_id, "requestID": request });
            Some(Step {
                entry: Some(SessionState::Attached { attempt, connection_id, browser_state, pending }),
                next_attempt: next,
                effects: vec![Effect::EmitControl(control)],
            })
        }
        Event::RequestSettled { connection, request, .. }
        | Event::RequestTimedOut { connection, request, .. }
        | Event::RequestCancelled { connection, request, .. } => {
            if *connection != connection_id {
                return None;
            }
            let index = pending.iter().position(|(id, _)| id == request)?;
            let (_, command) = pending.remove(index);
            let effects = settle_request(&connection_id, event, &command);
            Some(Step {
                entry: Some(SessionState::Attached { attempt, connection_id, browser_state, pending }),
                next_attempt: next,
                effects,
            })
        }
        Event::SessionDeleted { .. } => {
            let mut effects = fail_pending(&pending);
            effects.push(Effect::CloseAttachment { attempt, outcome: Outcome::Closed });
            Some(Step { entry: None, next_attempt: next, effects })
        }
        Event::AttachClosed { session, attempt: closed } if *closed == attempt => {
            Some(revoke_attached(attempt, &pending, &session_of(session), next))
        }
        Event::SessionMoved { session } => Some(revoke_attached(attempt, &pending, session, next)),
        _ => None,
    }
}

fn transition_revoking(sync: PermissionSync, deleted: bool, event: &Event, next: u64) -> Option<Step> {
    match event {
        Event::SessionDeleted { .. } => {
            if deleted {
                return None;
            }
            Some(Step { entry: Some(SessionState::Revoking { sync, deleted: true }), next_attempt: next, effects: vec![] })
        }
        Event::Permissions { session, attempt, result } => {
            if *attempt != sync.attempt {
                return None;
            }
            let mut effects = sync_warning(result, sync);
            // A grant settled after its attachment closed, or timed out and may still have written: revoke it.
            if sync.allow && !deleted {
                let revoke = PermissionSync { attempt: next, allow: false };
                effects.push(Effect::SyncPermissions { session: session.clone(), attempt: revoke.attempt, allow: false });
                return Some(Step {
                    entry: Some(SessionState::Revoking { sync: revoke, deleted: false }),
                    next_attempt: next + 1,
                    effects,
                });
            }
            Some(Step { entry: None, next_attempt: next, effects })
        }
        _ => None,
    }
}

fn dispose(state: &mut ConnectionState) -> Vec<Effect> {
    if state.disposed {
        return vec![];
    }
    let mut effects = Vec::new();
    let mut sessions = IndexMap::new();
    let mut next = state.next_attempt;
    for (session, entry) in std::mem::take(&mut state.sessions) {
        match entry {
            SessionState::Revoking { .. } => {
                sessions.insert(session, entry);
            }
            SessionState::Attaching { attempt, sync, .. } => {
                sessions.insert(session, SessionState::Revoking { sync, deleted: false });
                effects.push(Effect::CloseAttachment { attempt, outcome: Outcome::Closed });
            }
            SessionState::Attached { attempt, pending, .. } => {
                let step = revoke_attached(attempt, &pending, &session, next);
                if let Some(entry) = step.entry {
                    sessions.insert(session, entry);
                }
                effects.extend(step.effects);
                next = step.next_attempt;
            }
        }
    }
    if sessions.is_empty() {
        effects.push(Effect::NotifyDrained);
    }
    *state = ConnectionState { disposed: true, next_attempt: next, sessions };
    effects
}

fn revoke_attached(attempt: u64, pending: &[(String, Value)], session: &str, next: u64) -> Step {
    let sync = PermissionSync { attempt: next, allow: false };
    let mut effects = fail_pending(pending);
    effects.push(Effect::CloseAttachment { attempt, outcome: Outcome::Closed });
    effects.push(Effect::SyncPermissions { session: session.to_string(), attempt: sync.attempt, allow: false });
    Step { entry: Some(SessionState::Revoking { sync, deleted: false }), next_attempt: next + 1, effects }
}

fn settle_request(connection: &str, event: &Event, command: &Value) -> Vec<Effect> {
    let (request, settlement) = match event {
        Event::RequestSettled { request, outcome, .. } => (request, Some(outcome.clone())),
        Event::RequestTimedOut { request, .. } | Event::RequestCancelled { request, .. } => (request, None),
        _ => return vec![],
    };
    let cancel = Effect::EmitControl(json!({ "type": "cancel", "connectionID": connection, "requestID": request }));
    match event {
        // The interrupted caller no longer waits; only the desktop needs to stop.
        Event::RequestCancelled { .. } => vec![cancel],
        Event::RequestTimedOut { .. } => {
            let action = command["action"]["type"].as_str().unwrap_or_default();
            vec![Effect::SettleRequest { request: request.clone(), settlement: Err(timed_out(action)) }, cancel]
        }
        _ => vec![Effect::SettleRequest { request: request.clone(), settlement: settlement.unwrap_or_else(|| Err(String::new())) }],
    }
}

fn fail_pending(pending: &[(String, Value)]) -> Vec<Effect> {
    pending
        .iter()
        .map(|(request, _)| Effect::SettleRequest { request: request.clone(), settlement: Err(disconnected_after_dispatch()) })
        .collect()
}

fn sync_warning(result: &SyncResult, sync: PermissionSync) -> Vec<Effect> {
    let action = if sync.allow { "grant" } else { "revoke" };
    match result {
        SyncResult::Synced => vec![],
        SyncResult::Failed(error) => {
            vec![Effect::Warn { message: format!("Browser permission {action} failed"), error: Some(error.clone()) }]
        }
        SyncResult::TimedOut => vec![Effect::Warn {
            message: format!("Browser permission {action} timed out after {} ms", PERMISSION_SYNC_TIMEOUT.as_millis()),
            error: None,
        }],
    }
}

/// `reconcileBrowserPermissions`: the session's rules with exactly one set of browser allows (`allow`) or none,
/// every other rule kept in order. `None` when they already are, so a replace writes nothing.
pub fn reconcile(current: &[oc_schema::types::PermissionRule], allow: bool) -> Option<Vec<oc_schema::types::PermissionRule>> {
    use oc_schema::types::Effect as Rule;
    let access = super::access(Rule::Allow);
    let mut next: Vec<_> = current
        .iter()
        .filter(|rule| rule.effect != Rule::Allow || !access.iter().any(|a| a.action == rule.action && a.resource == rule.resource))
        .cloned()
        .collect();
    if allow {
        next.extend(access);
    }
    let unchanged = next.len() == current.len()
        && next.iter().zip(current).all(|(a, b)| a.action == b.action && a.resource == b.resource && a.effect == b.effect);
    (!unchanged).then_some(next)
}

pub fn is_attached_connection(state: &ConnectionState, session: &str, connection: &str) -> bool {
    !state.disposed
        && matches!(state.sessions.get(session), Some(SessionState::Attached { connection_id, .. }) if connection_id == connection)
}

/// The attempt of the session's attachment with `connection`, attaching or attached.
pub fn select_attachment(state: &ConnectionState, session: &str, connection: &str) -> Option<u64> {
    if state.disposed {
        return None;
    }
    match state.sessions.get(session)? {
        SessionState::Attaching { attempt, connection_id, .. } | SessionState::Attached { attempt, connection_id, .. }
            if connection_id == connection =>
        {
            Some(*attempt)
        }
        _ => None,
    }
}

pub fn select_pending_command(state: &ConnectionState, session: &str, connection: &str, request: &str) -> Option<Value> {
    if state.disposed {
        return None;
    }
    match state.sessions.get(session)? {
        SessionState::Attached { connection_id, pending, .. } if connection_id == connection => {
            pending.iter().find(|(id, _)| id == request).map(|(_, command)| command.clone())
        }
        _ => None,
    }
}

/// `selectTarget`: the attachment and tab a browser tool call targets, or the error the tool returns.
pub fn select_target(state: &ConnectionState, session: &str, action: &Value) -> Result<(String, Option<Value>), String> {
    let entry = if state.disposed { None } else { state.sessions.get(session) };
    let (connection_id, browser_state) = match entry {
        None | Some(SessionState::Revoking { .. }) => {
            return Err("[browser.disconnected] No desktop browser is connected to this session. Open this session in the desktop app and wait for it to connect. Then call browser.tabs.list({}). Repeating browser actions while disconnected will not help.".into());
        }
        Some(SessionState::Attaching { .. }) => {
            return Err("[browser.disconnected] The desktop browser is still connecting to this session; no action ran. Call browser.tabs.list({}) once it finishes connecting.".into());
        }
        Some(SessionState::Attached { connection_id, browser_state, .. }) => (connection_id, browser_state),
    };
    let tab = match action.get("tabID") {
        None => None,
        Some(id) => {
            let tab = browser_state["tabs"].as_array().and_then(|tabs| tabs.iter().find(|t| t["id"] == *id).cloned());
            if tab.is_none() {
                return Err("[browser.tab_unavailable] This tab is closed or does not belong to the connected session. Call browser.tabs.list({}) and use an exact returned tabID. If no tabs exist, use browser.tabs.open({}). Never substitute a request ID, file ID, or element ref for tabID.".into());
            }
            tab
        }
    };
    Ok((connection_id.clone(), tab))
}

fn disconnected_before_dispatch() -> String {
    "[browser.disconnected] The desktop browser disconnected before this action was sent; no action ran. Reconnect this session in the desktop app, then call browser.tabs.list({}).".into()
}

pub fn disconnected_after_dispatch() -> String {
    "[browser.disconnected] Browser connection closed; the action may already have run. Reconnect this session in the desktop app, call browser.tabs.list({}), and inspect the target tab with browser.snapshot({tabID}). Do not repeat clicks, submissions, uploads, or evaluations until their outcome is known.".into()
}

fn timed_out(action: &str) -> String {
    format!(
        "[browser.timeout] browser.{action} did not finish within {} seconds; its outcome is unknown. Check the desktop connection, call browser.tabs.list({{}}), and inspect the tab or browser.files.list({{tabID}}) for completed work. Do not blindly repeat a mutating action or start another recording.",
        super::REQUEST_TIMEOUT.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_schema::types::{Effect as Rule, PermissionRule};

    fn attach(state: &mut ConnectionState, session: &str, connection: &str) -> (u64, Vec<Effect>) {
        let attempt = state.next_attempt;
        let effects = transition(state, Event::AttachStarted { session: session.into(), attempt, connection: connection.into() });
        (attempt, effects)
    }

    fn synced(state: &mut ConnectionState, session: &str, attempt: u64) -> Vec<Effect> {
        transition(state, Event::Permissions { session: session.into(), attempt, result: SyncResult::Synced })
    }

    fn sync_attempt(effects: &[Effect]) -> u64 {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::SyncPermissions { attempt, .. } => Some(*attempt),
                _ => None,
            })
            .expect("a permission sync")
    }

    #[test]
    fn grant_then_attached_then_revoke_on_close() {
        let mut state = ConnectionState::default();
        let (attempt, effects) = attach(&mut state, "ses_1", "c1");
        assert_eq!(effects, vec![Effect::SyncPermissions { session: "ses_1".into(), attempt: 2, allow: true }]);
        assert_eq!(
            synced(&mut state, "ses_1", 2),
            vec![Effect::EmitControl(json!({"type": "attached", "connectionID": "c1", "version": 4}))]
        );
        let effects = transition(&mut state, Event::AttachClosed { session: "ses_1".into(), attempt });
        assert_eq!(
            effects,
            vec![
                Effect::CloseAttachment { attempt, outcome: Outcome::Closed },
                Effect::SyncPermissions { session: "ses_1".into(), attempt: 3, allow: false },
            ]
        );
        assert!(synced(&mut state, "ses_1", 3).is_empty());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn a_replacement_grants_again_without_revoking_first() {
        let mut state = ConnectionState::default();
        let (first, effects) = attach(&mut state, "ses_1", "c1");
        synced(&mut state, "ses_1", sync_attempt(&effects));
        let (_, effects) = attach(&mut state, "ses_1", "c2");
        assert_eq!(effects[0], Effect::CloseAttachment { attempt: first, outcome: Outcome::Replaced });
        assert!(matches!(effects[1], Effect::SyncPermissions { allow: true, .. }));
        // The old attachment's own close afterwards changes nothing.
        assert!(transition(&mut state, Event::AttachClosed { session: "ses_1".into(), attempt: first }).is_empty());
    }

    #[test]
    fn an_attach_during_a_revoke_waits_for_it() {
        let mut state = ConnectionState::default();
        let (first, effects) = attach(&mut state, "ses_1", "c1");
        synced(&mut state, "ses_1", sync_attempt(&effects));
        let revoke = sync_attempt(&transition(&mut state, Event::AttachClosed { session: "ses_1".into(), attempt: first }));
        let (_, effects) = attach(&mut state, "ses_1", "c2");
        assert!(effects.is_empty());
        let effects = synced(&mut state, "ses_1", revoke);
        assert!(matches!(effects[..], [Effect::SyncPermissions { allow: true, .. }]));
        let effects = synced(&mut state, "ses_1", sync_attempt(&effects));
        assert!(matches!(effects[..], [Effect::EmitControl(_)]));
    }

    #[test]
    fn a_failed_grant_ends_the_attach_and_revokes() {
        let mut state = ConnectionState::default();
        let (attempt, effects) = attach(&mut state, "ses_1", "c1");
        let effects = transition(
            &mut state,
            Event::Permissions { session: "ses_1".into(), attempt: sync_attempt(&effects), result: SyncResult::TimedOut },
        );
        assert!(matches!(&effects[0], Effect::Warn { message, .. } if message == "Browser permission grant timed out after 10000 ms"));
        assert_eq!(effects[1], Effect::CloseAttachment { attempt, outcome: Outcome::Unavailable });
        assert!(matches!(effects[2], Effect::SyncPermissions { allow: false, .. }));
    }

    #[test]
    fn requests_settle_time_out_and_fail_on_disposal() {
        let mut state = ConnectionState::default();
        let (_, effects) = attach(&mut state, "ses_1", "c1");
        synced(&mut state, "ses_1", sync_attempt(&effects));
        let command = json!({"action": {"type": "tabs.list"}, "files": []});
        let start = |request: &str| Event::RequestStarted {
            session: "ses_1".into(),
            connection: "c1".into(),
            request: request.into(),
            command: command.clone(),
        };
        assert_eq!(
            transition(&mut state, start("r1")),
            vec![Effect::EmitControl(json!({"type": "command", "connectionID": "c1", "requestID": "r1"}))]
        );
        assert_eq!(select_pending_command(&state, "ses_1", "c1", "r1"), Some(command.clone()));
        let effects =
            transition(&mut state, Event::RequestTimedOut { session: "ses_1".into(), connection: "c1".into(), request: "r1".into() });
        assert!(
            matches!(&effects[0], Effect::SettleRequest { settlement: Err(m), .. } if m.starts_with("[browser.timeout] browser.tabs.list did not finish within 60 seconds"))
        );
        assert_eq!(effects[1], Effect::EmitControl(json!({"type": "cancel", "connectionID": "c1", "requestID": "r1"})));
        transition(&mut state, start("r2"));
        let effects = transition(&mut state, Event::Disposed);
        assert_eq!(effects[0], Effect::SettleRequest { request: "r2".into(), settlement: Err(disconnected_after_dispatch()) });
        assert!(matches!(effects[1], Effect::CloseAttachment { outcome: Outcome::Closed, .. }));
        let revoke = sync_attempt(&effects);
        // A request started after the attachment went away was never sent.
        assert!(
            matches!(&transition(&mut state, start("r3"))[..], [Effect::SettleRequest { settlement: Err(m), .. }] if m.contains("before this action was sent"))
        );
        assert_eq!(synced(&mut state, "ses_1", revoke), vec![Effect::NotifyDrained]);
    }

    #[test]
    fn targets() {
        let mut state = ConnectionState::default();
        let action = json!({"type": "snapshot", "tabID": "tab_1"});
        assert!(select_target(&state, "ses_1", &action).unwrap_err().contains("No desktop browser is connected"));
        let (_, effects) = attach(&mut state, "ses_1", "c1");
        assert!(select_target(&state, "ses_1", &action).unwrap_err().contains("still connecting"));
        synced(&mut state, "ses_1", sync_attempt(&effects));
        assert!(select_target(&state, "ses_1", &action).unwrap_err().starts_with("[browser.tab_unavailable]"));
        let tab = json!({"id": "tab_1", "generation": 2});
        transition(
            &mut state,
            Event::StateUpdated { session: "ses_1".into(), connection: "c1".into(), state: json!({"tabs": [tab], "focusedTabID": null}) },
        );
        assert_eq!(select_target(&state, "ses_1", &action), Ok(("c1".into(), Some(tab))));
    }

    #[test]
    fn reconcile_keeps_one_set_of_allows() {
        let rule = |action: &str, resource: &str, effect: Rule| PermissionRule { action: action.into(), resource: resource.into(), effect };
        let allows = vec![rule("browser", "*", Rule::Allow), rule("skill", "browser", Rule::Allow)];
        assert_eq!(reconcile(&allows, true), None);
        assert_eq!(reconcile(&allows, false), Some(vec![]));
        let mixed = vec![rule("browser", "*", Rule::Allow), rule("read", "*", Rule::Deny), rule("browser", "*", Rule::Deny)];
        assert_eq!(
            reconcile(&mixed, true),
            Some(vec![rule("read", "*", Rule::Deny), rule("browser", "*", Rule::Deny), allows[0].clone(), allows[1].clone()])
        );
        assert_eq!(reconcile(&[], false), None);
    }
}
