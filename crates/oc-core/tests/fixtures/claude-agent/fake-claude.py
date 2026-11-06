#!/usr/bin/env python3
"""A stand-in for Claude Code's `claude` that speaks its SDK stdio protocol (NDJSON control protocol, hooks,
the in-process MCP server) from a script. It never runs anything else and never contacts the network.

Environment:
  CLAUDE_STANDIN_SCENARIO  JSON scenario file (optional; defaults below)
  CLAUDE_STANDIN_LOG       JSON-lines log of every spawn (argv, cwd, selected env), message in and out, and exit

Scenario keys (all optional):
  "init":     overrides merged into the `initialize` response (account, models, claude_code_version, ...)
  "contextWindows": {model: rawMaxTokens} for the login probe's `get_context_usage`
  "exit":     {"code": n, "stderr": "..."} exit right after reading `initialize` (no answer)
  "turns":    a list of turns, taken in order by every chat `claude` (shared through "<log>.state"); a turn is a list
              of steps:
                {"text": "..."}                         a streamed text reply
                {"thinking": "..."}                     a streamed thinking block (in the same reply as a following text)
                {"tools": [{"name": "read", "input": {...}}]}  one reply calling OpenCode tools through the MCP server
                {"bash": {...}}                         a reply calling a tool OpenCode did not offer (Claude's own)
                {"hold": true}                          wait until interrupted
                {"result": {...}}                       the result (fields override the default success)
                {"assistant_error": "billing_error", "text": "..."}  a synthetic API error message
                {"rate_limit": {...}}                   a rate_limit_event
                {"init_servers": [...]}                 the system/init mcp_servers list
                {"exit": n, "stderr": "..."}            exit now
              A turn without a result step ends with a default success result.
"""
import fcntl
import json
import os
import queue
import sys
import threading
import time
import uuid

LOG = os.environ.get("CLAUDE_STANDIN_LOG")
SCENARIO = {}
if os.environ.get("CLAUDE_STANDIN_SCENARIO"):
    with open(os.environ["CLAUDE_STANDIN_SCENARIO"]) as f:
        SCENARIO = json.load(f)

ENV_PREFIXES = ("CLAUDE", "ANTHROPIC", "API_TIMEOUT", "DISABLE_", "ENABLE_", "MAX_MCP", "MCP_", "NODE_OPTIONS", "DEBUG")
log_lock = threading.Lock()


def log(entry):
    if not LOG:
        return
    with log_lock, open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


ARGS = sys.argv[1:]


def flag(name):
    for a in ARGS:
        if a.startswith(name + "="):
            return a.split("=", 1)[1]
    return None


SESSION = flag("--session-id") or flag("--resume") or str(uuid.uuid4())
PROBE = "--include-partial-messages" not in ARGS
PERSIST = "--no-session-persistence" not in ARGS
MODEL = flag("--model") or "default"
log({"event": "spawn", "pid": os.getpid(), "argv": ARGS, "cwd": os.getcwd(),
     "env": {k: v for k, v in sorted(os.environ.items()) if k.startswith(ENV_PREFIXES)}})

out_lock = threading.Lock()


def write(message):
    with out_lock:
        # Logged first, so the log never shows the peer's answer before the line it answers.
        log({"event": "out", "message": message})
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def finish(code=0, stderr=None):
    if stderr:
        sys.stderr.write(stderr)
        sys.stderr.flush()
    log({"event": "exit", "code": code})
    os._exit(code)


inbox = queue.Queue()


def reader():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        message = json.loads(line)
        log({"event": "in", "message": message})
        inbox.put(message)
    inbox.put(None)


threading.Thread(target=reader, daemon=True).start()

MODELS = [
    {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)", "description": "Opus 5.5",
     "supportsEffort": True, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
    {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5", "description": "For complex work",
     "supportsEffort": True, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
    {"value": "haiku", "resolvedModel": "claude-haiku-5-5", "displayName": "Haiku 5.5", "description": "Fastest",
     "supportsEffort": True, "supportedEffortLevels": ["low", "medium", "high"]},
]
INIT = {
    "commands": [], "agents": [], "output_style": "default", "models": MODELS,
    "account": {"email": "user@example.com", "organization": "Example Org", "subscriptionType": "Claude Team", "apiProvider": "firstParty"},
    "pid": 0, "claude_code_version": "2.1.302",
}
INIT.update(SCENARIO.get("init", {}))
WINDOWS = SCENARIO.get("contextWindows", {"opus": 1000000, "haiku": 200000})

state = {"initialize": None, "turn": None, "interrupted": False, "steer": [], "msg": 0, "parent": None}


def respond(request_id, response=None, error=None):
    if error is not None:
        write({"type": "control_response", "response": {"subtype": "error", "request_id": request_id, "error": error}})
        return
    body = {"subtype": "success", "request_id": request_id}
    if response is not None:
        body["response"] = response
    write({"type": "control_response", "response": body})


def answer_control(message):
    """Answers one of the SDK's control requests; returns False for an interrupt during a turn."""
    request = message.get("request", {})
    subtype = request.get("subtype")
    rid = message.get("request_id")
    if subtype == "initialize":
        state["initialize"] = request
        if "exit" in SCENARIO:
            finish(SCENARIO["exit"].get("code", 1), SCENARIO["exit"].get("stderr"))
        respond(rid, dict(INIT, pid=os.getpid()))
    elif subtype == "set_model":
        global MODEL
        MODEL = request.get("model")
        respond(rid)
    elif subtype == "get_context_usage":
        respond(rid, {"categories": [], "totalTokens": 0, "maxTokens": WINDOWS.get(MODEL, 200000), "rawMaxTokens": WINDOWS.get(MODEL, 200000)})
    elif subtype == "apply_flag_settings":
        respond(rid)
    elif subtype == "interrupt":
        respond(rid, {"still_queued": []})
        if state["turn"] is not None:
            state["interrupted"] = True
    else:
        respond(rid, error=f"Unsupported control request subtype: {subtype}")


def handle(message):
    if message is None:
        finish(0)
    kind = message.get("type")
    if kind == "control_request":
        answer_control(message)
    elif kind == "control_response":
        pass
    elif kind == "user":
        if state["turn"] is not None:
            state["steer"].append(message)
        else:
            run_turn(message)


def ask(request):
    """Sends a control request to the SDK and waits for its answer, handling everything else meanwhile."""
    rid = str(uuid.uuid4())
    write({"type": "control_request", "request_id": rid, "request": request})
    while True:
        message = inbox.get()
        if message is not None and message.get("type") == "control_response" and message["response"].get("request_id") == rid:
            return message["response"]
        handle(message)


def transcript(entry):
    if not PERSIST:
        return
    root = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(os.path.expanduser("~"), ".claude")
    project = "".join(c if c.isalnum() else "-" for c in os.getcwd())
    directory = os.path.join(root, "projects", project)
    os.makedirs(directory, exist_ok=True)
    record = {"parentUuid": state["parent"], "isSidechain": False, "userType": "external", "cwd": os.getcwd(),
              "sessionId": SESSION, "version": INIT.get("claude_code_version"), "gitBranch": "", **entry,
              "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())}
    state["parent"] = entry["uuid"]
    with open(os.path.join(directory, f"{SESSION}.jsonl"), "a") as f:
        f.write(json.dumps(record) + "\n")


def load_parent():
    root = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(os.path.expanduser("~"), ".claude")
    projects = os.path.join(root, "projects")
    if not os.path.isdir(projects):
        return
    for d in os.listdir(projects):
        path = os.path.join(projects, d, f"{SESSION}.jsonl")
        if os.path.exists(path):
            with open(path) as f:
                lines = [json.loads(l) for l in f if l.strip()]
            uuids = [l.get("uuid") for l in lines if l.get("uuid")]
            if uuids:
                state["parent"] = uuids[-1]


def base(extra):
    return dict(extra, session_id=SESSION)


def next_turn():
    """The next scripted turn, shared by every `claude` of the scenario."""
    turns = SCENARIO.get("turns", [])
    path = (LOG or "/dev/null") + ".state"
    if not LOG:
        return turns[0] if turns else [{"text": "OK"}]
    with open(path, "a+") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        f.seek(0)
        index = int(f.read() or 0)
        f.seek(0)
        f.truncate()
        f.write(str(index + 1))
    return turns[index] if index < len(turns) else [{"text": "OK"}]


USAGE = {"input_tokens": 3, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100, "output_tokens": 5}


def stream(event, message_id):
    write(base({"type": "stream_event", "event": event, "parent_tool_use_id": None, "uuid": str(uuid.uuid4()), "api_message_id": message_id}))


def assistant(message_id, content, error=None):
    uid = str(uuid.uuid4())
    msg = base({"type": "assistant", "message": {"model": "claude-haiku-5-5", "id": message_id, "type": "message", "role": "assistant",
                                                   "content": content, "stop_reason": None, "usage": USAGE},
                "parent_tool_use_id": None, "uuid": uid})
    if error:
        msg["error"] = error
    write(msg)
    transcript({"type": "assistant", "uuid": uid, "message": msg["message"]})
    return uid


def reply(blocks):
    """One API message with text/thinking blocks, streamed."""
    state["msg"] += 1
    mid = f"msg_{state['msg']}"
    stream({"type": "message_start", "message": {"model": "claude-haiku-5-5", "id": mid, "type": "message", "role": "assistant", "content": [], "usage": USAGE}}, mid)
    for i, (kind, text) in enumerate(blocks):
        if kind == "thinking":
            stream({"type": "content_block_start", "index": i, "content_block": {"type": "thinking", "thinking": ""}}, mid)
            stream({"type": "content_block_delta", "index": i, "delta": {"type": "thinking_delta", "thinking": text}}, mid)
            assistant(mid, [{"type": "thinking", "thinking": text, "signature": "sig"}])
        else:
            stream({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}, mid)
            half = len(text) // 2
            stream({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": text[:half]}}, mid)
            if state["interrupted"]:
                return False
            stream({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": text[half:]}}, mid)
            assistant(mid, [{"type": "text", "text": text}])
        stream({"type": "content_block_stop", "index": i}, mid)
    stream({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": dict(USAGE, output_tokens=9, output_tokens_details={"thinking_tokens": 0})}, mid)
    stream({"type": "message_stop"}, mid)
    return True


def hook(event, tool_use_id, name, tool_input, response=None):
    callback = state["initialize"]["hooks"][event][0]["hookCallbackIds"][0]
    payload = {"session_id": SESSION, "hook_event_name": event, "tool_name": name, "tool_input": tool_input, "tool_use_id": tool_use_id}
    if response is not None:
        payload["tool_response"] = response
    return ask({"subtype": "hook_callback", "callback_id": callback, "input": payload, "tool_use_id": tool_use_id})


def tools(calls):
    state["msg"] += 1
    mid = f"msg_{state['msg']}"
    stream({"type": "message_start", "message": {"model": "claude-haiku-5-5", "id": mid, "type": "message", "role": "assistant", "content": [], "usage": USAGE}}, mid)
    ids = []
    for i, call in enumerate(calls):
        tid = call.get("id") or f"toolu_{state['msg']}_{i}"
        name = call["name"] if call["name"].startswith("mcp__") or call.get("native") else "mcp__opencode__" + call["name"]
        ids.append((tid, name, call.get("input", {})))
        stream({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": tid, "name": name, "input": {}}}, mid)
        stream({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": json.dumps(call.get("input", {}))}}, mid)
        assistant(mid, [{"type": "tool_use", "id": tid, "name": name, "input": call.get("input", {})}])
        if i == 0 and name.startswith("mcp__opencode__"):
            hook("PreToolUse", tid, name, call.get("input", {}))
        stream({"type": "content_block_stop", "index": i}, mid)
    stream({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": dict(USAGE, output_tokens=20, output_tokens_details={"thinking_tokens": 0})}, mid)
    stream({"type": "message_stop"}, mid)
    mcp_id = 1
    for i, (tid, name, tool_input) in enumerate(ids):
        if not name.startswith("mcp__opencode__"):
            content = "Error: No such tool available: " + name
            echo(tid, content, True)
            continue
        if i > 0:
            hook("PreToolUse", tid, name, tool_input)
        mcp_id += 1
        answer = ask({"subtype": "mcp_message", "server_name": "opencode", "message": {
            "method": "tools/call", "params": {"name": name[len("mcp__opencode__"):], "arguments": tool_input,
                                               "_meta": {"claudecode/toolUseId": tid, "progressToken": mcp_id}},
            "jsonrpc": "2.0", "id": mcp_id}})
        mcp = answer.get("response", {}).get("mcp_response", {})
        result = mcp.get("result", {})
        post = hook("PostToolUse", tid, name, tool_input, result.get("content", []))
        replaced = post.get("response", {}).get("hookSpecificOutput", {}).get("updatedMCPToolOutput")
        content = replaced if replaced is not None else result.get("content", mcp.get("error", {}).get("message", ""))
        echo(tid, content, bool(result.get("isError")) or "error" in mcp)
        if state["interrupted"]:
            return


def echo(tid, content, is_error):
    uid = str(uuid.uuid4())
    block = {"tool_use_id": tid, "type": "tool_result", "content": content}
    if is_error:
        block["is_error"] = True
    message = base({"type": "user", "message": {"role": "user", "content": [block]}, "parent_tool_use_id": None, "uuid": uid})
    write(message)
    transcript({"type": "user", "uuid": uid, "message": message["message"]})


def result(overrides=None, error=False):
    message = base({"type": "result", "subtype": "error_during_execution" if error else "success", "is_error": bool(error),
                    "duration_ms": 10, "duration_api_ms": 9, "num_turns": 1, "stop_reason": None if error else "end_turn",
                    "total_cost_usd": 0.0001, "usage": dict(USAGE, output_tokens=9), "modelUsage": {}, "uuid": str(uuid.uuid4())})
    if error:
        message["errors"] = []
    else:
        message["result"] = ""
    message.update(overrides or {})
    write(message)
    write(base({"type": "system", "subtype": "session_state_changed", "state": "idle", "sdk_host_only": True, "uuid": str(uuid.uuid4())}))


def interrupted_end():
    uid = str(uuid.uuid4())
    write(base({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}, "parent_tool_use_id": None, "uuid": uid}))
    transcript({"type": "user", "uuid": uid, "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}})
    result(error=True)


def run_turn(message):
    state["turn"] = message
    state["interrupted"] = False
    transcript({"type": "user", "uuid": message.get("uuid") or str(uuid.uuid4()), "message": message["message"]})
    steps = next_turn()
    servers = [{"name": n, "status": "connected", "source": "sdk"} for n in (state["initialize"] or {}).get("sdkMcpServers", [])]
    for step in steps:
        if "init_servers" in step:
            servers = step["init_servers"]
    write(base({"type": "system", "subtype": "session_state_changed", "state": "running", "sdk_host_only": True, "uuid": str(uuid.uuid4())}))
    write(base({"type": "system", "subtype": "init", "cwd": os.getcwd(), "tools": [], "mcp_servers": servers, "model": MODEL,
                "permissionMode": "dontAsk", "uuid": str(uuid.uuid4())}))
    write(base({"type": "system", "subtype": "status", "status": "requesting", "uuid": str(uuid.uuid4())}))
    ended = False
    pending_thinking = None
    for step in steps:
        while not inbox.empty() and not state["interrupted"]:
            handle(inbox.get())
        if state["interrupted"]:
            break
        if "exit" in step:
            finish(step["exit"], step.get("stderr"))
        if "rate_limit" in step:
            write(base({"type": "rate_limit_event", "rate_limit_info": step["rate_limit"], "uuid": str(uuid.uuid4())}))
        if "assistant_error" in step:
            state["msg"] += 1
            assistant(f"msg_err_{state['msg']}", [{"type": "text", "text": step.get("text", "API Error")}], step["assistant_error"])
            continue
        if "thinking" in step:
            pending_thinking = step["thinking"]
            continue
        if "text" in step:
            blocks = ([("thinking", pending_thinking)] if pending_thinking else []) + [("text", step["text"])]
            pending_thinking = None
            if not reply(blocks):
                break
        if "tools" in step:
            tools(step["tools"])
        if "bash" in step:
            tools([{"name": "Bash", "native": True, "input": step["bash"]}])
        if "hold" in step:
            while not state["interrupted"]:
                handle(inbox.get())
            break
        if "result" in step:
            result(step["result"], error=step["result"].get("is_error") and step["result"].get("subtype") not in (None, "success"))
            ended = True
            break
    if state["interrupted"]:
        interrupted_end()
    elif not ended:
        result()
    state["turn"] = None
    steer = state["steer"]
    state["steer"] = []
    for message in steer:
        run_turn(message)


if not PROBE and flag("--resume"):
    load_parent()
while True:
    handle(inbox.get())
