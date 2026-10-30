//! `/connect` (`component/dialog-integration.tsx`): upstream's `integration-options`,
//! `connected-provider` and `dialog-integration` tests, plus render and flow checks against a mock
//! server (checked against the 2.1.4 build at 100x30 and 56x24).

use super::integration::*;
use super::tests::{app, render};
use super::*;
use crate::keymap::Stroke;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn integration(id: &str, name: &str, extra: Value) -> Value {
    let mut v = json!({ "id": id, "name": name, "methods": [], "connections": [] });
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    v
}

fn frame(a: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    term.backend().buffer().clone()
}

/// Option titles of the top select dialog.
fn titles(a: &App) -> Vec<String> {
    match a.dialogs.last().map(|d| &d.body) {
        Some(crate::ui::dialog::Body::Select(s)) => s.options.iter().map(|o| o.title.clone()).collect(),
        _ => vec![],
    }
}

/// Description and body lines of the top prompt or text dialog.
fn texts(a: &App) -> String {
    match a.dialogs.last().map(|d| &d.body) {
        Some(crate::ui::dialog::Body::Prompt(p)) => p.description.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>().join("\n"),
        Some(crate::ui::dialog::Body::Text(t)) => t.lines.iter().map(|l| l.text()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn ids(list: &[Value]) -> Vec<String> {
    list.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

// ---- integration-options.test.ts ----------------------------------------------------------

#[test]
fn keeps_popular_integrations_first_and_sorts_the_rest_alphabetically() {
    let list = vec![
        integration("mistral", "Mistral", json!({})),
        integration("openai", "OpenAI", json!({})),
        integration("custom-z", "Zebra", json!({})),
        integration("anthropic", "Anthropic", json!({})),
        integration("opencode", "OpenCode Zen", json!({})),
        integration("opencode-go", "OpenCode Go", json!({})),
    ];
    assert_eq!(ids(&integration_options(&list)), ["opencode", "opencode-go", "openai", "anthropic", "mistral", "custom-z"]);
}

#[test]
fn keeps_mcp_integrations_above_popular_integrations_without_relying_on_their_ids() {
    let mcp = json!({ "metadata": { "source": "mcp" } });
    let list = vec![
        integration("openai", "OpenAI", json!({})),
        integration("linear", "Linear", mcp.clone()),
        integration("github", "GitHub", mcp),
        integration("opencode", "OpenCode Zen", json!({})),
        integration("opencode-go", "OpenCode Go", json!({})),
    ];
    assert_eq!(ids(&integration_options(&list)), ["github", "linear", "opencode", "opencode-go", "openai"]);
}

#[test]
fn offers_key_and_oauth_methods_but_not_environment_discovery() {
    let i = integration(
        "example",
        "Example",
        json!({ "methods": [{ "type": "env", "names": ["EXAMPLE_KEY"] }, { "type": "key", "label": "API key" }, { "type": "oauth", "id": "account", "label": "Account" }] }),
    );
    let types: Vec<String> = connect_methods(&i).iter().map(|m| m["type"].as_str().unwrap().to_string()).collect();
    assert_eq!(types, ["oauth", "key"]);
}

#[test]
fn returns_removable_credential_connections_only() {
    let i = integration(
        "example",
        "Example",
        json!({ "connections": [{ "type": "env", "name": "EXAMPLE_KEY" }, { "type": "credential", "method": "key", "id": "cred_1", "label": "Work" }] }),
    );
    assert_eq!(credential_connections(&i), vec![json!({ "type": "credential", "method": "key", "id": "cred_1", "label": "Work" })]);
}

#[test]
fn counts_several_connections() {
    let i = integration(
        "example",
        "Example",
        json!({ "connections": [{ "type": "credential", "method": "key", "id": "cred_1", "label": "Work" }, { "type": "env", "name": "EXAMPLE_KEY" }] }),
    );
    assert_eq!(connection_summary(&i), "2 connections");
    let env = integration("example", "Example", json!({ "connections": [{ "type": "env", "name": "EXAMPLE_KEY" }] }));
    assert_eq!(connection_summary(&env), "$EXAMPLE_KEY");
    assert_eq!(connection_summary(&integration("x", "X", json!({}))), "");
}

// ---- connected-provider.test.ts ---------------------------------------------------------------

#[test]
fn has_connected_provider_needs_a_connection() {
    assert!(!has_connected_provider(&[]));
    assert!(!has_connected_provider(&[json!({ "connections": [] })]));
    assert!(has_connected_provider(&[json!({ "connections": [{ "type": "credential", "method": "key", "id": "cred_1", "label": "Work" }] })]));
    assert!(has_connected_provider(&[json!({ "connections": [{ "type": "env", "name": "OPENAI_API_KEY" }] })]));
}

// ---- OpenTUI word wrap ---------------------------------------------------------------------------

#[test]
fn wraps_like_opentui() {
    // Breaks after `? / -` (and other punctuation), never inside `= & _`; a word that does not fit moves
    // to the next line and is broken by characters there.
    let url = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=x";
    let lines = wrap_opentui(url, 54);
    assert_eq!(lines[0], "https://auth.openai.com/oauth/authorize?");
    assert_eq!(lines[1], "response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hr");
    assert_eq!(lines[2], "ann&redirect_uri=x");
    let a = "a".repeat(78);
    assert_eq!(wrap_opentui(&format!("{a}.bbbbbbbbbb"), 82), [format!("{a}."), "bbbbbbbbbb".to_string()]);
    assert_eq!(wrap_opentui(&format!("{a}_bbbbbbbbbb"), 82), [format!("{a}_bbb"), "bbbbbbb".to_string()]);
    assert_eq!(wrap_opentui(&format!("{a}(bbbbbbbbbb"), 82), [format!("{a}("), "bbbbbbbbbb".to_string()]);
    assert_eq!(wrap_opentui("Complete authorization in your browser. This window will close automatically.", 54), ["Complete authorization in your browser. This window", "will close automatically."]);
    assert_eq!(wrap_opentui("one\ntwo", 10), ["one", "two"]);
}

// ---- rendering --------------------------------------------------------------------------------

fn openai_with_accounts() -> Value {
    json!({
        "id": "openai", "name": "OpenAI",
        "methods": [{ "type": "key", "label": "API key" }],
        "connections": [
            { "type": "credential", "method": "key", "id": "cred_personal", "label": "Personal" },
            { "type": "credential", "method": "key", "id": "cred_work", "label": "Work" },
            { "type": "env", "name": "OPENAI_API_KEY" },
        ],
    })
}

fn catalog() -> Vec<Value> {
    vec![
        json!({ "id": "anthropic", "name": "Anthropic", "methods": [{ "type": "key" }, { "type": "env", "names": ["ANTHROPIC_API_KEY"] }], "connections": [] }),
        json!({ "id": "302ai", "name": "302.AI", "methods": [{ "type": "key" }], "connections": [] }),
        json!({ "id": "envonly", "name": "Env Only", "methods": [{ "type": "env", "names": ["ENV_ONLY_KEY"] }], "connections": [] }),
        json!({ "id": "opencode", "name": "OpenCode Console", "methods": [{ "type": "key", "label": "API key (service account)" }, { "type": "oauth", "id": "device", "label": "OpenCode Console account" }], "connections": [] }),
        json!({ "id": "xai", "name": "xAI", "methods": [{ "type": "key" }], "connections": [{ "type": "env", "name": "XAI_API_KEY" }] }),
        json!({ "id": "google", "name": "Google", "methods": [{ "type": "key" }], "connections": [{ "type": "credential", "method": "key", "id": "cred_g", "label": "Google", "status": { "message": "expired" } }] }),
    ]
}

fn col(line: &str, needle: &str) -> usize {
    let byte = line.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
    crate::util::locale::width(&line[..byte])
}

/// The cells of a row under a medium dialog (x = 20..80 at 100 columns), trimmed.
fn inner(line: &str) -> String {
    line.chars().skip(20).take(60).collect::<String>().trim().to_string()
}

fn row<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines.iter().find(|l| l.contains(needle)).map(|s| s.as_str()).unwrap_or_else(|| panic!("no row with {needle:?}:\n{}", lines.join("\n")))
}

#[test]
fn the_list_groups_popular_and_services_with_connection_markers() {
    let mut a = app();
    a.store.location_mut("/work/proj").integrations = Some(catalog());
    a.run_command("provider.connect", None);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    // Upstream 2.1.4 at 100x30: title at the list inset, search, then the groups.
    assert_eq!(row(&lines, "Connect an integration"), "                        Connect an integration                           esc");
    let y = lines.iter().position(|l| l.contains("Popular")).unwrap();
    let trimmed: Vec<String> = lines[y..y + 7].iter().map(|l| inner(l)).collect();
    assert_eq!(trimmed[..3], ["Popular", "OpenCode Console", "Anthropic"], "{joined}");
    assert!(trimmed[3].starts_with("Google") && trimmed[3].ends_with("Sign in required →"), "{joined}");
    assert_eq!(trimmed[4..6], ["", "Services"], "{joined}");
    assert_eq!(col(&lines[y], "Popular"), 24);
    // A sign-in problem drops the check mark; an environment variable keeps it, with its name.
    assert!(!row(&lines, "Google").contains('✓'), "{joined}");
    let x = row(&lines, "xAI");
    assert_eq!(col(x, "✓ xAI"), 22, "{x:?}");
    assert_eq!(col(x, "$XAI_API_KEY") + "$XAI_API_KEY".len(), 76, "{x:?}");
    // Environment-only integrations without a saved credential are not listed.
    assert!(!joined.contains("Env Only"), "{joined}");
    let t = a.theme().dialog().clone();
    let buf = frame(&mut a, 100, 30);
    let gy = lines.iter().position(|l| l.contains("Sign in required")).unwrap() as u16;
    let gx = col(&lines[gy as usize], "Sign in") as u16;
    assert_eq!(buf[(gx, gy)].fg, t.text.feedback.warning.base.tui());
    let xy = lines.iter().position(|l| l.contains("xAI")).unwrap() as u16;
    assert_eq!(buf[(22, xy)].fg, t.text.feedback.success.base.tui());

    // Search keeps the categories; no match shows upstream's message.
    for c in "zzzq".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| l.trim() == "No integrations found"), "{}", lines.join("\n"));
    a.dialogs.clear();
    a.store.location_mut("/work/proj").integrations = Some(vec![]);
    a.run_command("provider.connect", None);
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| l.trim() == "No integrations available"), "{}", lines.join("\n"));
}

#[test]
fn keyboard_moves_center_the_selection() {
    let mut a = app();
    let mut list = vec![integration("anthropic", "Anthropic", json!({ "methods": [{ "type": "key" }] }))];
    for i in 0..30 {
        list.push(integration(&format!("svc{i:02}"), &format!("Service {i:02}"), json!({ "methods": [{ "type": "key" }] })));
    }
    a.store.location_mut("/work/proj").integrations = Some(list);
    a.run_command("provider.connect", None);
    for _ in 0..9 {
        a.on_key(Stroke::key("down"));
    }
    let lines = render(&mut a, 100, 30);
    // Nine rows (`floor(30 / 2) - 6`) from row 13; the selected row (Service 08) sits in the middle one.
    assert_eq!(inner(&lines[13 + 4]), "Service 08", "{}", lines.join("\n"));
    assert_eq!(inner(&lines[13]), "Service 04", "{}", lines.join("\n"));
}

#[test]
fn accounts_render_with_the_add_row_first_and_the_active_credential_marked() {
    let mut a = app();
    a.store.location_mut("/work/proj").integrations = Some(vec![openai_with_accounts()]);
    a.open_integrations(false, Some("openai".into()), true);
    let lines = render(&mut a, 100, 30);
    let frame = lines.join("\n");
    assert!(frame.find("Add account").unwrap() < frame.find("Connected accounts").unwrap(), "{frame}");
    assert!(frame.find("Connected accounts").unwrap() < frame.find("Personal").unwrap());
    assert!(frame.find("Personal").unwrap() < frame.find("Work").unwrap());
    assert!(row(&lines, "Personal").contains('●'));
    assert!(!row(&lines, "Work").contains('●'));
    assert!(!frame.contains("OPENAI_API_KEY"));
    // The add row hides the account actions; they show on an account.
    assert!(!frame.contains("rename") && !frame.contains("delete"), "{frame}");
    a.on_key(Stroke::ctrl("r"));
    a.on_key(Stroke::ctrl("d"));
    assert!(render(&mut a, 100, 30).join("\n").contains("Connected accounts"));
    a.on_key(Stroke::key("down"));
    let lines = render(&mut a, 100, 30);
    let r = row(&lines, "rename");
    assert_eq!(col(r, "rename ctrl+r  delete ctrl+d"), 24, "{r:?}");
    a.on_key(Stroke::key("up"));
    let frame = render(&mut a, 100, 30).join("\n");
    assert!(!frame.contains("rename") && !frame.contains("delete"), "{frame}");
}

#[test]
fn delete_asks_for_a_second_press_and_moving_cancels_it() {
    let mut a = app();
    a.store.location_mut("/work/proj").integrations = Some(vec![openai_with_accounts()]);
    a.open_integrations(false, Some("openai".into()), true);
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::ctrl("d"));
    let lines = render(&mut a, 100, 30);
    let r = row(&lines, "again to confirm");
    assert_eq!(inner(r), "Press ctrl+d again to confirm");
    let t = a.theme().dialog().clone();
    let buf = frame(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("again to confirm")).unwrap() as u16;
    assert_eq!(buf[(30, y)].bg, t.background.action.destructive.focused.tui());
    assert_eq!(buf[(30, y)].fg, t.text.action.destructive.focused.tui());
    a.on_key(Stroke::key("up"));
    let frame = render(&mut a, 100, 30).join("\n");
    assert!(!frame.contains("again to confirm") && frame.contains("Work"), "{frame}");
}

#[test]
fn methods_list_api_keys_last_and_a_single_method_opens_directly() {
    let mut a = app();
    a.store.location_mut("/work/proj").integrations = Some(catalog());
    a.run_command("provider.connect", None);
    for c in "console".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(row(&lines, "Connect OpenCode Console")), format!("Connect OpenCode Console{}esc", " ".repeat(25)));
    let y = lines.iter().position(|l| l.contains("OpenCode Console account")).unwrap();
    assert_eq!(inner(&lines[y + 1]), "API key (service account)");
    a.on_key(Stroke::key("escape"));
    a.run_command("provider.connect", None);
    for c in "anthropic".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    // `Connect Anthropic`, then the input right under the blank row (no description box).
    let y = lines.iter().position(|l| l.contains("Connect Anthropic")).unwrap();
    assert_eq!(inner(&lines[y + 2]), "API key", "{}", lines.join("\n"));
    assert_eq!(inner(&lines[y + 4]), "submit enter");
}

#[test]
fn text_fields_keep_an_empty_description_row_and_validate() {
    let mut a = app();
    let azure = json!({ "id": "azure", "name": "Azure", "methods": [{ "type": "key", "label": "API key", "form": [{ "key": "resourceName", "placeholder": "e.g. my-models", "required": true, "title": "Enter Azure Resource Name", "type": "string" }] }], "connections": [] });
    a.store.location_mut("/work/proj").integrations = Some(vec![azure]);
    a.open_integrations(false, Some("azure".into()), true);
    let lines = render(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("Enter Azure Resource Name")).unwrap();
    assert!(inner(&lines[y + 1]).is_empty() && inner(&lines[y + 2]).is_empty(), "{}", lines.join("\n"));
    assert_eq!(inner(&lines[y + 3]), "e.g. my-models");
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(&lines[y + 2]), "Answer required");
    assert_eq!(inner(&lines[y + 4]), "e.g. my-models");
    for c in "my-models".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    // Then the API key prompt, titled by the method's label.
    assert!(row(&lines, "API key").contains("esc"), "{}", lines.join("\n"));
}

#[test]
fn hidden_fields_take_their_defaults_and_when_conditions_need_an_answer() {
    let mut a = app();
    let copilot = json!({ "id": "github-copilot", "name": "GitHub Copilot", "methods": [{ "type": "oauth", "id": "device", "label": "Login with GitHub Copilot", "form": [
        { "key": "deploymentType", "options": [{ "description": "Public", "label": "GitHub.com", "value": "github.com" }, { "description": "Data residency or self-hosted", "label": "GitHub Enterprise", "value": "enterprise" }], "required": true, "title": "Select GitHub deployment type", "type": "string" },
        { "key": "enterpriseUrl", "placeholder": "company.ghe.com or https://company.ghe.com", "required": true, "title": "Enter your GitHub Enterprise URL or domain", "type": "string", "when": [{ "key": "deploymentType", "op": "eq", "value": "enterprise" }] },
        { "key": "server", "type": "string", "hidden": true, "default": "https://example.com" },
    ] }], "connections": [] });
    a.store.location_mut("/work/proj").integrations = Some(vec![copilot]);
    a.open_integrations(false, Some("github-copilot".into()), true);
    let lines = render(&mut a, 100, 30);
    assert_eq!(row(&lines, "GitHub.com").trim(), "GitHub.com Public");
    assert_eq!(row(&lines, "GitHub Enterprise").trim(), "GitHub Enterprise Data residency or self-hosted");
    // github.com skips the enterprise URL; the hidden field adds its default.
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| l.trim() == "Starting authorization…"), "{}", lines.join("\n"));
}

#[test]
fn choice_fields_number_their_rows() {
    let mut a = app();
    let bedrock = json!({ "id": "amazon-bedrock", "name": "Amazon Bedrock", "methods": [{ "type": "external", "id": "aws-profile", "label": "AWS profile (SSO or named profile)", "form": [
        { "custom": true, "description": "No profiles found in your AWS configuration.", "key": "profile", "minLength": 1, "options": [], "pattern": "\\S", "placeholder": "Profile name", "required": true, "title": "AWS profile", "type": "string" }
    ] }], "connections": [] });
    a.store.location_mut("/work/proj").integrations = Some(vec![bedrock]);
    a.open_integrations(false, Some("amazon-bedrock".into()), true);
    let lines = render(&mut a, 100, 30);
    // Upstream 2.1.4 at 100x30.
    let y = lines.iter().position(|l| l.contains("AWS profile")).unwrap();
    assert_eq!(lines[y].trim_end(), "                       AWS profile                                        esc");
    assert_eq!(inner(&lines[y + 2]), "No profiles found in your AWS configuration.");
    assert_eq!(inner(&lines[y + 4]), "1. Type your own answer");
    assert_eq!(inner(&lines[y + 6]), "select ↑/↓  confirm enter  cancel esc");
    a.on_key(Stroke::key("return"));
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(&lines[y + 4]), "1. Profile name");
    assert_eq!(inner(&lines[y + 6]), "Answer required");
    assert_eq!(inner(&lines[y + 8]), "submit enter  back esc");
    for c in "dev".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(&lines[y + 4]), "1. dev");
    a.on_key(Stroke::key("escape"));
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(&lines[y + 4]), "1. dev");
    assert_eq!(inner(&lines[y + 6]), "select ↑/↓  confirm enter  cancel esc");
    a.on_key(Stroke::key("escape"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn the_sidebar_offers_the_getting_started_card_until_something_connects() {
    let mut a = app();
    super::tests::session(&mut a);
    a.store.location_mut("/work/proj").integrations = Some(catalog().into_iter().take(1).collect());
    let lines = render(&mut a, 170, 40);
    let joined = lines.join("\n");
    // Upstream 2.1.4: the card ends two rows above the location, with its notes wrapped in the sidebar.
    let y = lines.iter().position(|l| l.contains("Getting started")).unwrap_or_else(|| panic!("{joined}"));
    let trimmed: Vec<String> = lines[y..y + 10].iter().map(|l| l.split("  ").filter(|p| !p.trim().is_empty()).last().unwrap_or("").trim().to_string()).collect();
    assert!(lines[y].trim_end().ends_with("⬖ Getting started                ✕"), "{joined}");
    assert_eq!(trimmed[2..4], ["OpenCode includes free models so", "you can start immediately."], "{joined}");
    assert_eq!(trimmed[5..8], ["Connect from 75+ providers to", "use other models, including", "Claude, GPT, Gemini etc"], "{joined}");
    assert!(lines[y + 9].trim_end().ends_with("Connect provider        /connect"), "{joined}");
    // Under 22 rows, or once dismissed, it is gone.
    assert!(!render(&mut a, 170, 21).join("\n").contains("Getting started"));
    a.onboarding_click("dismiss");
    assert!(!render(&mut a, 170, 40).join("\n").contains("Getting started"));
    let _ = std::fs::remove_dir_all(&a.keyed.dir);
}

#[test]
fn a_prompt_without_a_model_opens_the_integrations() {
    let mut a = app();
    a.store.location_mut("/work/proj").integrations = Some(catalog().into_iter().take(1).collect());
    a.store.location_mut("/work/proj").models = Some(vec![]);
    a.default_model = None;
    a.editor.insert("hello");
    a.submit("steer");
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Connect an integration to send prompts"));
    assert_eq!(a.dialogs.last().and_then(|d| match &d.body {
        crate::ui::dialog::Body::Select(s) => Some(s.title.clone()),
        _ => None,
    }), Some("Connect an integration".to_string()));
}

#[test]
fn an_mcp_server_that_needs_a_sign_in_enters_its_integration() {
    let mut a = app();
    let loc = a.store.location_mut("/work/proj");
    loc.mcp = Some(vec![
        McpServer { name: "linear".into(), status: McpStatus { status: "needs_auth".into(), error: Some("Sign in".into()) }, integration_id: Some("mcp-linear".into()) },
        McpServer { name: "local".into(), status: McpStatus { status: "needs_auth".into(), error: None }, integration_id: None },
    ]);
    loc.integrations = Some(vec![json!({ "id": "mcp-linear", "name": "Linear", "metadata": { "source": "mcp" }, "methods": [{ "type": "oauth", "id": "oauth", "label": "Sign in to Linear" }], "connections": [] })]);
    a.run_command("mcp.list", None);
    a.on_key(Stroke::key("return"));
    // One method: straight to its sign-in, without the integration list.
    assert_eq!(a.integration_top_step(), Some("waiting"));
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| inner(l) == format!("Sign in to Linear{}esc", " ".repeat(54 - 17 - 3))), "{}", lines.join("\n"));
    // A server without an integration keeps the old behavior.
    a.dialogs.clear();
    assert!(!a.mcp_sign_in("local"));
}

// ---- flows against a mock server --------------------------------------------------------------

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    query: String,
    body: Value,
}

#[derive(Default)]
struct MockState {
    accounts: Vec<Value>,
    form: Option<Value>,
    log: Vec<Req>,
    /// Statuses handed out by the OAuth status route, in order (the last repeats).
    oauth: Vec<Value>,
    oauth_mode: String,
    command: Vec<Value>,
    reads: std::collections::HashMap<String, usize>,
}

type Shared = Arc<Mutex<MockState>>;

fn openai(state: &MockState) -> Value {
    let mut connections = state.accounts.clone();
    connections.push(json!({ "type": "env", "name": "OPENAI_API_KEY" }));
    let mut key = json!({ "type": "key", "label": "API key" });
    if let Some(f) = &state.form {
        key["form"] = f.clone();
    }
    json!([
        { "id": "openai", "name": "OpenAI", "methods": [key], "connections": connections },
        { "id": "device", "name": "Device", "methods": [{ "type": "oauth", "id": "device", "label": "Device login" }], "connections": [] },
        { "id": "paste", "name": "Paste", "methods": [{ "type": "oauth", "id": "paste", "label": "Paste a code" }], "connections": [] },
        { "id": "cmd", "name": "Cmd", "methods": [{ "type": "command", "id": "login", "label": "Log in", "command": ["true"] }], "connections": [] },
    ])
}

fn route(state: &Shared, method: &str, path: &str, query: &str, body: Value) -> (u16, Value) {
    let mut s = state.lock().unwrap();
    let envelope = |data: Value| json!({ "location": { "directory": "/work/proj" }, "data": data });
    s.log.push(Req { method: method.into(), path: path.into(), query: query.into(), body: body.clone() });
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["api", "integration"]) => {
            *s.reads.entry("integration".into()).or_default() += 1;
            (200, envelope(openai(&s)))
        }
        ("GET", ["api", "model"]) | ("GET", ["api", "provider"]) => {
            *s.reads.entry(parts[1].to_string()).or_default() += 1;
            (200, envelope(json!([])))
        }
        ("POST", ["api", "integration", _, "connect", "key"]) => (204, Value::Null),
        ("POST", ["api", "integration", id, "connect", "oauth"]) => (
            200,
            envelope(json!({ "attemptID": "att_1", "url": format!("https://example.test/{id}"), "instructions": "Enter code: ABCD-12345", "mode": s.oauth_mode, "time": { "created": 1, "expires": 2 } })),
        ),
        ("GET", ["api", "integration", _, "connect", "oauth", _]) => {
            let next = if s.oauth.len() > 1 { s.oauth.remove(0) } else { s.oauth.first().cloned().unwrap_or(json!({ "status": "pending" })) };
            (200, envelope(next))
        }
        ("DELETE", ["api", "integration", _, "connect", ..]) => (204, Value::Null),
        ("POST", ["api", "integration", _, "connect", "oauth", _, "complete"]) => {
            if body["code"] == "bad" {
                (400, json!({ "_tag": "InvalidRequestError", "message": "Authentication failed" }))
            } else {
                (204, Value::Null)
            }
        }
        ("POST", ["api", "integration", _, "connect", "command"]) => (200, envelope(json!({ "attemptID": "cmd_1", "time": { "created": 1, "expires": 2 } }))),
        ("GET", ["api", "integration", _, "connect", "command", _]) => {
            let next = if s.command.len() > 1 { s.command.remove(0) } else { s.command.first().cloned().unwrap_or(json!({ "status": "pending" })) };
            (200, envelope(next))
        }
        ("POST", ["api", "credential", id, "activate"]) => {
            let id = id.to_string();
            if let Some(i) = s.accounts.iter().position(|a| a["id"] == id.as_str()) {
                let a = s.accounts.remove(i);
                s.accounts.insert(0, a);
            }
            (204, Value::Null)
        }
        ("PATCH", ["api", "credential", id]) => {
            let id = id.to_string();
            for a in s.accounts.iter_mut().filter(|a| a["id"] == id.as_str()) {
                a["label"] = body["label"].clone();
            }
            (204, Value::Null)
        }
        ("DELETE", ["api", "credential", id]) => {
            let id = id.to_string();
            s.accounts.retain(|a| a["id"] != id.as_str());
            (204, Value::Null)
        }
        _ => (404, json!({ "_tag": "NotFound", "message": format!("no route {method} {path}") })),
    }
}

async fn serve(state: Shared) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let state = state.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 8192];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[end + 4..].to_vec());
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let mut it = line.split(' ');
                let method = it.next().unwrap_or("").to_string();
                let target = it.next().unwrap_or("").to_string();
                let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target, String::new()));
                let path = path.replace("%2F", "/");
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, out) = route(&state, &method, &path, &query, body);
                let text = if out.is_null() { String::new() } else { out.to_string() };
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

/// Apply queued results until `done` holds (or the timeout passes).
async fn drive_until(a: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, ms: u64, done: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    while !done(a) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(AppMsg::Apply(f))) => f(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
}

async fn flow_app(state: &Shared) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
    let url = serve(state.clone()).await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    let list = a.client.integrations(Some("/work/proj")).await.unwrap();
    a.store.location_mut("/work/proj").integrations = Some(list);
    state.lock().unwrap().reads.clear();
    (a, rx)
}

fn accounts() -> Vec<Value> {
    vec![
        json!({ "type": "credential", "method": "key", "id": "cred_personal", "label": "Personal" }),
        json!({ "type": "credential", "method": "key", "id": "cred_work", "label": "Work" }),
    ]
}

fn requests(state: &Shared) -> Vec<(String, String)> {
    state.lock().unwrap().log.iter().filter(|r| !(r.method == "GET" && matches!(r.path.as_str(), "/api/integration" | "/api/model" | "/api/provider"))).map(|r| (r.method.clone(), r.path.clone())).collect()
}

fn event(kind: &str, data: Value) -> Event {
    Event { id: format!("evt_{}", crate::util::id_body(false)), created: 1, kind: kind.into(), data, location: Some(LocationRef { directory: "/work/proj".into(), workspace_id: None }), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opens_the_key_prompt_from_the_add_row_and_sends_hidden_defaults() {
    let state: Shared = Arc::new(Mutex::new(MockState { accounts: accounts(), ..Default::default() }));
    state.lock().unwrap().form = Some(json!([
        { "type": "string", "key": "server", "title": "Console URL", "hidden": true, "default": "https://example.com/console" },
        { "type": "string", "key": "optional", "hidden": true },
    ]));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(false, Some("openai".into()), true);
    a.on_key(Stroke::key("return"));
    let frame = render(&mut a, 100, 30).join("\n");
    assert!(frame.contains("API key") && !frame.contains("Connected accounts") && !frame.contains("Console URL"), "{frame}");
    assert!(requests(&state).is_empty());
    for c in "test-key".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.as_ref().is_some_and(|t| t.message == "Connected OpenAI")).await;
    let log = state.lock().unwrap().log.clone();
    let key = log.iter().find(|r| r.path == "/api/integration/openai/connect/key").unwrap();
    assert_eq!(key.body, json!({ "key": "test-key", "answer": { "server": "https://example.com/console" } }));
    assert_eq!(key.query, "location%5Bdirectory%5D=%2Fwork%2Fproj");
    // Without `onConnected` the dialog closes; the catalogs were reloaded.
    assert!(a.dialogs.is_empty());
    let reads = state.lock().unwrap().reads.clone();
    assert_eq!((reads.get("integration"), reads.get("model"), reads.get("provider")), (Some(&1), Some(&1), Some(&1)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switches_rename_and_delete_accounts() {
    let state: Shared = Arc::new(Mutex::new(MockState { accounts: accounts(), ..Default::default() }));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(false, Some("openai".into()), true);

    // Selecting the active account does nothing.
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 300, |_| false).await;
    assert!(requests(&state).is_empty());

    // Enter on another account activates it; `credential.switched` moves the marker without a refetch.
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 2000, |_| requests(&state).len() == 1).await;
    assert_eq!(requests(&state), [("POST".to_string(), "/api/credential/cred_work/activate".to_string())]);
    assert_eq!(state.lock().unwrap().log.last().unwrap().query, "", "credential routes are not location scoped");
    a.on_event(event("credential.switched", json!({ "integrationID": "openai", "credentialID": "cred_work" })));
    drive_until(&mut a, &mut rx, 2000, |_| state.lock().unwrap().reads.contains_key("provider")).await;
    let lines = render(&mut a, 100, 30);
    assert!(row(&lines, "Work").contains('●') && !row(&lines, "Personal").contains('●'), "{}", lines.join("\n"));
    assert_eq!(state.lock().unwrap().reads.get("integration"), None);

    // Rename through a prefilled prompt; the account manager comes back.
    a.on_key(Stroke::ctrl("r"));
    match &a.dialogs.last().unwrap().body {
        crate::ui::dialog::Body::Prompt(p) => assert_eq!((p.title.as_str(), p.value.as_str()), ("Rename account", "Work")),
        other => panic!("{other:?}"),
    }
    for c in " Account".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 2000, |a| matches!(a.dialogs.last().map(|d| &d.body), Some(crate::ui::dialog::Body::Select(_)))).await;
    assert_eq!(state.lock().unwrap().log.iter().find(|r| r.method == "PATCH").unwrap().body, json!({ "label": "Work Account" }));
    a.on_event(event("credential.updated", json!({})));
    drive_until(&mut a, &mut rx, 2000, |a| titles(a).iter().any(|t| t == "Work Account")).await;

    // Delete needs a second press and keeps the manager while an account remains; renaming during the
    // confirmation renames the label, not the message.
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::ctrl("d"));
    a.on_key(Stroke::ctrl("r"));
    match &a.dialogs.last().unwrap().body {
        crate::ui::dialog::Body::Prompt(p) => assert_eq!(p.value, "Work Account"),
        other => panic!("{other:?}"),
    }
    a.on_key(Stroke::key("escape"));
    a.open_integrations(false, Some("openai".into()), true);
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("down"));
    let selected = a.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| s.selected_value());
    assert_eq!(selected.as_deref(), Some("cred_work"));
    a.on_key(Stroke::ctrl("d"));
    assert!(!requests(&state).iter().any(|r| r.0 == "DELETE"));
    a.on_key(Stroke::ctrl("d"));
    drive_until(&mut a, &mut rx, 2000, |_| requests(&state).iter().any(|r| r.0 == "DELETE")).await;
    a.on_event(event("credential.updated", json!({})));
    // Under load the list can reload once more after the entry is gone, so also wait for the selection to settle.
    drive_until(&mut a, &mut rx, 5000, |a| {
        !titles(a).iter().any(|t| t.starts_with("Work"))
            && matches!(a.dialogs.last().map(|d| &d.body), Some(crate::ui::dialog::Body::Select(s))
                if s.selected_value().as_deref() == Some("cred_personal"))
    })
    .await;
    let shown = render(&mut a, 100, 30).join("\n");
    assert!(shown.contains("Add account") && shown.contains("Personal") && !shown.contains("Work"), "{shown}");

    // Deleting the last account says so and closes.
    let selected = a.dialogs.last_mut().and_then(|d| d.sel()).and_then(|s| s.selected_value());
    assert_eq!(selected.as_deref(), Some("cred_personal"));
    a.on_key(Stroke::ctrl("d"));
    a.on_key(Stroke::ctrl("d"));
    drive_until(&mut a, &mut rx, 2000, |a| a.dialogs.is_empty()).await;
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Disconnected OpenAI"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_sign_in_shows_the_link_copies_the_code_and_cancels_when_closed() {
    let state: Shared = Arc::new(Mutex::new(MockState { oauth_mode: "auto".into(), ..Default::default() }));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(true, Some("device".into()), true);
    let lines = render(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("Device login")).unwrap();
    assert_eq!(inner(&lines[y + 2]), "Starting authorization…");
    assert!(inner(&lines[y + 3]).is_empty() && inner(&lines[y + 4]).is_empty());
    drive_until(&mut a, &mut rx, 2000, |a| a.integration_top_step() == Some("auto")).await;
    let lines = render(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("Device login")).unwrap();
    assert_eq!(inner(&lines[y + 2]), "https://example.test/device");
    assert_eq!(inner(&lines[y + 4]), "Enter code: ABCD-12345");
    assert_eq!(inner(&lines[y + 6]), "Waiting for authorization…");
    assert_eq!(inner(&lines[y + 8]), "open o  copy c");
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.on_key(Stroke::key("c"));
    assert_eq!(crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().clone()).as_deref(), Some("ABCD-12345"));
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Copied to clipboard"));
    a.on_key(Stroke::key("escape"));
    let _ = render(&mut a, 100, 30);
    drive_until(&mut a, &mut rx, 2000, |_| requests(&state).iter().any(|r| r.0 == "DELETE")).await;
    assert!(requests(&state).contains(&("DELETE".to_string(), "/api/integration/device/connect/oauth/att_1".to_string())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_sign_in_continues_to_the_models() {
    let state: Shared = Arc::new(Mutex::new(MockState { oauth_mode: "auto".into(), oauth: vec![json!({ "status": "pending" }), json!({ "status": "complete" })], ..Default::default() }));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(true, Some("device".into()), true);
    drive_until(&mut a, &mut rx, 4000, |a| a.toast.as_ref().is_some_and(|t| t.message == "Connected Device")).await;
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("model"));
    // Settled: closing the model list cancels nothing.
    a.dialogs.clear();
    let _ = render(&mut a, 100, 30);
    drive_until(&mut a, &mut rx, 300, |_| false).await;
    assert!(!requests(&state).iter().any(|r| r.0 == "DELETE"));

    // A failed or expired attempt closes with a toast.
    state.lock().unwrap().oauth = vec![json!({ "status": "failed", "message": "Denied by the user" })];
    a.open_integrations(true, Some("device".into()), true);
    drive_until(&mut a, &mut rx, 3000, |a| a.dialogs.is_empty()).await;
    assert_eq!(a.toast.as_ref().map(|t| (t.variant.as_str(), t.message.as_str())), Some(("error", "Denied by the user")));
    state.lock().unwrap().oauth = vec![json!({ "status": "expired" })];
    a.toast = None;
    a.open_integrations(true, Some("device".into()), true);
    drive_until(&mut a, &mut rx, 3000, |a| a.dialogs.is_empty()).await;
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Authorization expired"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_code_attempt_asks_for_the_code_and_shows_errors() {
    let state: Shared = Arc::new(Mutex::new(MockState { oauth_mode: "code".into(), ..Default::default() }));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(false, Some("paste".into()), true);
    drive_until(&mut a, &mut rx, 2000, |a| matches!(a.dialogs.last().map(|d| &d.body), Some(crate::ui::dialog::Body::Prompt(_)))).await;
    let lines = render(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("Paste a code")).unwrap();
    assert_eq!(inner(&lines[y + 2]), "Enter code: ABCD-12345");
    assert_eq!(inner(&lines[y + 4]), "https://example.test/paste");
    assert_eq!(inner(&lines[y + 6]), "Authorization code");
    for c in "bad".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 2000, |a| texts(a).contains("Authentication failed")).await;
    let lines = render(&mut a, 100, 30);
    assert_eq!(inner(&lines[y + 6]), "Authentication failed");
    assert_eq!(inner(&lines[y + 8]), "bad");
    a.on_key(Stroke::ctrl("u"));
    for c in "good".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("return"));
    drive_until(&mut a, &mut rx, 2000, |a| a.dialogs.is_empty()).await;
    assert_eq!(state.lock().unwrap().log.iter().filter(|r| r.path.ends_with("/complete")).map(|r| r.body.clone()).collect::<Vec<_>>(), [json!({ "code": "bad" }), json!({ "code": "good" })]);
    assert!(!requests(&state).iter().any(|r| r.0 == "DELETE"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_command_shows_its_output_until_it_finishes() {
    let state: Shared = Arc::new(Mutex::new(MockState {
        command: vec![json!({ "status": "pending" }), json!({ "status": "pending", "message": "Open the page to continue.\n" }), json!({ "status": "complete" })],
        ..Default::default()
    }));
    let (mut a, mut rx) = flow_app(&state).await;
    a.open_integrations(false, Some("cmd".into()), true);
    let lines = render(&mut a, 100, 30);
    let y = lines.iter().position(|l| l.contains("Log in")).unwrap();
    assert_eq!(col(&lines[y], "Log in"), 9);
    assert_eq!(col(&lines[y], "esc close") + "esc close".len(), 91);
    assert_eq!(lines[y + 6].trim(), "Starting command…");
    drive_until(&mut a, &mut rx, 3000, |a| texts(a).contains("Open the page")).await;
    let lines = render(&mut a, 100, 30);
    assert_eq!(lines[y + 3].trim(), "Open the page to continue.");
    assert_eq!(lines[y + 6].trim(), "Waiting for command to finish…");
    let t = a.theme().clone();
    let buf = frame(&mut a, 100, 30);
    assert_eq!(buf[(6, y as u16 + 2)].bg, t.background.raised.high.tui());
    drive_until(&mut a, &mut rx, 3000, |a| a.dialogs.is_empty()).await;
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Connected Cmd"));
}
