# Smoke-test helpers

- `mock_provider.py <port> <file>`: an OpenAI-compatible SSE server. The first request answers with a `read` tool call
  for `<file>`. A request carrying a tool result answers "The file says hi.", and title requests answer "Smoke test".
  `mock_provider.py <port> --script turns.json` serves a scripted conversation instead (reasoning, parallel tool
  calls, HTTP errors, 429 + Retry-After, cut streams, delays; format in `parity/mock.py`).
- `parity.py`: the differential parity suite against upstream opencode (scenarios in `parity/`; see
  `docs/PARITY.md`). `python3 scripts/parity.py --list` lists scenarios.
- `vt_render.py <capture> <cols> <rows>`: replays a `script -qfec ...` capture and prints the final screen.

Example (isolated state; the project's `opencode.json` points provider `mock` at the port):

```sh
python3 scripts/mock_provider.py 18471 "$PWD/project/hello.txt" &
export XDG_DATA_HOME=$PWD/s/data XDG_CONFIG_HOME=$PWD/s/config XDG_STATE_HOME=$PWD/s/state XDG_CACHE_HOME=$PWD/s/cache \
  OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_TEST_HOME=$PWD/s/home TERM=xterm-256color
cd project && target/release/ocd run --standalone "what is in hello.txt?"
# TUI: wait more than 1s before typing; under `script` nothing answers the terminal color probe
(sleep 5; printf 'what is in hello.txt?'; sleep 1; printf '\r'; sleep 20) | \
  timeout 9 script -qfec "stty cols 100 rows 30; ocd --standalone" tui.raw >/dev/null
python3 scripts/vt_render.py tui.raw 100 30
```
