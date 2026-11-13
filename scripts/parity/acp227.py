"""`opencode acp` over stdio (2.2.7 `cli/src/acp`): a background subagent the root turn starts keeps running after the
turn ends or is cancelled, and the agent follows it (`turn.ts` `handoff`/`followChildren`): its child updates and its
permission asks still reach the client until it ends, while the root session's own later work (its answer to the
subagent's report) is not streamed. Both binaries run `acp` in the project with the isolated environment and talk to
their own private server; the dump is every JSON-RPC message the client received, in order."""
import json
import os
import subprocess
import threading
import time

from parity.registry import scenario

CHILD_UPDATES = {"_meta": {"opencode/child-session-updates": True}}


class Acp:
    """A minimal ACP client: newline-delimited JSON-RPC over the agent's stdio."""

    def __init__(self, ctx, permission="once"):
        env = ctx.env()
        env["PWD"] = ctx.project
        self.proc = subprocess.Popen([ctx.binary, "acp"], cwd=ctx.project, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.messages = []
        self.cond = threading.Condition()
        self.next_id = 0
        self.permission = permission
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except Exception:
                msg = {"unparsed": line}
            with self.cond:
                self.messages.append(msg)
                self.cond.notify_all()
            if msg.get("method") == "session/request_permission" and "id" in msg:
                self.send({"jsonrpc": "2.0", "id": msg["id"],
                           "result": {"outcome": {"outcome": "selected", "optionId": self.permission}}})

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def start(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        return self.next_id

    def wait(self, pred, timeout=30):
        deadline = time.time() + timeout
        with self.cond:
            while True:
                for m in self.messages:
                    if pred(m):
                        return m
                left = deadline - time.time()
                if left <= 0:
                    return None
                self.cond.wait(left)

    def response(self, rid, timeout=60):
        return self.wait(lambda m: m.get("id") == rid and "method" not in m, timeout)

    def request(self, method, params, timeout=60):
        return self.response(self.start(method, params), timeout)

    def quiet(self, gap=2.0, timeout=30):
        """Waits until no message arrived for `gap` seconds."""
        deadline = time.time() + timeout
        with self.cond:
            count = len(self.messages)
            while time.time() < deadline:
                self.cond.wait(gap)
                if len(self.messages) == count:
                    return
                count = len(self.messages)

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(10)
        except Exception:
            self.proc.kill()
            self.proc.wait(5)
        return self.proc.returncode


def child_update(status=None):
    def pred(m):
        if m.get("method") != "opencode/session/child_update":
            return False
        p = m.get("params") or {}
        return status is None or p.get("status") == status
    return pred


def transcript(ctx, acp, prompt_id, label="acp"):
    """Every message the client received, in three ordered lists: the root session's updates with the responses and
    requests, the child updates, and everything after the prompt's response (what the background follower sent).
    The server publishes a background child's start and the root's tool result concurrently, so how the first two
    lists interleave during the turn is a race in both binaries. The model and command lists are summarized."""
    out = []
    for m in acp.messages:
        m = json.loads(json.dumps(m))
        update = (m.get("params") or {}).get("update") or {}
        if update.get("sessionUpdate") == "available_commands_update":
            update["availableCommands"] = len(update.get("availableCommands") or [])
        result = m.get("result")
        if isinstance(result, dict) and "configOptions" in result:
            result["configOptions"] = [o.get("id") for o in result["configOptions"]]
        if isinstance(result, dict) and "agentCapabilities" in result:
            m["result"] = {"protocolVersion": result.get("protocolVersion"),
                           "_meta": result.get("_meta"), "agentCapabilities": result.get("agentCapabilities")}
        out.append(m)
    child = [m for m in out if m.get("method") == "opencode/session/child_update"]
    ctx.rec(label + ".root", [m for m in out if m.get("method") != "opencode/session/child_update"])
    ctx.rec(label + ".child", child)
    after = next((i for i, m in enumerate(out) if m.get("id") == prompt_id and "method" not in m), len(out))
    ctx.rec(label + ".after_response", out[after + 1:])


def record_llm(ctx):
    """The root's and the child's requests, each in order (they race each other)."""
    groups = {}
    for r in ctx.mock.main_requests():
        msgs = [m for m in r["body"].get("messages", []) if m.get("role") != "system"]
        first = str((msgs[0] if msgs else {}).get("content"))
        key = "child" if first.startswith("You are a subagent") else "root"
        groups.setdefault(key, []).append(msgs)
    for key in ("root", "child"):
        ctx.rec("llm." + key, groups.get(key, []))


def setup(ctx, script, config_extra=None):
    ctx.set_script(script)
    ctx.make_project(config_extra=config_extra)
    acp = Acp(ctx)
    acp.request("initialize", {"protocolVersion": 1, "clientCapabilities": CHILD_UPDATES})
    sid = acp.request("session/new", {"cwd": ctx.project, "mcpServers": []})["result"]["sessionId"]
    # The commands update follows the response.
    acp.wait(lambda m: (m.get("params") or {}).get("update", {}).get("sessionUpdate") == "available_commands_update")
    return acp, sid


def launch(task, call="call_launch"):
    return {"match": "start the research", "tools": [{"name": "subagent", "id": call, "args": {
        "agent": "general", "description": "Research notes", "prompt": task, "background": True}}]}


@scenario("acp")
def acp_child_after_turn(ctx):
    """the root turn starts a background subagent and ends; the agent keeps sending the child's updates until it
    completes, and the root session's answer to its report is not streamed."""
    acp, sid = setup(ctx, [
        launch("CHILD-A: summarize the notes"),
        {"match": "working in the background", "text": "Launched the research."},
        {"match": "CHILD-A: summarize", "delay": 2.0, "text": "Child report: three notes."},
        {"match": "Child report: three notes", "text": "The research is done."},
    ])
    rid = acp.start("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]})
    acp.response(rid)
    acp.wait(child_update("completed"), timeout=30)
    acp.quiet(gap=2.0)
    transcript(ctx, acp, rid)
    ctx.rec("acp.exit", acp.close())
    record_llm(ctx)


@scenario("acp")
def acp_child_outlives_cancel(ctx):
    """the root turn starts a background subagent and is cancelled mid-step; the subagent keeps running, its
    permission ask (`read: ask`) still reaches the client and is answered, and its updates arrive until it completes."""
    outside = os.path.join(os.path.dirname(ctx.project), "outside-notes.txt")
    acp, sid = setup(ctx, [
        launch("CHILD-B: read the outside notes"),
        {"match": "working in the background", "hang": True},
        {"match": "CHILD-B: read the outside", "delay": 2.0,
         "tools": [{"name": "read", "id": "call_outside", "args": {"path": outside}}]},
        {"match": "outside line one", "text": "Child report: one line."},
        {"match": "Child report: one line", "text": "Noted the report."},
    ], config_extra={"permission": {"read": "ask"}})
    with open(outside, "w") as f:
        f.write("outside line one\n")
    rid = acp.start("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]})
    acp.wait(child_update("running"), timeout=30)
    # Cancel once the root's second step is waiting on the model (the hanging turn was taken).
    deadline = time.time() + 30
    while not ctx.mock.used[1] and time.time() < deadline:
        time.sleep(0.05)
    time.sleep(0.3)
    acp.notify("session/cancel", {"sessionId": sid})
    acp.response(rid)
    acp.wait(child_update("completed"), timeout=30)
    acp.quiet(gap=2.0)
    transcript(ctx, acp, rid)
    ctx.rec("acp.exit", acp.close())
    record_llm(ctx)
    os.remove(outside)


@scenario("acp")
def acp_child_close_stops(ctx):
    """closing the session while its background subagent runs stops the following: nothing about the child reaches
    the client after the close response."""
    acp, sid = setup(ctx, [
        launch("CHILD-C: list the notes"),
        {"match": "working in the background", "text": "Launched the research."},
        {"match": "CHILD-C: list", "delay": 3.0, "text": "Child report: two notes."},
        {"match": "Child report: two notes", "text": "The research is done."},
    ])
    rid = acp.start("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "start the research"}]})
    acp.response(rid)
    acp.request("session/close", {"sessionId": sid})
    time.sleep(5.0)
    acp.quiet(gap=1.0)
    transcript(ctx, acp, rid)
    ctx.rec("acp.exit", acp.close())
    record_llm(ctx)
