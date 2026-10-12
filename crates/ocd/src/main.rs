//! `opencode` — the command line interface (port of `packages/cli`).

mod acp;
mod askpass;
mod cli;
mod client;
mod commands;
mod connection;
mod run;
mod runtime;
mod serve;
mod service;
mod term;
mod ui;

use cli::Outcome;

fn main() {
    // SSH askpass helper mode never parses CLI args (`index.ts:23-26`).
    if std::env::var_os("OPENCODE_SSH_ASKPASS_PORT").is_some_and(|v| !v.is_empty()) {
        std::process::exit(askpass::run());
    }
    let _ = connection::startup_dir();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let root = cli::spec::root();
    let outcome = cli::resolve(&root, &argv);

    // Internal: print the decoded handler input like the TS framework would (`tests/parse.rs`).
    if let Outcome::Run(inv) = &outcome
        && std::env::var("OPENCODE_CLI_PARSE_DUMP").as_deref() == Ok("1")
    {
        println!("HANDLER {} {}", inv.leaf(), inv.input.dump());
        return;
    }

    // `Global` (with `$OPENCODE_CONFIG_DIR` as config) is acquired for every command: it creates
    // the standard directories.
    let global = match oc_util::Global::acquire() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let inv = match outcome {
        Outcome::Run(inv) => inv,
        other => std::process::exit(cli::emit(&root, &other)),
    };
    if inv.print_logs {
        unsafe { std::env::set_var("OPENCODE_PRINT_LOGS", "1") };
    }
    let role = if inv.name == "serve" { "server" } else { "cli" };
    oc_util::log::init(&global.log_file(oc_util::version::CHANNEL), role, inv.log_level.as_deref());
    tracing::info!(
        version = oc_util::version::VERSION,
        channel = oc_util::version::CHANNEL,
        local = oc_util::version::local(),
        args = %serde_json::to_string(&argv).unwrap_or_default(),
        "cli starting"
    );

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = rt.block_on(async move {
        match commands::dispatch(&global, &inv).await {
            Ok(code) => code,
            Err(e) => {
                tracing::error!(cause = %e, args = %serde_json::to_string(&argv).unwrap_or_default(), "cli process failed");
                term::write_stderr(&format!("Error: {e}\n"));
                1
            }
        }
    });
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}
