"""OpenAI-compatible SSE mock provider (CLI wrapper around scripts/parity/mock.py).

usage:
  mock_provider.py <port> <file>             legacy smoke script: `read <file>` tool call, then "The file says hi."
                                             (repeated, so it serves many prompts); titles answer "Smoke test"
  mock_provider.py <port> --script <json>    a turn script (see scripts/parity/mock.py for the turn format)
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from parity.mock import Mock, legacy_script  # noqa: E402


def main():
    port = int(sys.argv[1])
    if len(sys.argv) > 3 and sys.argv[2] == "--script":
        with open(sys.argv[3]) as f:
            script = json.load(f)
    else:
        script = legacy_script(sys.argv[2]) * 1000
    mock = Mock(port, script)
    sys.stderr.write(f"mock provider on 127.0.0.1:{mock.port}\n")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        mock.close()


if __name__ == "__main__":
    main()
