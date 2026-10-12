"""Error shapes: 404s, validation (400) bodies, auth failures, content types."""
from parity.registry import scenario

MISSING_SES = "ses_000000000000aaaaaaaaaaaaaa"


def boot(ctx):
    ctx.set_script([])
    ctx.make_project()
    ctx.start()


@scenario("errors", hold_for_title=0)
def errors_not_found(ctx):
    """404 shapes for unknown routes and missing resources."""
    boot(ctx)
    sid = ctx.create_session(title="Errors")
    for label, method, path in [
        ("route.unknown", "GET", "/api/nope"),
        ("route.unknown.post", "POST", "/api/nope"),
        ("route.root.unknown", "GET", "/nope"),
        ("session.missing", "GET", f"/api/session/{MISSING_SES}"),
        ("session.missing.messages", "GET", f"/api/session/{MISSING_SES}/message"),
        ("session.missing.prompt", "POST", f"/api/session/{MISSING_SES}/prompt"),
        ("session.missing.delete", "DELETE", f"/api/session/{MISSING_SES}"),
        ("session.missing.fork", "POST", f"/api/session/{MISSING_SES}/fork"),
        ("session.missing.interrupt", "POST", f"/api/session/{MISSING_SES}/interrupt"),
        ("session.missing.permission", "GET", f"/api/session/{MISSING_SES}/permission"),
        ("session.missing.form", "GET", f"/api/session/{MISSING_SES}/form"),
        ("session.missing.inbox", "GET", f"/api/session/{MISSING_SES}/inbox"),
        ("session.missing.context", "GET", f"/api/session/{MISSING_SES}/context"),
        ("session.missing.export", "GET", f"/api/experimental/session/{MISSING_SES}/export"),
        ("message.missing", "GET", f"/api/session/{sid}/message/msg_000000000000aaaaaaaaaaaaaa"),
        ("inbox.missing.cancel", "DELETE", f"/api/session/{sid}/inbox/msg_000000000000aaaaaaaaaaaaaa"),
        ("agent.missing", "GET", "/api/agent/nope"),
        ("provider.missing", "GET", "/api/provider/nope"),
        ("integration.missing", "GET", "/api/integration/nope"),
        ("pty.missing", "GET", "/api/pty/pty_000000000000aaaaaaaaaaaaaa"),
        ("shell.missing", "GET", "/api/shell/sh_000000000000aaaaaaaaaaaaaa"),
        ("credential.missing.delete", "DELETE", "/api/credential/cred_000000000000aaaaaaaaaaaaaa"),
        ("project.missing.patch", "PATCH", "/api/project/0000000000000000000000000000000000000000"),
        ("fs.read.missing", "GET", "/api/fs/read/nope.txt"),
        ("method.not.allowed", "PUT", "/api/session"),
    ]:
        body = {} if method in ("POST", "PATCH") else None
        if label == "session.missing.prompt":
            body = {"text": "x"}
        if label == "project.missing.patch":
            body = {"name": "x"}
        ctx.call(label, method, path, body)


@scenario("errors", hold_for_title=0)
def errors_validation(ctx):
    """400 bodies for malformed params, queries and payloads."""
    boot(ctx)
    sid = ctx.create_session(title="Validation")
    raw = lambda label, method, path, data, ctype="application/json": ctx.call(  # noqa: E731
        label, method, path, raw_body=data, headers={"x-opencode-directory": ctx.project, "content-type": ctype})
    cases = [
        ("params.bad.sessionid", "GET", "/api/session/not-a-session", None),
        ("params.bad.messageid", "GET", f"/api/session/{sid}/message/bad", None),
        ("query.limit.zero", "GET", "/api/session?limit=0", None),
        ("query.limit.negative", "GET", "/api/session?limit=-1", None),
        ("query.limit.text", "GET", "/api/session?limit=abc", None),
        ("query.limit.float", "GET", "/api/session?limit=1.5", None),
        ("query.order.bad", "GET", "/api/session?order=sideways", None),
        ("query.parentid.bad", "GET", "/api/session?parentID=bad", None),
        ("query.directory.relative", "GET", "/api/session?directory=relative/dir", None),
        ("query.sanitize.bad", "GET", f"/api/experimental/session/{sid}/export?sanitize=yes", None),
        ("query.stats.tools.bad", "GET", "/api/experimental/session/stats?tools=lots", None),
        ("query.stats.from.bad", "GET", "/api/experimental/session/stats?from=abc", None),
        ("query.messages.limit.bad", "GET", f"/api/session/{sid}/message?limit=x", None),
        ("query.messages.cursor.bad", "GET", f"/api/session/{sid}/message?cursor=zzz", None),
        ("payload.prompt.missing.text", "POST", f"/api/session/{sid}/prompt", {}),
        ("payload.prompt.text.number", "POST", f"/api/session/{sid}/prompt", {"text": 5}),
        ("payload.prompt.text.null", "POST", f"/api/session/{sid}/prompt", {"text": None}),
        ("payload.prompt.delivery.bad", "POST", f"/api/session/{sid}/prompt", {"text": "x", "delivery": "later", "resume": False}),
        ("payload.prompt.files.bad", "POST", f"/api/session/{sid}/prompt", {"text": "x", "files": [{"uri": 5}], "resume": False}),
        ("payload.prompt.array", "POST", f"/api/session/{sid}/prompt", []),
        ("payload.prompt.id.bad", "POST", f"/api/session/{sid}/prompt", {"text": "x", "id": "nope", "resume": False}),
        ("payload.create.parent.bad", "POST", "/api/session", {"parentID": "nope"}),
        ("payload.create.title.number", "POST", "/api/session", {"title": 1}),
        ("payload.create.location.relative", "POST", "/api/session", {"location": {"directory": "rel"}}),
        ("payload.create.model.partial", "POST", "/api/session", {"model": {"id": "m1"}}),
        ("payload.update.title.bool", "PATCH", f"/api/session/{sid}", {"title": True}),
        ("payload.permission.reply.bad", "POST", f"/api/session/{sid}/permission/per_000000000000aaaaaaaaaaaaaa/reply", {"decision": "maybe"}),
        ("payload.permission.create.missing", "POST", f"/api/session/{sid}/permission", {"action": "x"}),
        ("payload.revert.missing", "POST", f"/api/session/{sid}/revert/stage", {}),
        ("payload.fork.before.number", "POST", f"/api/session/{sid}/fork", {"before": 3}),
        ("payload.agent.missing", "POST", f"/api/session/{sid}/agent", {}),
        ("payload.model.wrong", "POST", f"/api/session/{sid}/model", {"model": "mock/m1"}),
        ("payload.shell.missing", "POST", f"/api/session/{sid}/shell", {"cmd": "ls"}),
        ("payload.compact.delivery.bad", "POST", f"/api/session/{sid}/compact", {"delivery": "x"}),
        ("payload.form.field.bad", "POST", f"/api/session/{sid}/form", {"title": "t", "fields": [{"key": "x", "type": "nope"}]}),
        ("payload.form.fields.missing", "POST", f"/api/session/{sid}/form", {"title": "t"}),
        ("payload.credential.bad", "POST", "/api/credential", {"x": 1}),
        ("payload.extra.keys", "PATCH", f"/api/session/{sid}", {"title": "ok", "bogus": 1}),
    ]
    for label, method, path, body in cases:
        ctx.call(label, method, path, body)
    raw("payload.invalid.json", "POST", f"/api/session/{sid}/prompt", b"{not json")
    raw("payload.empty", "POST", f"/api/session/{sid}/prompt", b"")
    raw("payload.text.plain", "POST", f"/api/session/{sid}/prompt", b"hello", "text/plain")
    raw("payload.form.urlencoded", "POST", f"/api/session/{sid}/prompt", b"text=hi", "application/x-www-form-urlencoded")


@scenario("errors", hold_for_title=0)
def errors_auth(ctx):
    """auth failures: no credentials, wrong password, wrong user, bearer, query token."""
    boot(ctx)
    s = ctx.server
    import base64

    def basic(u, p):
        return "Basic " + base64.b64encode(f"{u}:{p}".encode()).decode()

    for label, headers in [
        ("none", {}),
        ("wrong.password", {"authorization": basic("opencode", "nope")}),
        ("wrong.user", {"authorization": basic("someone", "parity")}),
        ("bearer", {"authorization": "Bearer parity"}),
        ("malformed", {"authorization": "Basic !!!"}),
    ]:
        st, ct, data = s.request("GET", "/api/session", auth=False, headers=headers)
        ctx.rec("auth." + label, {"status": st, "content-type": (ct or "").split(";")[0], "body": data})
    st, ct, data = s.request("GET", "/api/info", auth=False)
    ctx.rec("auth.info.noauth", {"status": st, "content-type": (ct or "").split(";")[0], "body": data})
    st, ct, data = s.request("GET", "/api/session?auth_token=" + base64.b64encode(b"opencode:parity").decode(), auth=False)
    ctx.rec("auth.query.token", {"status": st, "content-type": (ct or "").split(";")[0], "body": data})
