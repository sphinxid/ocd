//! The Azure plugin's connect form lists the `az login` account's resources (core/src/plugin/provider/azure.ts
//! `listResources`), with a fake `az` on `PATH` and a mock Azure Resource Graph. Never touches a real
//! Azure account: `az` resolves to the script below and the management endpoint is local.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::HeaderMap;
use axum::routing::post;
use parking_lot::Mutex;
use serde_json::{Value, json};

fn azure_form(models: &oc_core::models::Models, key: &str) -> Value {
    let integrations = models.integrations_for(Some(key));
    let info = futures::executor::block_on(integrations.get("azure")).unwrap().unwrap();
    let method = info.methods.iter().map(|m| serde_json::to_value(m).unwrap()).find(|m| m["id"] == "azure-cli").expect("azure-cli method");
    method["form"][0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_cli_form_lists_resources() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    // A fake Azure CLI: only `account get-access-token` is used by the listing.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let az = bin.join("az");
    std::fs::write(
        &az,
        "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/calls.txt\"\n\
         if [ \"$1 $2\" = \"account get-access-token\" ]; then echo '{\"accessToken\":\"fake-token\",\"expires_on\":4102444800}'; exit 0; fi\n\
         echo 'unexpected' >&2; exit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&az, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A signed-in profile, written with a byte order mark like the Azure CLI does.
    let config = root.join("azure");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("azureProfile.json"), "\u{FEFF}{\"subscriptions\":[{\"id\":\"sub\"}]}").unwrap();
    // SAFETY: this test binary runs only this test.
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
        std::env::set_var("AZURE_CONFIG_DIR", &config);
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
        std::env::remove_var("AZURE_RESOURCE_NAME");
        std::env::remove_var("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME");
    }

    // Mock Azure Resource Graph.
    let seen: Arc<Mutex<Vec<(HeaderMap, Value)>>> = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().route(
        "/providers/Microsoft.ResourceGraph/resources",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let record = record.clone();
            async move {
                record.lock().push((headers, body));
                Json(json!({ "data": [
                    { "resourceName": "alpha-models", "resourceGroup": "rg-one", "location": "eastus" },
                    { "resourceName": "bad name", "resourceGroup": "rg", "location": "westus" },
                    { "resourceName": "", "resourceGroup": "rg", "location": "westus" },
                    { "resourceGroup": "rg", "location": "westus" },
                    { "resourceName": "zeta", "resourceGroup": "rg-two", "location": "swedencentral" },
                ]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let db = oc_storage::Database::memory().unwrap();
    let models = oc_core::models::Models::new(db.clone(), oc_core::bus::Bus::new(db), "latest", "1.2.3");
    models.set_azure_management(format!("http://127.0.0.1:{port}"));
    models.catalog_at(&[], Some("/loc")).await.unwrap();
    // Until the listing answers, the field is plain text.
    let mut form = azure_form(&models, "/loc");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while form.get("options").is_none() && tokio::time::Instant::now() < deadline {
        assert_eq!(form["description"], "Requests use your `az login` session.");
        tokio::time::sleep(Duration::from_millis(50)).await;
        form = azure_form(&models, "/loc");
    }
    assert_eq!(
        form,
        json!({
            "type": "string",
            "key": "resourceName",
            "title": "Azure resource",
            "placeholder": "e.g. my-models",
            "required": true,
            "pattern": "^[a-zA-Z0-9][a-zA-Z0-9-]*$",
            "custom": true,
            "options": [
                { "value": "alpha-models", "label": "alpha-models", "description": "rg-one · eastus" },
                { "value": "zeta", "label": "zeta", "description": "rg-two · swedencentral" },
            ],
        })
    );
    let seen = seen.lock().clone();
    assert_eq!(seen.len(), 1, "listed once");
    let (headers, body) = &seen[0];
    assert_eq!(headers["authorization"], "Bearer fake-token");
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["user-agent"], "opencode/latest/1.2.3/opencode");
    assert_eq!(body["options"], json!({ "$top": 1000 }));
    assert!(body["query"].as_str().unwrap().starts_with("resources | where type =~ 'microsoft.cognitiveservices/accounts'"));
    let calls = std::fs::read_to_string(bin.join("calls.txt")).unwrap();
    assert_eq!(calls.trim(), "account get-access-token --scope https://management.azure.com/.default --output json");

    // A rebuild keeps the listing and does not list again; another location lists for itself.
    models.catalog_at(&[], Some("/loc")).await.unwrap();
    assert_eq!(azure_form(&models, "/loc")["options"].as_array().unwrap().len(), 2);
    models.catalog_at(&[], Some("/other")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while azure_form(&models, "/other").get("options").is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(azure_form(&models, "/other")["options"].as_array().unwrap().len(), 2);

    // Signed out (no subscriptions): no `az` call, the field stays plain text.
    std::fs::write(config.join("azureProfile.json"), "{\"subscriptions\":[]}").unwrap();
    std::fs::remove_file(bin.join("calls.txt")).unwrap();
    models.catalog_at(&[], Some("/signed-out")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(azure_form(&models, "/signed-out")["description"], "Requests use your `az login` session.");
    assert!(!bin.join("calls.txt").exists());
}

#[test]
fn azure_cli_method_states() {
    use oc_core::integrations::azure_cli_method;
    let form = |m: oc_ai::credentials::Method| serde_json::to_value(m).unwrap()["form"].clone();
    assert_eq!(form(azure_cli_method(false, None))[0]["description"], "Requests use your `az login` session.");
    assert_eq!(
        form(azure_cli_method(false, Some(&[])))[0]["description"],
        "No Azure OpenAI or AI Services resources found for your `az login` account."
    );
    let listed = form(azure_cli_method(false, Some(&[json!({"value": "a", "label": "a", "description": "g · l"})])));
    assert_eq!(listed[0]["custom"], true);
    assert!(listed[0].get("description").is_none());
    assert!(form(azure_cli_method(true, None)).is_null());
}
