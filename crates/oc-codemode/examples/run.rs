//! Runs a program from stdin (no tools) and prints the upstream-shaped result JSON.
//! Usage: `echo 'return 1 + 1' | cargo run --example run`
use std::io::Read;
use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;

fn main() {
    let mut code = String::new();
    std::io::stdin().read_to_string(&mut code).unwrap();
    let runtime = CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap();
    let result = runtime.execute_blocking(&code);
    println!("{}", serde_json::to_string(&result.to_json()).unwrap());
}
