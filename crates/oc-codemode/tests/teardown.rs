//! Executions release their memory: closures, generators, and promise graphs form reference cycles that teardown
//! must break, so repeated runs keep a flat resident set.

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;

fn rss_kb() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    statm.split_whitespace().nth(1).and_then(|p| p.parse::<usize>().ok()).unwrap_or(0) * 4
}

#[tokio::test]
async fn repeated_executions_do_not_leak() {
    if !std::path::Path::new("/proc/self/statm").exists() {
        return;
    }
    let runtime = CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap();
    let code = r#"
        const items = []
        for (let i = 0; i < 1000; i++) {
          const o = { i, payload: [i, i + 1, i + 2, i + 3, i + 4, i + 5, i + 6, i + 7] }
          o.self = () => o
          items.push(o)
        }
        function* g() { yield items }
        const it = g(); it.next()
        const p = new Promise((r) => r(items))
        p.then(() => items)
        return items.length
    "#;
    for _ in 0..5 {
        runtime.execute(code).await;
    }
    let before = rss_kb();
    for _ in 0..20 {
        let r = runtime.execute(code).await;
        assert!(r.ok());
    }
    let after = rss_kb();
    // 20 runs × ~0.3 MB of retained objects each would add ~20 MB if cycles leaked.
    assert!(after < before + 4_000, "rss grew from {before} KB to {after} KB");
}
