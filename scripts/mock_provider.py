import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

calls = {"n": 0}

def chunk(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode()

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        text = json.dumps(body)
        import time; sys.stderr.write("%.3f %s\n" % (time.time(), "title" if "title generator" in text else ("tool" if '"role": "tool"' in text else "first"))); sys.stderr.flush()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        if "title generator" in text:
            out = [{"id": "t", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Smoke test"}}]}]
        elif '"role": "tool"' in text:
            out = [{"id": "b", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "The file says hi."}}]}]
        else:
            args = json.dumps({"path": sys.argv[2]})
            out = [{"id": "a", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read", "arguments": args}}]}}]},
                   {"id": "a", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}]
        if '"role": "tool"' in text or "title generator" in text:
            out.append({"id": "b", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 4, "total_tokens": 24}})
        for o in out:
            self.wfile.write(chunk(o))
        self.wfile.write(b"data: [DONE]\n\n")

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
