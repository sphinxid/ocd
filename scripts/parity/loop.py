"""Prompt-loop scenarios: the mock script drives the model; we record events, stored messages and LLM requests."""
from parity.registry import scenario


def run_prompt(ctx, script, text="hello", files=None, git_repo=False, config_extra=None, llm_full=False, before=None,
               after=None, timeout=60):
    ctx.set_script(script)
    ctx.make_project(files=files, git_repo=git_repo, config_extra=config_extra)
    ctx.start()
    sid = ctx.create_session()
    if before:
        before(ctx, sid)
    ctx.prompt(sid, text, timeout=timeout)
    if after:
        after(ctx, sid)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()
    ctx.record_llm(full=llm_full)
    return sid


@scenario("loop")
def loop_text_only(ctx):
    """A plain text answer (no tools)."""
    run_prompt(ctx, [{"text": ["Hello", " there!"], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}])


@scenario("loop")
def loop_read_tool(ctx):
    """The original smoke scenario: read tool then answer."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": "The file says hi.", "usage": {"prompt_tokens": 20, "completion_tokens": 4}},
    ], text="what is in hello.txt?", files={"hello.txt": "hi there\n"})
