use oc_ai::credentials::store::{
    CreateCredential, CredentialEvent, CredentialStore, CredentialUpdate, MemoryCredentialStore, Row, list_order,
    plan_create, replacement, selected, unique_label,
};
use oc_ai::credentials::{Credential, CredentialValue};

fn key(k: &str) -> CredentialValue {
    CredentialValue::key(k)
}

fn row(id: &str, active: Option<bool>, time: i64) -> Row {
    Row {
        id: id.into(),
        integration_id: Some("openai".into()),
        label: id.into(),
        value: key(id),
        active,
        time_created: time,
        time_updated: time,
    }
}

#[test]
fn ordering_matches_sqlite() {
    let mut rows = vec![row("c", Some(true), 1), row("a", None, 5), row("b", Some(false), 2), row("d", Some(false), 2)];
    rows.sort_by(list_order);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "d", "c"]);
    assert_eq!(selected(rows.iter()).unwrap().id, "c");
    // Legacy rows without the flag: newest wins.
    let legacy = [row("x", None, 1), row("y", None, 3), row("z", None, 2)];
    assert_eq!(selected(legacy.iter()).unwrap().id, "y");
    assert_eq!(replacement(legacy.iter()).unwrap().id, "y");
    // Same time: id DESC.
    let tie = [row("p", Some(false), 1), row("q", Some(false), 1)];
    assert_eq!(selected(tie.iter()).unwrap().id, "q");
}

#[test]
fn create_plan() {
    assert_eq!(plan_create(&[], Some(false)).activate, true);
    let existing = [row("a", None, 1)];
    let plan = plan_create(&existing, Some(false));
    assert!(!plan.activate);
    assert_eq!(plan.pin.as_deref(), Some("a"));
    let existing = [row("a", Some(true), 1)];
    assert_eq!(plan_create(&existing, Some(false)).pin, None);
    assert!(plan_create(&existing, None).activate);
}

#[test]
fn unique_labels() {
    assert_eq!(unique_label("OpenAI", &[]), "OpenAI");
    assert_eq!(unique_label("OpenAI", &["OpenAI".into()]), "OpenAI 2");
    assert_eq!(unique_label("OpenAI", &["OpenAI".into(), "OpenAI 2".into()]), "OpenAI 3");
    assert_eq!(unique_label("OpenAI", &["OpenAI 2".into()]), "OpenAI");
    assert_eq!(unique_label("OpenAI", &["OpenAI".into(), "OpenAI 3".into()]), "OpenAI 2");
}

#[tokio::test]
async fn memory_store_selection_and_events() {
    let store = MemoryCredentialStore::new();
    let mut events = store.subscribe();
    let a = store.create(CreateCredential::new("openai", key("a"))).await.unwrap();
    assert_eq!(a.label, "default");
    assert!(a.id.starts_with("cred_"));
    assert_eq!(events.try_recv().unwrap(), CredentialEvent::Updated);
    assert_eq!(
        events.try_recv().unwrap(),
        CredentialEvent::Switched { integration_id: "openai".into(), credential_id: Some(a.id.clone()) }
    );
    let mut input = CreateCredential::new("openai", key("b"));
    input.activate = Some(false);
    let b = store.create(input).await.unwrap();
    assert_eq!(b.active, Some(false));
    assert_eq!(events.try_recv().unwrap(), CredentialEvent::Updated);
    assert!(events.try_recv().is_err());

    // List order: inactive first.
    let list: Vec<String> = store.list("openai").await.unwrap().into_iter().map(|c| c.id).collect();
    assert_eq!(list, [b.id.clone(), a.id.clone()]);

    store.activate(&b.id).await.unwrap();
    let list: Vec<Credential> = store.list("openai").await.unwrap();
    assert_eq!(list.last().unwrap().id, b.id);
    assert_eq!(
        events.try_recv().unwrap(),
        CredentialEvent::Switched { integration_id: "openai".into(), credential_id: Some(b.id.clone()) }
    );
    // Activating the selected one is a no-op.
    store.activate(&b.id).await.unwrap();
    assert!(events.try_recv().is_err());

    // Removing the selected credential activates the newest remaining.
    let c = store.create(CreateCredential::new("openai", key("c"))).await.unwrap();
    let _ = events.try_recv();
    let _ = events.try_recv();
    store.remove(&c.id).await.unwrap();
    assert_eq!(events.try_recv().unwrap(), CredentialEvent::Updated);
    assert_eq!(
        events.try_recv().unwrap(),
        CredentialEvent::Switched { integration_id: "openai".into(), credential_id: Some(b.id.clone()) }
    );
    let rows = store.rows();
    assert_eq!(rows.iter().find(|r| r.id == b.id).unwrap().active, Some(true));
    assert_eq!(rows.iter().find(|r| r.id == a.id).unwrap().active, Some(false));

    // Removing a non-selected credential does not switch.
    store.remove(&a.id).await.unwrap();
    assert_eq!(events.try_recv().unwrap(), CredentialEvent::Updated);
    assert!(events.try_recv().is_err());
    // Removing the last one switches to null.
    store.remove(&b.id).await.unwrap();
    let _ = events.try_recv();
    assert_eq!(
        events.try_recv().unwrap(),
        CredentialEvent::Switched { integration_id: "openai".into(), credential_id: None }
    );
}

#[tokio::test]
async fn legacy_row_is_pinned_when_not_activating() {
    let store = MemoryCredentialStore::new();
    store.insert_row(row("legacy", None, 1));
    let mut input = CreateCredential::new("openai", key("new"));
    input.activate = Some(false);
    let created = store.create(input).await.unwrap();
    let rows = store.rows();
    assert_eq!(rows.iter().find(|r| r.id == "legacy").unwrap().active, Some(true));
    assert_eq!(rows.iter().find(|r| r.id == created.id).unwrap().active, Some(false));
}

#[tokio::test]
async fn update_semantics() {
    let store = MemoryCredentialStore::new();
    let a = store.create(CreateCredential::new("x", key("a"))).await.unwrap();
    let mut events = store.subscribe();
    store.update(&a.id, CredentialUpdate { label: Some("default".into()), value: None }).await.unwrap();
    assert!(events.try_recv().is_err());
    store.update(&a.id, CredentialUpdate { label: None, value: Some(key("b")) }).await.unwrap();
    assert!(events.try_recv().is_err());
    assert_eq!(store.get(&a.id).await.unwrap().unwrap().value, key("b"));
    store.update(&a.id, CredentialUpdate { label: Some("Work".into()), value: None }).await.unwrap();
    assert_eq!(events.try_recv().unwrap(), CredentialEvent::Updated);
    assert_eq!(store.get(&a.id).await.unwrap().unwrap().label, "Work");
}

#[test]
fn credential_value_serde_shape() {
    let value = CredentialValue::Oauth {
        method_id: "device".into(),
        refresh: "r".into(),
        access: "a".into(),
        expires: 0,
        metadata: None,
    };
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        r#"{"type":"oauth","methodID":"device","refresh":"r","access":"a","expires":0}"#
    );
}
