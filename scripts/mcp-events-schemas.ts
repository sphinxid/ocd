// Regenerates assets/mcp/list-mcp-events.json from an opencode checkout (with node_modules):
//   bun scripts/mcp-events-schemas.ts <opencode-checkout> assets/mcp/list-mcp-events.json
// The `list_mcp_events` tool's model-facing definition comes from core's own `definition` (tool/runtime.ts), so its
// JSON Schemas are exactly what the model receives. Its input and output are declared inline in
// core/src/tool/plugin/mcp-events.ts; they are repeated here verbatim.
const root = process.argv[2]
const out = process.argv[3]
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const EventType = Schema.Struct({
  server: Schema.String,
  event: Schema.String,
  description: Schema.optional(Schema.String),
  args: Schema.optional(
    Schema.Record(Schema.String, Schema.Unknown).annotate({ description: "JSON Schema of the trigger's args" }),
  ),
  data: Schema.optional(
    Schema.Record(Schema.String, Schema.Unknown).annotate({
      description: "JSON Schema of the data each event carries",
    }),
  ),
})
const input = Schema.Struct({
  server: Schema.String.annotate({ description: "MCP server name as configured" }),
})
const output = Schema.Struct({ events: Schema.Array(EventType) })
const definition = runtime.definition({
  name: "list_mcp_events",
  description: "",
  input,
  output,
  options: { namespace: "opencode", codemode: true },
})
await Bun.write(
  out,
  JSON.stringify({ name: definition.name, inputSchema: definition.inputSchema, outputSchema: definition.outputSchema }, null, 1) +
    "\n",
)
