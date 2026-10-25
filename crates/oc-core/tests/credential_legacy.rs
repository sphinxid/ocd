//! `core/test/credential.test.ts`: key form answers stored in `configuration` before v2.1 read as metadata.

use oc_ai::credentials::{CredentialStore, CredentialValue};
use oc_core::bus::Bus;
use oc_core::models::DbCredentials;
use oc_storage::Database;
use serde_json::json;

#[tokio::test]
async fn reads_key_form_answers_stored_in_configuration_before_v2_1_as_metadata() {
    let db = Database::memory().unwrap();
    let store = DbCredentials::new(db.clone(), Bus::new(db.clone()));
    // A row from an earlier release.
    let legacy = json!({
        "type": "key",
        "key": "secret",
        "metadata": { "imported": true, "resourceName": "older" },
        "configuration": { "resourceName": "answered" },
    });
    db.with(|c| {
        c.execute(
            "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated) VALUES ('cred_legacy', 'azure', 'Legacy', ?, 1, 0, 0)",
            [legacy.to_string()],
        )
    })
    .unwrap();
    let value = store.get("cred_legacy").await.unwrap().unwrap().value;
    assert_eq!(
        value,
        CredentialValue::Key {
            key: "secret".into(),
            metadata: Some(json!({ "imported": true, "resourceName": "answered" }).as_object().unwrap().clone()),
        }
    );
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({ "type": "key", "key": "secret", "metadata": { "imported": true, "resourceName": "answered" } })
    );
}
