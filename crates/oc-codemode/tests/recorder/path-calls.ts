// Regenerates tests/fixtures/path/path-calls.json: every function of Node's path module as Bun gives it (both
// flavours), called directly with generated arguments. Run with Bun 1.4.4 (the Bun opencode 2.3.1 ships):
//
//   bun run path-calls.ts crates/oc-codemode/tests/fixtures/path/path-calls.json
//
// The working directory is /usr, which `win32.toNamespacedPath` of a relative path reads.
import path from "path"
process.chdir("/usr")
let seed = 0x2311
const rand = () => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return seed / 4294967296 }
const pick = <T>(items: ReadonlyArray<T>): T => items[Math.floor(rand() * items.length)]!
const tokens = ["", ".", "..", "...", "a", "b.c", ".d", "e..", "f.", "g.h.i", "/", "\\", "//", "\\\\", "///", "C:", "c:", "z:", "1:", ":", "C:\\", "C:/",
  "\\\\server\\share", "//server/share", "\\\\?\\", "\\\\.\\", "?", "UNC", "srv", "shr", " ", "é", "日本", "CON", "con:", "COM1", "nul.txt", "x/", "y\\", "./", "../", ".\\", "..\\", "a b", "*", "~"]
const pathOf = () => { const n = Math.floor(rand() * 5); let s = ""; for (let i = 0; i < n; i++) s += pick(tokens); return s }
const cases: Array<unknown> = []
const call = (flavour: "posix" | "win32", fn: string, args: Array<unknown>) => {
  const m: any = path[flavour]
  let result: unknown
  try { result = { ok: m[fn](...args) } } catch (e: any) { result = { throw: `${e.name}: ${e.message}` } }
  cases.push([flavour, fn, args, result])
}
for (const flavour of ["posix", "win32"] as const) {
  const base = flavour === "posix" ? "/w/p" : "C:\\w\\p"
  for (let i = 0; i < 500; i++) call(flavour, "normalize", [pathOf()])
  for (let i = 0; i < 300; i++) call(flavour, "isAbsolute", [pathOf()])
  for (let i = 0; i < 400; i++) call(flavour, "join", Array.from({ length: Math.floor(rand() * 4) }, pathOf))
  for (let i = 0; i < 400; i++) call(flavour, "resolve", [pick([base, path[flavour].sep, "/", "\\", "D:\\x", "\\\\s\\h"]), ...Array.from({ length: Math.floor(rand() * 3) }, pathOf)])
  for (let i = 0; i < 400; i++) {
    const start = pick([base, path[flavour].sep])
    call(flavour, "relative", [path[flavour].resolve(start, pathOf()), path[flavour].resolve(start, pathOf())])
  }
  for (let i = 0; i < 100; i++) call(flavour, "relative", [pathOf(), pathOf()].map((p) => (flavour === "posix" ? "/" : "C:\\") + p))
  for (let i = 0; i < 300; i++) call(flavour, "dirname", [pathOf()])
  for (let i = 0; i < 300; i++) call(flavour, "basename", [pathOf()])
  for (let i = 0; i < 300; i++) call(flavour, "basename", [pathOf(), pick(["", ".c", "c", "b.c", ".", "..", "a", "/", "\\", "d", ".d", "h.i", pathOf()])])
  for (let i = 0; i < 300; i++) call(flavour, "extname", [pathOf()])
  for (let i = 0; i < 400; i++) call(flavour, "parse", [pathOf()])
  for (let i = 0; i < 150; i++) {
    const o: Record<string, unknown> = {}
    for (const k of ["root", "dir", "base", "name", "ext"]) if (rand() < 0.5) o[k] = pick(["", "/", "\\", "C:\\", "a", "b", ".c", "c", "x/y", pathOf()])
    call(flavour, "format", [o])
  }
  for (let i = 0; i < 200; i++) call(flavour, "toNamespacedPath", [pick([base, "", "a", "\\\\s\\h\\x", "//s/h", "\\\\?\\C:\\x", "C:", "C:x", "/x", pathOf()])])
}
await Bun.write(process.argv[2], "[\n" + cases.map((c) => JSON.stringify(c)).join(",\n") + "\n]\n")
console.log(cases.length)
