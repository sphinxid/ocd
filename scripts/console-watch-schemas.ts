// Regenerates assets/console/watch-tools.json from an opencode checkout (with node_modules):
//   bun scripts/console-watch-schemas.ts <opencode-checkout> assets/console/watch-tools.json
// The `console_watch` tools' model-facing definitions come from core's own `definition` (tool/runtime.ts), so their
// JSON Schemas are exactly what the model receives. The input and output schemas are copied from
// core/src/plugin/provider/opencode-watches.ts, which does not export them.
const root = process.argv[2]
const out = process.argv[3]
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const MAX_MINUTES = 14 * 24 * 60
const Watch = Schema.Struct({
  id: Schema.String,
  sessionID: Schema.String,
  routingKey: Schema.String,
  mode: Schema.String,
  expiresAt: Schema.String,
  serverSlug: Schema.optional(Schema.String),
  eventType: Schema.optional(Schema.String),
  arguments: Schema.optional(Schema.Record(Schema.String, Schema.Unknown)),
})
const EventType = Schema.Struct({
  server: Schema.String,
  name: Schema.String,
  description: Schema.optional(Schema.String),
  inputSchema: Schema.Record(Schema.String, Schema.Unknown),
})
const EventTypeList = Schema.Struct({ types: Schema.Array(EventType) })
const WatchInput = Schema.Struct({
  target: Schema.optional(
    Schema.String.annotate({
      description:
        "A Slack message or thread link, a GitHub pull request, issue, commit or Actions run link, owner/repo#123, or owner/repo@<full commit SHA>",
    }),
  ),
  server: Schema.optional(
    Schema.String.annotate({ description: "Instead of target: the server of an event type `events` lists" }),
  ),
  event: Schema.optional(Schema.String.annotate({ description: "The event type's name, with server" })),
  arguments: Schema.optional(
    Schema.Record(Schema.String, Schema.Unknown).annotate({
      description: "The event type's arguments, as its input schema describes",
    }),
  ),
  minutes: Schema.optional(
    Schema.Int.check(Schema.isBetween({ minimum: 1, maximum: MAX_MINUTES })).annotate({
      description: "How long to watch from now, in minutes; the event type's own default otherwise",
    }),
  ),
})
const definitions = [
  ["watch", WatchInput, Watch, true],
  ["unwatch", Schema.Struct({ id: Schema.String }), Schema.Struct({ id: Schema.String, removed: Schema.Boolean }), false],
  ["watches", Schema.Struct({}), Schema.Struct({ watches: Schema.Array(Watch) }), false],
  ["events", Schema.Struct({}), EventTypeList, false],
].map(([name, input, output, pinned]) => {
  const definition = runtime.definition({
    name,
    description: "",
    input,
    output,
    options: { namespace: "console_watch", codemode: true, ...(pinned ? { pinned } : {}) },
  })
  return { name, inputSchema: definition.inputSchema, outputSchema: definition.outputSchema }
})
await Bun.write(out, JSON.stringify(definitions, null, 1) + "\n")
