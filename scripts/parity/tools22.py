"""opencode 2.2 tool changes: webfetch follows redirects itself and checks each target, patch self-moves and chained
hunks, subagents refuse image models by type, and custom commands keep a literal `$0`."""
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
