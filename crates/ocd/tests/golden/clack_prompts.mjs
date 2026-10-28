// Records @clack/prompts 1.0.0-alpha.1 output bytes per keypress for `src/clack/tests.rs`. Run with Bun in a
// folder where @clack/prompts@1.0.0-alpha.1 is installed: `FORCE_COLOR=1 bun clack_prompts.mjs > clack_prompts.json`.
// Output is piped, so nothing wraps unless a case sets `process.stdout.columns`, and `limitOptions` assumes 10 rows.
import { PassThrough } from "node:stream"
import { multiselect, select, text, password, confirm } from "@clack/prompts"

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function run(kind, opts, keys, columns) {
  process.stdout.columns = columns
  const input = new PassThrough()
  const output = new PassThrough()
  let buf = ""
  output.on("data", (d) => (buf += d.toString()))
  const prompts = { multiselect, select, text, password, confirm }
  const done = prompts[kind]({ ...opts, input, output })
  await sleep(20)
  const frames = [buf]
  buf = ""
  let result
  done.then((v) => (result = v))
  for (const k of keys) {
    input.write(k)
    await sleep(k === "\x1b" ? 120 : 20)
    frames.push(buf)
    buf = ""
  }
  await sleep(20)
  frames.push(buf)
  return { kind, opts, keys, columns: columns ?? null, frames, result: typeof result === "symbol" ? "<cancel>" : result }
}

const fruits = [
  { value: "apple", label: "Apple", hint: "red" },
  { value: "banana", label: "Banana" },
  { value: "cherry", label: "Cherry", hint: "small" },
]
const many = Array.from({ length: 9 }, (_, i) => ({ value: `v${i}`, label: `Option ${i}` }))
const out = []
out.push(await run("multiselect", { message: "Pick fruits", options: fruits }, [" ", "\x1b[B", "j", " ", "\r"]))
out.push(await run("multiselect", { message: "Pick fruits", options: fruits, required: true }, ["\r", "k", "a", "A", "a", "\r"]))
out.push(await run("multiselect", { message: "Pick fruits", options: fruits, required: false }, ["\r"]))
out.push(await run("multiselect", { message: "Pick fruits", options: fruits, initialValues: ["banana"] }, ["\x1b[A", " ", "\x03"]))
out.push(await run("multiselect", { message: "Pick fruits", options: fruits }, ["\x1b"]))
out.push(await run("multiselect", { message: "Many", options: many, required: false }, ["\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", " ", "\r"]))
out.push(await run("select", { message: "Pick one", options: fruits, initialValue: "banana" }, ["\x1b[B", "\x1b[B", "l", "\r"]))
out.push(await run("select", { message: "Pick one", options: fruits }, ["\x1b[A", "\x03"]))
out.push(await run("select", { message: "Many", options: many }, ["k", "k", "\r"]))
out.push(await run("text", { message: "Name", placeholder: "your name", validate: (v) => (v ? undefined : "Required"), _validate: true }, ["\r", "a", "b", "\x1b[D", "c", "\x7f", "\r"]))
out.push(await run("text", { message: "Value", initialValue: "xyz" }, ["\x1b[D", "\x1b[D", "\x03"]))
out.push(await run("text", { message: "Empty" }, ["\r"]))
out.push(await run("password", { message: "Key", validate: (v) => (v ? undefined : "Required"), _validate: true }, ["\r", "s", "e", "\x1b[D", "\r"]))
out.push(await run("password", { message: "Key" }, ["x", "\x03"]))
const long = [
  { value: "a", label: "A rather long option label that wraps", hint: "with a hint too" },
  { value: "b", label: "Short" },
  { value: "c", label: "Supercalifragilisticexpialidocious-and-more", hint: "x" },
]
out.push(await run("multiselect", { message: "Choose the services you want to enable now", options: long }, ["\r", " ", "\x1b[B", " ", "\x1b[B", "\r"], 30))
out.push(await run("select", { message: "Pick", options: long }, ["\x1b[B", "\x1b[B", "\r"], 30))
out.push(await run("text", { message: "Long input", placeholder: "type something long here please" }, [..."hello world this is long", "\x1b[H", "\x05", "\x17", "\x15", "x", "\r"], 20))
out.push(await run("multiselect", { message: "Pick", options: fruits }, ["\x1b[C", "\x1b[D", "h", "l", "\x1bx", "y", "\x04", "\x1b[1;5A", "\r"]))
out.push(await run("confirm", { message: "Continue?" }, ["\x1b[C", "h", "\r"]))
out.push(await run("confirm", { message: "Continue?", initialValue: false }, ["y"]))
out.push(await run("confirm", { message: "Re-authenticate?" }, ["\x1b[B", "N"]))
out.push(await run("confirm", { message: "Expose?" }, ["\x1b"]))
out.push(await run("confirm", { message: "Expose?" }, ["\x1b[D", "\x03"]))
out.push(await run("confirm", { message: "Expose?" }, [" ", "x", "\r"]))
console.log(JSON.stringify(out))
process.exit(0)
