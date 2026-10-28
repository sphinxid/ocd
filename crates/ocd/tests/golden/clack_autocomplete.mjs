// Records @clack/prompts 1.0.0-alpha.1 `autocomplete` and upstream 2.1.4's integration picker (built on
// @clack/core's AutocompletePrompt) output bytes per keypress for `src/clack/tests.rs`. Run with Bun next to
// node_modules with @clack/prompts@1.0.0-alpha.1 and a copy of cli/src/ui/integration-picker.ts that passes
// `io` (input/output) through: `FORCE_COLOR=1 bun clack_autocomplete.mjs > clack_autocomplete.json`.
// Output is piped: `autocomplete`'s limitOptions assumes 10 rows; the picker reads process.stdout.rows/columns,
// which each case sets (undefined columns: nothing wraps).
import { PassThrough } from "node:stream"
import { autocomplete } from "@clack/prompts"
import { selectIntegration } from "./integration-picker.ts"

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function run(kind, opts, keys, size = {}) {
  process.stdout.columns = size.columns
  process.stdout.rows = size.rows
  const input = new PassThrough()
  const output = new PassThrough()
  let buf = ""
  output.on("data", (d) => (buf += d.toString()))
  const done =
    kind === "picker"
      ? selectIntegration(opts.choices, opts.kind, { input, output })
      : autocomplete({ ...opts, input, output })
  await sleep(20)
  const frames = [buf]
  buf = ""
  let result = "<pending>"
  done.then(
    (v) => (result = typeof v === "symbol" ? "<cancel>" : v === undefined ? "<undefined>" : v),
    (e) => (result = `<error ${e.message}>`),
  )
  for (const k of keys) {
    input.write(k)
    await sleep(k === "\x1b" ? 120 : 20)
    frames.push(buf)
    buf = ""
  }
  await sleep(20)
  frames.push(buf)
  return { kind, opts, keys, columns: size.columns ?? null, rows: size.rows ?? null, frames, result }
}

const fruits = [
  { value: "apple", label: "Apple", hint: "red" },
  { value: "banana", label: "Banana" },
  { value: "cherry", label: "Cherry", hint: "small" },
  { value: "date", label: "Date", hint: "brown" },
]
const many = Array.from({ length: 12 }, (_, i) => ({ value: `v${i}`, label: `Option ${i}`, hint: `h${i}` }))
const integrations = [
  { value: "mcp-a", label: "Alpha MCP", category: "MCP", connected: true, hint: "needs authentication" },
  { value: "opencode", label: "OpenCode Zen", category: "Popular", connected: false },
  { value: "openai", label: "OpenAI", category: "Popular", connected: true },
  { value: "anthropic", label: "Anthropic", category: "Popular", connected: false },
  ...Array.from({ length: 9 }, (_, i) => ({ value: `svc${i}`, label: `Service ${i}`, category: "Services", connected: i === 2 })),
]
const out = []
out.push(await run("autocomplete", { message: "Select integration", options: fruits, maxItems: 8 }, ["b", "r", "\x7f", "\x7f", "\x1b[B", "\x1b[B", "x", "\x7f", "\r"]))
out.push(await run("autocomplete", { message: "Pick", options: fruits }, ["s", "m", "a", "\x1b[D", "\x1b[D", "z", "\r"]))
out.push(await run("autocomplete", { message: "Pick", options: fruits }, ["z", "z", "\r"]))
out.push(await run("autocomplete", { message: "Many", options: many, maxItems: 8 }, ["\x1b[A", ...Array(9).fill("\x1b[B"), "1", "\r"]))
out.push(await run("autocomplete", { message: "Pick", options: fruits }, ["c", "h", "\x1b"]))
out.push(await run("autocomplete", { message: "Pick", options: fruits }, ["\t", "j", "\x03"]))
out.push(await run("picker", { choices: integrations, kind: "integration" }, ["\x1b[B", "\x1b[B", "\x1b[B", "s", "e", "r", "\x1b[B", "\r"], { rows: 30, columns: 100 }))
out.push(await run("picker", { choices: integrations, kind: "integration" }, [...Array(8).fill("\x1b[B"), "\x1b[A", "\r"], { rows: 16, columns: 40 }))
out.push(await run("picker", { choices: integrations, kind: "integration" }, ["z", "z", "\r", "\x7f", "\x7f", "\r"]))
out.push(await run("picker", { choices: integrations.slice(0, 1), kind: "MCP server" }, ["a", "\x03"], { rows: 24, columns: 80 }))
out.push(await run("picker", { choices: integrations, kind: "integration" }, ["p", "o", "p", "\r"], { rows: 20, columns: 60 }))
console.log(JSON.stringify(out))
process.exit(0)
