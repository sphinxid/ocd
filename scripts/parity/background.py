"""Background compaction (`experimental.background_compaction`): a summary of the prefix the provider just processed
is written off the critical path once the context is within a fixed lead of the compaction ceiling (60k, at most half
the room the cycle started with; 2.2.7), held, and swapped in when automatic compaction is due. The mock reports large
prompt token counts so a short conversation crosses the limits: a 100k window compacts at 84k, and with a first step
measured at 30k a background summary starts at 57k."""
from parity.registry import scenario

SUMMARY = "You MUST summarize the conversation above"


def step(text, prompt_tokens):
    return {"text": text, "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 5}}


@scenario("compaction")
def compaction_background_swap(ctx):
    """a background summary starts after a step past the lead before the ceiling and is swapped in when compaction is
    due."""
    ctx.set_script([
        step("First answer.", 30_000),
        step("Second answer.", 72_000),
        {"match": SUMMARY, "text": "## Objective\n- Answer the questions", "usage": {"prompt_tokens": 70_000, "completion_tokens": 12}},
        step("Third answer.", 90_000),
        step("Fourth answer.", 20_000),
    ])
    ctx.make_project(config_extra={"experimental": {"background_compaction": True}})
    ctx.start()
    sid = ctx.create_session(title="Background compaction")
    for text in ("first question", "second question", "third question", "fourth question"):
        ctx.prompt(sid, text)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    ctx.record_llm()


@scenario("compaction")
def compaction_background_off(ctx):
    """the same conversation without the flag: compaction summarizes when due, no background request."""
    ctx.set_script([
        step("First answer.", 30_000),
        step("Second answer.", 72_000),
        step("Third answer.", 90_000),
        {"match": SUMMARY, "text": "## Objective\n- Answer the questions", "usage": {"prompt_tokens": 70_000, "completion_tokens": 12}},
        step("Fourth answer.", 20_000),
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Background compaction")
    for text in ("first question", "second question", "third question", "fourth question"):
        ctx.prompt(sid, text)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: not e.get("type", "").endswith(".delta"))
    ctx.record_llm()
