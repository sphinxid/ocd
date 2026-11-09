"""Scheduled prompts (`experimental.schedules`, 2.2.5): the schedule HTTP API, `/schedule` and the schedule tools,
firing, shell and Code Mode checks (valid, invalid, paused), the leader lease on KV `schedule:leader` and the events.

Times differ between runs (epoch-ms values are masked); schedules that fire use short `in` delays or the 15 s check
minimum, so these scenarios take a while."""
import glob
import json
import os
import sqlite3
import sys
import time

from parity.harness import Server
from parity.registry import scenario

HERE = os.path.dirname(os.path.abspath(__file__))
ALLOW = {"permission": {"*": "allow"}}


def config(extra=None, schedules=True, timezone="America/New_York", allow=True):
    cfg = {"experimental": {"schedules": schedules}}
    if timezone:
        cfg["timezone"] = timezone
    if allow:
        cfg.update(ALLOW)
    cfg.update(extra or {})
    return cfg


def boot(ctx, script=None, **kw):
    ctx.set_script(script or [])
    ctx.make_project(config_extra=config(**kw))
    ctx.start()


def create(ctx, label, sid, body):
    return ctx.call(f"create.{label}", "POST", "/api/experimental/schedule", {"sessionID": sid, **body})


def schedules(ctx, label, sid):
    return ctx.call(label, "GET", f"/api/experimental/schedule?sessionID={sid}")


def wait_inbox_empty(ctx, sid, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/session/{sid}", headers={"x-opencode-directory": ctx.project})
        if isinstance(data, dict):
            return
        time.sleep(0.2)


def wait_fired(ctx, sid, count=1, timeout=90):
    """Waits until `count` scheduled prompts ran to completion (a durable prompt event carries `metadata.schedule`)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        fired = [e for e in ctx.events() if e.get("type") == "session.inbox.enqueued"
                 and (((e.get("data") or {}).get("item") or {}).get("payload") or {}).get("metadata", {}).get("schedule")]
        if len(fired) >= count:
            break
        time.sleep(0.2)
    else:
        ctx.rec("TIMEOUT waiting for scheduled prompts", count)
        return
    ctx.wait_idle(sid, timeout=timeout)


def finish(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("schedule", hold_for_title=0)
def schedule_api(ctx):
    """create/list/get/pause/resume/delete over HTTP, validation and service errors, the command list."""
    boot(ctx)
    sid = ctx.create_session(title="Schedules")
    missing = "ses_000000000000aaaaaaaaaaaaaa"
    _, created = create(ctx, "in", sid, {"name": "build", "when": {"in": "20 minutes"}, "prompt": "check the build"})
    sch = created.get("data", {}).get("id") if isinstance(created, dict) else None
    create(ctx, "every", sid, {"when": {"every": "1 hour", "tz": "Europe/Paris"}, "ends": {"times": 3, "after": "1 day"}, "prompt": "poll"})
    create(ctx, "cron", sid, {"when": {"cron": "0 9 * * 1-5"}, "ends": {"at": "2099-01-01T00:00:00Z"}, "prompt": "standup"})
    create(ctx, "at", sid, {"when": {"at": "2099-11-02T09:30"}, "prompt": "later"})
    for label, body in [
        ("at.past", {"when": {"at": "2020-01-01T00:00:00Z"}, "prompt": "x"}),
        ("at.invalid", {"when": {"at": "tomorrow"}, "prompt": "x"}),
        ("in.zero", {"when": {"in": "0 seconds"}, "prompt": "x"}),
        ("in.invalid", {"when": {"in": "soon"}, "prompt": "x"}),
        ("every.short", {"when": {"every": "30 seconds"}, "prompt": "x"}),
        ("cron.seconds", {"when": {"cron": "* * * * * *"}, "prompt": "x"}),
        ("cron.never", {"when": {"cron": "0 0 31 2 *"}, "prompt": "x"}),
        ("cron.invalid", {"when": {"cron": "61 * * * *"}, "prompt": "x"}),
        ("tz.unknown", {"when": {"in": "1 minute", "tz": "Mars/Base"}, "prompt": "x"}),
        ("ends.before", {"when": {"in": "2 hours"}, "ends": {"after": "1 hour"}, "prompt": "x"}),
        ("ends.times.zero", {"when": {"every": "1 hour"}, "ends": {"times": 0}, "prompt": "x"}),
        ("when.missing", {"prompt": "x"}),
        ("when.two", {"when": {"in": "5 minutes", "every": "1 hour"}, "prompt": "x"}),
        ("prompt.empty", {"when": {"in": "5 minutes"}, "prompt": ""}),
        ("if.until", {"if": {"run": "true"}, "until": {"run": "true"}, "prompt": "x"}),
        ("check.every.short", {"when": {"every": "10 seconds"}, "until": {"run": "true"}, "prompt": "x"}),
        ("check.predicates", {"until": {"run": "echo hi", "is": "hi", "matches": "hi"}, "prompt": "x"}),
        ("check.regex", {"until": {"run": "echo hi", "matches": "[unclosed"}, "prompt": "x"}),
        ("check.run.code", {"until": {"run": "true", "code": "return true"}, "prompt": "x"}),
        ("session.missing", {"when": {"in": "5 minutes"}, "prompt": "x", "sessionID": missing}),
    ]:
        create(ctx, label, sid, body)
    schedules(ctx, "list", sid)
    schedules(ctx, "list.missing", missing)
    if sch:
        ctx.call("get", "GET", f"/api/experimental/schedule/{sch}")
        ctx.call("pause", "POST", f"/api/experimental/schedule/{sch}/pause")
        ctx.call("pause.again", "POST", f"/api/experimental/schedule/{sch}/pause")
        ctx.call("resume", "POST", f"/api/experimental/schedule/{sch}/resume")
        ctx.call("resume.again", "POST", f"/api/experimental/schedule/{sch}/resume")
        ctx.call("delete", "DELETE", f"/api/experimental/schedule/{sch}")
        ctx.call("get.deleted", "GET", f"/api/experimental/schedule/{sch}")
        ctx.call("delete.deleted", "DELETE", f"/api/experimental/schedule/{sch}")
    ctx.call("get.unknown", "GET", "/api/experimental/schedule/sch_000000000000aaaaaaaaaaaaaa")
    ctx.call("get.bad", "GET", "/api/experimental/schedule/nope")
    schedules(ctx, "list.after", sid)
    ctx.settle(f"/api/command?directory={ctx.project}")
    commands = ctx.data("commands", "GET", "/api/command", record=False)
    ctx.rec("commands", [c for c in commands if c.get("name") == "schedule"] if isinstance(commands, list) else commands)
    ctx.record_events()


@scenario("schedule", hold_for_title=0)
def schedule_api_unattended(ctx):
    """HTTP creation settles permission asks unattended: a shell check the rules would ask for is forbidden."""
    boot(ctx, allow=False)
    sid = ctx.create_session(title="Schedules")
    create(ctx, "shell.ask", sid, {"until": {"run": "git status -s"}, "prompt": "x"})
    create(ctx, "code.ask", sid, {"until": {"code": "return true"}, "prompt": "x"})
    schedules(ctx, "list", sid)


@scenario("schedule", hold_for_title=0, timeout=240)
def schedule_fires(ctx):
    """an `in` schedule fires its prompt into the session (steer, `metadata.schedule`), and the model answers."""
    boot(ctx, script=[{"text": "Build checked."}])
    sid = ctx.create_session(title="Schedules")
    _, created = create(ctx, "in", sid, {"name": "soon", "when": {"in": "2 seconds"}, "prompt": "check the build"})
    wait_fired(ctx, sid)
    sch = created.get("data", {}).get("id") if isinstance(created, dict) else None
    if sch:
        ctx.call("get.after", "GET", f"/api/experimental/schedule/{sch}")
    finish(ctx, sid)


@scenario("schedule", timeout=240)
def schedule_command(ctx):
    """/schedule hands the request to the model with the schedule instructions; the model creates a schedule through
    Code Mode (`opencode.schedule_create`), lists it, and the schedule then fires."""
    code = ('const s = await tools.opencode.schedule_create({ name: "stretch", when: { in: "3 seconds" }, '
            'prompt: "Time to stretch." }); const l = await tools.opencode.schedule_list({}); '
            'return [s.status, s.when, s.tz, l.schedules.length]')
    boot(ctx, script=[
        {"tools": [{"name": "execute", "id": "call_1", "args": {"code": code}}]},
        {"text": "Scheduled stretch in 3 seconds."},
        {"text": "Stretched."},
    ])
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.call("command", "POST", f"/api/session/{sid}/command", {"name": "schedule", "text": "  in 3 seconds remind me to stretch  "})
    ctx.wait_idle(sid, start=mark)
    wait_fired(ctx, sid)
    schedules(ctx, "list", sid)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("schedule", timeout=240)
def schedule_tools_direct(ctx):
    """the schedule tools without Code Mode (`execute` denied): their summaries, errors and another session's
    schedules reading as missing."""
    boot(ctx, script=[
        {"tools": [{"name": "opencode_schedule_create", "id": "call_1",
                    "args": {"name": "ci", "when": {"every": "1 hour"}, "prompt": "check CI"}}]},
        {"tools": [{"name": "opencode_schedule_create", "id": "call_2",
                    "args": {"when": {"cron": "0 9 * * 1-5"}, "if": {"run": "git status -s", "changes": True}, "prompt": "x" * 100}}]},
        {"tools": [{"name": "opencode_schedule_create", "id": "call_3",
                    "args": {"when": {"in": "5 minutes", "every": "1 hour"}, "prompt": "two"}}]},
        {"tools": [{"name": "opencode_schedule_create", "id": "call_4", "args": {"until": {"code": "return true"}, "prompt": "now"}}]},
        {"tools": [{"name": "opencode_schedule_list", "id": "call_5", "args": {}}]},
        {"tools": [{"name": "opencode_schedule_pause", "id": "call_6", "args": {"id": "sch_000000000000aaaaaaaaaaaaaa"}}]},
        {"tools": [{"name": "opencode_schedule_delete", "id": "call_7", "args": {"id": "nope"}}]},
        {"text": "Done."},
    ], extra={"permission": {"*": "allow", "execute": "deny"}})
    sid = ctx.create_session(title="Schedules")
    ctx.prompt(sid, "make some schedules")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("schedule", hold_for_title=0, timeout=240)
def schedule_checks(ctx):
    """shell and Code Mode checks over HTTP: immediate matches, seeding, validation and creation failures, and a
    code check that returns an invalid shape on its first tick pauses with a wake."""
    server = os.path.join(HERE, "fake_mcp.py")
    boot(ctx, script=[{"text": "Noted."}, {"text": "Noted again."}],
         extra={"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server]}}}})
    sid = ctx.create_session(title="Checks")
    deadline = time.time() + 20
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/mcp?directory={ctx.project}", headers={"x-opencode-directory": ctx.project})
        if "connected" in json.dumps(data):
            break
        time.sleep(0.2)
    for label, body in [
        ("shell.until.now", {"until": {"run": "echo all green"}, "prompt": "CI passed."}),
        ("shell.until.wait", {"until": {"run": "exit 1"}, "prompt": "never"}),
        ("shell.if.changes", {"when": {"every": "1 hour"}, "if": {"run": "echo sha-1", "changes": True}, "prompt": "moved"}),
        ("shell.is", {"when": {"every": "1 hour"}, "until": {"run": "echo OPEN", "is": "MERGED"}, "prompt": "merged"}),
        ("code.until.now", {"until": {"code": "const r = await tools.fake.good_one({}); return { wake: r === \"ok\", message: \"said \" + r }"}, "prompt": "Done."}),
        ("code.until.bool", {"until": {"code": "return typeof fetch === \"undefined\" && runs === 0 && state === null"}, "prompt": "Bool."}),
        ("code.if.state", {"when": {"every": "1 hour"}, "if": {"code": "return { wake: false, state: { seen: now > 0, runs } }"}, "prompt": "x"}),
        ("code.syntax", {"until": {"code": "const x = {"}, "prompt": "x"}),
        ("code.dynamic", {"until": {"code": "return await tools.fake[name]({})"}, "prompt": "x"}),
        ("code.alias", {"until": {"code": "const t = tools.fake; return await t.good_one({})"}, "prompt": "x"}),
        ("code.unknown", {"until": {"code": "return await tools.fake.missing({})"}, "prompt": "x"}),
        ("code.forbidden", {"until": {"code": "return await tools.opencode.schedule_create({})"}, "prompt": "x"}),
        ("code.throws", {"until": {"code": "const r = await tools.fake.good_one({}); return r.nope.length > 0"}, "prompt": "x"}),
        ("code.shape", {"until": {"code": "return \"not-a-check-result\""}, "prompt": "x"}),
        ("code.message.type", {"until": {"code": "return { wake: true, message: 3 }"}, "prompt": "x"}),
        ("code.empty", {"until": {"code": "   "}, "prompt": "x"}),
        ("code.invalid.later", {"when": {"every": "15 seconds"},
                                "if": {"code": "if (state === null) return { wake: false, state: { ok: true } }; return \"bad\""},
                                "prompt": "Watch."}),
    ]:
        create(ctx, label, sid, body)
    schedules(ctx, "list.created", sid)
    wait_fired(ctx, sid, timeout=60)
    schedules(ctx, "list.after", sid)
    finish(ctx, sid)


def _lease_rows(ctx):
    rows = []
    for path in sorted(glob.glob(os.path.join(ctx.state, "data", "opencode", "*.db"))):
        con = sqlite3.connect(path)
        try:
            rows.extend(con.execute("SELECT key, value FROM kv WHERE key = 'schedule:leader'").fetchall())
        finally:
            con.close()
    return [{"key": k, "value": json.loads(v)} for k, v in rows]


@scenario("schedule", hold_for_title=0, timeout=240)
def schedule_lease(ctx):
    """two servers on one data directory: the lease on KV `schedule:leader` lets exactly one of them fire a
    schedule; the lease is held while a schedule is active and released when none is."""
    boot(ctx, script=[{"text": "Fired once."}, {"text": "Fired twice?"}])
    second = Server(ctx.binary, ctx.project, ctx.env(), ctx.name + "-2")
    try:
        sid = ctx.create_session(title="Lease")
        ctx.rec("lease.idle", _lease_rows(ctx))
        _, created = create(ctx, "in", sid, {"when": {"in": "3 seconds"}, "prompt": "fire once"})
        create(ctx, "every", sid, {"when": {"every": "1 hour"}, "prompt": "later"})
        time.sleep(1)
        ctx.rec("lease.active", [{"key": r["key"], "fields": sorted(r["value"])} for r in _lease_rows(ctx)])
        wait_fired(ctx, sid)
        time.sleep(4)
        _, data = ctx.call("messages", "GET", f"/api/session/{sid}/message", record=False)
        users = [m for m in (data.get("data", []) if isinstance(data, dict) else []) if m.get("type") == "user"]
        ctx.rec("user.messages", [m.get("text") for m in users])
        schedules(ctx, "list", sid)
        sch = created.get("data", {}).get("id") if isinstance(created, dict) else None
        if sch:
            ctx.call("get.fired", "GET", f"/api/experimental/schedule/{sch}")
    finally:
        second.close()
    ctx.record_events()
