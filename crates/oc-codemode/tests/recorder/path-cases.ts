// Re-runs the programs of tests/fixtures/path-2.3.1.json through upstream's Code Mode with core's `path` global and
// rewrites their results. Run with the opencode checkout's dependencies installed:
//
//   bun run path-cases.ts <opencode> crates/oc-codemode/tests/fixtures/path-2.3.1.json
//
// Each case names the host flavour: "posix" runs core's own `codemode/path.ts` with the session directory
// /work/proj; "win32" runs the same `flavor` code as a Windows host would (`path` is `path.win32`, resolving from
// C:\work\proj, and `path.posix` from `/`). The working directory is /usr, which only `win32.toNamespacedPath` of a
// relative path reads.
import nodePath from "path"
process.chdir("/usr")
const [root, file] = process.argv.slice(2)
const { CodeMode, Extension } = await import(`${root}/packages/codemode/src/index.ts`)
const { CodeModePath } = await import(`${root}/packages/core/src/codemode/path.ts`)
const { Location } = await import(`${root}/packages/core/src/location.ts`)
const { Effect } = await import(Bun.resolveSync("effect", `${root}/packages/codemode/src`))

// core's `flavor`, with the host's module as a parameter.
const flavor = (module: nodePath.PlatformPath, host: nodePath.PlatformPath, directory: string) => {
  const { posix: _posix, win32: _win32, ...functions } = module
  const base: unknown = module === host ? Effect.succeed(directory) : Effect.succeed(module.sep)
  return {
    ...functions,
    resolve: (...parts: Array<string>) => Effect.map(base as any, (from: string) => module.resolve(from, ...parts)),
    relative: (from: string, to: string) =>
      Effect.map(base as any, (start: string) => module.relative(module.resolve(start, from), module.resolve(start, to))),
  }
}
const windows = Extension.make({
  name: "path",
  globals: {
    path: {
      ...flavor(nodePath.win32, nodePath.win32, "C:\\work\\proj"),
      posix: flavor(nodePath.posix, nodePath.win32, "C:\\work\\proj"),
      win32: flavor(nodePath.win32, nodePath.win32, "C:\\work\\proj"),
    },
  },
})
const runtimes = {
  posix: CodeMode.make({ extensions: [CodeModePath.extension], limits: { timeoutMs: 5000 } }),
  win32: CodeMode.make({ extensions: [windows], limits: { timeoutMs: 5000 } }),
}
const cases: Array<{ host: "posix" | "win32"; code: string; result: unknown }> = await Bun.file(file).json()
let changed = 0
for (const item of cases) {
  const run = runtimes[item.host].execute(item.code).pipe(Effect.provideService(Location.Service, { directory: "/work/proj" } as never))
  const result = JSON.parse(JSON.stringify(await Effect.runPromise(run)))
  if (JSON.stringify(result) !== JSON.stringify(item.result)) changed++
  item.result = result
}
await Bun.write(file, "[\n" + cases.map((c) => JSON.stringify(c)).join(",\n") + "\n]\n")
console.log(`${cases.length} cases, ${changed} results changed`)
