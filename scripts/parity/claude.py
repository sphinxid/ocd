"""The Claude Agent provider (`opencode.provider.claude-agent`) against a stand-in `claude`
(crates/oc-core/tests/fixtures/claude-agent/fake-claude.py, which speaks Claude Code's SDK stdio protocol from a
script). Never runs the real Claude Code: set CLAUDE_STANDIN to a copy of the stand-in (an executable without a
`.py`/`.js` suffix, in a location the parity guard wrapper accepts for OPENCODE_CLAUDE_PATH).

Each scenario records the integration and provider APIs, the session's events and messages, and every line the
stand-in exchanged with the server (`claude.wire`: spawn arguments, environment, messages in and out)."""
import json
import os
import re
import shutil
import time

from parity.plugins import BUN
from parity.registry import scenario

STANDIN = os.environ.get("CLAUDE_STANDIN", "")
SDK = os.environ.get("CLAUDE_SDK_PACKAGE", "")  # a copy of @anthropic-ai/claude-agent-sdk 0.3.301 (for ocd's npm cache)

# Keep the mock provider as the default model; sessions pick Claude Agent explicitly.
CONFIG = {"model": "mock/m1"}


def _setup(ctx, scenario_body, files=None):
    with open(STANDIN) as f:
        if "A stand-in for Claude Code" not in f.read(2000):
            raise RuntimeError("CLAUDE_STANDIN must point at a copy of the stand-in `claude`")
    ctx.make_project(files=files)
    log = os.path.join(ctx.state, "claude.log")
    spec = os.path.join(ctx.state, "claude-scenario.json")
    with open(spec, "w") as f:
        json.dump(scenario_body, f)
    ctx.env_extra["OPENCODE_CLAUDE_PATH"] = STANDIN
    # ocd's sidecar (transcript forks) runs on this Bun; nothing is downloaded.
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.env_extra["CLAUDE_STANDIN_LOG"] = log
    ctx.env_extra["CLAUDE_STANDIN_SCENARIO"] = spec
    # ocd runs the SDK's transcript functions in its plugin sidecar; seed its npm cache (no network here).
    if SDK and ctx.name == "rust":
        target = os.path.join(ctx.state, "cache", "opencode", "npm", "@anthropic-ai", "claude-agent-sdk@0.3.301", "1",
                              "node_modules", "@anthropic-ai", "claude-agent-sdk")
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copytree(SDK, target)
        with open(os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(target))), "package.json"), "w") as f:
            f.write('{"dependencies":{"@anthropic-ai/claude-agent-sdk":"0.3.301"}}\n')
    return log


def _wire(ctx, log, label="claude.wire"):
    """The stand-in's log: request ids and pids normalized, environment reduced to the keys opencode sets."""
    if not os.path.exists(log):
        ctx.rec(label, [])
        return
    ids = {}

    def rid(value):
        if value not in ids:
            ids[value] = f"<req{len(ids) + 1}>"
        return ids[value]

    out = []
    for line in open(log):
        entry = json.loads(line)
        entry.pop("pid", None)
        if entry["event"] == "spawn":
            env = entry["env"]
            for key in ("CLAUDE_STANDIN_LOG", "CLAUDE_STANDIN_SCENARIO"):
                env.pop(key, None)
        message = entry.get("message")
        if isinstance(message, dict):
            if "request_id" in message:
                message["request_id"] = rid(message["request_id"])
            response = message.get("response")
            if isinstance(response, dict) and "request_id" in response:
                response["request_id"] = rid(response["request_id"])
                body = response.get("response")
                if isinstance(body, dict) and "pid" in body:
                    body["pid"] = "<pid>"
        out.append(entry)
    ctx.rec(label, out)


def _connect(ctx, wait_models=True):
    """Connect Claude Agent (external `claude-login`) and wait for the login check."""
    mark = len(ctx.server.events)
    ctx.call("connect", "POST", f"/api/integration/claude-agent/connect/external?directory={ctx.project}",
             {"methodID": "claude-login"})
    ctx.wait_for("integration.updated", start=mark, timeout=40)
    if wait_models:
        ctx.wait_for("provider.updated", start=mark, timeout=40)
    time.sleep(1.5)


def _inventory(ctx, label=""):
    integrations = ctx.data(f"integration.list{label}", "GET", f"/api/integration?directory={ctx.project}", record=False)
    claude = [i for i in integrations if i.get("id") == "claude-agent"] if isinstance(integrations, list) else integrations
    ctx.rec(f"integration.claude{label}", claude)
    providers = ctx.data(f"provider.list{label}", "GET", f"/api/provider?directory={ctx.project}", record=False)
    if isinstance(providers, list):
        providers = [p for p in providers if p.get("id") == "claude-agent"]
    ctx.rec(f"provider.claude{label}", providers)
    models = ctx.data(f"model.list{label}", "GET", f"/api/model?directory={ctx.project}", record=False)
    if isinstance(models, list):
        models = [m for m in models if m.get("providerID") == "claude-agent"]
    ctx.rec(f"model.claude{label}", models)


def _session(ctx, model="haiku", variant=None):
    body = {"title": "Claude", "model": {"providerID": "claude-agent", "id": model}}
    if variant:
        body["model"]["variant"] = variant
    return ctx.create_session(**body)


def _record(ctx, sid, log):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    _wire(ctx, log)


@scenario("claude")
def claude_connect(ctx):
    """Before and after connecting: the integration and its `claude-login` method, the login check (initialize,
    then set_model + get_context_usage per model), the saved catalog as provider and models (context windows,
    effort variants, no price for a subscription), and the plugin list."""
    log = _setup(ctx, {"contextWindows": {"opus": 1000000, "haiku": 200000}})
    ctx.start()
    ctx.booted()
    _inventory(ctx, ".before")
    _connect(ctx)
    _inventory(ctx, ".after")
    ctx.call("plugin.list", "GET", f"/api/plugin?directory={ctx.project}")
    ctx.record_events()
    _wire(ctx, log)


@scenario("claude")
def claude_logged_out(ctx):
    """A `claude` that is installed but not logged in: the connection reports `needs_auth` with the
    `claude auth login` hint; an API-key account is priced from Anthropic's catalog."""
    log = _setup(ctx, {"init": {"account": {"tokenSource": "none", "apiProvider": "firstParty"}}})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    _inventory(ctx)
    _wire(ctx, log)


@scenario("claude")
def claude_api_key_prices(ctx):
    """An API-key account (firstParty with apiKeySource) is priced from the anthropic provider's models."""
    log = _setup(ctx, {"init": {"account": {"apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"},
                                "models": [{"value": "sonnet", "resolvedModel": "claude-sonnet-4-5", "displayName": "Sonnet",
                                            "supportedEffortLevels": ["low", "high"]}]}})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    _inventory(ctx)
    _wire(ctx, log)


@scenario("claude")
def claude_text_turn(ctx):
    """A text turn and a follow-up on the same process (with an effort variant switch): user messages, the
    streamed text and thinking, usage, stored messages and the session record continuing the Claude session."""
    log = _setup(ctx, {"turns": [[{"thinking": "Let me answer."}, {"text": "COPPER-OTTER"}], [{"text": "LILAC-HERON"}]]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "Reply with a marker")
    ctx.call("session.switch", "POST", f"/api/session/{sid}/model",
             {"model": {"providerID": "claude-agent", "id": "haiku", "variant": "low"}})
    ctx.prompt(sid, "Another marker")
    _record(ctx, sid, log)


@scenario("claude")
def claude_tool_turn(ctx):
    """Claude calls OpenCode's `read` through the MCP tool server (PreToolUse hook, tools/call, PostToolUse hook);
    the step ends at the tool boundary, OpenCode runs the tool, and the next step resolves the parked call."""
    log = _setup(ctx, {"turns": [[{"tools": [{"name": "read", "input": {"path": "$PROJECT/marker.txt"}}]},
                                  {"text": "The marker is LEMON-IBIS."}]]},
                 files={"marker.txt": "LEMON-IBIS\n"})
    spec = os.path.join(ctx.state, "claude-scenario.json")
    body = json.load(open(spec))
    body["turns"][0][0]["tools"][0]["input"]["path"] = os.path.join(ctx.project, "marker.txt")
    json.dump(body, open(spec, "w"))
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "Read marker.txt")
    _record(ctx, sid, log)


@scenario("claude")
def claude_unoffered_tool(ctx):
    """Claude calls one of its own tools (not offered by OpenCode): no tool call event, the step continues."""
    log = _setup(ctx, {"turns": [[{"bash": {"command": "ls"}}, {"text": "Done."}]]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "List files")
    _record(ctx, sid, log)


@scenario("claude")
def claude_errors(ctx):
    """Failed turns: a billing error (assistant error + error result, no retry), then an overloaded 529."""
    log = _setup(ctx, {"turns": [
        [{"assistant_error": "billing_error", "text": "Credit balance is too low"},
         {"result": {"is_error": True, "result": "Credit balance is too low"}}],
        [{"result": {"is_error": False, "result": "", "api_error_status": 529}}],
        [{"result": {"is_error": True, "result": "Invalid API key · Please run /login"}}],
    ]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "first")
    ctx.prompt(sid, "second")
    ctx.prompt(sid, "third")
    _record(ctx, sid, log)


@scenario("claude")
def claude_cancel(ctx):
    """Interrupting a running turn: OpenCode cancels, the server sends `interrupt` and drains the turn, the session
    record marks it interrupted, and the next prompt continues the same Claude session."""
    log = _setup(ctx, {"turns": [[{"text": "Thinking about a very long list"}, {"hold": True}], [{"text": "Stopped."}]]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    mark = len(ctx.server.events)
    ctx.call("session.prompt", "POST", f"/api/session/{sid}/prompt", {"text": "Write a long list"})
    ctx.wait_for("session.text.delta", start=mark, timeout=30, pred=lambda e: e.get("data", {}).get("sessionID") == sid)
    time.sleep(0.5)
    ctx.call("session.interrupt", "POST", f"/api/session/{sid}/interrupt")
    ctx.wait_idle(sid, start=mark)
    ctx.prompt(sid, "Stop the list")
    _record(ctx, sid, log)


@scenario("claude")
def claude_missing_binary(ctx):
    """`OPENCODE_CLAUDE_PATH` names a file that does not exist: the login check fails at spawn, and the connection
    shows the SDK's "native binary not found" message."""
    _setup(ctx, {})
    ctx.env_extra["OPENCODE_CLAUDE_PATH"] = os.path.join(os.path.dirname(STANDIN), "missing-claude")
    ctx.start()
    ctx.booted()
    _connect(ctx, wait_models=False)
    _inventory(ctx)


@scenario("claude")
def claude_fork(ctx):
    """A forked OpenCode session's first prompt branches the source's Claude transcript (the SDK's `forkSession`
    up to the last settled checkpoint) and resumes the fork with only the new input."""
    log = _setup(ctx, {"turns": [[{"text": "COPPER-OTTER"}], [{"text": "FORKED-HERON"}]]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "Reply with a marker")
    fork = ctx.data("fork", "POST", f"/api/session/{sid}/fork", {})
    ctx.prompt(fork["id"], "And in the fork?")
    ctx.call("fork.messages", "GET", f"/api/session/{fork['id']}/message")
    _record(ctx, sid, log)


@scenario("claude")
def claude_compaction(ctx):
    """Manual compaction of a Claude Agent session: a side call that branches the chat's transcript, offers the
    chat's tools and system prompt, and stops the primary process; the next prompt starts from the summary."""
    log = _setup(ctx, {"turns": [[{"text": "COPPER-OTTER"}], [{"text": "Summary: the marker was COPPER-OTTER."}],
                                 [{"text": "Still COPPER-OTTER."}]]})
    ctx.start()
    ctx.booted()
    _connect(ctx)
    sid = _session(ctx)
    ctx.prompt(sid, "Remember COPPER-OTTER")
    mark = len(ctx.server.events)
    ctx.call("session.compact", "POST", f"/api/session/{sid}/compact", {})
    ctx.wait_idle(sid, start=mark)
    ctx.prompt(sid, "What was the marker?")
    _record(ctx, sid, log)


@scenario("claude")
def claude_exit_on_start(ctx):
    """`claude` exits during startup with stderr: the login check fails with the exit code and stderr tail."""
    log = _setup(ctx, {"exit": {"code": 3, "stderr": "boom: config unreadable\n"}})
    ctx.start()
    ctx.booted()
    _connect(ctx, wait_models=False)
    _inventory(ctx)
    _wire(ctx, log)
