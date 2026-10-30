// The plugin API additions of opencode 2.2: keyed session instructions, `experimental.generate`, typed event
// subscriptions and the `experimental.idle` session hook.
import { Plugin } from "@opencode/plugin"

const failure = (context: any, error: unknown) => {
  const value = error as { name?: string; message?: string; _tag?: string }
  return context.error("failed", String(value?.message ?? error), { name: String(value?._tag ?? value?.name ?? "") })
}

export default Plugin.define({
  id: "fixture.v22",
  setup: async (ctx) => {
    const controller = new AbortController()
    const record = (prefix: string, types?: string[]) =>
      void (async () => {
        for await (const event of ctx.event.subscribe({ signal: controller.signal, ...(types ? { types } : {}) } as any)) {
          await ctx.storage.set(`${prefix}:${event.type}`, true)
        }
      })()
    // Known and unknown types: only the known one is delivered.
    record("some", ["command.updated", "not.a.type"])
    // Only unknown types: never emits.
    record("none", ["not.a.type"])
    // No types: every server event.
    record("all")

    await ctx.session.hook("experimental.idle", async (event: any) => {
      await ctx.storage.set(`idle:${event.sessionID}`, event)
      if (event.sessionID === "ses_fail") throw new Error("idle hook failed")
    })

    await ctx.rpc.register(
      {
        id: "fixture.v22",
        methods: {
          instructions: {
            input: { type: "object", properties: { op: { type: "string" } }, required: ["op"] },
            output: {},
            errors: { failed: { type: "object", properties: { name: { type: "string" } } } },
          },
          generate: {
            input: { type: "object", properties: { kind: { type: "string" } }, required: ["kind"] },
            output: {},
            errors: { failed: { type: "object", properties: { name: { type: "string" } } } },
          },
        },
        events: {},
      },
      {
        instructions: async (input: any, context: any) => {
          const entry = ctx.session.instructions.entry
          try {
            if (input.op === "put") {
              await entry.put({ sessionID: input.sessionID, key: input.key, value: input.value })
              return { done: true }
            }
            if (input.op === "remove") {
              await entry.remove({ sessionID: input.sessionID, key: input.key })
              return { done: true }
            }
            return { entries: await entry.list({ sessionID: input.sessionID }) }
          } catch (error) {
            return failure(context, error)
          }
        },
        generate: async (input: any, context: any) => {
          const model = input.model ?? { providerID: "mock", id: "m1" }
          try {
            if (input.kind === "text") return await ctx.experimental.generate.text({ prompt: input.prompt, model })
            if (input.kind === "legacy") return await ctx.generate.text({ prompt: input.prompt, model })
            if (input.kind === "image") return await ctx.experimental.generate.image({ prompt: "a cat", model })
            return await ctx.experimental.generate.decisions({
              model,
              input: [{ type: "text", text: "the sky is blue" }],
              questions: [{ type: "predicate", name: "blue", instructions: "Is the sky blue?" }],
            })
          } catch (error) {
            return failure(context, error)
          }
        },
      },
    )
    return () => controller.abort()
  },
})
