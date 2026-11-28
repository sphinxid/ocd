"""opencode 2.3.0 freeform tool input (#61703): on models whose route lists `capabilities.freeform` (models.dev, or
the model config), Code Mode `execute` goes out as a free-text custom tool and `patch` as a custom tool with the Lark
patch grammar. The mock answers with `custom_tool_call` items streamed as `response.custom_tool_call_input.delta`
(Responses) or `custom` tool call deltas (Chat Completions); the calls run with their `{ field: raw }` input, and the
next requests replay them as custom calls. Every request body each binary sent and the stored parts are compared."""
import hashlib
import json
import os
import time

from parity.registry import scenario

PATCH = "*** Begin Patch\n*** Add File: hello.txt\n+hi \"there\"\n*** Update File: notes.txt\n@@\n-old\n+new\n*** End Patch"
CODE = 'const parts = ["two", String(1 + 1)]\nreturn parts.join(": ")'


def digest(text):
    return f"sha256:{hashlib.sha256(text.encode()).hexdigest()[:16]} ({len(text)} chars)"


def record_requests(ctx, label="requests"):
    """Every non-title request body: the tools in full (their custom definitions and grammar text), the tool choice,
    the input items / messages in full, and the long system text as a digest. The other body fields as sent."""
    out = []
    for r in ctx.mock.main_requests():
        body = r["body"]
        if not isinstance(body, dict):
            out.append(body)
            continue
        entry = {"path": r["path"], "keys": list(body.keys())}
        for key, value in body.items():
            if key == "instructions" and isinstance(value, str):
                entry[key] = digest(value)
            elif key == "messages" and isinstance(value, list):
                entry[key] = [{**m, "content": digest(m["content"])} if m.get("role") == "system"
                              and isinstance(m.get("content"), str) else m for m in value]
            elif key == "tools" and isinstance(value, list):
                # Each tool once (name and kind), the custom tools in full, and a digest of each function tool.
                entry["tools.names"] = [t.get("name") or (t.get("function") or {}).get("name")
                                        or (t.get("custom") or {}).get("name") for t in value]
                entry["tools.custom"] = [t for t in value if t.get("type") == "custom"]
                # Per tool, so a function schema another 2.3.0 change touched shows up on its own line.
                entry["tools.functions"] = {(t.get("name") or (t.get("function") or {}).get("name")):
                                            digest(json.dumps(t, sort_keys=True))
                                            for t in value if t.get("type") != "custom"}
            else:
                entry[key] = value
        out.append(entry)
    ctx.rec(label, out)


def finish(ctx, sid, label=""):
    ctx.call("messages" + label, "GET", f"/api/session/{sid}/message")
    ctx.call("session.get" + label, "GET", f"/api/session/{sid}")


def responses_config(ctx, capabilities, package="@opencode/ai/providers/openai/responses", model="gpt-5.4"):
    return {"model": f"mockff/{model}", "providers": {"mockff": {
        "name": "Mock Freeform", "package": package,
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test", "transport": "http"},
        "models": {model: {"name": "GPT Mock", "limit": {"context": 200000, "output": 8192},
                           "capabilities": capabilities}}}}}


@scenario("freeform230")
def freeform_responses_execute_patch(ctx):
    """A configured Responses model with `capabilities.freeform: ["text", "lark"]`: `execute` and `patch` go out as
    custom tools (text, and the Lark patch grammar); the model calls `execute` with raw JavaScript and `patch` with a
    raw patch (custom tool calls streamed in pieces); both run with their object input and the stored parts keep it;
    the next requests replay the calls as `custom_tool_call` items with `custom_tool_call_output` results."""
    ctx.set_script([
        {"tools": [{"custom": True, "name": "execute", "input": CODE, "id": "call_exec",
                    "chunks": [CODE[:9], CODE[9:20], CODE[20:]]}]},
        {"tools": [{"custom": True, "name": "patch", "input": PATCH, "id": "call_patch"}]},
        {"text": "Done."},
        {"text": "Again."},
    ])
    ctx.make_project(files={"notes.txt": "old\n"}, config=responses_config(ctx, {"freeform": ["text", "lark"]}))
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "run it and patch it")
    ctx.prompt(sid, "once more")
    finish(ctx, sid)
    for name in ("hello.txt", "notes.txt"):
        try:
            with open(os.path.join(ctx.project, name)) as f:
                ctx.rec("file " + name, f.read())
        except OSError as e:
            ctx.rec("file " + name, str(e))
    ctx.stop()
    record_requests(ctx)


@scenario("freeform230")
def freeform_capability_filter(ctx):
    """The tools declare freeform, but a request sends it only for formats the model lists: `["text"]` makes
    `execute` a custom tool and leaves `patch` a function tool; `[]` (a proxy that rejects custom tools) and no
    capability keep both JSON tools. A JSON `patch` call on the text-only model replays as a `function_call`."""
    ctx.set_script([
        {"tools": [{"name": "patch", "args": {"patchText": "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch"},
                    "id": "call_json"}]},
        {"text": "Patched."},
        {"text": "Off."},
        {"text": "Unset."},
    ])
    cfg = responses_config(ctx, {"freeform": ["text"]})
    models = cfg["providers"]["mockff"]["models"]
    models["gpt-5.4-off"] = {"name": "GPT Off", "limit": {"context": 200000, "output": 8192},
                             "capabilities": {"freeform": []}}
    models["gpt-5.4-unset"] = {"name": "GPT Unset", "limit": {"context": 200000, "output": 8192}}
    ctx.make_project(config=cfg)
    ctx.start()
    sid = ctx.create_session(title="Text only")
    ctx.prompt(sid, "patch a")
    off = ctx.create_session(title="Off", model={"providerID": "mockff", "id": "gpt-5.4-off"})
    ctx.prompt(off, "hello")
    unset = ctx.create_session(title="Unset", model={"providerID": "mockff", "id": "gpt-5.4-unset"})
    ctx.prompt(unset, "hello")
    finish(ctx, sid)
    ctx.stop()
    record_requests(ctx)


@scenario("freeform230")
def freeform_models_dev_openai(ctx):
    """The bundled models.dev catalog marks `openai/gpt-5.4-mini`'s Responses route freeform (`text`, `lark`,
    `regex`): `/api/model` lists them as its capabilities, and with the provider pointed at the mock `execute` and
    `patch` go out as custom tools without any model config, and a custom `execute` call runs."""
    ctx.set_script([
        {"tools": [{"custom": True, "name": "execute", "input": "return 40 + 2", "id": "call_1"}]},
        {"text": "42."},
    ])
    ctx.make_project(config={"model": "openai/gpt-5.4-mini", "providers": {"openai": {
        "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test", "transport": "http"}}}})
    ctx.start()
    # 2.4.1's model handler no longer waits for the location's plugins (#63814): poll until the catalog has the
    # models.dev models instead of recording whatever the first read finds.
    deadline = time.time() + 20
    while True:
        _, models = ctx.call("models", "GET", f"/api/model?directory={ctx.project}", record=False)
        items = models.get("data", models) if isinstance(models, dict) else models
        if time.time() > deadline or any(isinstance(m, dict) and m.get("providerID") == "openai" for m in (items or [])):
            break
        time.sleep(0.25)
    ctx.rec("model capabilities", {m["id"]: m.get("capabilities") for m in (items or [])
                                   if isinstance(m, dict) and m.get("providerID") == "openai"
                                   and m.get("id") in ("gpt-5.4-mini", "gpt-4.1", "gpt-5.6", "o3")})
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "compute")
    finish(ctx, sid)
    ctx.stop()
    record_requests(ctx)


def chat_chunk(calls, finish=None):
    return {"id": "chatcmpl_ff", "object": "chat.completion.chunk", "created": 0, "model": "gpt-5.4",
            "choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": finish}]}


@scenario("freeform230")
def freeform_chat_custom(ctx):
    """OpenAI Chat Completions with freeform: `execute` and `patch` as `{type: "custom", custom: {...}}` tools; a
    streamed `custom` tool call (raw text in pieces) runs as `execute`, and the next request replays it as a custom
    tool call."""
    raw = 'return "chat " + 1'
    ctx.set_script([
        {"raw": [
            chat_chunk([{"index": 0, "id": "call_chat", "type": "custom", "custom": {"name": "execute", "input": ""}}]),
            chat_chunk([{"index": 0, "custom": {"input": raw[:7]}}]),
            chat_chunk([{"index": 0, "custom": {"input": raw[7:]}}]),
            {"id": "chatcmpl_ff", "object": "chat.completion.chunk", "created": 0, "model": "gpt-5.4",
             "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            {"id": "chatcmpl_ff", "object": "chat.completion.chunk", "created": 0, "model": "gpt-5.4", "choices": [],
             "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}},
            "[DONE]",
        ]},
        {"text": "Chat done."},
    ])
    ctx.make_project(config=responses_config(ctx, {"freeform": ["text", "lark"]},
                                             package="@opencode/ai/providers/openai/chat"))
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "run in chat")
    finish(ctx, sid)
    ctx.stop()
    record_requests(ctx)
