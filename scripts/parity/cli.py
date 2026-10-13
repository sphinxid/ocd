"""CLI output: help for every (sub)command, version, and the commands that talk to a (standalone) server.

The background service is disabled in the isolated config (see harness.isolated_env), and commands that accept
it also get `--standalone`, so nothing ever reaches a service on the default port. The port's program name
`ocd` is rewritten to `opencode` in command position before diffing (by-design rename)."""
from parity.registry import scenario

TOP = ["upgrade", "uninstall", "acp", "api", "debug", "auth", "mcp", "plugin", "models", "stats", "mini", "run",
       "session", "service", "reload", "pair", "serve"]
NESTED = {
    "debug": ["agents", "config", "paths"],
    "auth": ["list", "login", "logout", "export", "import", "switch"],
    "mcp": ["list", "add", "auth", "logout"],
    "plugin": ["list", "add", "check", "update", "remove"],
    "session": ["list", "delete", "export", "import"],
    "service": ["start", "restart", "status", "stop", "get", "set", "unset"],
}


def boot(ctx, script=()):
    ctx.set_script(list(script))
    ctx.make_project(files={"hello.txt": "hi\n"})


@scenario("cli", hold_for_title=0)
def cli_help(ctx):
    """--help for the root and every subcommand; --version; an unknown flag; bash completions."""
    boot(ctx)
    ctx.cli("root --help", ["--help"])
    ctx.cli("root -h", ["-h"])
    ctx.cli("root --version", ["--version"])
    for c in TOP:
        ctx.cli(f"{c} --help", [c, "--help"])
    for c, subs in NESTED.items():
        for sub in subs:
            ctx.cli(f"{c} {sub} --help", [c, sub, "--help"])
    ctx.cli("unknown flag", ["models", "--frob"])
    ctx.cli("completions bash", ["--completions", "bash"])


@scenario("cli")
def cli_run(ctx):
    """`run` (default format and --format json) against the mock, with a tool call."""
    boot(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": "The file says hi."},
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": "Again."},
    ])
    # `--model` keeps both offline: without it both pick OpenCode Zen's free model (network).
    ctx.cli("run", ["run", "--standalone", "-m", "mock/m1", "what is in hello.txt?"], timeout=90)
    ctx.cli("run --format json", ["run", "--standalone", "-m", "mock/m1", "--format", "json", "what is in hello.txt?"],
            timeout=90)
    ctx.cli("run no message", ["run", "--standalone"], stdin="", timeout=30)


@scenario("cli")
def cli_inventory(ctx):
    """models, stats, session list, debug and list subcommands against a standalone server."""
    boot(ctx, [{"text": "Hello."}])
    ctx.cli("run (seed a session)", ["run", "--standalone", "-m", "mock/m1", "hello"], timeout=90)
    for label, args in [
        ("models", ["models", "--standalone"]),
        ("models mock", ["models", "--standalone", "mock"]),
        ("stats", ["stats", "--standalone"]),
        ("stats --json", ["stats", "--standalone", "--json"]),
        ("stats --all", ["stats", "--standalone", "--all"]),
        ("session list", ["session", "list", "--standalone"]),
        ("debug paths", ["debug", "paths"]),
        ("debug config", ["debug", "config"]),
        ("debug agents", ["debug", "agents"]),
        ("mcp list", ["mcp", "list"]),
        ("plugin list", ["plugin", "list"]),
        ("auth list", ["auth", "list", "--standalone"]),
        ("service status", ["service", "status"]),
        ("service get", ["service", "get"]),
    ]:
        ctx.cli(label, args, timeout=60)
