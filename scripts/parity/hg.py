"""Mercurial repositories (`plugin/vcs/hg.ts`, the `.hg` side of `project.ts`): project identity, the vcs endpoints,
`.hg/branch` watching, vcs init, nested git/hg repositories and session diffs. Needs `hg` on PATH (both binaries
run it); without it every scenario records only that it was skipped."""
import os
import shutil
import subprocess
import time

from parity.harness import git
from parity.registry import scenario

FILES = {
    "hello.txt": "hello\n",
    "src/main.rs": "fn main() {\n    println!(\"hi\");\n}\n",
    "src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    "docs/readme.md": "# Readme\n\nSome docs.\n",
    "long.txt": "".join(f"line-{i}\n" for i in range(20)),
}


def hg(cwd, *args):
    """hg with no user configuration and a fixed author and date, so changeset IDs (and the project IDs derived from
    them) are the same in both runs."""
    env = dict(os.environ, HGPLAIN="1", HGRCPATH="", HGUSER="Parity", HOME=cwd)
    if args and args[0] == "commit":
        args = (*args, "-d", "2026-01-01 00:00:00 +0000")
    subprocess.run(["hg", *args], cwd=cwd, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def commit(cwd, message):
    hg(cwd, "addremove", "-q")
    hg(cwd, "commit", "-q", "-m", message)


def write(path, content, mode="w"):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, mode) as f:
        f.write(content)


def boot(ctx, script=None, files=None, init=True, commit_files=True, watch=False):
    """A project with FILES (plus `files`); `init` makes it a Mercurial repository, `commit_files` commits
    everything. Returns False (after recording it) when hg is missing."""
    if not shutil.which("hg"):
        ctx.rec("skipped", "hg is not on PATH")
        return False
    if watch:
        ctx.env_extra["OPENCODE_DISABLE_FILEWATCHER"] = "0"
    ctx.set_script(script or [])
    ctx.make_project(files=dict(FILES, **(files or {})))
    if init:
        hg(ctx.project, "init")
        if commit_files:
            commit(ctx.project, "initial")
    ctx.start()
    return True


def warm(ctx, directory):
    """Upstream fills Vcs.info asynchronously (core/src/vcs.ts refresh via State notify): the first vcs.get of a
    location can answer {"branch": {}}. Boot the location and let the refresh land."""
    ctx.call("vcs.get.warmup", "GET", f"/api/vcs?directory={directory}", record=False,
             headers={"x-opencode-directory": directory})
    time.sleep(1.0)


def vcs_reads(ctx, directory, prefix="", extra=()):
    d = f"directory={directory}"
    headers = {"x-opencode-directory": directory}
    for label, path in [
        ("location", f"/api/location?{d}"),
        ("vcs.get", f"/api/vcs?{d}"),
        ("vcs.base", f"/api/vcs/base?{d}"),
        ("vcs.status", f"/api/vcs/status?{d}"),
        ("vcs.branch.list", f"/api/vcs/branch?{d}"),
        ("vcs.diff.working", f"/api/vcs/diff?{d}&mode=working"),
        ("vcs.diff.branch", f"/api/vcs/diff?{d}&mode=branch"),
        ("vcs.diff.committed", f"/api/vcs/diff?{d}&mode=committed"),
        *extra,
    ]:
        ctx.call(prefix + label, "GET", path, headers=headers)


@scenario("hg", hold_for_title=0)
def hg_clean(ctx):
    """a clean Mercurial repository: project identity from the root changeset, every vcs endpoint and mode."""
    if not boot(ctx):
        return
    ctx.create_session(title="Boot")
    warm(ctx, ctx.project)
    d = f"directory={ctx.project}"
    vcs_reads(ctx, ctx.project, extra=[
        ("vcs.branch.search", f"/api/vcs/branch?{d}&search=de&limit=3"),
        ("vcs.diff.working.base", f"/api/vcs/diff?{d}&mode=working&base=default"),
        ("vcs.diff.branch.base", f"/api/vcs/diff?{d}&mode=branch&base=default"),
        ("vcs.diff.badmode", f"/api/vcs/diff?{d}&mode=nope"),
        ("project.list", "/api/project"),
    ])
    ctx.call("vcs.init.again", "POST", f"/api/vcs/init?{d}&provider=hg", {})
    _, loc = ctx.call("location.again", "GET", f"/api/location?{d}", record=False)
    pid = ((loc or {}).get("data") or loc or {}).get("project", {}).get("id")
    # Worktrees: the only built-in strategy is git's.
    ctx.call("worktree.list", "GET", f"/api/worktree?projectID={pid}")
    ctx.call("worktree.create", "POST", "/api/worktree", {"projectID": pid, "name": "wt1"})
    ctx.call("worktree.refresh", "POST", "/api/worktree/refresh", {"projectID": pid})
    ctx.call("worktree.list.after", "GET", f"/api/worktree?projectID={pid}")


@scenario("hg", hold_for_title=0)
def hg_changes(ctx):
    """modified, added, removed, missing, renamed, untracked (files and folders), spaced and binary files; every
    diff mode with ?untracked= and ?context=; a location in a subdirectory."""
    if not boot(ctx, files={"bin.dat": "\0\1\2binary\n", "gone.txt": "bye\n"}):
        return
    p = ctx.project
    write(p + "/hello.txt", "hello changed\n")
    write(p + "/long.txt", "".join(("changed\n" if i == 10 else f"line-{i}\n") for i in range(20)))
    hg(p, "rm", "-q", "docs/readme.md")
    os.remove(p + "/gone.txt")
    write(p + "/added.txt", "added\nlines\n")
    hg(p, "add", "-q", "added.txt")
    hg(p, "mv", "-q", "src/main.rs", "src/app.rs")
    write(p + "/untracked.txt", "loose")
    write(p + "/spaced name.txt", "spaced\n")
    write(p + "/newdir/a.txt", "a\n")
    write(p + "/newdir/deep/b.txt", "b\n")
    write(p + "/src/extra.rs", "// extra\n")
    write(p + "/bin.dat", "\0\1\2changed\n")
    write(p + "/bom.txt", "﻿with bom\n")
    ctx.create_session(title="Boot")
    warm(ctx, p)
    d = f"directory={p}"
    vcs_reads(ctx, p, extra=[
        ("vcs.diff.working.files", f"/api/vcs/diff?{d}&mode=working&untracked=files"),
        ("vcs.diff.working.folders", f"/api/vcs/diff?{d}&mode=working&untracked=folders"),
        ("vcs.diff.working.context0", f"/api/vcs/diff?{d}&mode=working&context=0"),
        ("vcs.diff.working.context1", f"/api/vcs/diff?{d}&mode=working&context=1"),
        ("vcs.diff.branch.folders", f"/api/vcs/diff?{d}&mode=branch&untracked=folders"),
    ])
    sub = p + "/src"
    warm(ctx, sub)
    vcs_reads(ctx, sub, prefix="sub.")


@scenario("hg", hold_for_title=0)
def hg_branches(ctx):
    """named branches: branch diffs against the common ancestor with default, and the vcs.branch.updated event
    when `.hg/branch` changes."""
    if not boot(ctx, watch=True):
        return
    p = ctx.project
    hg(p, "branch", "-q", "feature")
    write(p + "/hello.txt", "hello feature\n")
    write(p + "/feature.txt", "feature\n")
    commit(p, "feature work")
    write(p + "/hello.txt", "hello feature, uncommitted\n")
    ctx.create_session(title="Boot")
    warm(ctx, p)
    d = f"directory={p}"
    vcs_reads(ctx, p, extra=[
        ("vcs.diff.branch.context1", f"/api/vcs/diff?{d}&mode=branch&context=1"),
        ("vcs.diff.branch.base", f"/api/vcs/diff?{d}&mode=branch&base=default"),
    ])
    time.sleep(0.5)
    mark = len(ctx.server.events)
    hg(p, "branch", "-q", "-f", "other")
    ev = ctx.wait_for("vcs.branch.updated", timeout=10, start=mark)
    ctx.rec("branch.event", ev and ev.get("data"))
    time.sleep(0.5)
    ctx.call("vcs.get.after", "GET", f"/api/vcs?{d}")
    ctx.call("vcs.diff.branch.after", "GET", f"/api/vcs/diff?{d}&mode=branch")
    mark = len(ctx.server.events)
    hg(p, "update", "-q", "-C", "default")
    ev = ctx.wait_for("vcs.branch.updated", timeout=10, start=mark)
    ctx.rec("branch.event.default", ev and ev.get("data"))
    time.sleep(0.5)
    ctx.call("vcs.get.default", "GET", f"/api/vcs?{d}")
    ctx.call("vcs.diff.branch.default", "GET", f"/api/vcs/diff?{d}&mode=branch")
    ctx.record_events(filt=lambda e: e.get("type") == "vcs.branch.updated" or (
        e.get("type") == "filesystem.changed" and str((e.get("data") or {}).get("file", "")).endswith("/.hg/branch")))


@scenario("hg", hold_for_title=0)
def hg_empty(ctx):
    """a repository with no commits: the global project, status and diffs of added and untracked files."""
    if not boot(ctx, commit_files=False):
        return
    hg(ctx.project, "add", "-q", "hello.txt", "src/main.rs")
    ctx.create_session(title="Boot")
    warm(ctx, ctx.project)
    vcs_reads(ctx, ctx.project, extra=[("project.list", "/api/project")])


@scenario("hg", hold_for_title=0)
def hg_nested(ctx):
    """git inside hg and hg inside git: the nearest marker wins; a store with no readable changesets."""
    if not boot(ctx, init=False):
        return
    p = ctx.project
    # a/ is git with an hg repository at a/h/; b/ is hg with a git repository at b/g/; c/ has a bare .hg folder.
    write(p + "/a/one.txt", "one\n")
    git(p + "/a", "init", "-q")
    git(p + "/a", "add", "-A")
    git(p + "/a", "commit", "-q", "-m", "a")
    write(p + "/a/h/app/two.txt", "two\n")
    hg(p + "/a/h", "init")
    commit(p + "/a/h", "h")
    write(p + "/a/h/app/two.txt", "two changed\n")
    write(p + "/b/three.txt", "three\n")
    # The outer hg repository lists the inner git directory as untracked; its index holds file times.
    write(p + "/b/.hgignore", "syntax: glob\ng/.git/index\n")
    hg(p + "/b", "init")
    commit(p + "/b", "b")
    write(p + "/b/g/app/four.txt", "four\n")
    git(p + "/b/g", "init", "-q")
    git(p + "/b/g", "add", "-A")
    git(p + "/b/g", "commit", "-q", "-m", "g")
    write(p + "/b/g/app/four.txt", "four changed\n")
    write(p + "/b/three.txt", "three changed\n")
    os.makedirs(p + "/c/.hg/store")
    write(p + "/c/five.txt", "five\n")
    ctx.create_session(title="Boot")
    for name, directory in [("a", p + "/a"), ("ah", p + "/a/h/app"), ("b", p + "/b"), ("bg", p + "/b/g/app"),
                            ("c", p + "/c")]:
        warm(ctx, directory)
        vcs_reads(ctx, directory, prefix=name + ".")
    ctx.call("project.list", "GET", "/api/project")


@scenario("hg", hold_for_title=0)
def hg_init(ctx):
    """vcs init with provider=hg in a markerless project, then the new repository's identity and vcs reads."""
    if not boot(ctx, init=False):
        return
    ctx.create_session(title="Boot")
    d = f"directory={ctx.project}"
    ctx.call("vcs.get.before", "GET", f"/api/vcs?{d}")
    ctx.call("vcs.init.unknown", "POST", f"/api/vcs/init?{d}&provider=svn", {})
    ctx.call("vcs.init", "POST", f"/api/vcs/init?{d}&provider=hg", {})
    ctx.rec("hg.dir", os.path.isdir(ctx.project + "/.hg"))
    warm(ctx, ctx.project)
    vcs_reads(ctx, ctx.project, extra=[("project.list", "/api/project")])
    ctx.call("vcs.init.again", "POST", f"/api/vcs/init?{d}&provider=hg", {})


@scenario("hg")
def hg_session(ctx):
    """a write tool in a Mercurial repository: no snapshots, so the session diff stays empty; vcs sees the file."""
    if not boot(ctx, script=[
        {"tools": [{"name": "write", "args": {"path": "$PROJECT/hello.txt", "content": "rewritten\n"}}]},
        {"text": "Wrote it."},
    ]):
        return
    sid = ctx.create_session()
    ctx.prompt(sid, "rewrite hello.txt")
    ctx.call("session.diff", "GET", f"/api/session/{sid}/diff")
    ctx.call("vcs.status", "GET", f"/api/vcs/status?directory={ctx.project}")
    ctx.record_events()
