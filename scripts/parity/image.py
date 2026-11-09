"""Image generation and decisions (`experimental_image_generate`, `POST /api/experimental/generate/{image,decisions}`,
`ctx.experimental.generate.*`): mock providers serve image and decision models; the mock records the wire requests
(multipart boundaries replaced) and answers from `ctx.mock.media` (default: OpenAI Images JSON with a 1x1 PNG)."""
import base64
import json
import os
import time

from parity.mock import PIXEL
from parity.registry import scenario

# A 2x1 JPEG-free set of tiny images in each input format the tool accepts.
PNG = base64.b64decode(PIXEL)
GIF = base64.b64decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")


def providers(port, image_models=None, decision_models=None, extra=None):
    """The chat mock plus image and decision models served by the same mock."""
    base = f"http://127.0.0.1:{port}/v1"
    models = {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
    for mid, model in (image_models or {}).items():
        models[mid] = {"type": "image", "package": "@opencode/ai/providers/openai-compatible/images", **model}
    out = {"mock": {"name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
                    "settings": {"baseURL": base, "apiKey": "test"}, "models": models}}
    if decision_models:
        out["deciders"] = {"name": "Deciders", "package": "@opencode/ai/providers/typesafe-ai/decisions",
                           "settings": {"baseURL": base, "apiKey": "test"}, "models": decision_models}
    out.update(extra or {})
    return out


def config(port, **kw):
    return {"model": "mock/m1", "providers": providers(port, **kw)}


def boot_global(ctx, cfg, project_config=None):
    """Start with `cfg` as the global config (the generate routes run at the global config location)."""
    ctx.set_script([])
    ctx.make_project(config=project_config if project_config is not None else cfg)
    path = os.path.join(ctx.state, "config", "opencode", "opencode.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(cfg, f)
    ctx.start()


def record_media(ctx, label="media"):
    out = []
    for r in ctx.mock.media_requests():
        hdr = {k.lower(): v for k, v in r["headers"].items()
               if k.lower() in ("content-type", "authorization", "x-goog-api-key", "typesafe-danger-zone-images")}
        out.append({"path": r["path"], "headers": hdr, "body": r["body"]})
    ctx.rec(label, out)


def image_body(**extra):
    return {"model": {"providerID": "mock", "id": "painter"}, "prompt": "a red square", **extra}


@scenario("image", hold_for_title=0)
def image_http_generate(ctx):
    """POST /generate/image: a generation, an edit with sizes, URL results downloaded, typed errors."""
    port = ctx.mock.port
    cfg = config(port, image_models={"painter": {"name": "Painter"}, "gpt-image-2": {"name": "GPT Image 2"}},
                 decision_models={"jev": {"name": "Jev", "type": "decision"}})
    ctx.mock.media = [
        {"json": {"data": [{"b64_json": PIXEL}], "output_format": "png", "size": "1024x1024",
                  "usage": {"input_tokens": 12, "output_tokens": 340, "total_tokens": 352}}},
        {"json": {"created": 1, "data": [{"url": f"http://127.0.0.1:{port}/files/out.gif"}, {"b64_json": PIXEL}]}},
        {"json": {"data": [{"b64_json": PIXEL}]}},
        {"status": 400, "json": {"error": {"message": "Your prompt was rejected", "type": "invalid_request_error"}}},
        {"status": 401, "json": {"error": {"message": "Bad key", "type": "invalid_request_error"}}},
        {"json": {"data": []}},
    ]
    ctx.mock.routes["/files/out.gif"] = (200, GIF, {"content-type": "application/octet-stream"})
    boot_global(ctx, cfg)
    url = "/api/experimental/generate/image"
    ctx.call("image.generate", "POST", url, image_body())
    ctx.call("image.edit", "POST", url, image_body(
        images=[{"data": base64.b64encode(PNG).decode(), "mime": "image/png", "name": "in.png"}],
        n=2, aspect="16:9", resolution="2K"))
    ctx.call("image.gpt-image-2", "POST", url, image_body(model={"providerID": "mock", "id": "gpt-image-2"},
                                                         aspect="9:16", resolution="4K"))
    ctx.call("image.provider.400", "POST", url, image_body())
    ctx.call("image.provider.401", "POST", url, image_body())
    ctx.call("image.no.images", "POST", url, image_body())
    ctx.call("image.chat.model", "POST", url, image_body(model={"providerID": "mock", "id": "m1"}))
    ctx.call("image.decision.model", "POST", url, image_body(model={"providerID": "deciders", "id": "jev"}))
    ctx.call("image.missing", "POST", url, image_body(model={"providerID": "mock", "id": "nope"}))
    ctx.call("image.variant", "POST", url, image_body(model={"providerID": "mock", "id": "painter", "variant": "hd"}))
    ctx.call("image.bad.aspect", "POST", url, image_body(aspect="wide"))
    ctx.call("image.signed.aspect", "POST", url, image_body(aspect="-1:2"))
    ctx.call("image.bad.n", "POST", url, image_body(n=0))
    ctx.call("image.bad.base64", "POST", url, image_body(images=[{"data": "not base64!", "mime": "image/png"}]))
    ctx.call("image.no.prompt", "POST", url, {"model": {"providerID": "mock", "id": "painter"}})
    record_media(ctx)


@scenario("image", hold_for_title=0)
def image_http_decisions(ctx):
    """POST /generate/decisions through System One (TypeSafe) and OpenAI Decisions: bodies, answers, errors."""
    port = ctx.mock.port
    openai = {"oai": {"name": "OAI", "package": "@opencode/ai/providers/vercel-ai-gateway/decisions",
                      "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
                      "models": {"luna": {"name": "Luna", "type": "decision"}}}}
    cfg = config(port, image_models={"painter": {"name": "Painter"}},
                 decision_models={"jev": {"name": "Jev", "type": "decision"}}, extra=openai)
    system_one = {
        "model": "jev-1.13", "id": "dec_1", "provider": "typesafe",
        "answers": {
            "department": {"type": "choice", "choice": "billing", "probabilities": {"technical": 0.1, "billing": 0.9},
                           "confidence": 0.8},
            "urgency": {"type": "score", "score": 1.2, "probabilities": {"2": 0.2, "0": 0, "1": 0.8}, "confidence": 0.6},
            "refund": {"type": "noul", "noul": 0.97},
        },
        "usage": {"input_tokens": 40, "output_tokens": 3, "cost": 0.0004},
    }
    openai_answer = {
        "model": "luna-1",
        "answers": [
            {"type": "refusal", "name": "refund", "message": "Can't say"},
            {"type": "predicate", "name": "spam", "probability": 0.1},
        ],
        "usage": {"input_tokens": 30, "output_tokens": 2, "total_tokens": 32, "cost": 0.001},
    }
    ctx.mock.media = [
        {"match": '"jev"', "json": system_one},
        {"match": '"luna"', "json": openai_answer},
        {"match": '"jev"', "json": {"model": "jev", "answers": {"refund": {"type": "noul", "noul": 0.5}}}},
        {"match": '"jev"', "status": 429, "json": {"error": "slow down"}, "headers": {"retry-after-ms": "10"}},
    ]
    boot_global(ctx, cfg)
    url = "/api/experimental/generate/decisions"
    questions = [
        {"name": "department", "type": "choice", "instructions": "Which team?",
         "choices": [{"value": "billing", "description": "Payments"}, {"value": "technical"}]},
        {"type": "score", "name": "urgency", "instructions": "How urgent?",
         "levels": [{"label": "Can wait"}, {"label": "Needs attention", "description": "Same day"}, {"label": "Blocking"}]},
        {"name": "refund", "type": "predicate", "instructions": "Refund asked?"},
    ]
    parts = [{"type": "text", "text": "I was charged twice."}, {"type": "json", "value": {"plan": "pro"}},
             {"type": "file", "data": base64.b64encode(PNG).decode(), "mime": "image/png", "name": "shot.png"}]
    ctx.call("decisions.system-one", "POST", url,
             {"model": {"providerID": "deciders", "id": "jev"}, "input": parts, "questions": questions})
    ctx.call("decisions.openai", "POST", url, {
        "model": {"providerID": "oai", "id": "luna"}, "input": parts[:2] + [{"type": "json", "value": [1, 2]}],
        "questions": [{"name": "refund", "type": "predicate", "instructions": "Refund?"},
                      {"instructions": "Spam?", "type": "predicate", "name": "spam", "extra": 1}]})
    ctx.call("decisions.mismatch", "POST", url,
             {"model": {"providerID": "deciders", "id": "jev"}, "input": parts[:1], "questions": questions})
    ctx.call("decisions.duplicate", "POST", url,
             {"model": {"providerID": "deciders", "id": "jev"}, "input": parts[:1], "questions": questions[2:] * 2})
    ctx.call("decisions.empty", "POST", url,
             {"model": {"providerID": "deciders", "id": "jev"}, "input": parts[:1], "questions": []})
    ctx.call("decisions.image.model", "POST", url,
             {"model": {"providerID": "mock", "id": "painter"}, "input": parts[:1], "questions": questions[2:]})
    record_media(ctx)


TOOL = "experimental_image_generate"


def call(args, id=None):
    tool = {"name": TOOL, "args": args}
    if id:
        tool["id"] = id
    return {"tools": [tool]}


def open_session(ctx, script, cfg_extra=None, files=None, image_models=None):
    ctx.set_script(script)
    cfg = config(ctx.mock.port, image_models=image_models or {"painter": {"name": "Painter"}})
    cfg.update(cfg_extra or {})
    ctx.make_project(config=cfg, files=files)
    ctx.start()
    return ctx.create_session(title="Images")


def wait_requests(ctx, count, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline and len(ctx.mock.main_requests()) < count:
        time.sleep(0.2)


def wait_notice(ctx, sid, count):
    """The image notice wakes the session after its turn ended: wait for that run to finish."""
    mark = len(ctx.server.events)
    wait_requests(ctx, count)
    if len(ctx.mock.main_requests()) >= count:
        ctx.server.wait_event(lambda e: e.get("type") == "session.execution.succeeded"
                              and e.get("data", {}).get("sessionID") == sid, timeout=30, start=max(0, mark - 50))
    time.sleep(1)


def saved_images(ctx):
    """The files the jobs wrote, under the project and the state directory (the default temp folder)."""
    out = {}
    for root in (ctx.project, ctx.state):
        for dirpath, _, names in os.walk(root):
            for name in names:
                if name.startswith("image_") or "result" in name:
                    path = os.path.join(dirpath, name)
                    with open(path, "rb") as f:
                        out[path] = base64.b64encode(f.read()).decode()
    return out


def close(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    ctx.record_llm()
    record_media(ctx)
    ctx.rec("files", saved_images(ctx))


@scenario("image", hold_for_title=0, timeout=240)
def image_tool_generate(ctx):
    """the tool is offered while an image model is available; a call starts a background job whose notice resumes
    the session."""
    ctx.mock.media = [{"delay": 2, "json": {"data": [{"b64_json": PIXEL}], "output_format": "png"}}]
    sid = open_session(ctx, [
        call({"prompt": "a red square"}),
        {"text": "Started the image."},
        {"match": "<image id=", "text": "Your image is ready."},
    ])
    ctx.prompt(sid, "draw a red square", timeout=60)
    wait_notice(ctx, sid, 3)
    ctx.record_llm("llm.full", full=True)
    close(ctx, sid)


@scenario("image", hold_for_title=0, timeout=240)
def image_tool_edit(ctx):
    """an edit of a project image saved at a requested path: the extension follows the image's real format."""
    ctx.mock.media = [{"delay": 2, "json": {"data": [{"b64_json": PIXEL}]}}]
    sid = open_session(ctx, [
        call({"prompt": "make it blue", "images": ["in.gif"], "path": "out/result.jpg"}),
        {"text": "Editing."},
        {"match": "<image id=", "text": "Edited."},
    ])
    with open(os.path.join(ctx.project, "in.gif"), "wb") as f:
        f.write(GIF)
    ctx.prompt(sid, "edit in.gif", timeout=60)
    wait_notice(ctx, sid, 3)
    close(ctx, sid)


@scenario("image", hold_for_title=0, timeout=240)
def image_tool_models(ctx):
    """model selection and input failures, then a configured default the tool can't use: it picks one and says so."""
    ctx.mock.media = [{"delay": 2, "json": {"data": [{"b64_json": PIXEL}]}}]
    sid = open_session(ctx, [
        call({"prompt": "x", "model": "nope"}),
        call({"prompt": "x", "model": "mock/missing"}),
        call({"prompt": "x", "model": "mock/painter#hd"}),
        call({"prompt": "x", "images": ["missing.png"]}),
        call({"prompt": "x", "images": ["notes.txt"]}),
        call({"prompt": "a cat"}),
        {"text": "Started."},
        {"match": "<image id=", "text": "Done."},
    ], cfg_extra={"experimental": {"image_generate": {"model": "mock/missing#hd"}}},
        files={"notes.txt": "not an image"},
        image_models={"painter": {"name": "Painter"}, "sketch": {"name": "Sketch"}})
    ctx.prompt(sid, "draw", timeout=90)
    wait_notice(ctx, sid, 8)
    close(ctx, sid)


@scenario("image", hold_for_title=0)
def image_tool_off(ctx):
    """`experimental.image_generate: false` keeps the tool off; without image models it is never offered."""
    sid = open_session(ctx, [{"text": "No images."}], cfg_extra={"experimental": {"image_generate": False}})
    ctx.prompt(sid, "hi", timeout=60)
    ctx.record_llm("llm.off")
    ctx.stop()
    ctx.set_script([{"text": "Still no images."}])
    ctx.make_project()
    ctx.start()
    other = ctx.create_session(title="Plain")
    ctx.prompt(other, "hi", timeout=60)
    ctx.record_llm("llm.plain")


@scenario("image", hold_for_title=0, timeout=240)
def image_tool_failure(ctx):
    """a provider failure becomes the job's error notice."""
    ctx.mock.media = [{"delay": 2, "status": 400, "json": {"error": {"message": "Prompt rejected", "type": "x"}}}]
    sid = open_session(ctx, [
        call({"prompt": "something odd"}),
        {"text": "Started."},
        {"match": "<image id=", "text": "It failed."},
    ])
    ctx.prompt(sid, "draw", timeout=60)
    wait_notice(ctx, sid, 3)
    close(ctx, sid)


@scenario("image", hold_for_title=0, timeout=300)
def image_tool_restart(ctx):
    """a job still running when the server dies becomes a cancellation notice when `serve --service` recovers."""
    from parity.session225 import _service_password
    ctx.mock.media = [{"hang": True}]
    sid = open_session(ctx, [call({"prompt": "slow"}), {"text": "Started."}, {"text": "Noticed."}])
    ctx.prompt(sid, "draw slowly", timeout=60)
    deadline = time.time() + 20
    while time.time() < deadline and not ctx.mock.media_requests():
        time.sleep(0.2)
    time.sleep(1)
    ctx.server.proc.kill()
    ctx.server.proc.wait(5)
    ctx.server = None
    _service_password(ctx)
    port = 47000 + (os.getpid() % 1000)
    ctx.start(["--service", "--port", str(port), "--hostname", "127.0.0.1"])
    time.sleep(5)
    ctx.rec("requests", len(ctx.mock.main_requests()))
    # The service asks for its own password: read the result through a plain server on the same state.
    ctx.stop()
    ctx.start()
    # The notice waits in the inbox (restart notices never revive an idle owner): the next turn delivers it.
    ctx.prompt(sid, "anything new?", timeout=60)
    ctx.record_llm()
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.stop()


@scenario("image", hold_for_title=0, timeout=240)
def image_plugin_api(ctx):
    """`ctx.experimental.generate.image` / `.decisions` from a plugin (the v22 fixture's `generate` RPC): the same
    requests and answers as the HTTP routes, failures as typed RPC errors."""
    from parity.plugins import BUN, plugin_files
    port = ctx.mock.port
    cfg = config(port, image_models={"painter": {"name": "Painter"}},
                 decision_models={"jev": {"name": "Jev", "type": "decision"}})
    ctx.mock.media = [
        {"match": '"jev"', "json": {"model": "jev", "answers": {"blue": {"type": "noul", "noul": 0.99}},
                                    "usage": {"input_tokens": 5, "output_tokens": 1}}},
    ]
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    project = dict(cfg, plugins=["./plugins/v22-plugin"])
    ctx.set_script([])
    ctx.make_project(config=project, files=plugin_files("v22-plugin"))
    path = os.path.join(ctx.state, "config", "opencode", "opencode.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(cfg, f)
    ctx.start()
    ctx.create_session(title="Boot")
    ctx.settle(f"/api/plugin?directory={ctx.project}", timeout=10)
    rpc = "/api/rpc/fixture.v22/generate"
    ctx.call("plugin.image", "POST", rpc, {"input": {"kind": "image", "model": {"providerID": "mock", "id": "painter"}}})
    ctx.call("plugin.decisions", "POST", rpc,
             {"input": {"kind": "decisions", "model": {"providerID": "deciders", "id": "jev"}}})
    ctx.call("plugin.image.chat", "POST", rpc, {"input": {"kind": "image", "model": {"providerID": "mock", "id": "m1"}}})
    ctx.call("plugin.decisions.missing", "POST", rpc,
             {"input": {"kind": "decisions", "model": {"providerID": "deciders", "id": "nope"}}})
    record_media(ctx)


@scenario("image", hold_for_title=0, timeout=240)
def image_idle_background(ctx):
    """the `experimental.idle` hook (v22 fixture plugin) lists a running image job as `{kind: "image", ...}` background
    work, and none once its notice settled."""
    from parity.plugins import BUN, plugin_files, record_storage
    ctx.mock.media = [{"delay": 4, "json": {"data": [{"b64_json": PIXEL}]}}]
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script([call({"prompt": "slow"}), {"text": "Started."}, {"match": "<image id=", "text": "Done."}])
    cfg = config(ctx.mock.port, image_models={"painter": {"name": "Painter"}})
    cfg["plugins"] = ["./plugins/v22-plugin"]
    ctx.make_project(config=cfg, files=plugin_files("v22-plugin"))
    ctx.start()
    sid = ctx.create_session(title="Idle")
    ctx.settle(f"/api/plugin?directory={ctx.project}", timeout=10)
    ctx.prompt(sid, "draw slowly", timeout=60)
    record_storage(ctx, "storage.running")
    wait_notice(ctx, sid, 3)
    record_storage(ctx, "storage.settled")


@scenario("image", hold_for_title=0, timeout=240)
def image_http_retry(ctx):
    """the one-shot retry: a rate limit or an overloaded host is retried, a server error is not."""
    cfg = config(ctx.mock.port, image_models={"painter": {"name": "Painter"}})
    quick = {"retry-after-ms": "10"}
    busy = {"status": 429, "json": {"error": {"message": "Slow down", "type": "rate_limit"}}, "headers": quick}
    overloaded = {"status": 503, "json": {"error": {"message": "Overloaded", "type": "server_error"}}, "headers": quick}
    ctx.mock.media = [
        busy, {"json": {"data": [{"b64_json": PIXEL}]}},
        {"status": 500, "json": {"error": {"message": "Internal", "type": "server_error"}}},
        overloaded, overloaded, overloaded, overloaded,
    ]
    boot_global(ctx, cfg)
    url = "/api/experimental/generate/image"
    for label in ("rate.limit", "server.error", "overloaded"):
        ctx.call(f"image.{label}", "POST", url, image_body())
        ctx.rec(f"requests.{label}", len(ctx.mock.media_requests()))
