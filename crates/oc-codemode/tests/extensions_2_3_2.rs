//! opencode 2.3.2 (#64516): an extension can name the modules that load its globals (upstream `extensions.test.ts`,
//! "imports").

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecResult, ExecutionLimits, RunOptions};
use oc_codemode::host::{Extension, GlobalsExtension, HostCall, HostFn, HostValue, Json, NoTools};
use serde_json::json;

fn runtime(extensions: Vec<Arc<dyn Extension>>) -> CodeMode {
    CodeMode::new(Vec::new(), Arc::new(NoTools), extensions, ExecutionLimits { timeout_ms: Some(10_000.0), ..Default::default() })
        .unwrap()
}

fn value(result: ExecResult) -> Json {
    match result {
        ExecResult::Success { value, .. } => value,
        ExecResult::Failure { error, .. } => panic!("expected success, got {:?}: {}", error.kind, error.message),
    }
}

/// `{ <global>: { <method>: () => <returns> } }` with `modules`.
fn extension(name: &str, global: &str, method: &str, returns: Option<&str>, modules: &[(&str, &str)]) -> Arc<dyn Extension> {
    let returns = returns.map(String::from);
    let function = HostFn::new(method.to_string(), 1, move |args| {
        HostCall::Ready(Ok(match &returns {
            Some(text) => HostValue::String(text.clone()),
            // `file(path) => ({ name: path })`
            None => HostValue::Object(vec![("name".into(), args.into_iter().next().unwrap_or(HostValue::Undefined))]),
        }))
    })
    .into_value();
    Arc::new(GlobalsExtension {
        name: name.into(),
        globals: vec![(global.into(), HostValue::Object(vec![(method.into(), function)]))],
        modules: modules.iter().map(|(module, global)| (module.to_string(), global.to_string())).collect(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extensions_module_names_load_its_globals_only_in_runs_that_get_the_extension() {
    let bun = extension("bun", "Bun", "file", None, &[("bun", "Bun")]);
    let target = runtime(vec![bun]);
    let code = r#"import Loaded, { file } from "bun"
         return [file("a").name, Loaded === Bun, typeof require("bun").file, typeof (await import("bun")).file]"#;
    assert_eq!(value(target.execute(code).await), json!(["a", true, "function", "function"]));

    let without = target.execute_with(r#"import { file } from "bun""#, &RunOptions { extensions: Some(vec![]) }).await.unwrap();
    let ExecResult::Failure { error, .. } = without else { panic!("expected a failure") };
    assert!(error.message.contains(r#"Cannot import "bun""#), "{}", error.message);

    let plain = runtime(Vec::new());
    let failed = plain.execute(r#"import { file } from "bun""#).await.to_json();
    assert!(failed["error"]["message"].as_str().unwrap().starts_with(r#"Error: Cannot import "bun""#), "{failed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_extensions_module_name_wins() {
    let one = extension("one", "A", "id", Some("a"), &[("shared", "A")]);
    let two = extension("two", "B", "id", Some("b"), &[("shared", "B")]);
    assert_eq!(value(runtime(vec![one, two]).execute(r#"return require("shared").id()"#).await), json!("b"));
}
