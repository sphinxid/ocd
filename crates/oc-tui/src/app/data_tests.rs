//! 2.1's data-store changes (`packages/client/src/solid/data.ts`): archiving, the switched-from
//! model, inline images on optimistic prompts.

use super::tests::{app, session};
use super::*;
use serde_json::json;

fn ev(kind: &str, data: serde_json::Value) -> Event {
    Event { id: "evt_0123456789abABCDEFGHIJKLMN".into(), created: 5000, kind: kind.into(), data, ..Default::default() }
}

#[test]
fn archiving_sets_and_clears_the_archive_time() {
    let mut a = app();
    let sid = session(&mut a);
    a.on_event(ev("session.archived", json!({ "sessionID": sid })));
    assert_eq!(a.store.session(&sid).unwrap().time.archived, Some(5000));
    a.on_event(ev("session.unarchived", json!({ "sessionID": sid })));
    assert_eq!(a.store.session(&sid).unwrap().time.archived, None);
}

#[test]
fn model_switches_remember_the_previous_model() {
    let mut a = app();
    let sid = session(&mut a);
    a.on_event(ev("session.model.selected", json!({ "sessionID": sid, "model": { "providerID": "openai", "id": "gpt-5" } })));
    let m = a.store.messages(&sid).iter().find(|m| m.kind == "model-switched").cloned().unwrap();
    assert_eq!(m.previous, Some(json!({ "providerID": "anthropic", "id": "claude-sonnet" })));
    // The event's own `previous` wins.
    a.on_event(Event { id: "evt_0123456789abABCDEFGHIJKLMO".into(), ..ev("session.model.selected", json!({ "sessionID": sid, "model": { "providerID": "x", "id": "y" }, "previous": { "providerID": "p", "id": "q" } })) });
    let m = a.store.messages(&sid).iter().rev().find(|m| m.kind == "model-switched").cloned().unwrap();
    assert_eq!(m.previous, Some(json!({ "providerID": "p", "id": "q" })));
}

#[test]
fn optimistic_prompts_keep_inline_images() {
    let mut a = app();
    let sid = session(&mut a);
    let files = vec![
        json!({ "uri": "data:image/png;base64,iVBORw0KGgo=", "name": "clipboard", "mention": { "start": 0, "end": 9, "text": "[Image 1]" } }),
        json!({ "uri": "file:///work/proj/a.ts", "name": "a.ts" }),
        json!({ "uri": "data:text/plain,hello" }),
    ];
    a.store.admit_prompt_files(&sid, "msg_9", "[Image 1] look", json!([]), &files, "steer");
    let item = a.store.pending.get(&sid).unwrap().iter().find(|i| i.id == "msg_9").cloned().unwrap();
    assert_eq!(
        item.payload["files"],
        json!([{ "data": "iVBORw0KGgo=", "mime": "image/png", "source": { "type": "inline" }, "name": "clipboard", "mention": { "start": 0, "end": 9, "text": "[Image 1]" } }])
    );
}
