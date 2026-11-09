// Regenerates assets/artifact/tools.json from an opencode checkout (with node_modules):
//   bun scripts/artifact-schemas.ts <opencode-checkout> assets/artifact/tools.json
// The artifact tools' model-facing definitions come from core's own `definition` (tool/runtime.ts), so
// their JSON Schemas are exactly what the model receives (core/src/tool/plugin/artifact.ts).
const root = process.argv[2]
const out = process.argv[3]
const tools = await import(`${root}/packages/core/src/tool/plugin/artifact.ts`)
const { Artifact } = await import(`${root}/packages/core/src/artifact.ts`)
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const DeleteOutput = Schema.Struct({ id: Artifact.ID, deleted: Schema.Boolean })
const definitions = [
  ["artifact_create", tools.CreateToolInput, Artifact.Info],
  ["artifact_read", tools.ReadToolInput, Artifact.Content],
  ["artifact_update", tools.UpdateToolInput, Artifact.Info],
  ["artifact_delete", tools.DeleteToolInput, DeleteOutput],
].map(([name, input, output]) => {
  const definition = runtime.definition({
    name,
    description: "",
    input,
    output,
    options: { namespace: "opencode", codemode: true },
  })
  return { name, inputSchema: definition.inputSchema, outputSchema: definition.outputSchema }
})
await Bun.write(out, JSON.stringify(definitions, null, 1) + "\n")
