// Re-runs the stdlib parity cases (dumped with `STDLIB_DUMP=<file> cargo test -p oc-codemode --test 'stdlib_*'`)
// through upstream's interpreter and reports every case whose recorded expectation differs from what upstream
// returns now. Run from an upstream checkout with node_modules:
//
//   bun run stdlib-check.ts <upstream>/packages/codemode <dump.jsonl> [fixed.jsonl]
//
// `fixed.jsonl` receives `{file, code, expected, upstream}` for each differing case.
const [pkg, dump, fixed] = process.argv.slice(2)
const { CodeMode } = await import(`${pkg}/src/index.ts`)
const { Effect } = await import(Bun.resolveSync("effect", `${pkg}/src`))

const sameJson = (a: unknown, b: unknown): boolean => JSON.stringify(a) === JSON.stringify(b)
const runtime = CodeMode.make({})
const seen = new Set<string>()
const out: Array<string> = []
let total = 0
for (const line of (await Bun.file(dump).text()).split("\n")) {
  if (!line.trim()) continue
  const { file, code, expected } = JSON.parse(line)
  const key = `${file}\0${code}`
  if (seen.has(key)) continue
  seen.add(key)
  total++
  const result = await Effect.runPromise(runtime.execute(code))
  // Normalize the way the Rust side compares: a round trip through JSON text.
  const upstream = JSON.parse(JSON.stringify(result))
  if (!sameJson(upstream, JSON.parse(expected))) out.push(JSON.stringify({ file, code, expected, upstream }))
}
if (fixed) await Bun.write(fixed, out.join("\n") + (out.length ? "\n" : ""))
console.log(`${total} cases, ${out.length} differ from upstream`)
