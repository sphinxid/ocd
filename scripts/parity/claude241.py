"""2.4.1 changes to the Claude Agent provider (`opencode.provider.claude-agent`) against the stand-in `claude` (see
`claude.py`, whose setup these reuse): config folders (`configDir`, #62514), account rotation over a folder list
(#65035), a deleted session's process and records (#62649), model IDs kept across renames (#65032) and the provider
as a web search provider (#65704). Every config folder lives under the scenario's isolated HOME."""
import json
import os
import time

from parity import claude
from parity.registry import scenario

FOLDERS_LABEL = "claude.folders"


def _setup(ctx, body, provider=None, files=None):
    """`claude._setup` with Claude Agent provider config (settings and per-model settings) in the project."""
    log = claude._setup(ctx, body, files=files)
    if provider is not None:
        path = os.path.join(ctx.project, "opencode.json")
        with open(path) as f:
            config = json.load(f)
        config.setdefault("providers", {})["claude-agent"] = provider
        with open(path, "w") as f:
            json.dump(config, f, indent=2)
    return log


def _home(ctx, *parts):
    return os.path.join(ctx.state, "home", *parts)


def _folders(ctx, log):
    """Each spawn's mode and config folder: the login check, a chat or a web search."""
    out = []
    if os.path.exists(log):
        for line in open(log):
            entry = json.loads(line)
            if entry["event"] != "spawn":
                continue
            argv = entry["argv"]
            kind = "search" if "--tools=WebSearch" in argv else "chat" if "--include-partial-messages" in argv else "check"
            out.append([kind, entry["env"].get("CLAUDE_CONFIG_DIR")])
    ctx.rec(FOLDERS_LABEL, out)


def _exits(log):
    return sum(1 for line in open(log) if json.loads(line)["event"] == "exit") if os.path.exists(log) else 0


LOGGED_OUT = {"account": {"tokenSource": "none", "apiProvider": "firstParty"}}
LIMIT = [{"limit": {}}]


@scenario("claude241")
def claude_config_dir(ctx):
    """#62514: the provider's `configDir: "~/.claude-work"` becomes `CLAUDE_CONFIG_DIR` (with `~` expanded) for the
    login check and chats; a model with its own logged-out `configDir` fails before any prompt with the command that
    logs in to that folder, and leaves the provider's status alone; the chat's transcript stays in its folder."""
    log = _setup(ctx, {"turns": [[{"text": "COPPER-OTTER"}]]})
    model_dir = _home(ctx, "model-folder")
    spec = os.path.join(ctx.state, "claude-scenario.json")
    with open(spec) as f:
        body = json.load(f)
    body["folders"] = {model_dir: LOGGED_OUT}
    with open(spec, "w") as f:
        json.dump(body, f)
    path = os.path.join(ctx.project, "opencode.json")
    with open(path) as f:
        config = json.load(f)
    config.setdefault("providers", {})["claude-agent"] = {
        "settings": {"configDir": "~/.claude-work"},
        "models": {"opus": {"settings": {"configDir": model_dir}}},
    }
    with open(path, "w") as f:
        json.dump(config, f, indent=2)
    ctx.start()
    ctx.booted()
    claude._connect(ctx)
    claude._inventory(ctx, ".connected")
    sid = claude._session(ctx, model="opus")
    ctx.prompt(sid, "Reply with a marker")
    claude._inventory(ctx, ".after_model_folder")
    sid2 = claude._session(ctx)
    ctx.prompt(sid2, "Reply with a marker")
    ctx.call("messages.model_folder", "GET", f"/api/session/{sid}/message")
    ctx.call("messages.provider_folder", "GET", f"/api/session/{sid2}/message")
    projects = _home(ctx, ".claude-work", "projects")
    ctx.rec("transcripts.claude-work", sorted(
        len(os.listdir(os.path.join(projects, d))) for d in os.listdir(projects)) if os.path.isdir(projects) else [])
    ctx.record_events()
    _folders(ctx, log)
    claude._wire(ctx, log)


@scenario("claude241")
def claude_config_dir_default(ctx):
    """#62514: `configDir: "~/.claude"` names the default folder, which runs without `CLAUDE_CONFIG_DIR` so Claude
    Code keeps its usual state file."""
    log = _setup(ctx, {"turns": [[{"text": "COPPER-OTTER"}]]}, provider={"settings": {"configDir": "~/.claude"}})
    ctx.start()
    ctx.booted()
    claude._connect(ctx)
    sid = claude._session(ctx)
    ctx.prompt(sid, "Reply with a marker")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    _folders(ctx, log)


@scenario("claude241")
def claude_accounts(ctx):
    """#65035: `configDir: ["~/.acc-a", "~/.acc-b"]`: a turn the first account refuses at its plan limit before any
    output runs again on the second, unseen; the next turn stays there; a turn refused on both accounts shows one
    error with the reset time; an API key in settings would skip all this. Then a Claude Agent web search follows
    the account chats used last (#65704)."""
    output = {"query": "Node.js release schedule", "results": [
        {"tool_use_id": "srv_1", "content": [{"title": "Node.js Releases", "url": "https://nodejs.org/en/about/releases/"}]},
        "Node.js ships a major release every year."]}
    log = _setup(ctx, {"turns": [LIMIT, [{"text": "COPPER-OTTER"}], [{"text": "LILAC-HERON"}], LIMIT, LIMIT,
                                 [{"websearch": output}, {"hold": True}]]},
                 provider={"settings": {"configDir": ["~/.acc-a", "~/.acc-b"]}})
    ctx.start()
    ctx.booted()
    claude._connect(ctx)
    sid = claude._session(ctx)
    ctx.prompt(sid, "Reply with a marker")
    ctx.prompt(sid, "Another marker")
    ctx.prompt(sid, "And one more")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("websearch.query", "POST", "/api/websearch", {"query": "Node.js release schedule", "providerID": "claude-agent"})
    claude._inventory(ctx)
    ctx.record_events()
    _folders(ctx, log)
    claude._wire(ctx, log)


@scenario("claude241")
def claude_session_deleted(ctx):
    """#62649: deleting a Claude Agent session stops its `claude` (stdin ends, the stand-in exits) instead of
    leaving it for the idle timer."""
    log = _setup(ctx, {"turns": [[{"text": "COPPER-OTTER"}]]})
    ctx.start()
    ctx.booted()
    claude._connect(ctx)
    sid = claude._session(ctx)
    ctx.prompt(sid, "Reply with a marker")
    before = _exits(log)
    ctx.call("session.delete", "DELETE", f"/api/session/{sid}")
    deadline = time.time() + 20
    while time.time() < deadline and _exits(log) == before:
        time.sleep(0.2)
    ctx.rec("claude.exited_after_delete", _exits(log) - before)


@scenario("claude241")
def claude_renamed_models(ctx):
    """#65032: Claude Code lists a model under another ID (here `fable` → `claude-fable-5-1` and back): every ID
    ever listed stays in the catalog, a renamed model keeps the ID it was offered under, and the IDs no longer
    listed become `deprecated`."""
    first = [
        {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)"},
        {"value": "claude-fable-5-1", "resolvedModel": "claude-fable-5-1", "displayName": "Fable 5.1",
         "supportedEffortLevels": ["low", "high"]},
        {"value": "sonnet", "resolvedModel": "claude-sonnet-5", "displayName": "Sonnet 5"},
        {"value": "claude-retired", "displayName": "Retired"},
        {"value": "haiku", "resolvedModel": "claude-haiku-5-5", "displayName": "Haiku 5.5"},
    ]
    log = _setup(ctx, {"init": {"models": first}, "contextWindows": {"claude-fable-5-1": 500000, "haiku": 200000},
                       "turns": [[{"text": "COPPER-OTTER"}]]})
    ctx.start()
    ctx.booted()
    claude._connect(ctx)
    claude._inventory(ctx, ".first")
    spec = os.path.join(ctx.state, "claude-scenario.json")
    with open(spec) as f:
        body = json.load(f)
    body["init"]["models"] = [
        {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)"},
        {"value": "fable", "resolvedModel": "claude-fable-5-1", "displayName": "Fable 5.1",
         "supportedEffortLevels": ["low", "high"]},
        {"value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet 5.5"},
        {"value": "claude-sonnet-5", "resolvedModel": "claude-sonnet-5", "displayName": "Sonnet 5"},
        {"value": "haiku", "resolvedModel": "claude-haiku-5-5", "displayName": "Haiku 5.5"},
    ]
    with open(spec, "w") as f:
        json.dump(body, f)
    # A chat's own `claude` reports the new list.
    sid = claude._session(ctx)
    mark = len(ctx.server.events)
    ctx.prompt(sid, "Reply with a marker")
    ctx.wait_for("provider.updated", start=mark, timeout=20)
    time.sleep(2)
    claude._inventory(ctx, ".renamed")
    _folders(ctx, log)


@scenario("claude241")
def claude_websearch(ctx):
    """#65704: Claude Agent is a web search provider only while connected (never the default): a query runs a
    one-off `claude` with only WebSearch (haiku, thinking off, two turns, no transcript, outside the project, the
    provider's gateway settings), and the results carry Claude's summary on the first link."""
    output = {"query": "Node.js release schedule", "results": [
        {"tool_use_id": "srv_1", "content": [
            {"title": "Evolving the Node.js Release Schedule", "url": "https://nodejs.org/blog/announcements/evolving"},
            {"title": "Node.js Releases", "url": "https://nodejs.org/en/about/releases/"}]},
        "Node.js is changing its release schedule.", "  ", "Every release becomes LTS."]}
    # The relay closes `claude` as soon as the results arrive; the stand-in then waits for its input to end.
    log = _setup(ctx, {"turns": [[{"websearch": output}, {"hold": True}],
                                 LIMIT, [{"text": "I know this one."}]]},
                 provider={"settings": {"baseURL": "http://127.0.0.1:9/gateway"}})
    ctx.start()
    ctx.booted()
    ctx.call("websearch.providers.before", "GET", "/api/websearch/provider")
    ctx.call("websearch.query.before", "POST", "/api/websearch", {"query": "q", "providerID": "claude-agent"})
    claude._connect(ctx)
    deadline = time.time() + 20
    while time.time() < deadline:
        providers = ctx.data("websearch.poll", "GET", "/api/websearch/provider", record=False)
        if isinstance(providers, list) and any(p.get("id") == "claude-agent" for p in providers):
            break
        time.sleep(0.3)
    ctx.call("websearch.providers.after", "GET", "/api/websearch/provider")
    ctx.call("websearch.query", "POST", "/api/websearch", {"query": "Node.js release schedule", "providerID": "claude-agent"})
    ctx.call("websearch.query.limited", "POST", "/api/websearch", {"query": "q2", "providerID": "claude-agent"})
    ctx.call("websearch.query.no_search", "POST", "/api/websearch", {"query": "q3", "providerID": "claude-agent"})
    ctx.call("websearch.query.default", "POST", "/api/websearch", {"query": "q4"})
    _folders(ctx, log)
    claude._wire(ctx, log)
