"""opencode 2.2 tool changes: webfetch follows redirects itself and checks each target, patch self-moves and chained
hunks, subagents refuse image models by type, and custom commands keep a literal `$0`."""
import json
import os
import sys

from parity.loop import run_prompt
from parity.registry import scenario


@scenario("tools22")
def tools22_webfetch_redirects(ctx):
    """webfetch follows a redirect chain, stops at a target the rules deny, and refuses a non-HTTP target."""
    base = f"http://127.0.0.1:{ctx.mock.port}"
    ctx.mock.routes["/r1"] = (302, "", {"location": "/r2"})
    ctx.mock.routes["/r2"] = (301, "", {"location": f"{base}/page"})
    ctx.mock.routes["/page"] = (200, "fetched page\n")
    ctx.mock.routes["/to-secret"] = (307, "", {"location": "/secret"})
    ctx.mock.routes["/secret"] = (200, "secret\n")
    ctx.mock.routes["/to-file"] = (308, "", {"location": "file:///etc/hostname"})
    permission = {"webfetch": {"*": "allow", f"{base}/secret": "deny"}}
    run_prompt(ctx, [
        {"tools": [{"name": "webfetch", "args": {"url": f"{base}/r1", "format": "text"}, "id": "call_chain"}]},
        {"tools": [{"name": "webfetch", "args": {"url": f"{base}/to-secret", "format": "text"}, "id": "call_deny"}]},
        {"tools": [{"name": "webfetch", "args": {"url": f"{base}/to-file", "format": "text"}, "id": "call_file"}]},
        {"text": "Done."},
    ], text="fetch them", config_extra={"permission": permission})
    ctx.rec("fetched", [r["path"] for r in ctx.mock.requests if r["kind"] == "GET"])


def gpt_provider(port, models):
    return {"model": "mock/gpt-9", "providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"}, "models": models}}}


@scenario("tools22")
def tools22_patch_moves(ctx):
    """patch: a move onto the file itself is an update, and later hunks chain onto a moved file."""
    patch = "\n".join([
        "*** Begin Patch", "*** Update File: same.txt", "*** Move to: ./same.txt", "@@", "-before", "+after",
        "*** Update File: old.txt", "@@", "-a", "+A",
        "*** Update File: old.txt", "*** Move to: moved.txt", "@@", "-b", "+B",
        "*** Update File: moved.txt", "@@", "-c", "+C", "*** End Patch"])
    gone = "\n".join(["*** Begin Patch", "*** Delete File: same.txt", "*** Delete File: same.txt", "*** End Patch"])
    models = {"gpt-9": {"name": "GPT Nine", "limit": {"context": 100000, "output": 4096}}}
    run_prompt(ctx, [
        {"tools": [{"name": "patch", "args": {"patchText": patch}, "id": "call_patch"}]},
        {"tools": [{"name": "patch", "args": {"patchText": gone}, "id": "call_gone"}]},
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/moved.txt"}, "id": "call_read"}]},
        {"text": "Done."},
    ], text="patch it", files={"same.txt": "before\n", "old.txt": "a\nb\nc\n"},
        config_extra=gpt_provider(ctx.mock.port, models))


@scenario("tools22")
def tools22_subagent_decision_model(ctx):
    """subagent names a decision model in its chat-model error, and the tool texts are 2.2's. (An image model would
    also expose 2.2's image generation tool, which ocd does not port yet.)"""
    extra = {"providers": {"mock": {
        "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test"},
        "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}},
                   "classifier": {"name": "Classifier", "type": "decision"}}}}}
    run_prompt(ctx, [
        {"tools": [{"name": "subagent", "id": "call_decision", "args": {
            "agent": "explore", "description": "Sort it", "prompt": "Sort", "model": "mock/classifier"}}]},
        {"tools": [{"name": "subagent", "id": "call_missing", "args": {
            "agent": "explore", "description": "Find it", "prompt": "Find", "model": "mock/nope"}}]},
        {"text": "Done."},
    ], text="delegate", config_extra=extra, llm_full=True)


@scenario("tools22", hold_for_title=0)
def tools22_command_dollar_zero(ctx):
    """custom commands keep a literal $0 and never expand $ARGUMENTS typed in the arguments."""
    ctx.set_script([{"text": "One."}, {"text": "Two."}, {"text": "Three."}])
    ctx.make_project(files={
        ".opencode/command/awk.md": "---\ndescription: Awk\n---\nRun awk '{print $0}' on $1.\n",
        ".opencode/command/plain.md": "---\ndescription: Plain\n---\nRun awk '{print $0}'.\n",
        ".opencode/command/split.md": "---\ndescription: Split\n---\nFirst $1. Rest $2.\n"})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.settle(f"/api/command?directory={ctx.project}")
    for name, text in [("awk", "app.log"), ("plain", "app.log"), ("split", '"$ARGUMENTS" tail')]:
        mark = len(ctx.server.events)
        ctx.call(f"command.{name}", "POST", f"/api/session/{sid}/command", {"name": name, "text": text})
        ctx.wait_idle(sid, start=mark)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_llm()


def _json_text(value):
    """Tool-call arguments that are a JSON string holding the object (a stringified root)."""
    return json.dumps(json.dumps(value))


REPAIR_BUILTIN = [
    # Coerced numbers and an undeclared key dropped from the closed read input.
    {"tools": [{"name": "read", "id": "call_coerce",
                "args": {"path": "$PROJECT/a.txt", "offset": "2", "limit": "1", "extra": True}}]},
    # The whole input as a JSON string.
    {"tools": [{"name": "read", "id": "call_string", "args": _json_text({"path": "$PROJECT/a.txt", "limit": "1"})}]},
    # Still invalid after the repair: the error shows the repaired arguments.
    {"tools": [{"name": "read", "id": "call_invalid", "args": {"path": 5, "limit": "3", "bogus": 1, "offset": "x"}}]},
    # Optional null and empty-object placeholders are dropped, boolean strings coerced.
    {"tools": [{"name": "glob", "id": "call_glob",
                "args": {"pattern": "*.txt", "path": None, "limit": "5", "hidden": "false"}}]},
    {"tools": [{"name": "read", "id": "call_placeholder", "args": {"path": "$PROJECT/a.txt", "limit": {}}}]},
    {"tools": [{"name": "edit", "id": "call_edit", "args": {
        "path": "$PROJECT/b.txt", "oldString": "x", "newString": "y", "replaceAll": "true", "why": "because"}}]},
    {"tools": [{"name": "shell", "id": "call_shell", "args": {
        "command": "echo hi", "timeout": "5000", "background": "false", "description": "Say hi"}}]},
    # Required nulls and numbers that are not safe integers stay as they are.
    {"tools": [{"name": "read", "id": "call_kept", "args": {"path": None, "limit": "1.5", "offset": "9007199254740993"}}]},
    {"text": "Done."},
]


@scenario("tools22")
def tools22_repair_builtin(ctx):
    """opencode.tool.input.repair: built-in tool inputs are coerced to their schemas, closed objects lose undeclared
    keys, and a stringified input is parsed; validation errors show the repaired arguments."""
    run_prompt(ctx, REPAIR_BUILTIN, text="repair", files={"a.txt": "one\ntwo\nthree\n", "b.txt": "x x\n"})


@scenario("tools22")
def tools22_repair_disabled(ctx):
    """`-opencode.tool.input.repair` removes the repair: the same calls fail validation with the model's arguments."""
    run_prompt(ctx, REPAIR_BUILTIN, text="repair", files={"a.txt": "one\ntwo\nthree\n", "b.txt": "x x\n"},
               config_extra={"plugins": ["-opencode.tool.input.repair"]})


def _echo_mcp(extra=None):
    """The fake MCP server in echo mode (typed tools that answer with the arguments they received)."""
    server = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_mcp.py")
    config = {"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server, "--echo"]}}}}
    config.update(extra or {})
    return config


def _mcp_connected(ctx, sid, timeout=15):
    """Wait (unrecorded) until the fake server is connected: upstream connects MCP servers in the background, and a
    first step racing that would see no MCP tools."""
    import time
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/mcp?directory={ctx.project}",
                                        headers={"x-opencode-directory": ctx.project})
        if "connected" in json.dumps(data):
            return
        time.sleep(0.2)


@scenario("tools22")
def tools22_repair_codemode_mcp(ctx):
    """Code Mode nested MCP calls are repaired against the server's schema (closed and open objects, unions, arrays,
    nested objects); the outer execute input is not."""
    code = "\n".join([
        "const a = await tools.fake.typed({ count: \"3\", flag: \"true\", tags: \"one\", size: \"2.5\", extra: 1,"
        " nested: '{\"n\":\"4\",\"x\":1}' })",
        "const b = await tools.fake.open({ count: \"5\", extra: true })",
        "let c",
        "try { c = await tools.fake.typed({ count: \"x\", extra: 1 }) } catch (e) { c = String(e.message ?? e) }",
        "return [a, b, c]",
    ])
    run_prompt(ctx, [
        {"tools": [{"name": "execute", "id": "call_code", "args": {"code": code, "extra": "kept"}}]},
        {"text": "Done."},
    ], text="call mcp", config_extra=_echo_mcp(), before=_mcp_connected)


@scenario("tools22")
def tools22_repair_direct_mcp(ctx):
    """With Code Mode denied the MCP tools are direct: their calls are repaired the same way."""
    run_prompt(ctx, [
        {"tools": [{"name": "fake_typed", "id": "call_typed",
                    "args": {"count": "3", "flag": "false", "tags": "[\"a\",\"b\"]", "size": None, "extra": 1}}]},
        {"tools": [{"name": "fake_typed", "id": "call_bad", "args": {"count": "3.5", "extra": 1}}]},
        {"tools": [{"name": "fake_open", "id": "call_open", "args": _json_text({"count": "7", "other": "x"})}]},
        {"text": "Done."},
    ], text="call mcp", config_extra=_echo_mcp({"permission": {"execute": "deny"}}), before=_mcp_connected)
