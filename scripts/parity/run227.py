"""`run` as rewritten in opencode 2.2.7 (`run/noninteractive.ts`): message arguments, piped stdin, `--worktree`,
`--archived`, the wait for background jobs, retries, permission and question answers, and SIGINT/SIGTERM.

Every run uses `--standalone` and the mock model explicitly, so nothing can reach a real provider."""
import json
import os
import signal
import subprocess
import time

from parity.registry import scenario

RUN = ["run", "--standalone", "-m", "mock/m1"]


def boot(ctx, script=(), git_repo=False, config_extra=None):
    ctx.set_script(list(script))
    ctx.make_project(files={"hello.txt": "hi\n"}, git_repo=git_repo, config_extra=config_extra)


def run(ctx, label, args, script=(), stdin=None, timeout=90):
    """One `run` with its own script; the conversation it sent is recorded too."""
    ctx.set_script(list(script))
    ctx.cli(label, [*RUN, *args], stdin=stdin, timeout=timeout)
    ctx.record_llm(label + ".llm")


def signalled(ctx, label, args, sig, after=3.0, script=({"hang": True},)):
    """A run stopped by `sig` once the model request is pending: exit code and output."""
    ctx.set_script(list(script))
    env = ctx.env()
    env["PWD"] = ctx.project
    p = subprocess.Popen([ctx.binary, *RUN, *args], cwd=ctx.project, env=env, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    deadline = time.time() + 30
    while time.time() < deadline and not ctx.mock.main_requests():
        time.sleep(0.05)
    time.sleep(after)
    os.kill(p.pid, sig)
    try:
        out, err = p.communicate(timeout=30)
        code = p.returncode
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        out, err = p.communicate()
        code = "TIMEOUT"
    try:
        os.killpg(p.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    ctx.rec(label, {"exit": code, "stdout": out, "stderr": err})


@scenario("run227", hold_for_title=0)
def run227_args(ctx):
    """Message arguments are joined with spaces as given (2.2.5 quoted those with spaces, and `a -- b` sent "a b b");
    piped stdin is appended, a pipe read in full even with a message argument."""
    boot(ctx)
    answer = [{"text": "ok"}]
    run(ctx, "run a -- b", ["a", "--", "b"], answer)
    run(ctx, "run quoted", ["say hello world", 'it\'s "quoted"'], answer)
    run(ctx, "run message + stdin", ["from args"], answer, stdin="from stdin\n")
    run(ctx, "run stdin only", [], answer, stdin="only stdin\n")
    run(ctx, "run empty stdin", [], answer, stdin="")


@scenario("run227", hold_for_title=0)
def run227_background(ctx):
    """A background shell started by the prompt holds the run until it finishes and the model answered its notice:
    `waiting for 1 background job` on stderr, or the `background_wait` JSON event."""
    boot(ctx)
    script = [
        {"tools": [{"name": "shell", "args": {"command": "sleep 2; echo bg-done", "background": True}}]},
        {"text": "Waiting for the command."},
        {"match": "bg-done", "text": "The command finished."},
    ]
    run(ctx, "run background", ["run it in the background"], script)
    run(ctx, "run background --format json", ["--format", "json", "run it in the background"], script)


@scenario("run227", hold_for_title=0)
def run227_retry(ctx):
    """A retried model request: `! retrying (attempt 1): ...` on stderr, or the `retry` JSON event."""
    boot(ctx)
    busy = {"status": 503, "body": {"error": {"message": "Overloaded", "type": "server_error"}},
            "headers": {"retry-after-ms": "10"}}
    run(ctx, "run retry", ["hi"], [busy, {"text": "Recovered."}])
    run(ctx, "run retry --format json", ["--format", "json", "hi"], [busy, {"text": "Recovered."}])


@scenario("run227", hold_for_title=0)
def run227_permission_question(ctx):
    """Permission requests are rejected (or approved with --auto); a question is cancelled with feedback."""
    boot(ctx, config_extra={"permission": {"read": "ask"}})
    read = {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]}
    run(ctx, "run permission", ["read hello.txt"], [read, {"text": "Could not read it."}])
    run(ctx, "run permission --auto", ["--auto", "read hello.txt"], [read, {"text": "It says hi."}])
    question = {"tools": [{"name": "question", "args": {"questions": [{
        "question": "Which color?", "header": "Color",
        "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cool"}]}]}}]}
    run(ctx, "run question", ["ask me"], [question, {"text": "No answer, picking red."}])
    run(ctx, "run question --format json", ["--format", "json", "ask me"], [question, {"text": "Red."}])


@scenario("run227", hold_for_title=0)
def run227_worktree(ctx):
    """`--worktree[=name]` starts the session in a new git worktree (`worktree <root>` on stderr); it cannot be combined
    with --continue or --session."""
    boot(ctx, git_repo=True)
    answer = [{"text": "ok"}]
    run(ctx, "run --worktree=feat", ["--worktree=feat", "hi"], answer)
    run(ctx, "run -w=feat2 --format json", ["-w=feat2", "--format", "json", "hi"], answer)
    run(ctx, "run --worktree --continue", ["--worktree", "--continue", "hi"], answer)
    run(ctx, "run -w --session", ["-w", "--session", "ses_x", "--format", "json", "hi"], answer)


@scenario("run227", hold_for_title=0)
def run227_archived(ctx):
    """`--archived` creates the session archived (out of session lists); `--continue` skips archived sessions."""
    boot(ctx)
    answer = [{"text": "ok"}]
    run(ctx, "run plain", ["first"], answer)
    run(ctx, "run --archived --title", ["--archived", "--title", "Hidden", "second"], answer)
    ctx.cli("session list", ["session", "list", "--standalone", "--format", "json"], timeout=60)
    run(ctx, "run --continue", ["--continue", "third"], answer)
    run(ctx, "run --continue --fork --archived", ["--continue", "--fork", "--archived", "fourth"], answer)
    ctx.cli("session list again", ["session", "list", "--standalone", "--format", "json"], timeout=60)


@scenario("run227", hold_for_title=0, timeout=240)
def run227_signal(ctx):
    """SIGINT or SIGTERM while the model request is pending cancels the session; the process exits 130 or 143."""
    boot(ctx)
    signalled(ctx, "run SIGINT", ["hi"], signal.SIGINT)
    signalled(ctx, "run SIGTERM --format json", ["--format", "json", "hi"], signal.SIGTERM)
    signalled(ctx, "run SIGTERM background", ["run it in the background"], signal.SIGTERM, after=1.0, script=[
        {"tools": [{"name": "shell", "args": {"command": "sleep 30", "background": True}}]},
        {"text": "Waiting for the command."},
    ])
