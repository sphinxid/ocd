"""2.2.5 API surface: disabled artifact and schedule answers, the managed-policy split (`integration.connect`,
`integration.use` by account, legacy `integration.use` conversion), new config keys."""
import json
import os
import time

from parity.registry import scenario


def boot(ctx, config_extra=None, global_config=None):
    ctx.set_script([])
    ctx.make_project(config_extra=config_extra)
    if global_config is not None:
        path = os.path.join(ctx.state, "config", "opencode", "opencode.json")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump(global_config, f)
    ctx.start()


@scenario("api225", hold_for_title=0)
def api225_artifacts_disabled(ctx):
    """artifact routes answer in upstream's order with experimental.artifacts off: 400, location, then 403."""
    boot(ctx)
    sid = ctx.create_session(title="Artifacts")
    missing = "ses_000000000000aaaaaaaaaaaaaa"
    files = {"index.html": "<p>x</p>"}
    nowhere = {"x-opencode-directory": "/nonexistent-parity-dir"}
    for label, method, path, body, headers in [
        ("providers", "GET", "/api/experimental/artifacts/providers", None, None),
        ("providers.nowhere", "GET", "/api/experimental/artifacts/providers", None, nowhere),
        ("list", "GET", "/api/experimental/artifacts", None, None),
        ("list.session", "GET", f"/api/experimental/artifacts?sessionID={sid}", None, None),
        ("list.session.bad", "GET", "/api/experimental/artifacts?sessionID=bad", None, None),
        ("get.bad.id", "GET", "/api/experimental/artifacts/nope", None, None),
        ("get.bad.id.nowhere", "GET", "/api/experimental/artifacts/nope", None, nowhere),
        ("get", "GET", "/api/experimental/artifacts/art_x", None, None),
        ("get.nowhere", "GET", "/api/experimental/artifacts/art_x", None, nowhere),
        ("view", "GET", "/api/experimental/artifacts/art_x/view", None, None),
        ("view.slash", "GET", "/api/experimental/artifacts/art_x/view/", None, None),
        ("view.file", "GET", "/api/experimental/artifacts/art_x/view/a/b.html", None, None),
        ("view.bad.id", "GET", "/api/experimental/artifacts/nope/view/x", None, None),
        ("create.empty", "POST", "/api/experimental/artifacts", {}, None),
        ("create", "POST", "/api/experimental/artifacts", {"title": "t", "files": files}, None),
        ("create.session.missing", "POST", "/api/experimental/artifacts", {"title": "t", "files": files, "sessionID": missing}, None),
        ("create.session.missing.noheader", "POST", "/api/experimental/artifacts", {"title": "t", "files": files, "sessionID": missing}, {}),
        ("create.session", "POST", "/api/experimental/artifacts", {"title": "t", "files": files, "sessionID": sid}, {}),
        ("update.empty", "PATCH", "/api/experimental/artifacts/art_x", {}, None),
        ("update.title.bad", "PATCH", "/api/experimental/artifacts/art_x", {"title": 5}, None),
        ("update", "PATCH", "/api/experimental/artifacts/art_x", {"files": files}, None),
        ("delete", "DELETE", "/api/experimental/artifacts/art_x", None, None),
        ("delete.bad.id", "DELETE", "/api/experimental/artifacts/x", None, None),
    ]:
        kw = {} if headers is None else {"headers": headers}
        ctx.call(f"artifacts.{label}", method, path, body, **kw)


@scenario("api225", hold_for_title=0)
def api225_schedule_code_checks(ctx):
    """schedule conditions accept Code Mode `code` checks (then the disabled 403); mixed members fail validation."""
    boot(ctx)
    sid = ctx.create_session(title="Schedules")
    missing = "ses_000000000000aaaaaaaaaaaaaa"
    cases = [
        ("code", {"sessionID": sid, "prompt": "p", "if": {"code": "return true"}}),
        ("code.until", {"sessionID": sid, "prompt": "p", "until": {"code": "return false"}}),
        ("code.missing.session", {"sessionID": missing, "prompt": "p", "if": {"code": "return true"}}),
        ("run.and.code", {"sessionID": sid, "prompt": "p", "if": {"run": "true", "code": "x"}}),
        ("code.and.is", {"sessionID": sid, "prompt": "p", "if": {"code": "x", "is": "y"}}),
        ("code.empty", {"sessionID": sid, "prompt": "p", "if": {"code": ""}}),
        ("code.number", {"sessionID": sid, "prompt": "p", "if": {"code": 5}}),
        ("empty", {"sessionID": sid, "prompt": "p", "if": {}}),
        ("code.null", {"sessionID": sid, "prompt": "p", "until": {"run": "x", "code": None}}),
        ("run", {"sessionID": sid, "prompt": "p", "if": {"run": "true"}}),
    ]
    for label, body in cases:
        ctx.call(f"schedule.create.{label}", "POST", "/api/experimental/schedule", body)


PROJECT_POLICIES = [
    {"action": "integration.use", "resource": "mcp:github", "effect": "deny"},
    {"action": "integration.use", "resource": "skill:review", "effect": "deny"},
    {"action": "integration.use", "resource": "plugin:opencode-foo", "effect": "deny"},
    {"action": "integration.use", "resource": "*", "effect": "allow"},
    {"action": "integration.connect", "resource": "github-copilot", "effect": "deny"},
    {"action": "integration.connect", "resource": "nope*", "effect": "deny"},
    {"action": "integration.connect", "resource": "https://wk.example.com", "effect": "deny"},
    {"action": "integration.use", "resource": "opencode#org_1", "effect": "deny"},
    {"action": "integration.use", "resource": "exa", "effect": "deny"},
]


@scenario("api225", hold_for_title=0)
def api225_policy_split(ctx):
    """integration.connect filters and refuses connections; integration.use withholds providers by account."""
    boot(
        ctx,
        config_extra={
            "policies": PROJECT_POLICIES,
            "compaction": {"buffer": "20%", "keep": {"tokens": 1000}},
            "experimental": {"artifacts": False, "portable_shell_scanner": True},
        },
        global_config={"policies": [{"action": "integration.connect", "resource": "anthropic", "effect": "deny"}]},
    )
    ctx.booted()
    status, data = ctx.call("config", "GET", "/api/config", record=False)
    entries = data if isinstance(data, list) else data.get("data", [])
    project = [e for e in entries if e.get("type") == "document" and str(e.get("path", "")).startswith(ctx.project)]
    ctx.rec("config.project", {"status": status, "documents": project})
    status, data = ctx.call("integration.list", "GET", "/api/integration", record=False)
    ids = sorted(i["id"] for i in data.get("data", []))
    ctx.rec("integration.list.ids", {"status": status, "github-copilot": "github-copilot" in ids,
                                     "anthropic": "anthropic" in ids, "opencode": "opencode" in ids})
    for label, method, path, body in [
        ("connect.key.denied", "POST", "/api/integration/github-copilot/connect/key", {"key": "x"}),
        ("connect.key.denied.missing", "POST", "/api/integration/nope-x/connect/key", {"key": "x"}),
        ("connect.key.missing", "POST", "/api/integration/zzz/connect/key", {"key": "x"}),
        ("connect.external.denied", "POST", "/api/integration/github-copilot/connect/external", {"methodID": "m"}),
        ("connect.oauth.denied", "POST", "/api/integration/github-copilot/connect/oauth", {"methodID": "m"}),
        ("connect.command.denied", "POST", "/api/integration/github-copilot/connect/command", {"methodID": "m"}),
        ("wellknown.add.denied", "POST", "/api/experimental/integration/wellknown", {"url": "https://wk.example.com//"}),
        ("credential.create.denied.global", "POST", "/api/credential", {"integrationID": "anthropic", "value": {"type": "key", "key": "sk"}}),
    ]:
        ctx.call(label, method, path, body)
    status, data = ctx.call("credential.create.project.policy", "POST", "/api/credential",
                            {"integrationID": "github-copilot", "value": {"type": "key", "key": "sk"}, "id": "crd_copilot_parity"},
                            record=False)
    ctx.rec("credential.create.project.policy", {"status": status})
    providers = lambda: sorted(p["id"] for p in ctx.data("provider.list", "GET", "/api/provider", record=False))  # noqa: E731
    ctx.rec("providers.before", providers())
    ctx.rec("websearch.providers", ctx.data("websearch.provider", "GET", "/api/websearch/provider", record=False))
    ctx.call("credential.create.console", "POST", "/api/credential",
             {"integrationID": "opencode", "value": {"type": "key", "key": "sk-test", "metadata": {"orgID": "org_1"}},
              "id": "crd_console_parity"}, record=False)
    ctx.settle("/api/provider", timeout=4.0)
    ctx.rec("providers.after.account", providers())


CATALOG_EVENTS = ("provider.updated", "model.updated", "websearch.updated")


@scenario("api225", hold_for_title=0)
def api225_credential_events(ctx):
    """what each credential change announces before it goes on: at every loaded location, in load order, a change
    of the available providers (provider + model), then the policy plugin's reload when an integration's account
    changed (provider + model, web search when its list changed), then `credential.switched`. Credential creation
    through `/api/credential` boots the global config location, which announces too."""
    boot(ctx)
    ctx.booted()
    ctx.settle("/api/provider")
    config_dir = os.path.realpath(os.path.join(ctx.state, "config", "opencode"))

    def where(e):
        directory = (e.get("location") or {}).get("directory")
        if directory is None:
            return ""
        if os.path.realpath(directory) == os.path.realpath(ctx.project):
            return "@project"
        return "@config" if os.path.realpath(directory) == config_dir else "@other"

    def step(label, method, path, body=None):
        mark = len(ctx.server.events)
        status, _ = ctx.call(label, method, path, body, record=False)
        time.sleep(1.5)
        events = [e["type"] + where(e) for e in ctx.server.events[mark:]
                  if e.get("type", "").startswith("credential.") or e.get("type") in CATALOG_EVENTS]
        ctx.rec(label, {"status": status, "events": events})

    def credentials():
        return [c["id"] for c in ctx.data("credential.list", "GET", "/api/credential", record=False)]

    step("connect.first", "POST", "/api/integration/anthropic/connect/key", {"key": "sk-first"})
    first = credentials()[0]
    step("connect.second", "POST", "/api/integration/anthropic/connect/key", {"key": "sk-second"})
    second = [c for c in credentials() if c != first][0]
    step("rename", "PATCH", f"/api/credential/{first}", {"label": "old"})
    step("rename.same", "PATCH", f"/api/credential/{first}", {"label": "old"})
    step("activate.first", "POST", f"/api/credential/{first}/activate")
    step("activate.again", "POST", f"/api/credential/{first}/activate")
    step("remove.inactive", "DELETE", f"/api/credential/{second}")
    step("remove.last", "DELETE", f"/api/credential/{first}")
    step("create.global", "POST", "/api/credential",
         {"integrationID": "openai", "value": {"type": "key", "key": "sk-openai"}, "id": "crd_openai_parity"})
    step("create.inactive", "POST", "/api/credential",
         {"integrationID": "openai", "value": {"type": "key", "key": "sk-other"}, "id": "crd_openai_other", "activate": False})
    step("remove.global", "DELETE", "/api/credential/crd_openai_parity")
