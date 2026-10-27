//! The free usage limit: the transcript notice, the recalled prompt, the OpenCode Go offer (at most
//! once a day) and the suppressed failure toast (2.1.4 `routes/session/index.tsx`).

use super::tests::{app, render};
use super::*;
use crate::keymap::Stroke;
use serde_json::json;

const NOW: i64 = 1_800_000_000_000;

fn free_limit_session(a: &mut App) -> String {
    let id = "ses_free".to_string();
    a.store.remember(SessionInfo {
        id: id.clone(),
        title: Some("Free".into()),
        agent: Some("build".into()),
        location: LocationRef { directory: "/work/proj".into(), workspace_id: None },
        ..Default::default()
    });
    let msgs: Vec<Message> = serde_json::from_value(json!([
        {"id":"msg_1","type":"user","text":"hello there","time":{"created":NOW - 2000}},
        {"id":"msg_2","type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
         "time":{"created":NOW - 1000,"completed":NOW},"finish":"error",
         "error":{"type":"provider.free-limit","message":"Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.","retryAfterMs":3_600_000.0},
         "content":[]},
        {"id":"msg_3","type":"idle","outcome":"failed","time":{"created":NOW},
         "error":{"type":"provider.free-limit","message":"Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models."}}
    ]))
    .unwrap();
    a.store.messages.insert(id.clone(), msgs);
    a.store.loaded.insert(format!("session.message:{id}"));
    a.store.loaded.insert(format!("session.pending:{id}"));
    a.route = Route::Session(id.clone());
    a.clock = Some(NOW + 1000);
    id
}

fn step_failed(sid: &str, assistant: &str) -> Event {
    Event {
        id: format!("evt_{}", crate::util::id_body(false)),
        created: NOW,
        kind: "session.step.failed".into(),
        data: json!({ "sessionID": sid, "assistantMessageID": assistant, "error": { "type": "provider.free-limit", "message": "Free usage limit reached.", "retryAfterMs": 3_600_000.0 } }),
        ..Default::default()
    }
}

#[test]
fn notice_replaces_the_error_and_names_the_reset_time() {
    let mut a = app();
    free_limit_session(&mut a);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let reset = crate::util::locale::time(NOW + 3_600_000);
    assert!(lines.iter().any(|l| l == &format!("     You've used today's free messages. They reset at {reset}.")), "{joined}");
    assert!(lines.iter().any(|l| l == "     Upgrade to OpenCode Go → /connect · Switch model → /models"), "{joined}");
    // No "Error:" row: neither on the step nor for the idle marker.
    assert!(!joined.contains("Error:"), "{joined}");
    // Over a day away (or beyond 7 days) the reset shows a date, or nothing.
    assert_eq!(crate::util::locale::free_limit_reset(Some(3.0 * 86_400_000.0), NOW, NOW), Some(crate::util::locale::datetime(NOW + 3 * 86_400_000)));
    assert_eq!(crate::util::locale::free_limit_reset(Some(8.0 * 86_400_000.0), NOW, NOW), None);
    assert_eq!(crate::util::locale::free_limit_reset(None, NOW, NOW), None);
}

#[test]
fn free_limit_recalls_the_prompt_and_offers_go_once_a_day() {
    let mut a = app();
    let sid = free_limit_session(&mut a);
    a.on_event(step_failed(&sid, "msg_2"));
    assert_eq!(a.editor.text, "hello there");
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("go-upsell"));
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let head = lines.iter().find(|l| l.contains("Free limit reached")).unwrap();
    assert_eq!(head.find("Free limit reached"), Some(23), "{joined}");
    assert!(joined.contains("OpenCode Go gives you reliable access to the best"), "{joined}");
    assert!(joined.contains("https://opencode.ai/go"), "{joined}");
    let buttons = lines.iter().find(|l| l.contains("don't show again")).unwrap();
    assert!(buttons.contains("don't show again             dismiss     subscribe"), "{buttons:?}");
    // Arrows move between the buttons; dismiss closes.
    a.on_key(Stroke::key("left"));
    a.on_key(Stroke::key("return"));
    assert!(a.dialogs.is_empty());
    // The same failure is handled once; another one within a day keeps the offer closed.
    a.editor.clear();
    a.on_event(step_failed(&sid, "msg_2"));
    assert!(a.editor.is_empty());
    a.store.messages.get_mut(&sid).unwrap().push(serde_json::from_value(json!({"id":"msg_4","type":"user","text":"again","time":{"created":NOW + 10}})).unwrap());
    a.store.messages.get_mut(&sid).unwrap().push(serde_json::from_value(json!({"id":"msg_5","type":"assistant","time":{"created":NOW + 20},"content":[]})).unwrap());
    a.on_event(step_failed(&sid, "msg_5"));
    assert_eq!(a.editor.text, "again");
    assert!(a.dialogs.is_empty());
    let stored = a.keyed.get("go-upsell").unwrap();
    assert_eq!(stored["lastSeenAt"], json!(NOW + 1000));
}

#[test]
fn a_typed_prompt_is_kept_and_dont_show_again_sticks() {
    let mut a = app();
    let sid = free_limit_session(&mut a);
    a.editor.insert("draft");
    a.on_event(step_failed(&sid, "msg_2"));
    assert_eq!(a.editor.text, "draft");
    assert!(a.dialogs.is_empty());
    // Don't show again: stored and remembered, even past a day.
    let mut a = app();
    let sid = free_limit_session(&mut a);
    a.on_event(step_failed(&sid, "msg_2"));
    a.on_key(Stroke::key("left"));
    a.on_key(Stroke::key("left"));
    a.on_key(Stroke::key("return"));
    assert!(a.dialogs.is_empty());
    assert_eq!(a.keyed.get("go-upsell").unwrap()["dontShow"], json!(true));
    a.clock = Some(NOW + 3 * 86_400_000);
    a.editor.clear();
    a.store.messages.get_mut(&sid).unwrap().push(serde_json::from_value(json!({"id":"msg_6","type":"assistant","time":{"created":NOW + 20},"content":[]})).unwrap());
    a.on_event(step_failed(&sid, "msg_6"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn free_limit_failures_skip_the_failed_toast() {
    let mut a = app();
    let sid = free_limit_session(&mut a);
    let ev = |t: &str| Event { kind: "session.execution.failed".into(), created: NOW, data: json!({ "sessionID": sid, "error": { "type": t, "message": "boom" } }), ..Default::default() };
    a.on_event(ev("provider.free-limit"));
    assert!(a.toast.is_none());
    a.on_event(ev("unknown"));
    assert_eq!(a.toast.as_ref().and_then(|t| t.title.as_deref()), Some("Session failed"));
}

#[test]
fn compact_offer_on_short_terminals() {
    let mut a = app();
    let sid = free_limit_session(&mut a);
    a.on_event(step_failed(&sid, "msg_2"));
    // Short: centered, no art, the body and link without gaps.
    let lines = render(&mut a, 100, 20);
    let joined = lines.join("\n");
    let y = lines.iter().position(|l| l.contains("Free limit reached")).unwrap();
    assert!(lines[y + 1].contains("You've used today's free messages."), "{joined}");
    assert!(lines[y + 3].contains("open-source models for $10/month."), "{joined}");
    assert!(lines[y + 4].contains("https://opencode.ai/go"), "{joined}");
    assert!(lines[y + 5].contains("don't show again"), "{joined}");
    // Narrow: the buttons stack.
    let lines = render(&mut a, 50, 30);
    let joined = lines.join("\n");
    let y = lines.iter().position(|l| l.contains("don't show again")).unwrap();
    assert!(lines[y + 1].contains("dismiss") && lines[y + 2].contains("subscribe"), "{joined}");
}
