//! The command tree (`cli/src/commands/commands.ts`), declared as data so the Effect-CLI
//! compatible parser (`parse.rs`) and help renderer (`help.rs`) can walk it.

/// Value primitive of a flag or argument (`effect/cli` `Primitive`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Prim {
    Bool,
    Str,
    /// `Flag.Int`, optionally checked (`Schema.Int.check(...)`).
    Int(IntCheck),
    Choice(&'static [&'static str]),
    /// `Flag.KeyValuePair` — collected into one record.
    KeyValue,
    /// `Flag.String` + `Schema.NonEmptyString`.
    NonEmpty,
    /// `pair --url` (`Flag.mapTryCatch` URL validation).
    PairUrl,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntCheck {
    None,
    Min(i64),
    Between(i64, i64),
}

/// How many values a flag yields (`Flag.withDefault`, `Flag.optional`, `Flag.atMost`, ...).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arity {
    /// Boolean with default false / string with default.
    Default(&'static str),
    Optional,
    /// `Flag.atMost(n)` / `Flag.atLeast(0)`: every occurrence.
    Many,
}

#[derive(Debug, Clone)]
pub struct FlagDef {
    /// Input key (TS property name).
    pub key: &'static str,
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub description: Option<&'static str>,
    pub prim: Prim,
    pub arity: Arity,
    pub hidden: bool,
    /// Help metavar overriding the type name (`Flag.withMetavar`).
    pub metavar: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArgMode {
    Required,
    Optional,
    Variadic { min: usize, max: Option<usize> },
}

#[derive(Debug, Clone)]
pub struct ArgDef {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub prim: Prim,
    pub mode: ArgMode,
}

#[derive(Debug, Clone)]
pub enum Param {
    Flag(FlagDef),
    Arg(ArgDef),
}

#[derive(Debug, Clone)]
pub struct Cmd {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub description: &'static str,
    pub params: Vec<Param>,
    pub subcommands: Vec<Cmd>,
    /// Whether a handler is registered (`$` for groups).
    pub handler: bool,
}

impl Cmd {
    pub fn flags(&self) -> impl Iterator<Item = &FlagDef> {
        self.params.iter().filter_map(|p| match p {
            Param::Flag(f) => Some(f),
            _ => None,
        })
    }
    pub fn args(&self) -> impl Iterator<Item = &ArgDef> {
        self.params.iter().filter_map(|p| match p {
            Param::Arg(a) => Some(a),
            _ => None,
        })
    }
    pub fn find(&self, name: &str) -> Option<&Cmd> {
        self.subcommands.iter().find(|c| c.name == name || c.alias == Some(name))
    }
}

// ---------------------------------------------------------------------------------------------
// builders

fn flag(key: &'static str, name: &'static str, prim: Prim, arity: Arity) -> FlagDef {
    FlagDef { key, name, alias: None, description: None, prim, arity, hidden: false, metavar: None }
}

impl FlagDef {
    fn alias(mut self, a: &'static str) -> Self {
        self.alias = Some(a);
        self
    }
    fn desc(mut self, d: &'static str) -> Self {
        self.description = Some(d);
        self
    }
    fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
    pub fn metavar(mut self, m: &'static str) -> Self {
        self.metavar = Some(m);
        self
    }
    fn p(self) -> Param {
        Param::Flag(self)
    }
}

fn boolean(key: &'static str, name: &'static str) -> FlagDef {
    flag(key, name, Prim::Bool, Arity::Default("false"))
}
fn string(key: &'static str, name: &'static str) -> FlagDef {
    flag(key, name, Prim::Str, Arity::Optional)
}
fn arg(key: &'static str, name: &'static str, description: &'static str, mode: ArgMode) -> Param {
    Param::Arg(ArgDef { key, name, description, prim: Prim::Str, mode })
}

fn cmd(name: &'static str, description: &'static str, params: Vec<Param>) -> Cmd {
    Cmd { name, alias: None, description, params, subcommands: vec![], handler: true }
}
fn group(name: &'static str, description: &'static str, subcommands: Vec<Cmd>) -> Cmd {
    Cmd { name, alias: None, description, params: vec![], subcommands, handler: false }
}

fn server_params() -> Vec<Param> {
    vec![
        boolean("standalone", "standalone").desc("Run with a private server instead of the background service").p(),
        string("server", "server").desc("Connect to a server URL instead of the background service").p(),
    ]
}

fn permission_params() -> Vec<Param> {
    vec![
        boolean("auto", "auto").desc("Auto-approve permissions that are not explicitly denied").p(),
        boolean("yolo", "yolo").hidden().p(),
        boolean("dangerouslySkipPermissions", "dangerously-skip-permissions").hidden().p(),
    ]
}

fn with(mut a: Vec<Param>, b: Vec<Param>) -> Vec<Param> {
    a.extend(b);
    a
}

/// The executable's own name (`OPENCODE_CLI_NAME` build constant upstream).
pub const CLI_NAME: &str = "ocd";

/// ocd's own `upgrade` texts (upstream upgrades through an install method; ocd installs its GitHub releases).
pub const UPGRADE_DESCRIPTION: &str = "Upgrade ocd to the latest GitHub release, or to the given version";
pub const METHOD_DESCRIPTION: &str = "Ignored: ocd installs only from its GitHub releases";
pub const YES_DESCRIPTION: &str = "Replace a local build without asking (required without an interactive terminal)";

pub const UPDATER_METHODS: &[&str] = &["curl", "npm", "pnpm", "bun", "yarn", "vp", "brew", "winget"];
pub const PATH_NAMES: &[&str] = &["db", "home", "data", "config", "cache", "state", "tmp", "bin", "log", "repos"];

/// Global flags (`GlobalFlag.BuiltIns` + the app's `--print-logs`), in help order.
pub fn global_flags() -> Vec<FlagDef> {
    vec![
        boolean("help", "help").alias("h").desc("Show help information"),
        boolean("version", "version").alias("v").desc("Show version information"),
        boolean("wizard", "wizard").desc("Start wizard mode for a command"),
        flag("completions", "completions", Prim::Choice(&["bash", "zsh", "fish", "sh"]), Arity::Optional)
            .metavar("<bash|zsh|fish|sh>")
            .desc("Print shell completion script"),
        flag(
            "logLevel",
            "log-level",
            Prim::Choice(&["all", "trace", "debug", "info", "warn", "warning", "error", "fatal", "none"]),
            Arity::Optional,
        )
        .desc("Sets the minimum log level")
        .metavar("<all|trace|debug|info|warn|warning|error|fatal|none>"),
        boolean("printLogs", "print-logs").desc("Print logs to stderr (server logs require --standalone)"),
    ]
}

/// The full command tree.
pub fn root() -> Cmd {
    let continue_flag = || boolean("continue", "continue").alias("c").desc("Continue the last session").p();
    let session_flag =
        || string("session", "session").alias("s").desc("Session ID to continue, or to create if it does not exist").p();
    let mut root = cmd(
        CLI_NAME,
        "ocd command line interface",
        with(
            with(server_params(), permission_params()),
            vec![
                arg("directory", "directory", "Directory to start OpenCode in", ArgMode::Optional),
                continue_flag(),
                session_flag(),
                string("prompt", "prompt").desc("Prompt to use").p(),
            ],
        ),
    );
    let mut upgrade = cmd(
        "upgrade",
        UPGRADE_DESCRIPTION,
        vec![
            arg("target", "target", "Version to upgrade to (with or without a leading v)", ArgMode::Optional),
            flag("method", "method", Prim::Choice(UPDATER_METHODS), Arity::Optional)
                .alias("m")
                .desc(METHOD_DESCRIPTION)
                .p(),
            boolean("yes", "yes").alias("y").desc(YES_DESCRIPTION).p(),
        ],
    );
    upgrade.alias = Some("update");
    let paths_arg = Param::Arg(ArgDef {
        key: "name",
        name: "name",
        description: "Print only one path: db, home, data, config, cache, state, tmp, bin, log, repos",
        prim: Prim::Choice(PATH_NAMES),
        mode: ArgMode::Optional,
    });
    let format = |choices: &'static [&'static str], default: &'static str| {
        flag("format", "format", Prim::Choice(choices), Arity::Default(default)).desc("Output format").p()
    };
    root.subcommands = vec![
        upgrade,
        cmd(
            "uninstall",
            "Uninstall OpenCode, keeping session data, configuration, and state",
            vec![
                boolean("dryRun", "dry-run").desc("Show what would be removed without removing").p(),
                boolean("force", "force").alias("f").desc("Skip confirmation prompts").p(),
            ],
        ),
        cmd("acp", "Start an Agent Client Protocol server", vec![
            boolean("login", "login").desc("Run auth login instead of starting the server").p(),
        ]),
        cmd(
            "api",
            "Make a request to the running server",
            with(
                server_params(),
                vec![
                    arg(
                        "request",
                        "operation | method path",
                        "OpenAPI operation ID, or an HTTP method followed by a path",
                        ArgMode::Variadic { min: 1, max: Some(2) },
                    ),
                    string("data", "data").alias("d").desc("Request body").p(),
                    flag("header", "header", Prim::Str, Arity::Many).alias("H").desc("Request header in name:value form").p(),
                    flag("param", "param", Prim::KeyValue, Arity::Optional).desc("OpenAPI path or query parameter").p(),
                ],
            ),
        ),
        group(
            "debug",
            "Debugging and troubleshooting tools",
            vec![
                cmd("agents", "List all agents", vec![]),
                cmd("config", "List configuration sources", vec![]),
                cmd(
                    "heap-dump",
                    "Write a heap snapshot of the running server and print its path",
                    vec![
                        string("directory", "directory")
                            .desc("Directory to write the snapshot into instead of the server's log directory")
                            .p(),
                        string("server", "server").desc("Connect to a server URL instead of the background service").p(),
                    ],
                ),
                cmd("paths", "Show global paths (data, config, cache, state)", vec![paths_arg]),
            ],
        ),
        group(
            "auth",
            "manage integrations and credentials",
            vec![
                cmd("list", "list integrations and credentials", with(server_params(), vec![format(&["default", "json"], "default")])),
                cmd(
                    "login",
                    "connect an integration",
                    with(
                        server_params(),
                        vec![
                            arg("target", "target", "Integration ID, name, or well-known provider URL", ArgMode::Optional),
                            string("method", "method").desc("Authentication method ID").p(),
                            flag("answer", "answer", Prim::Str, Arity::Many)
                                .desc("Provider form answer (key=value; repeat for multiple fields)")
                                .p(),
                        ],
                    ),
                ),
                cmd(
                    "logout",
                    "log out of a saved account",
                    with(
                        server_params(),
                        vec![
                            arg("target", "target", "Integration ID or name", ArgMode::Optional),
                            arg(
                                "credential",
                                "credential",
                                "Credential ID or label (opens an account picker when omitted)",
                                ArgMode::Optional,
                            ),
                        ],
                    ),
                ),
                cmd(
                    "export",
                    "print stored credentials, including secrets, as JSON",
                    with(
                        server_params(),
                        vec![arg(
                            "target",
                            "target",
                            "Integration ID or name (exports every integration when omitted)",
                            ArgMode::Optional,
                        )],
                    ),
                ),
                cmd(
                    "import",
                    "import credentials exported by auth export",
                    with(
                        server_params(),
                        vec![arg("file", "file", "JSON file to import (reads stdin when omitted)", ArgMode::Optional)],
                    ),
                ),
                cmd(
                    "switch",
                    "switch the active account for an integration",
                    with(
                        server_params(),
                        vec![
                            arg("target", "target", "Integration ID or name", ArgMode::Optional),
                            arg(
                                "credential",
                                "credential",
                                "Credential ID or label (opens an account picker when omitted)",
                                ArgMode::Optional,
                            ),
                        ],
                    ),
                ),
            ],
        ),
        group(
            "mcp",
            "Manage MCP (Model Context Protocol) servers",
            vec![
                cmd("list", "List configured MCP servers and their status", vec![]),
                cmd(
                    "add",
                    "Add an MCP server to your configuration",
                    vec![
                        arg("name", "name", "Name of the MCP server", ArgMode::Required),
                        arg(
                            "command",
                            "command",
                            "Command and arguments for a local server, passed after --",
                            ArgMode::Variadic { min: 0, max: None },
                        ),
                        string("url", "url").desc("URL for a remote MCP server").p(),
                        flag("header", "header", Prim::KeyValue, Arity::Optional)
                            .desc("HTTP header for a remote server, as name=value")
                            .p(),
                        flag("env", "env", Prim::KeyValue, Arity::Optional)
                            .desc("Environment variable for a local server, as name=value")
                            .p(),
                        boolean("global", "global").desc("Write to the global config instead of the project config").p(),
                    ],
                ),
                cmd("auth", "Authenticate with an OAuth-capable remote MCP server", vec![arg(
                    "name",
                    "name",
                    "Name of the MCP server",
                    ArgMode::Optional,
                )]),
                cmd("logout", "Remove stored OAuth credentials for an MCP server", vec![arg(
                    "name",
                    "name",
                    "Name of the MCP server",
                    ArgMode::Required,
                )]),
            ],
        ),
        group(
            "plugin",
            "Manage plugins",
            vec![
                cmd("list", "List plugins", vec![boolean("builtin", "builtin").desc("Include built-in server plugins").p()]),
                cmd("add", "Install a plugin and add it to the global configuration", vec![arg(
                    "package",
                    "package",
                    "npm registry or Git package specifier",
                    ArgMode::Required,
                )]),
                cmd("check", "Check package plugins for updates", vec![arg(
                    "target",
                    "target",
                    "Configured package target",
                    ArgMode::Optional,
                )]),
                cmd("update", "Update package plugins", vec![arg(
                    "target",
                    "target",
                    "Configured package target; omit to update all outdated plugins",
                    ArgMode::Optional,
                )]),
                cmd("remove", "Remove a plugin from global configuration", vec![arg(
                    "package",
                    "package",
                    "configured package specifier",
                    ArgMode::Required,
                )]),
            ],
        ),
        cmd("models", "List all available models", server_params()),
        cmd(
            "stats",
            "Show shareable usage statistics",
            with(
                server_params(),
                vec![
                    flag("days", "days", Prim::Int(IntCheck::Min(0)), Arity::Optional).desc("Show the last N days; 0 means today").p(),
                    flag("year", "year", Prim::Int(IntCheck::Between(1970, 9999)), Arity::Optional).desc("Show a calendar year").p(),
                    boolean("all", "all").desc("Show lifetime statistics").p(),
                    string("project", "project").desc("Filter by project ID, or use \".\" for the current project").p(),
                    boolean("models", "models").desc("Show model usage").p(),
                    boolean("tools", "tools").desc("Show tool reliability").p(),
                    boolean("cost", "cost").desc("Show cost and token details").p(),
                    boolean("full", "full").desc("Show every detailed section").p(),
                    flag("limit", "limit", Prim::Int(IntCheck::Min(1)), Arity::Default("5"))
                        .desc("Number of rows in detailed sections")
                        .p(),
                    boolean("json", "json").desc("Output statistics as JSON").p(),
                ],
            ),
        ),
        cmd(
            "mini",
            "Start the minimal interactive interface",
            with(
                server_params(),
                vec![
                    continue_flag(),
                    session_flag(),
                    boolean("fork", "fork").desc("Fork the session when continuing").p(),
                    flag("replay", "replay", Prim::Bool, Arity::Optional)
                        .desc("Restore session history on resume and resize (disable with --no-replay)")
                        .p(),
                    flag("replayLimit", "replay-limit", Prim::Int(IntCheck::None), Arity::Optional)
                        .desc("Limit replay to the newest N messages (default: 200)")
                        .p(),
                    string("model", "model").alias("m").desc("Model to use in the format provider/model").p(),
                    string("agent", "agent").desc("Agent to use").p(),
                    string("prompt", "prompt").desc("Prompt to use").p(),
                    boolean("demo", "demo").hidden().p(),
                ],
            ),
        ),
        cmd(
            "run",
            "Run OpenCode with a message",
            with(
                with(
                    server_params(),
                    vec![
                        arg("message", "message", "Message to send", ArgMode::Variadic { min: 0, max: None }),
                        continue_flag(),
                        session_flag(),
                        boolean("fork", "fork").desc("Fork the session before continuing").p(),
                        string("model", "model").alias("m").desc("Model to use in the format provider/model#variant").p(),
                        string("agent", "agent").desc("Agent to use").p(),
                        format(&["default", "json"], "default"),
                        flag("file", "file", Prim::Str, Arity::Many).alias("f").desc("File to attach to the message").p(),
                        string("title", "title").desc("Session title").p(),
                        boolean("thinking", "thinking").desc("Show thinking blocks").p(),
                    ],
                ),
                permission_params(),
            ),
        ),
        group(
            "session",
            "Manage sessions",
            vec![
                cmd(
                    "list",
                    "List top-level sessions in the current project, newest first",
                    with(
                        server_params(),
                        vec![
                            flag("maxCount", "max-count", Prim::Int(IntCheck::Min(1)), Arity::Optional)
                                .alias("n")
                                .desc("Limit to N most recent sessions (default: 100)")
                                .p(),
                            format(&["table", "json"], "table"),
                        ],
                    ),
                ),
                cmd(
                    "delete",
                    "Delete a session and its child sessions",
                    with(server_params(), vec![arg("sessionID", "sessionID", "Session ID to delete", ArgMode::Required)]),
                ),
                cmd(
                    "export",
                    "Export session data as JSON",
                    with(
                        server_params(),
                        vec![
                            arg("session", "session", "Session ID to export", ArgMode::Optional),
                            boolean("sanitize", "sanitize").desc("Redact sensitive transcript and file data").p(),
                        ],
                    ),
                ),
                cmd(
                    "import",
                    "Import session data from a JSON file or URL",
                    with(
                        server_params(),
                        vec![
                            arg("file", "file", "JSON file or URL to import", ArgMode::Required),
                            string("directory", "directory").desc("Directory in which to import the session").p(),
                        ],
                    ),
                ),
            ],
        ),
        group(
            "service",
            "Manage the background server",
            vec![
                cmd("start", "Start the background server", vec![]),
                cmd("restart", "Restart the background server", vec![]),
                cmd("status", "Show background server status", vec![]),
                cmd("stop", "Stop the background server", vec![]),
                cmd("get", "Get service configuration", vec![
                    arg("key", "key", "Service setting or env", ArgMode::Optional),
                    arg("name", "name", "Environment variable name", ArgMode::Optional),
                ]),
                cmd("set", "Set service configuration", vec![
                    arg("key", "key", "Service setting or env", ArgMode::Required),
                    arg("value", "value", "Setting value or environment variable name", ArgMode::Required),
                    arg("nestedValue", "env-value", "Environment variable value", ArgMode::Optional),
                ]),
                cmd("unset", "Unset service configuration", vec![
                    arg("key", "key", "Service setting or env", ArgMode::Required),
                    arg("name", "name", "Environment variable name", ArgMode::Optional),
                ]),
            ],
        ),
        cmd("reload", "Reload configuration", server_params()),
        cmd("pair", "Print one-time links to connect a browser or app", vec![
            flag("url", "url", Prim::PairUrl, Arity::Optional).desc("Use an external HTTP(S) server URL in pairing links").p(),
            boolean("remote", "remote")
                .desc("Pair through the OpenTunnel remote address, enabling remote access if needed (exposes the server to the internet)")
                .p(),
            boolean("yes", "yes")
                .alias("y")
                .desc("Accept exposing the server to the internet when --remote enables remote access for the first time (required without an interactive terminal)")
                .p(),
        ]),
        cmd("serve", "Start the v2 API and web server", vec![
            string("hostname", "hostname").p(),
            flag("port", "port", Prim::Int(IntCheck::None), Arity::Optional).p(),
            flag("cors", "cors", Prim::NonEmpty, Arity::Many)
                .desc("Additional allowed CORS origin (repeat for multiple origins)")
                .p(),
            boolean("service", "service").p(),
            boolean("stdio", "stdio").p(),
        ]),
    ];
    root
}
