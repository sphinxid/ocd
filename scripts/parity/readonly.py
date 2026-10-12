"""Read-only endpoints against a small project (plain and git)."""
from parity.registry import scenario

FILES = {
    "hello.txt": "hello\n",
    "src/main.rs": "fn main() {\n    println!(\"hi\");\n}\n",
    "src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    "docs/readme.md": "# Readme\n\nSome docs.\n",
    "AGENTS.md": "Always be concise.\n",
    ".opencode/command/hello.md": "---\ndescription: Say hello\n---\nSay hello to $ARGUMENTS\n",
    ".opencode/skill/demo/SKILL.md": "---\nname: demo\ndescription: A demo skill\n---\nDo the demo.\n",
    ".opencode/agent/helper.md": "---\ndescription: A helper agent\nmode: subagent\n---\nYou help.\n",
}


def boot(ctx, git_repo=False, files=None):
    ctx.set_script([])
    ctx.make_project(files=dict(FILES, **(files or {})), git_repo=git_repo)
    ctx.start()


def get_all(ctx, paths):
    for label, path in paths:
        ctx.call(label, "GET", path)


@scenario("readonly", hold_for_title=0)
def readonly_server(ctx):
    """server info, location, config, agents, commands, skills, models, providers, integrations, mcp, plugins."""
    boot(ctx)
    ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    get_all(ctx, [
        ("info", "/api/info"),
        ("location", f"/api/location?{d}"),
        ("config", f"/api/config?{d}"),
        ("config.shells", f"/api/config/shell?{d}"),
        ("agent.list", f"/api/agent?{d}"),
        ("agent.get.build", f"/api/agent/build?{d}"),
        ("agent.get.helper", f"/api/agent/helper?{d}"),
        ("command.list", f"/api/command?{d}"),
        ("skill.list", f"/api/skill?{d}"),
        ("model.list", f"/api/model?{d}"),
        ("model.default", f"/api/model/default?{d}"),
        ("provider.list", f"/api/provider?{d}"),
        ("provider.get", f"/api/provider/mock?{d}"),
        ("integration.list", f"/api/integration?{d}"),
        ("integration.get", f"/api/integration/mock?{d}"),
        ("mcp.list", f"/api/mcp?{d}"),
        ("mcp.resource.catalog", f"/api/mcp/resource?{d}"),
        ("plugin.list", f"/api/plugin?{d}"),
        ("credential.list", "/api/credential"),
        ("websearch.providers", f"/api/websearch/provider?{d}"),
        ("reference.list", f"/api/reference?{d}"),
        ("form.list", f"/api/form?{d}"),
        ("permission.request.list", f"/api/permission/request?{d}"),
        ("permission.saved.list", "/api/permission/saved"),
        ("pty.list", f"/api/pty?{d}"),
        ("shell.list", f"/api/shell?{d}"),
        ("project.list", "/api/project"),
        ("debug.location.list", "/api/debug/location"),
        ("migration.v1", "/api/experimental/migration/v1"),
        ("session.active", "/api/session/active"),
    ])


@scenario("readonly", hold_for_title=0)
def readonly_fs(ctx):
    """fs list/read/find in a plain directory."""
    boot(ctx)
    ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    get_all(ctx, [
        ("fs.list.root", f"/api/fs/list?{d}"),
        ("fs.list.src", f"/api/fs/list?{d}&path=src"),
        ("fs.list.missing", f"/api/fs/list?{d}&path=nope"),
        ("fs.read.text", f"/api/fs/read/hello.txt?{d}"),
        ("fs.read.nested", f"/api/fs/read/src/main.rs?{d}"),
        ("fs.read.missing", f"/api/fs/read/nope.txt?{d}"),
        ("fs.read.dir", f"/api/fs/read/src?{d}"),
        ("fs.read.escape", f"/api/fs/read/..%2F..%2Fetc%2Fpasswd?{d}"),
        ("fs.find.main", f"/api/fs/find?{d}&query=main"),
        ("fs.find.files", f"/api/fs/find?{d}&query=rs&type=file"),
        ("fs.find.dirs", f"/api/fs/find?{d}&query=s&type=directory"),
        ("fs.find.limit", f"/api/fs/find?{d}&query=r&limit=1"),
        ("fs.find.none", f"/api/fs/find?{d}&query=zzzzzz"),
    ])
    st, ct, raw = ctx.server.request("GET", f"/api/fs/read/hello.txt?{d}", headers={"x-opencode-directory": ctx.project})
    ctx.rec("fs.read.headers", {"status": st, "content-type": ct})


@scenario("readonly", hold_for_title=0)
def readonly_vcs(ctx):
    """vcs endpoints and worktree list in a git repo with a working-tree change."""
    boot(ctx, git_repo=True)
    with open(ctx.project + "/hello.txt", "w") as f:
        f.write("hello changed\n")
    with open(ctx.project + "/new.txt", "w") as f:
        f.write("new file\n")
    sid = ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    import time
    time.sleep(1.0)  # upstream fills Vcs.info asynchronously after the location boots (core/src/vcs.ts refresh)
    _, loc = ctx.call("location", "GET", f"/api/location?{d}")
    pid = ((loc or {}).get("data") or {}).get("project", {}).get("id")
    # Upstream builds Vcs lazily and forks the first refresh: the very first vcs.get of a location answers
    # {"branch": {}} before the refresh lands (core/src/vcs.ts `refresh` via State notify). Warm it up.
    ctx.call("vcs.get.warmup", "GET", f"/api/vcs?{d}", record=False)
    time.sleep(0.5)
    get_all(ctx, [
        ("vcs.get", f"/api/vcs?{d}"),
        ("vcs.base", f"/api/vcs/base?{d}"),
        ("vcs.status", f"/api/vcs/status?{d}"),
        ("vcs.branch.list", f"/api/vcs/branch?{d}"),
        ("vcs.diff.working", f"/api/vcs/diff?{d}&mode=working"),
        ("vcs.diff.branch", f"/api/vcs/diff?{d}&mode=branch"),
        ("vcs.diff.committed", f"/api/vcs/diff?{d}&mode=committed"),
        ("vcs.diff.badmode", f"/api/vcs/diff?{d}&mode=nope"),
        ("project.list", "/api/project"),
        ("session.diff", f"/api/session/{sid}/diff"),
    ])
    if pid:
        ctx.call("worktree.list", "GET", f"/api/worktree?projectID={pid}")
    ctx.call("worktree.list.missing", "GET", "/api/worktree?projectID=0000000000000000000000000000000000000000")


@scenario("readonly", hold_for_title=0)
def readonly_vcs_none(ctx):
    """vcs endpoints outside a repository; vcs init."""
    boot(ctx)
    ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    get_all(ctx, [
        ("vcs.get", f"/api/vcs?{d}"),
        ("vcs.status", f"/api/vcs/status?{d}"),
        ("vcs.branch.list", f"/api/vcs/branch?{d}"),
        ("vcs.diff.working", f"/api/vcs/diff?{d}&mode=working"),
    ])
    ctx.call("vcs.init", "POST", f"/api/vcs/init?{d}", {})
    import time
    ctx.call("vcs.get.after.warmup", "GET", f"/api/vcs?{d}", record=False)  # see readonly_vcs
    time.sleep(0.5)
    ctx.call("vcs.get.after", "GET", f"/api/vcs?{d}")


@scenario("readonly")
def readonly_instructions(ctx):
    """session instructions/context after one prompt (AGENTS.md, skills, commands present)."""
    ctx.set_script([{"text": "ok"}])
    ctx.make_project(files=FILES)
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "hi")
    ctx.call("instructions.entries", "GET", f"/api/experimental/session/{sid}/instructions/entries")
    ctx.call("context", "GET", f"/api/session/{sid}/context")
    ctx.call("stats", "GET", "/api/experimental/session/stats?tools=detail&timezone=UTC")
    ctx.record_llm(full=True)


@scenario("readonly", hold_for_title=0)
def readonly_debug(ctx):
    """debug location list/evict, location reload."""
    boot(ctx)
    ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    ctx.call("debug.location.list", "GET", "/api/debug/location")
    ctx.call("location.reload", "POST", f"/api/location/reload?{d}", {})
    ctx.call("debug.location.evict", "DELETE", f"/api/debug/location?{d}")
    ctx.call("debug.location.list.after", "GET", "/api/debug/location")
    import time
    time.sleep(0.5)
    ctx.record_events()
