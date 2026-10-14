"""How a run ends: odd finish reasons, cut and empty streams, stalls past the provider timeouts, retry exhaustion,
compaction in the middle of a long run, background work resuming a finished turn. Group `stop` (slow: the retry
schedule and timeouts run in real time)."""
import time

from parity.registry import scenario


def _mock_provider(ctx, package="@opencode/ai/providers/openai-compatible", settings=None, limit=None):
    s = {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test"}
    s.update(settings or {})
    return {"mock": {"name": "Mock", "package": package, "settings": s,
                     "models": {"m1": {"name": "Mock One", "limit": limit or {"context": 100000, "output": 4096}}}}}


def _open(ctx, script, providers=None, files=None, git_repo=False):
    ctx.set_script(script)
    ctx.make_project(files=files, git_repo=git_repo, config_extra={"providers": providers} if providers else None)
    ctx.start()
    return ctx.create_session(title="Fixed")


def _close(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()
    ctx.record_llm()


def _chunk(content=None, finish=None, tool=None):
    delta = {"role": "assistant"}
    if content is not None:
        delta["content"] = content
    if tool is not None:
        delta["tool_calls"] = [tool]
    choice = {"index": 0, "delta": delta}
    if finish is not None:
        choice["finish_reason"] = finish
    out = {"id": "chatcmpl-raw", "choices": [choice]}
    if finish is not None:
        out["usage"] = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    return out


@scenario("stop", hold_for_title=0, timeout=300)
def stop_finish_reasons(ctx):
    """chat completions: `stop` with tool calls, `tool_calls` without, `length` mid tool call, `content_filter`,
    unknown and `network_error` finish reasons, an empty stream, a stream without finish_reason, without [DONE]."""
    read = {"name": "read", "args": {"path": "$PROJECT/hello.txt"}}
    sid = _open(ctx, [
        {"match": "p-stop-tools", "tools": [read], "finish": "stop"},
        {"match": "p-stop-tools", "text": "after stop+tools"},
        {"match": "p-calls-no-tools", "text": "claims tool calls", "finish": "tool_calls"},
        {"match": "p-length-tool", "tools": [{"name": "read", "args": "{\"path\": \"$PRO"}], "finish": "length"},
        {"match": "p-length-tool", "text": "after length"},
        {"match": "p-length-text", "text": "cut by length", "finish": "length"},
        {"match": "p-filter", "text": "filtered", "finish": "content_filter"},
        {"match": "p-unknown", "text": "odd finish", "finish": "sensitive"},
        {"match": "p-unknown", "text": "after unknown"},
        {"match": "p-network", "text": "net", "finish": "network_error"},
        {"match": "p-network", "text": "after network_error"},
        {"match": "p-empty", "raw": ["[DONE]"]},
        {"match": "p-empty", "text": "after empty"},
        {"match": "p-nofinish", "raw": [_chunk("no finish"), "[DONE]"]},
        {"match": "p-nofinish", "text": "after no finish"},
        {"match": "p-nodone", "raw": [_chunk("no done"), _chunk(finish="stop")]},
        {"match": "p-onlyfinish", "raw": [_chunk(finish="stop"), "[DONE]"]},
    ], files={"hello.txt": "hi\n"})
    for p in ("p-stop-tools", "p-calls-no-tools", "p-length-tool", "p-length-text", "p-filter", "p-unknown",
              "p-network", "p-empty", "p-nofinish", "p-nodone", "p-onlyfinish"):
        ctx.prompt(sid, p, label="prompt." + p, timeout=60)
    _close(ctx, sid)


@scenario("stop", hold_for_title=0, timeout=300)
def stop_anthropic_finish(ctx):
    """Anthropic Messages: `end_turn` with a tool call, `max_tokens`, `pause_turn`, an unknown stop reason, a
    stream cut before `message_delta`, an `error` event mid-stream."""
    read = {"name": "read", "args": {"path": "$PROJECT/hello.txt"}}
    start = {"type": "message_start", "message": {"id": "msg_raw", "type": "message", "role": "assistant",
                                                  "model": "m1", "content": [], "stop_reason": None,
                                                  "stop_sequence": None,
                                                  "usage": {"input_tokens": 10, "output_tokens": 1}}}
    sid = _open(ctx, [
        {"match": "p-endturn-tools", "tools": [read], "finish": "end_turn"},
        {"match": "p-endturn-tools", "text": "after end_turn+tools"},
        {"match": "p-maxtokens", "text": "cut by max_tokens", "finish": "max_tokens"},
        {"match": "p-pause", "text": "paused", "finish": "pause_turn"},
        {"match": "p-weird", "text": "weird stop", "finish": "weird_reason"},
        {"match": "p-weird", "text": "after weird"},
        {"match": "p-cut", "text": "cut before delta", "cut": True},
        {"match": "p-cut", "text": "after cut"},
        {"match": "p-error-event", "raw": [
            start,
            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}},
            {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}},
        ]},
        {"match": "p-error-event", "text": "after error event"},
    ], providers=_mock_provider(ctx, package="@opencode/ai/providers/anthropic"), files={"hello.txt": "hi\n"})
    for p in ("p-endturn-tools", "p-maxtokens", "p-pause", "p-weird", "p-cut", "p-error-event"):
        ctx.prompt(sid, p, label="prompt." + p, timeout=60)
    _close(ctx, sid)


@scenario("stop", hold_for_title=0, timeout=300)
def stop_stall_timeouts(ctx):
    """chunkTimeout/headerTimeout 2 s: a stall mid-answer (read timeout after output), then requests that never
    answer in time until the timeout retry cap (3) gives up."""
    sid = _open(ctx, [
        {"text": ["Partial ", "answer"], "stall": 4},
        {"text": "slow headers", "delay": 4},
        {"text": "slow headers", "delay": 4},
        {"text": "slow headers", "delay": 4},
        {"text": "slow headers", "delay": 4},
        {"text": "not reached"},
    ], providers=_mock_provider(ctx, settings={"chunkTimeout": 2000, "headerTimeout": 2000}))
    ctx.prompt(sid, "hello", timeout=200)
    _close(ctx, sid)


@scenario("stop", hold_for_title=0, timeout=400)
def stop_retry_exhausted(ctx):
    """HTTP 503 on every attempt: the step retries 10 times (2, 4, 8, 10 s...) and then the run fails."""
    sid = _open(ctx, [{"status": 503, "body": {"error": {"message": "unavailable", "type": "server_error"}}}] * 12)
    ctx.prompt(sid, "hello", timeout=300)
    ctx.rec("llm.request.count", len(ctx.mock.main_requests()))
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()


SUMMARY = "## Decisions\n- keep going\n\n## Next Move\n1. finish the remaining steps"


@scenario("stop", hold_for_title=0, timeout=400)
def stop_long_run_compaction(ctx):
    """A 24-step run (write/read/shell) that crosses the auto compaction threshold at step 8 and hits a context
    overflow at step 16: both compactions complete and the run continues to the final answer."""
    script = []
    for i in range(1, 25):
        if i % 3 == 1:
            turn = {"tools": [{"name": "write", "args": {"path": f"$PROJECT/f{i}.txt", "content": f"step {i}\n"}}]}
        elif i % 3 == 2:
            turn = {"tools": [{"name": "read", "args": {"path": f"$PROJECT/f{i - 1}.txt"}}]}
        else:
            turn = {"tools": [{"name": "shell", "args": {"command": f"echo step {i}"}}]}
        if i == 8:
            turn["usage"] = {"prompt_tokens": 90000, "completion_tokens": 5}
        script.append(turn)
        if i == 8:
            script.append({"match": "structured summary", "text": SUMMARY})
        if i == 15:
            script.append({"status": 400, "body": {"error": {
                "message": "This model's maximum context length is 100000 tokens. However, your messages resulted in 120000 tokens.",
                "type": "invalid_request_error", "code": "context_length_exceeded"}}})
            script.append({"match": "structured summary", "text": SUMMARY})
    script.append({"text": "All 24 steps done."})
    sid = _open(ctx, script)
    ctx.prompt(sid, "do the 24 steps", timeout=300)
    main = ctx.mock.main_requests()
    ctx.rec("llm.request.count", len(main))
    ctx.rec("llm.unused_turns", [i for i, used in enumerate(ctx.mock.used) if not used])
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()


@scenario("stop", hold_for_title=0, timeout=300)
def stop_background_shell(ctx):
    """The model starts a background shell command and ends its turn; the command's completion notice resumes
    the session and the model answers again."""
    sid = _open(ctx, [
        {"tools": [{"name": "shell", "args": {"command": "sleep 2; echo bg-done", "background": True}}]},
        {"text": "Waiting for the command."},
        {"match": "bg-done", "text": "The command finished."},
    ])
    ctx.prompt(sid, "run it in the background", timeout=60)
    # The completion notice wakes the session again after the first turn ended.
    deadline = time.time() + 30
    while time.time() < deadline and len(ctx.mock.main_requests()) < 3:
        time.sleep(0.2)
    mark = len(ctx.server.events)
    if len(ctx.mock.main_requests()) >= 3:
        ctx.server.wait_event(lambda e: e.get("type") == "session.execution.succeeded"
                              and e.get("data", {}).get("sessionID") == sid, timeout=30, start=max(0, mark - 50))
    ctx.rec("llm.request.count", len(ctx.mock.main_requests()))
    time.sleep(1)
    _close(ctx, sid)
