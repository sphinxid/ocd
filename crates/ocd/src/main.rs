//! `opencode` — the command line interface (port of `packages/cli`).

mod acp;
mod askpass;
mod cli;
mod client;
mod commands;
mod connection;
mod remote_tunnel;
mod run;
mod runtime;
mod serve;
mod service;
mod term;
mod ui;
mod web_ui;

use cli::Outcome;

/// The ocd build, e.g. `1.2.0+abc1234` (release tag and commit): `OCD_BUILD_VERSION`, set by the release
/// workflow or worked out from `git describe` by build.rs. Only `--version` and the log show it; everything
/// opencode-compatible (user agent, plugins, service registration) keeps `oc_util::version::VERSION`.
pub const BUILD: Option<&str> = match option_env!("OCD_BUILD_VERSION") {
    Some(v) if !v.is_empty() => Some(v),
    _ => None,
};

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
    oc_util::panic::install();
    tracing::info!(
        version = oc_util::version::VERSION,
        build = BUILD.unwrap_or(""),
        channel = oc_util::version::CHANNEL,
        local = oc_util::version::local(),
        args = %serde_json::to_string(&argv).unwrap_or_default(),
        "cli starting"
    );

    // `serve --service` loads the service's saved environment itself (723a2f689): Desktop, systemd and
    // direct launches start it without the variables a CLI-spawned service inherits. Upstream merges it
    // after the incumbent check; nothing before that check reads these variables (its probe ignores
    // proxy settings, and an incumbent ends the process), so merging it here, before the runtime's
    // threads start, is equivalent.
    if inv.name == "serve" && inv.input.flag("service") && !inv.input.flag("stdio") {
        for (key, value) in service::ServiceConfig::new(&global).read().env.unwrap_or_default() {
            // SAFETY: single-threaded here except the log-trim thread, which never reads the environment.
            unsafe { std::env::set_var(key, value) };
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = rt.block_on(async move {
        match commands::dispatch(&global, &inv).await {
            Ok(code) => code,
            Err(e) => {
                tracing::error!(cause = %e, args = %serde_json::to_string(&argv).unwrap_or_default(), "cli process failed");
                if e.downcast_ref::<remote_tunnel::Inspected>().is_some() {
                    term::write_stderr(&format!("{e}\n"));
                } else {
                    term::write_stderr(&format!("Error: {e}\n"));
                }
                1
            }
        }
    });
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}
