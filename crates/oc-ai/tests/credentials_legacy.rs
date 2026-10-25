//! `packages/schema/test/credential-legacy.test.ts`: key credentials written before v2.1 kept their setup-form
//! answers in `configuration`; they decode with the answers folded into `metadata`.

use oc_ai::credentials::{Credential, CredentialValue};
use serde_json::{json, Value};

fn decode(v: Value) -> CredentialValue {
    serde_json::from_value(v).unwrap()
}

fn legacy() -> Value {
    json!({
        "type": "key",
        "key": "secret",
        "metadata": { "imported": true, "resourceName": "older" },
        "configuration": { "resourceName": "answered", "accountId": "account" },
    })
}

#[test]
fn decodes_key_form_answers_written_to_configuration_into_metadata_keeping_answers_first() {
    assert_eq!(
        serde_json::to_value(decode(legacy())).unwrap(),
        json!({ "type": "key", "key": "secret", "metadata": { "imported": true, "resourceName": "answered", "accountId": "account" } })
    );
    assert_eq!(
        serde_json::to_value(decode(json!({ "type": "key", "key": "secret", "configuration": { "resourceName": "answered" } }))).unwrap(),
        json!({ "type": "key", "key": "secret", "metadata": { "resourceName": "answered" } })
    );
}

#[test]
fn decodes_current_values_and_exported_entries_unchanged() {
    let current = json!({ "type": "key", "key": "secret", "metadata": { "resourceName": "answered" } });
    assert_eq!(serde_json::to_value(decode(current.clone())).unwrap(), current);
    let external = json!({ "type": "external", "methodID": "azure-cli", "metadata": { "resourceName": "answered" } });
    assert_eq!(serde_json::to_value(decode(external.clone())).unwrap(), external);
    let entry: Credential = serde_json::from_value(json!({
        "id": "cred_legacy",
        "integrationID": "azure",
        "label": "Work",
        "active": true,
        "value": legacy(),
    }))
    .unwrap();
    assert_eq!(entry.value, decode(legacy()));
}

#[test]
fn rejects_invalid_legacy_configuration() {
    assert!(serde_json::from_value::<CredentialValue>(json!({ "type": "key", "key": "secret", "configuration": "answered" })).is_err());
}

#[test]
fn encoding_never_writes_configuration() {
    let v = CredentialValue::Key { key: "secret".into(), metadata: None };
    assert_eq!(serde_json::to_value(v).unwrap(), json!({ "type": "key", "key": "secret" }));
}
