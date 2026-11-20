#!/usr/bin/env python3
"""Stdio MCP server for parity scenarios that speaks the 2026-07-28 revision the way `@modelcontextprotocol/server`
2.4.0 does (results carry `resultType: "complete"`, `server/discover` answers the probe, `subscriptions/listen` is
acknowledged), so connections to it can be pooled (`pool: true`).

Flags:
  --pid-file=PATH   append this process's pid to PATH on start
  --wire-log=PATH   append every request method to PATH
  --legacy          speak only the legacy `initialize` handshake (`server/discover` is an unknown method)
  --events          offer MCP Events: `events/list` lists `job.finished` (push, `{job: string}` arguments) and
                    `poll.only`; each `events/stream` is held open and gets `notifications/events/active`
  --emit=PATH       with --events: while PATH holds JSON lines `{"eventId", "data", "cursor"}`, send each line once as
                    `notifications/events/event` (name `job.finished`) on every open stream
"""
import json
import os
import sys
import threading
import time

ARGS = sys.argv[1:]


def opt(name):
    for arg in ARGS:
        if arg.startswith(f"--{name}="):
            return arg.split("=", 1)[1]
    return None


LEGACY = "--legacy" in ARGS
EVENTS = "--events" in ARGS
PID_FILE, WIRE_LOG, EMIT = opt("pid-file"), opt("wire-log"), opt("emit")
SUBSCRIPTION = "io.modelcontextprotocol/subscriptionId"
lock = threading.Lock()
streams = []  # open events/stream request ids

if PID_FILE:
    with open(PID_FILE, "a") as f:
        f.write(f"{os.getpid()}\n")


def send(message):
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def complete(result):
    return {**result, "resultType": "complete",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake-modern", "version": "1.0.0"}}}


def capabilities():
    caps = {"tools": {}}
    if EVENTS:
        caps["extensions"] = {"io.modelcontextprotocol/events": {}}
    return caps


TOOLS = [{"name": "echo", "description": "Echo.", "inputSchema": {"type": "object", "properties": {}}}]
EVENT_TYPES = [
    {"name": "job.finished", "description": "A job ended", "delivery": ["push"],
     "inputSchema": {"type": "object", "properties": {"job": {"type": "string"}}, "required": ["job"]},
     "payloadSchema": {"type": "object", "properties": {"status": {"type": "string"}}, "required": ["status"]}},
    {"name": "poll.only", "delivery": ["poll"]},
]


def emitter():
    sent = set()
    while True:
        time.sleep(0.1)
        try:
            with open(EMIT) as f:
                lines = [line for line in f.read().splitlines() if line.strip()]
        except OSError:
            continue
        for line in lines:
            if line in sent:
                continue
            with lock:
                open_streams = list(streams)
            if not open_streams:
                continue
            sent.add(line)
            event = json.loads(line)
            for sid in open_streams:
                send({"jsonrpc": "2.0", "method": "notifications/events/event", "params": {
                    "eventId": event["eventId"], "name": "job.finished", "timestamp": "2026-11-10T00:00:00Z",
                    "data": event.get("data", {}), "cursor": event.get("cursor"), "_meta": {SUBSCRIPTION: sid}}})


if EVENTS and EMIT:
    threading.Thread(target=emitter, daemon=True).start()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    params = msg.get("params") or {}
    if WIRE_LOG and method:
        with open(WIRE_LOG, "a") as f:
            f.write(method + "\n")
    if method == "notifications/cancelled":
        with lock:
            if params.get("requestId") in streams:
                streams.remove(params.get("requestId"))
        continue
    if mid is None:
        continue
    modern = "io.modelcontextprotocol/protocolVersion" in (params.get("_meta") or {})
    if modern and LEGACY:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
        continue
    wrap = complete if modern else (lambda r: r)
    if method == "server/discover":
        send({"jsonrpc": "2.0", "id": mid, "result": complete(
            {"supportedVersions": ["2026-07-28"], "capabilities": capabilities(), "ttlMs": 0, "cacheScope": "private"})})
    elif method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": params.get("protocolVersion", "2025-11-25"), "capabilities": capabilities(),
            "serverInfo": {"name": "fake-legacy", "version": "1.0.0"}}})
    elif method == "subscriptions/listen":
        send({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged",
              "params": {"notifications": {}, "_meta": {SUBSCRIPTION: mid}}})
    elif method == "tools/list":
        # Like the SDK's server, list results carry their cache hints on 2026-07-28 (the client requires them).
        hints = {"ttlMs": 0, "cacheScope": "private"} if modern else {}
        send({"jsonrpc": "2.0", "id": mid, "result": wrap({"tools": TOOLS, **hints})})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": mid, "result": wrap({"content": [{"type": "text", "text": "ok"}]})})
    elif method == "events/list" and EVENTS:
        send({"jsonrpc": "2.0", "id": mid, "result": wrap({"events": EVENT_TYPES})})
    elif method == "events/stream" and EVENTS:
        with lock:
            streams.append(mid)
        send({"jsonrpc": "2.0", "method": "notifications/events/active",
              "params": {"cursor": params.get("cursor") or "c0", "truncated": False, "_meta": {SUBSCRIPTION: mid}}})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": wrap({})})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
