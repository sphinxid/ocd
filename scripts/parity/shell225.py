"""opencode 2.2.5 shell permission scanner: the built-in scanner always runs (no tree-sitter), redirects and
commandless redirects are permission resources, a dynamic `cd` stays a resource, eval sinks are refused, and
PowerShell method calls are resources saved as `{head}(*`."""
import os

from parity.registry import scenario

# The PowerShell scenario runs a stand-in `pwsh` that only prints its arguments: the permission scan is what is
# compared, and a real PowerShell would make the output depend on the machine.
FAKE_PWSH = "#!/bin/sh\nprintf 'pwsh:'\nprintf ' [%s]' \"$@\"\nprintf '\\n'\n"


RACY = ("shell.exited", "session.usage.updated", "session.tool.progress")


def _shell_calls(commands):
    script = [{"tools": [{"name": "shell", "id": f"call_{i}", "args": {"command": command}}]}
              for i, command in enumerate(commands)]
    return script + [{"text": "Done."}]


def _run(ctx, commands, shell, files, executable=()):
    ctx.set_script(_shell_calls(commands))
    ctx.make_project(files=files, config_extra={"shell": shell.replace("$PROJECT", ctx.project),
                                                "permission": {"shell": "ask"}})
    for rel in executable:
        os.chmod(os.path.join(ctx.project, rel), 0o755)
    ctx.start()
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "run them", wait=False)
    # Reply `always` to every ask in order, so the saved patterns are compared too.
    answered = set()
    while True:
        ev = ctx.wait_for("permission.asked", start=mark, timeout=15,
                          pred=lambda e: e["data"]["id"] not in answered)
        if ev is None:
            break
        pid = ev["data"]["id"]
        answered.add(pid)
        ctx.call("permission.reply", "POST", f"/api/session/{sid}/permission/{pid}/reply", {"decision": "always"})
    ctx.wait_idle(sid, start=mark)
    ctx.call("permission.saved", "GET", "/api/permission/saved")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    # A fast command's shell.exited races the step's usage and progress events; the permission events stay.
    ctx.record_events(filt=lambda e: e.get("type") not in RACY)
    ctx.record_llm()


@scenario("shell225")
def shell225_bash_permissions(ctx):
    """bash: a redirected command, a commandless redirect, a saved prefix reused, a dynamic `cd`, an eval sink
    (refused), a group redirect and a heredoc with a redirect, each asked with `shell: ask` and saved."""
    _run(ctx, [
        "echo hi > out.txt",
        "> empty.txt",
        "echo again > out.txt",
        "D=sub; cd $D && ls",
        "x='$(echo hi)'; eval \"echo $x\"",
        "{ echo a; echo b; } >> log.txt",
        "cat <<EOF > here.txt\nbody\nEOF",
        "cd sub && ls",
    ], "/bin/bash", {"sub/a.txt": "a\n"})


@scenario("shell225")
def shell225_powershell_permissions(ctx):
    """PowerShell (a stand-in `pwsh`): member and static method calls, an expression with a file redirect, a dynamic
    and a -StackName Set-Location/Push-Location, and a static Set-Location."""
    _run(ctx, [
        "$x = 'abc'; $x.ToUpper()",
        "[System.IO.Path]::GetFileName('a/b.txt')",
        "'x' >> notes.txt",
        "Set-Location $target; Get-ChildItem",
        "Push-Location -StackName s sub",
        "Get-ChildItem    -Path sub",
        "Set-Location sub; Get-ChildItem",
    ], "$PROJECT/bin/pwsh", {"sub/a.txt": "a\n", "bin/pwsh": FAKE_PWSH}, executable=["bin/pwsh"])
