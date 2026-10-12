// Uses ctx.storage, ctx.event.subscribe and a plugin RPC with an event.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.storage",
  setup: async (ctx) => {
    const previous = (await ctx.storage.get("boots")) as { count: number } | undefined
    await ctx.storage.set("boots", { count: (previous?.count ?? 0) + 1 })
    await ctx.storage.set("prefix/a", 1)
    await ctx.storage.set("prefix/b", 2)
    const scanned = await ctx.storage.scan({ prefix: "prefix/" })
    await ctx.storage.set("scanned", scanned.entries.map((entry) => entry.key))

    const controller = new AbortController()
    void (async () => {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        await ctx.storage.set(`event:${event.type}`, event.data as never)
      }
    })()

    const registration = await ctx.rpc.register(
      {
        id: "fixture.echo",
        methods: {
          ping: {
            input: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
            output: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
            errors: { refused: { type: "object", properties: { why: { type: "string" } } } },
          },
        },
        events: { pong: { schema: { type: "object", properties: { text: { type: "string" } } } } },
      },
      {
        ping: async (input: any, context: any) => {
          if (input.text === "no") return context.error("refused", "Refusing to ping", { why: "said no" })
          await registration.events.emit("pong", { text: input.text })
          return { text: `pong: ${input.text}` }
        },
      },
    )
    return () => controller.abort()
  },
})
