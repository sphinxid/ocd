// Regenerates assets/mcp/watch-mcp-event.json from an opencode checkout (with node_modules):
//   bun scripts/mcp-events-schemas.ts <opencode-checkout> assets/mcp/watch-mcp-event.json
// The `watch_mcp_event` tool's model-facing definition comes from core's own `definition` (tool/runtime.ts), so its
// JSON Schemas are exactly what the model receives. Its input and output are declared inline in
// core/src/tool/plugin/mcp-events.ts; they are repeated here verbatim.
const root = process.argv[2]
const out = process.argv[3]
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { McpClient } = await import(`${root}/packages/core/src/mcp/client.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const EventType = Schema.Struct({ server: Schema.String, ...McpClient.EventType.fields })
const input = Schema.Struct({
  server: Schema.optionalKey(
    Schema.String.annotate({ description: "MCP server name as configured. Omit to list every server." }),
  ),
  event: Schema.optionalKey(
    Schema.String.annotate({ description: "Event type to watch. Omit to list event types instead." }),
  ),
  arguments: Schema.optionalKey(
    Schema.Record(Schema.String, Schema.Unknown).annotate({
      description: "The event type's arguments, as its inputSchema describes",
    }),
  ),
  cursor: Schema.optionalKey(
    Schema.String.annotate({
      description: "Where to start, such as a cursor a tool result gave. Omit to start from now.",
    }),
  ),
  once: Schema.optionalKey(
    Schema.Boolean.annotate({
      description: "Stop after the first event (default). false wakes on every event until the session is deleted.",
    }),
  ),
})
const output = Schema.Struct({
  events: Schema.optional(Schema.Array(EventType)),
  watching: Schema.optional(Schema.String),
})
const definition = runtime.definition({
  name: "watch_mcp_event",
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
