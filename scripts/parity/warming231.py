"""Opt-in session warming (`core/src/plugin/warming.ts`, 2.2.6/2.2.7, as of 2.3.1): after a real request, a top-level
session gets keep-alive requests (the session's context plus the warming prompt, sent through `session.generate`)
every `interval` until `duration` has passed since that request. The scenarios use second-long intervals and compare
every request the mock receives over a warming cycle (body and headers), the events and what is stored: a ping records
its usage (`session.usage.recorded`, source `generate`) and adds it to the session's totals, and nothing else."""
import time

from parity.registry import scenario

ANSWER = {"text": "Hello there.", "usage": {"prompt_tokens": 100, "completion_tokens": 5}}


def _openai(port, models, **settings):
    return {"name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test", **settings},
            "models": models}


def _model(name, **extra):
    return {"name": name, "limit": {"context": 100000, "output": 4096},
            "cost": {"input": 1, "output": 2, "cache": {"read": 0.1, "write": 1.25}}, **extra}


def _pings(ctx, label="requests"):
    """Per main request: the model, the last message and whether it is a keep-alive ping."""
    out = []
    for r in ctx.mock.main_requests():
        body = r["body"] if isinstance(r["body"], dict) else {}
        last = (body.get("messages") or [{}])[-1]
        out.append({"model": body.get("model"), "last": last})
    ctx.rec(label, out)


def _record(ctx, sids):
    ctx.record_llm(full=True)
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    for i, sid in enumerate(sids):
        ctx.call(f"messages.{i}", "GET", f"/api/session/{sid}/message")
        ctx.call(f"session.{i}", "GET", f"/api/session/{sid}")


def _ping_answer(read=0, write=None):
    usage = {"prompt_tokens": 120, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": read}}
    return {"text": "OK", "usage": usage}


@scenario("warming231", hold_for_title=0, timeout=120)
def warming231_cycle(ctx):
    """`warming: {interval: "2 seconds", duration: "7 seconds"}` on an OpenAI-compatible model: one turn, then pings
    two, four and six seconds after the turn's request was sent, and none after the window closes. Each ping is the
    session's context plus the default keep-alive prompt; its usage is recorded and added to the totals, and the
    history stays as the turn left it."""
    port = ctx.mock.port
    ctx.set_script([ANSWER, _ping_answer(90), _ping_answer(100), _ping_answer(110), {"text": "unexpected"}])
    ctx.make_project(config={"model": "mock/m1", "warming": {"interval": "2 seconds", "duration": "7 seconds"},
                             "providers": {"mock": _openai(port, {"m1": _model("Mock One")})}})
    ctx.start()
    sid = ctx.create_session(title="Warm me")
    ctx.prompt(sid, "hi")
    time.sleep(9.5)
    _pings(ctx)
    _record(ctx, [sid])


def _messages_turn(text, read=0, write=0, prompt=10):
    """An Anthropic Messages answer with cache usage in `message_start`."""
    return {"raw": [
        {"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": "claude-mock", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": prompt, "output_tokens": 1, "cache_read_input_tokens": read,
                      "cache_creation_input_tokens": write}}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}},
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
         "usage": {"output_tokens": 3}},
        {"type": "message_stop"},
    ]}


@scenario("warming231", hold_for_title=0, timeout=150)
def warming231_anthropic_miss(ctx):
    """Anthropic Messages with `cache.warming` on the provider and an explicit interval: the pings carry the turn's
    prompt-cache markers. A ping that hits the cache keeps warming, one that writes more than it reads stops it, and the
    next real request starts warming again."""
    port = ctx.mock.port
    ctx.set_script([
        _messages_turn("First answer.", write=1000),
        _messages_turn("OK", read=1000, write=10),
        _messages_turn("OK", read=0, write=1000),
        _messages_turn("Second answer.", read=1000, write=50),
        _messages_turn("OK", read=1100, write=0),
        _messages_turn("OK", read=1100, write=0),
        {"text": "unexpected"},
    ])
    ctx.make_project(config={"model": "anth/claude-mock", "providers": {"anth": {
        "name": "Anth", "package": "@opencode/ai/providers/anthropic",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test",
                     "cache": {"warming": {"interval": "2 seconds", "duration": "5 seconds"}}},
        "models": {"claude-mock": _model("Claude Mock")}}}})
    ctx.start()
    sid = ctx.create_session(title="Cache miss")
    ctx.prompt(sid, "one")
    time.sleep(6)
    ctx.prompt(sid, "two")
    time.sleep(6)
    _pings(ctx)
    _record(ctx, [sid])


@scenario("warming231", hold_for_title=0, timeout=150)
def warming231_scope(ctx):
    """Which sessions are warmed: a model's `cache.warming: false` turns it off; `"auto"` on a `cache.ttl` of an hour
    outlives a 7-second window; an explicit interval as long as the window is rejected; an invalid `cache.warming`
    falls back to the top level; the `opencode.warming` plugin turned off in another location stops nothing it doesn't
    run. Only the fallback session is pinged."""
    port = ctx.mock.port
    ctx.set_script([ANSWER] * 4 + [_ping_answer(90), _ping_answer(90), _ping_answer(90), {"text": "unexpected"}])
    ctx.make_project(config={"model": "mock/off", "warming": {"interval": "2 seconds", "duration": "7 seconds"},
                             "providers": {"mock": _openai(port, {
                                 "off": _model("Off", settings={"cache": {"warming": False}}),
                                 "hour": _model("Hour", settings={"cache": {"ttl": "1h", "warming": {"interval": "auto"}}}),
                                 "long": _model("Long", settings={"cache": {"warming": {"interval": "7 seconds"}}}),
                                 "fallback": _model("Fallback", settings={"cache": {"warming": {"interval": "soon"}}}),
                             })}})
    ctx.start()
    sids = []
    for model in ("off", "hour", "long", "fallback"):
        sid = ctx.create_session(label=f"session.create.{model}", title=f"Scope {model}",
                                 model={"providerID": "mock", "id": model})
        ctx.prompt(sid, f"hi {model}", label=f"session.prompt.{model}")
        sids.append(sid)
    time.sleep(8.5)
    _pings(ctx)
    ctx.record_llm()


def _wait_requests(ctx, count, timeout=30):
    """Until the mock received `count` main requests."""
    deadline = time.time() + timeout
    while len(ctx.mock.main_requests()) < count:
        if time.time() > deadline:
            ctx.rec("TIMEOUT waiting for requests", count)
            return
        time.sleep(0.05)


PING = "keep-alive request"


@scenario("warming231", hold_for_title=0, timeout=150)
def warming231_lifecycle(ctx):
    """An interrupted turn ends its request, so the pings come on schedule; deleting the session cancels the ping in
    flight and stops the rest. A manual
    compaction is a real request: warming restarts from it. A failed ping changes nothing and the next one comes an
    interval after it was sent. Pings are told apart by their prompt, so the turns they take don't depend on timing."""
    port = ctx.mock.port
    ok = {"text": "OK", "match": PING, "usage": {"prompt_tokens": 50, "completion_tokens": 1}}
    ctx.set_script([
        ok, dict(ok, delay=3),
        {"status": 400, "match": PING, "body": {"error": {"message": "warming refused", "type": "invalid_request_error"}}},
        ok, ok, ok, ok,
        {"text": "## Objective\n- Say hello.", "match": "MUST summarize", "usage": {"prompt_tokens": 40, "completion_tokens": 6}},
        {"hang": True, "match": "first"},
        {"text": "Second session.", "match": "second", "usage": {"prompt_tokens": 30, "completion_tokens": 3}},
        {"text": "unexpected"},
    ])
    ctx.make_project(config={"model": "mock/m1", "warming": {"interval": "2 seconds", "duration": "9 seconds"},
                             "providers": {"mock": _openai(port, {"m1": _model("Mock One")})}})
    ctx.start()
    first = ctx.create_session(label="session.create.first", title="Interrupted")
    ctx.prompt(first, "first", label="session.prompt.first", wait=False)
    _wait_requests(ctx, 1)
    ctx.call("interrupt", "POST", f"/api/session/{first}/interrupt")
    ctx.wait_idle(first, settle=0)
    # Two pings, two and four seconds after the turn's request; deleting the session while the second one waits for
    # its answer cancels it (no usage recorded) and stops the rest.
    _wait_requests(ctx, 3)
    ctx.call("session.delete", "DELETE", f"/api/session/{first}")
    time.sleep(5)
    ctx.rec("requests.after.delete", len(ctx.mock.main_requests()))
    second = ctx.create_session(label="session.create.second", title="Compacted")
    mark = len(ctx.server.events)
    ctx.prompt(second, "second", label="session.prompt.second", wait=False)
    ctx.wait_idle(second, start=mark, settle=0)
    ctx.call("compact", "POST", f"/api/session/{second}/compact", {})
    ctx.wait_for("session.compaction.ended", timeout=30)
    # The first ping after the compaction fails, the next one comes two seconds later.
    _wait_requests(ctx, 7)
    time.sleep(0.5)
    _pings(ctx)
    ctx.record_llm()
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    ctx.call("session.second", "GET", f"/api/session/{second}")
    ctx.call("messages.second", "GET", f"/api/session/{second}/message")
