"""Video generation (2.4.1: `generate_video`, `POST /api/experimental/generate/video`, `GET .../video/{id}`, blobs):
an xAI video model served by the mock. A submission answers `req_<n>`; `GET /v1/videos/req_<n>` answers what
`ctx.mock.routes` holds for it (pending until the scenario sets it done), and the finished video downloads from the
mock. The server polls a running job every 5 seconds."""
import base64
import time

from parity.image import boot_global, config as image_config
from parity.registry import scenario

# The start of an MP4: a `ftyp` box, so its type is read from its bytes.
MP4 = bytes([0, 0, 0, 0x14]) + b"ftypmp42" + bytes([0, 0, 0, 0, 1, 2, 3, 4])
TOOL = "generate_video"


def config(port, **kw):
    cfg = image_config(port, **kw)
    cfg["providers"]["vids"] = {
        "name": "Vids",
        "package": "@opencode/ai/providers/xai/videos",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
        "models": {"director": {"name": "Director", "type": "video"}},
    }
    return cfg


def serve_videos(ctx, done=True):
    """Each submission gets `req_<n>`; its status is pending until `finish(ctx, n)` (or at once with `done`)."""
    port = ctx.mock.port
    count = {"n": 0}

    def submit(body):
        count["n"] += 1
        n = count["n"]
        ctx.mock.routes[f"/v1/videos/req_{n}"] = (200, {"status": "pending", "progress": 10})
        if done:
            finish(ctx, n)
        return 200, {"request_id": f"req_{n}"}

    ctx.mock.post_routes["/v1/videos/generations"] = submit
    ctx.mock.routes["/files/clip.mp4"] = (200, MP4, {"content-type": "video/mp4"})
    return port


def finish(ctx, n):
    url = f"http://127.0.0.1:{ctx.mock.port}/files/clip.mp4"
    ctx.mock.routes[f"/v1/videos/req_{n}"] = (200, {"status": "done", "video": {"url": url, "duration": 2,
                                                                                 "respect_moderation": True}})


def record_wire(ctx, label="video.wire"):
    out = []
    for r in ctx.mock.requests:
        if "/videos" in r["path"] or r["path"].startswith("/files/"):
            hdr = {k.lower(): v for k, v in r["headers"].items() if k.lower() in ("content-type", "authorization")}
            out.append({"kind": r["kind"], "path": r["path"], "headers": hdr, "body": r["body"]})
    # Status polls repeat on the server's clock; keep the distinct requests in order.
    seen = []
    for item in out:
        if item not in seen:
            seen.append(item)
    ctx.rec(label, seen)


def wait_job(ctx, vid, timeout=60):
    deadline = time.time() + timeout
    data = None
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/experimental/generate/video/{vid}",
                                        headers={"x-opencode-directory": ctx.project})
        if isinstance(data, dict) and data.get("data", {}).get("status") != "running":
            return
        time.sleep(0.5)


@scenario("video", hold_for_title=0, timeout=240)
def video_http_generate(ctx):
    """POST /generate/video submits at the base config and answers running; the server polls the job and stores the
    finished video as a blob; typed errors for unknown and non-video models and unknown jobs."""
    serve_videos(ctx)
    cfg = config(ctx.mock.port)
    boot_global(ctx, cfg)
    url = "/api/experimental/generate/video"
    body = {"model": {"providerID": "vids", "id": "director"}, "prompt": "a red balloon", "duration": 2,
            "aspect": "16:9", "resolution": "480p", "sound": False}
    _, created = ctx.call("video.create", "POST", url, body)
    vid = created.get("data", {}).get("id") if isinstance(created, dict) else None
    ctx.call("video.create.unknown", "POST", url, {"model": {"providerID": "nope", "id": "x"}, "prompt": "p"})
    ctx.call("video.create.chat", "POST", url, {"model": {"providerID": "mock", "id": "m1"}, "prompt": "p"})
    ctx.call("video.get.unknown", "GET", f"{url}/video_0123456789abABCDEFGHIJKLMN")
    if vid:
        wait_job(ctx, vid)
        _, job = ctx.call("video.get", "GET", f"{url}/{vid}")
        blobs = job.get("data", {}).get("blobs", []) if isinstance(job, dict) else []
        for blob in blobs:
            status, ctype, data = ctx.server.request("GET", f"/api/experimental/blob/{blob['id']}")
            ctx.rec("blob", {"status": status, "content-type": ctype,
                             "body": base64.b64encode(data if isinstance(data, bytes) else str(data).encode()).decode()})
    ctx.call("blob.unknown", "GET", "/api/experimental/blob/blob_0123456789abABCDEFGHIJKLMN")
    record_wire(ctx)


def open_session(ctx, script, cfg_extra=None):
    ctx.set_script(script)
    cfg = config(ctx.mock.port)
    cfg.update(cfg_extra or {})
    ctx.make_project(config=cfg)
    ctx.start()
    return ctx.create_session(title="Videos")


def wait_notice(ctx, sid, count, timeout=60):
    mark = len(ctx.server.events)
    deadline = time.time() + timeout
    while time.time() < deadline and len(ctx.mock.main_requests()) < count:
        time.sleep(0.2)
    if len(ctx.mock.main_requests()) >= count:
        ctx.server.wait_event(lambda e: e.get("type") == "session.execution.succeeded"
                              and e.get("data", {}).get("sessionID") == sid, timeout=30, start=max(0, mark - 50))
    time.sleep(1)


@scenario("video", hold_for_title=0, timeout=300)
def video_tool_generate(ctx):
    """the tool is offered while a video model is available; a call submits the job, waits for the server to store the
    video, copies it into the project and resumes the session with its `<background-result kind="video">` notice."""
    serve_videos(ctx)
    sid = open_session(ctx, [
        {"tools": [{"name": TOOL, "args": {"prompt": "a red balloon", "duration": 2, "aspect": "16:9",
                                           "path": "balloon.mp4"}}]},
        {"text": "Started the video."},
        {"match": 'kind=\\"video\\"', "text": "Your video is ready."},
    ])
    ctx.prompt(sid, "make a video of a red balloon", timeout=90)
    wait_notice(ctx, sid, 3)
    ctx.record_llm("llm.full", full=True)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    record_wire(ctx)
    try:
        with open(f"{ctx.project}/balloon.mp4", "rb") as f:
            ctx.rec("file", base64.b64encode(f.read()).decode())
    except OSError as e:
        ctx.rec("file", str(e))


@scenario("video", hold_for_title=0)
def video_tool_off(ctx):
    """`experimental.video_generate: false` keeps the tool off."""
    sid = open_session(ctx, [{"text": "No videos."}], cfg_extra={"experimental": {"video_generate": False}})
    ctx.prompt(sid, "hi", timeout=60)
    ctx.record_llm("llm.off")
