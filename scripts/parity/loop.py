"""Prompt-loop scenarios: the mock script drives the model; we record events, stored messages and LLM requests."""
from parity.registry import scenario


def run_prompt(ctx, script, text="hello", files=None, git_repo=False, config_extra=None, llm_full=False, before=None,
               after=None, timeout=60):
    ctx.set_script(script)
    ctx.make_project(files=files, git_repo=git_repo, config_extra=config_extra)
    ctx.start()
    sid = ctx.create_session()
    if before:
        before(ctx, sid)
    ctx.prompt(sid, text, timeout=timeout)
    if after:
        after(ctx, sid)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()
    ctx.record_llm(full=llm_full)
    return sid


@scenario("loop")
def loop_text_only(ctx):
    """A plain text answer (no tools)."""
    run_prompt(ctx, [{"text": ["Hello", " there!"], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}])


@scenario("loop")
def loop_read_tool(ctx):
    """The original smoke scenario: read tool then answer."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": "The file says hi.", "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
    ], text="what is in hello.txt?", files={"hello.txt": "hi there\n"})


@scenario("loop")
def loop_reasoning(ctx):
    """reasoning deltas followed by text."""
    run_prompt(ctx, [{"reasoning": ["Let me ", "think."], "text": "Answer.",
                      "usage": {"prompt_tokens": 30, "completion_tokens": 9}}])


@scenario("loop")
def loop_parallel_tools(ctx):
    """three tool calls in one step (read, glob, grep), then an answer."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/a.txt"}, "id": "call_a"},
                   {"name": "glob", "args": {"pattern": "**/*.txt"}, "id": "call_b"},
                   {"name": "grep", "args": {"pattern": "beta"}, "id": "call_c"}]},
        {"text": "Done looking."},
    ], text="look around", files={"a.txt": "alpha\n", "b.txt": "beta\n", "sub/c.txt": "gamma beta\n"})


@scenario("loop")
def loop_write_edit(ctx):
    """write a new file, edit an existing one, then answer; file contents recorded."""
    def after(ctx, sid):
        for f in ("new.txt", "edit.txt"):
            try:
                ctx.rec("file." + f, open(f"{ctx.project}/{f}").read())
            except OSError as e:
                ctx.rec("file." + f, str(e))
    run_prompt(ctx, [
        {"tools": [{"name": "write", "args": {"path": "$PROJECT/new.txt", "content": "fresh\n"}}]},
        {"tools": [{"name": "edit", "args": {"path": "$PROJECT/edit.txt", "oldString": "old", "newString": "new"}}]},
        {"text": "Edited."},
    ], text="write and edit", files={"edit.txt": "old line\nkeep\n"}, after=after)


@scenario("loop")
def loop_patch(ctx):
    """apply_patch style patch tool adding, updating and deleting files."""
    patch = ("*** Begin Patch\n*** Add File: added.txt\n+added line\n*** Update File: upd.txt\n@@\n-one\n+uno\n two\n"
             "*** Delete File: gone.txt\n*** End Patch")
    def after(ctx, sid):
        import os
        for f in ("added.txt", "upd.txt", "gone.txt"):
            p = f"{ctx.project}/{f}"
            ctx.rec("file." + f, open(p).read() if os.path.exists(p) else "<missing>")
    run_prompt(ctx, [
        {"tools": [{"name": "patch", "args": {"patchText": patch}}]},
        {"text": "Patched."},
    ], text="patch files", files={"upd.txt": "one\ntwo\n", "gone.txt": "bye\n"}, after=after)


@scenario("loop")
def loop_shell_tool(ctx):
    """the shell tool runs a command (stdout + exit code)."""
    run_prompt(ctx, [
        {"tools": [{"name": "shell", "args": {"command": "echo hello; echo err >&2; exit 3"}}]},
        {"text": "Ran it."},
    ], text="run a command")


@scenario("loop")
def loop_tool_error(ctx):
    """tool errors: reading a missing file, an unknown tool, invalid tool arguments."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/missing.txt"}, "id": "call_a"},
                   {"name": "nonexistent_tool", "args": {"x": 1}, "id": "call_b"},
                   {"name": "read", "args": "{not json", "id": "call_c"}]},
        {"text": "Errors seen."},
    ], text="break things")


@scenario("loop", hold_for_title=0)
def loop_http_500(ctx):
    """provider answers HTTP 500: a retry is scheduled (session.retry.scheduled), then the user interrupts."""
    ctx.set_script([{"status": 500, "body": {"error": {"message": "boom", "type": "server_error"}}}] * 20)
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    mark = len(ctx.server.events)
    ctx.prompt(sid, "hello", wait=False)
    # Retries back off 2s, 4s, ...: stop after the first retry notice and interrupt.
    ctx.wait_for("session.retry.scheduled", start=mark, timeout=20) or ctx.wait_for("session.step.failed", start=mark, timeout=5)
    ctx.call("interrupt", "POST", f"/api/session/{sid}/interrupt")
    ctx.wait_idle(sid, start=mark)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.rec("llm.request.count>=1", len(ctx.mock.main_requests()) >= 1)


@scenario("loop", hold_for_title=0)
def loop_http_400(ctx):
    """provider answers a non-retryable HTTP 400: the step fails once."""
    run_prompt(ctx, [{"status": 400, "body": {"error": {"message": "bad request: nope", "type": "invalid_request_error"}}}],
               before=lambda ctx, sid: None)


@scenario("loop", hold_for_title=0)
def loop_429_retry(ctx):
    """HTTP 429 with Retry-After: 1, then success."""
    ctx.set_script([
        {"status": 429, "headers": {"retry-after": "1"}, "body": {"error": {"message": "slow down", "type": "rate_limit"}}},
        {"text": "Recovered."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "hello", timeout=30)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_stream_cut(ctx):
    """the stream is cut mid-way (no finish_reason, no [DONE]), then the retry answers."""
    ctx.set_script([{"text": ["Partial ", "answer"], "cut": True}, {"text": "Second try."}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "hello", timeout=30)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_error_after_text(ctx):
    """text, then a failing finish reason (`network_error`) read in the same chunk: the text is kept, the step fails and
    the run continues with the "interrupted, continue" notice; then an unknown finish reason the same way."""
    ctx.set_script([
        {"text": ["Partial ", "answer"], "finish": "network_error", "coalesce": True},
        {"text": "Recovered."},
        {"match": "second", "text": "Odd finish", "finish": "sensitive", "coalesce": True},
        {"match": "second", "text": "Recovered again."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "hello", timeout=30)
    ctx.prompt(sid, "second", timeout=30)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_location_reload_ask(ctx):
    """the location reloads while a permission ask is pending: the ask is cancelled ("location shut down"), not
    declined, and the run continues on the reloaded location."""
    ctx.set_script([{"tools": [{"name": "read", "args": {"path": "$PROJECT/.env"}}]}, {"text": "After the reload."}])
    ctx.make_project(files={".env": "SECRET=1\n"})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    mark = len(ctx.server.events)
    ctx.prompt(sid, "read .env", wait=False)
    ctx.wait_for("permission.asked", start=mark, timeout=30)
    ctx.call("location.reload", "POST", f"/api/location/reload?directory={ctx.project}", {})
    ctx.wait_idle(sid, start=mark, timeout=30)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events(filt=lambda e: e.get("type", "").startswith("session."))
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_context_overflow(ctx):
    """provider reports context overflow: the session compacts and retries."""
    overflow = {"status": 400, "body": {"error": {
        "message": "This model's maximum context length is 1000 tokens. However, your messages resulted in 5000 tokens.",
        "type": "invalid_request_error", "code": "context_length_exceeded"}}}
    ctx.set_script([overflow, {"text": "Summary of the conversation."}, {"text": "After compaction."}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "hello", timeout=40)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_compact_manual(ctx):
    """manual compaction after one exchange, then another prompt."""
    ctx.set_script([{"text": "First."}, {"text": "Compacted summary."}, {"text": "Second."}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "one")
    mark = len(ctx.server.events)
    ctx.call("compact", "POST", f"/api/session/{sid}/compact", {})
    ctx.wait_idle(sid, start=mark)
    ctx.prompt(sid, "two")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("context", "GET", f"/api/session/{sid}/context")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_inbox(ctx):
    """steer and queue while busy; inbox list/update/cancel."""
    ctx.set_script([{"text": ["Slow ", "one."], "stall": 3.0}, {"text": "Steered."}, {"text": "Queued."}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    mark = len(ctx.server.events)
    ctx.prompt(sid, "first", wait=False)
    # The first chunk starts the step; the mock then stalls for 3s, so these land mid-stream.
    ctx.wait_for("session.text.started", start=mark, timeout=20)
    ctx.call("steer", "POST", f"/api/session/{sid}/prompt", {"text": "steer this", "delivery": "steer"})
    _, q = ctx.call("queue", "POST", f"/api/session/{sid}/prompt", {"text": "queue this", "delivery": "queue"})
    _, q2 = ctx.call("queue2", "POST", f"/api/session/{sid}/prompt", {"text": "cancel me", "delivery": "queue"})
    ctx.call("inbox", "GET", f"/api/session/{sid}/inbox")
    qid = ((q2 or {}).get("data") or {}).get("id")
    if qid:
        ctx.call("inbox.cancel", "DELETE", f"/api/session/{sid}/inbox/{qid}")
    q1 = ((q or {}).get("data") or {}).get("id")
    if q1:
        ctx.call("inbox.update", "PATCH", f"/api/session/{sid}/inbox/{q1}", {"delivery": "queue"})
    ctx.call("inbox.after", "GET", f"/api/session/{sid}/inbox")
    deadline = __import__("time").time() + 30
    while __import__("time").time() < deadline:
        _, inbox = ctx.call("inbox.poll", "GET", f"/api/session/{sid}/inbox", record=False)
        _, active = ctx.call("active.poll", "GET", "/api/session/active", record=False)
        if not (inbox or {}).get("data") and sid not in json_dumps(active):
            break
        __import__("time").sleep(0.3)
    __import__("time").sleep(1.0)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


def json_dumps(v):
    import json
    return json.dumps(v)


@scenario("loop", hold_for_title=0)
def loop_session_shell(ctx):
    """`!` shell command on a session (session.shell) and a synthetic message."""
    ctx.set_script([{"text": "Saw synthetic."}])
    ctx.make_project(files={"x.txt": "x\n"})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    mark = len(ctx.server.events)
    ctx.call("shell", "POST", f"/api/session/{sid}/shell", {"command": "ls; echo done"})
    ctx.wait_for("session.shell.ended", start=mark, timeout=20)
    __import__("time").sleep(0.5)
    ctx.call("synthetic", "POST", f"/api/session/{sid}/synthetic", {"text": "synthetic note", "resume": True})
    ctx.wait_idle(sid, start=mark)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop", hold_for_title=0)
def loop_command_skill(ctx):
    """slash command (/review is built-in; custom markdown command) and skill activation."""
    ctx.set_script([{"text": "Command ran."}, {"text": "Skill used."}])
    ctx.make_project(files={".opencode/command/greet.md": "---\ndescription: Greet\n---\nGreet $ARGUMENTS politely.\n"})
    ctx.start()
    sid = ctx.create_session(title="Fixed")
    ctx.settle(f"/api/command?directory={ctx.project}")
    mark = len(ctx.server.events)
    ctx.call("command", "POST", f"/api/session/{sid}/command", {"name": "greet", "text": "Bob"})
    ctx.wait_idle(sid, start=mark)
    ctx.call("command.missing", "POST", f"/api/session/{sid}/command", {"name": "nope", "text": ""})
    mark = len(ctx.server.events)
    ctx.call("skill", "POST", f"/api/experimental/session/{sid}/skill", {"id": "report"})
    ctx.wait_idle(sid, start=mark)
    ctx.call("skill.missing", "POST", f"/api/experimental/session/{sid}/skill", {"id": "nope"})
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("loop")
def loop_llm_request(ctx):
    """the full first LLM request (system prompt, tools, headers) for a tool round trip."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": "Done."},
    ], text="read hello", files={"hello.txt": "hi\n"}, llm_full=True)


@scenario("loop")
def loop_trace_headers(ctx):
    """trace propagation headers of every outgoing request (title, model steps, webfetch) over two prompts."""
    url = f"http://127.0.0.1:{ctx.mock.port}/page"
    ctx.set_script([
        {"tools": [{"name": "webfetch", "args": {"url": url, "format": "text"}}]},
        {"text": "Fetched."},
        {"text": "Again."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "fetch the page")
    # An incoming trace context is not adopted by the session's requests.
    mark = len(ctx.server.events)
    ctx.call("session.prompt", "POST", f"/api/session/{sid}/prompt", {"text": "again"},
             headers={"x-opencode-directory": ctx.project,
                      "traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"})
    ctx.wait_idle(sid, start=mark)
    # Title requests race the first step; list them first, the rest in arrival order.
    reqs = sorted(ctx.mock.requests, key=lambda r: r["kind"] != "title")
    out = []
    for r in reqs:
        h = {k.lower(): v for k, v in r["headers"].items()}
        out.append({"kind": r["kind"], "method": "GET" if r["kind"] == "GET" else "POST", "path": r["path"],
                    "b3": h.get("b3"), "traceparent": h.get("traceparent")})
    ctx.rec("requests", out)
