// Regenerates tests/fixtures/path/glob-calls.json: Bun's `path.<flavour>.matchesGlob` on generated patterns and
// paths, as many matches as misses. Run with Bun 1.4.4:
//
//   bun run glob-calls.ts crates/oc-codemode/tests/fixtures/path/glob-calls.json
//
// Each flavour is recorded in a process of its own (it runs this script again): Bun 1.4.4 keeps state between
// `matchesGlob` calls of different flavours, so a pattern can match differently right after the other flavour used it.
const flavour = process.argv[3] as "posix" | "win32" | undefined
if (flavour === undefined) {
  const parts = ["posix", "win32"].map((name) => {
    const part = `${process.argv[2]}.${name}`
    Bun.spawnSync([process.execPath, "run", import.meta.path, part, name], { stdout: "inherit", stderr: "inherit" })
    return part
  })
  const all = (await Promise.all(parts.map((part) => Bun.file(part).json()))).flat()
  for (const part of parts) await Bun.file(part).delete()
  await Bun.write(process.argv[2], "[\n" + all.map((c: unknown) => JSON.stringify(c)).join(",\n") + "\n]\n")
  process.exit(0)
}
import path from "path"
let seed = flavour === "posix" ? 0x6107 : 0x6108
const rand = () => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return seed / 4294967296 }
const pick = <T>(items: ReadonlyArray<T>): T => items[Math.floor(rand() * items.length)]!
const pat = ["a", "b", "c", ".", "/", "/", "*", "*", "**", "**/", "/**", "?", "[a-c]", "[!a]", "[^b]", "[]a]", "[a-]", "{a,b}", "{,a}", "{a,{b,c}}", "{a/b,c}", "{}", "@(a|b)", "!(a)", "+(a)", "*(b)", "?(c)", "@(a|b/c)", "\\*", "\\", "\\n", "\\a", "\\\\", "[", "]", "{", "}", "(", ")", "|", ",", "é", "!", "-", "x"]
const pth = ["a", "b", "c", ".", "/", "/", "ab", "é", "*", "(", ")", "!", "[", "]", "{", "}", ",", "|", "-", "\\", "x", ".a", "bb", "\n", "\t"]
const gen = (tokens: ReadonlyArray<string>, max: number) => { const n = Math.floor(rand() * max); let s = ""; for (let i = 0; i < n; i++) s += pick(tokens); return s }
const cases: Array<unknown> = []
const seen = new Set<string>()
while (cases.length < (flavour === "posix" ? 40000 : 15000)) {
  const p = gen(pth, 6), g = gen(pat, 5)
  const key = `${flavour}\0${p}\0${g}`
  if (seen.has(key)) continue
  seen.add(key)
  let r: unknown
  try { r = { ok: path[flavour].matchesGlob(p, g) } } catch (e: any) { r = { throw: `${e.name}: ${e.message}` } }
  cases.push([flavour, "matchesGlob", [p, g], r])
}
// Make the matches as common as the misses: keep every match and as many misses.
const hits = cases.filter((c: any) => c[3].ok === true)
const misses = cases.filter((c: any) => c[3].ok !== true).slice(0, Math.max(hits.length, 600))
const out = [...hits, ...misses]
await Bun.write(process.argv[2], "[\n" + out.map((c) => JSON.stringify(c)).join(",\n") + "\n]\n")
console.log(out.length, hits.length)
