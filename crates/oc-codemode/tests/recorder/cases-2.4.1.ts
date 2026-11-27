// Re-runs the programs of tests/fixtures/cases-2.4.1.json through an upstream Code Mode package and rewrites their
// results (each `gap` note is kept). Run from anywhere with the package's dependencies installed:
//
//   TZ=UTC bun run cases-2.4.1.ts <opencode>/packages/codemode crates/oc-codemode/tests/fixtures/cases-2.4.1.json
//
// Every program runs in one runtime, which tests/upstream_2_3_1.rs rebuilds: the tools `echo` (returns its input)
// and `shop.show` (returns nothing), both with an any-JSON input, and the extension `host`, whose `echo` returns its
// arguments, `echoAsync` does the same asynchronously, `bytes` returns a host Buffer of "host", and `open` throws
// a host Error with a cause.
const [pkg, file] = process.argv.slice(2)
const { CodeMode, Extension, Tool } = await import(`${pkg}/src/index.ts`)
const { Effect } = await import(Bun.resolveSync("effect", `${pkg}/src`))
const echo = Tool.make({ description: "Echo the input", input: {}, output: {}, execute: (input: unknown) => Effect.succeed(input) })
const show = Tool.make({ description: "Show raw input", input: { type: "object" }, execute: () => Effect.succeed("ok") })
const host = Extension.make({
  name: "host",
  globals: {
    echo: (...args: Array<unknown>) => args,
    echoAsync: Extension.async(async (...args: Array<unknown>) => args),
    bytes: () => Buffer.from("host"),
    open: () => {
      const cause = Object.assign(new Error("ENOENT: no such file or directory, open 'x'"), {
        code: "ENOENT",
        errno: -2,
        path: "x",
        detail: { retried: true },
      })
      throw new Error("open failed", { cause })
    },
  },
})
const runtime = CodeMode.make({ tools: { echo, shop: { show } }, extensions: [host], limits: { timeoutMs: 5000 } })
const cases: Array<{ code: string; result: unknown; gap?: string }> = await Bun.file(file).json()
let changed = 0
for (const item of cases) {
  // Normalize the way the Rust side compares: a round trip through JSON text.
  const result = JSON.parse(JSON.stringify(await Effect.runPromise(runtime.execute(item.code))))
  if (JSON.stringify(result) !== JSON.stringify(item.result)) changed++
  item.result = result
}
await Bun.write(file, JSON.stringify(cases, null, 1) + "\n")
console.log(`${cases.length} cases, ${changed} results changed`)
