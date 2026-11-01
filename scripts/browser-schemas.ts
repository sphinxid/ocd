// Regenerates assets/browser/schemas.json from an opencode checkout (with node_modules):
//   bun scripts/browser-schemas.ts <opencode-checkout> assets/browser/schemas.json
// The browser tools' model-facing definitions come from core's own `definition` (tool/runtime.ts), so
// their JSON Schemas are exactly what the model receives; the RPC inputs are Effect's JSON Schema
// documents of `Browser.Definition` (plugin-browser/src/rpc.ts).
const root = process.argv[2]
const out = process.argv[3]
const { Browser } = await import(`${root}/packages/plugin-browser/src/rpc.ts`)
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const operations = Browser.Operations.map((operation: any) => {
  const separator = operation.name.lastIndexOf(".")
  const definition = runtime.definition({
    name: operation.name.slice(separator + 1),
    description: operation.description,
    input: operation.input,
    output: operation.output,
    options: {
      namespace: separator < 0 ? "browser" : `browser.${operation.name.slice(0, separator)}`,
      permission: "browser",
      codemode: true,
    },
  })
  return {
    name: operation.name,
    description: operation.description,
    inputSchema: definition.inputSchema,
    outputSchema: definition.outputSchema,
  }
})

const rpc: Record<string, unknown> = {}
for (const [name, method] of Object.entries(Browser.Definition.methods) as any) {
  const document = Schema.toJsonSchemaDocument(method.input)
  rpc[name] = Object.keys(document.definitions).length ? { ...document.schema, $defs: document.definitions } : document.schema
}
await Bun.write(out, JSON.stringify({ operations, rpc }, null, 1) + "\n")
