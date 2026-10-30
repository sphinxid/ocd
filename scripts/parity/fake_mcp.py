#!/usr/bin/env python3
"""Minimal stdio MCP server (legacy 2025-11-25 handshake) for parity scenarios. Its tools/list mixes valid tools
with malformed ones: opencode 2.2 drops the malformed tools and keeps the rest."""
import json
import sys

TOOLS = [
    {"name": "good_one", "description": "A valid tool.", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}},
    {"name": "bad_schema", "description": "Input schema is not an object.", "inputSchema": {"type": "string"}},
    {"name": "bad_annotations", "description": "Hint is not a boolean.", "inputSchema": {"type": "object"},
     "annotations": {"readOnlyHint": "yes"}},
    {"name": "good_two", "description": "Another valid tool.", "inputSchema": {"type": "object"}},
]


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
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "ok"}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found"}})
