// Re-records the test262 sample (tests/fixtures/upstream-2.3.0/test262) on the language 2.4.1 runs: psychoscript
// 0.2.1, with a port of upstream's test/test262/run.ts harness (eb2dfb2638^, before the language left the repository)
// over psychoscript's runtime modules. Each record keeps its program (the vendored file after run.ts's prelude) and
// gets the new result. Run with Bun 1.4.4 from anywhere:
//
//   bun run test262-2.4.1.ts <opencode 2.4.1 checkout> crates/oc-codemode/tests/fixtures/upstream-2.3.0/test262 \
//     crates/oc-codemode/tests/fixtures/upstream-2.4.1/test262
import { readdirSync, mkdirSync } from "node:fs"
import path from "node:path"

const [checkout, from, to] = process.argv.slice(2)
const dist = path.dirname(Bun.resolveSync("psychoscript", path.join(checkout!, "packages/codemode/src")))
const load = (file: string) => import(path.join(dist, file))
const { finishExecution, startProgram } = await load("runtime/execute.js")
const { Throw } = await load("runtime/model.js")
const { createErrorValue } = await load("runtime/intrinsics.js")
const { constructor, fn, methods } = await load("runtime/native.js")
const { Callable, define, get, hidden, Arr, Fn, Obj } = await load("runtime/objects.js")

const show = (value: unknown): string => {
  if (typeof value === "string") return JSON.stringify(value)
  if (Object.is(value, -0)) return "-0"
  if (value instanceof Arr) return `[${value.items.map(show).join(", ")}]`
  if (value instanceof Fn) return "program function"
  if (value instanceof Callable) return String(get(value, "name"))
  if (!(value instanceof Obj)) return String(value)
  const message = get(value, "message")
  return typeof message === "string" ? `${String(get(value, "name") ?? "object")}: ${message}` : "object"
}

const harness = (ctx: any) => {
  const builtins = ctx.builtins
  const test262Prototype = new Obj(builtins.Object)
  define(test262Prototype, "name", "Test262Error", hidden)
  const test262 = (args: Array<unknown>) => createErrorValue(test262Prototype, args[0] === undefined ? "" : String(args[0]))
  const fail = (message: string) => {
    throw new Throw(test262([message]))
  }
  const prefix = (message: unknown) => (message === undefined ? "" : `${String(message)} `)
  const expectedName = (ctor: unknown) => (ctor instanceof Callable ? String(get(ctor, "name")) : show(ctor))
  const compare = (a: any, b: any) =>
    a instanceof Arr && b instanceof Arr && a.items.length === b.items.length && a.items.every((v: unknown, i: number) => Object.is(v, b.items[i]))
  const test262Error = constructor(builtins, test262Prototype, {
    name: "Test262Error",
    call: (_: unknown, args: Array<unknown>) => test262(args),
    construct: (args: Array<unknown>) => test262(args),
  })
  methods(builtins, test262Error, [["thrower", 1, (_: unknown, args: Array<unknown>) => fail(String(args[0]))]])
  const compareArray = fn(builtins, "compareArray", 2, (_: unknown, args: Array<unknown>) => compare(args[0], args[1]))
  methods(builtins, compareArray, [["format", 1, (_: unknown, args: Array<unknown>) => show(args[0])]])
  const assert = fn(builtins, "$assert", 2, (_: unknown, args: Array<unknown>) =>
    args[0] === true ? undefined : fail(args[1] === undefined ? `Expected true but got ${show(args[0])}` : String(args[1])),
  )
  methods(builtins, assert, [
    ["sameValue", 3, (_: unknown, a: Array<unknown>) => (Object.is(a[0], a[1]) ? undefined : fail(`${prefix(a[2])}Expected SameValue(«${show(a[0])}», «${show(a[1])}») to be true`))],
    ["notSameValue", 3, (_: unknown, a: Array<unknown>) => (Object.is(a[0], a[1]) ? fail(`${prefix(a[2])}Expected SameValue(«${show(a[0])}», «${show(a[1])}») to be false`) : undefined)],
    ["compareArray", 3, (_: unknown, a: Array<unknown>) => (compare(a[0], a[1]) ? undefined : fail(`Actual ${show(a[0])} and expected ${show(a[1])} should have the same contents. ${prefix(a[2])}`))],
    ["notThrown", 2, (_: unknown, a: Array<unknown>) => fail(`${prefix(a[1])}Expected a ${expectedName(a[0])} to be thrown but no exception was thrown at all`)],
    [
      "thrown",
      3,
      (_: unknown, a: Array<unknown>) => {
        if (!(a[2] instanceof Obj)) return fail(`${prefix(a[1])}Thrown value was not an object!`)
        const actual = get(a[2], "constructor")
        return actual === a[0] ? undefined : fail(`${prefix(a[1])}Expected a ${expectedName(a[0])} but got a ${show(actual)}`)
      },
    ],
  ])
  return [
    ["$assert", assert],
    ["compareArray", compareArray],
    ["Test262Error", test262Error],
    ["$DONE", fn(builtins, "$DONE", 1, () => undefined)],
    ["$DONOTEVALUATE", fn(builtins, "$DONOTEVALUATE", 0, () => { throw new Throw("Test262: This statement should not be evaluated.") })],
  ]
}

let total = 0
let changed = 0
for (const group of ["built-ins", "language"]) {
  mkdirSync(path.join(to!, group), { recursive: true })
  for (const name of readdirSync(path.join(from!, group))) {
    const lines = (await Bun.file(path.join(from!, group, name)).text()).split("\n").filter(Boolean)
    const out: Array<string> = []
    for (const line of lines) {
      const record = JSON.parse(line)
      const ended = await finishExecution(
        startProgram({ limits: { timeoutMs: 5000, maxOutputBytes: undefined }, programGlobals: (ctx: any) => harness(ctx) }, record.code).run(),
      )
      const { metadata: _, ...rest } = ended
      const result = JSON.parse(JSON.stringify({ ...rest, toolCalls: [] }))
      if (JSON.stringify(result) !== JSON.stringify(record.result)) changed++
      total++
      out.push(JSON.stringify({ ...record, result }))
    }
    await Bun.write(path.join(to!, group, name), out.join("\n") + "\n")
  }
}
console.log(`${total} programs, ${changed} results changed from the 2.3.0 recording`)
