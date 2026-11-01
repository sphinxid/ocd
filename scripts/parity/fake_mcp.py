#!/usr/bin/env python3
"""Minimal stdio MCP server (legacy 2025-11-25 handshake) for parity scenarios. Its tools/list mixes valid tools
with malformed ones: opencode 2.2 drops the malformed tools and keeps the rest. With `--echo` it lists typed tools
instead and answers every call with the arguments it received (as JSON text)."""
import json
import sys

TOOLS = [
    {"name": "good_one", "description": "A valid tool.", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}},
    {"name": "bad_schema", "description": "Input schema is not an object.", "inputSchema": {"type": "string"}},
    {"name": "bad_annotations", "description": "Hint is not a boolean.", "inputSchema": {"type": "object"},
     "annotations": {"readOnlyHint": "yes"}},
    {"name": "good_two", "description": "Another valid tool.", "inputSchema": {"type": "object"}},
]

ECHO_TOOLS = [
    {"name": "typed", "description": "Echo typed arguments (closed object).", "inputSchema": {
        "type": "object",
        "properties": {"count": {"type": "integer"}, "flag": {"type": "boolean"},
                       "tags": {"type": "array", "items": {"type": "string"}},
                       "size": {"anyOf": [{"type": "number"}, {"type": "null"}]},
                       "nested": {"type": "object", "properties": {"n": {"type": "integer"}}, "additionalProperties": False}},
        "required": ["count"], "additionalProperties": False}},
    {"name": "open", "description": "Echo arguments (open object).", "inputSchema": {
        "type": "object", "properties": {"count": {"type": "integer"}}}},
]

ECHO = "--echo" in sys.argv[1:]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": msg.get("params", {}).get("protocolVersion", "2025-11-25"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1.0.0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": ECHO_TOOLS if ECHO else TOOLS}})
    elif method == "tools/call":
        text = "received " + json.dumps(msg.get("params", {}).get("arguments")) if ECHO else "ok"
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
