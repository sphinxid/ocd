import { Cause, Effect, Formatter, Layer, Option } from "effect"
import { HttpClient, HttpClientRequest, HttpClientResponse } from "effect/http"
import { OpenAPI, Tool } from "./src/index.js"
import { inputTypeScript, outputTypeScript } from "./src/tool-schema.js"

const cases = JSON.parse(await Bun.file(process.argv[2]!).text())
const out: Array<unknown> = []
for (const c of cases) {
  const requests: Array<unknown> = []
  const layer = Layer.succeed(HttpClient.HttpClient)(
    HttpClient.make((request) =>
      Effect.sync(() => {
        const url = Option.getOrElse(Option.map(HttpClientRequest.toUrl(request), (u) => u.toString()), () => request.url)
        requests.push({
          method: request.method,
          url,
          headers: { ...request.headers },
          body: request.body._tag === "Uint8Array" ? new TextDecoder().decode(request.body.body) : null,
        })
        const r = c.response ?? {}
        return HttpClientResponse.fromWeb(request, new Response(r.body ?? null, { status: r.status ?? 200, headers: r.headers ?? {} }))
      }),
    ),
  )
  const contexts: Array<unknown> = []
  const resolve = (ctx: any) => {
    contexts.push({ name: ctx.name, definition: ctx.definition, scopes: ctx.scopes })
    const mode = typeof c.auth === "string" ? c.auth : c.auth[ctx.name]
    switch (mode) {
      case "bearer": return Effect.succeed({ type: "bearer", token: "tok" })
      case "basic": return Effect.succeed({ type: "basic", username: "Ada", password: "pässword" })
      case "header": return Effect.succeed({ type: "header", name: "X-Custom", value: "v" })
      case "apikey": return Effect.succeed({ type: "apiKey", value: "key" })
      case "fail": return Effect.fail(new Error("vault locked"))
      default: return Effect.succeed(undefined)
    }
  }
  let api: any
  try {
    api = OpenAPI.fromSpec({ spec: c.spec, baseUrl: c.baseUrl, headers: c.headers, auth: c.auth ? { resolve } : undefined } as any)
  } catch (e: any) {
    out.push({ throws: { name: e.name, message: e.message } })
    continue
  }
  if (!c.tool) {
    const tools: Array<unknown> = []
    const walk = (node: any, path: Array<string>) => {
      for (const [key, value] of Object.entries(node)) {
        if (Tool.isTool(value)) {
          const t: any = value
          tools.push({ path: [...path, key].join("."), description: t.description, input: t.input, output: t.output ?? null, inputTs: inputTypeScript(t), outputTs: outputTypeScript(t) })
        } else walk(value, [...path, key])
      }
    }
    walk(api.tools, [])
    out.push({ skipped: api.skipped, tools })
    continue
  }
  const tool = c.tool.split(".").reduce((t: any, s: string) => t?.[s], api.tools)
  if (!Tool.isTool(tool)) {
    out.push({ missing: true, skipped: api.skipped })
    continue
  }
  const exit = await Effect.runPromiseExit(tool.execute(c.input).pipe(Effect.provide(layer)) as any)
  const result: any = { requests, contexts, inputTs: inputTypeScript(tool), outputTs: outputTypeScript(tool) }
  if (exit._tag === "Success") result.value = exit.value === undefined ? { $undefined: true } : exit.value
  else
    result.error = Cause.prettyErrors(exit.cause)
      .map((error: any) => (error.cause ? Formatter.format(error) : error.message || error.name))
      .join("\n")
  out.push(result)
}
console.log(JSON.stringify(out, null, 1))
