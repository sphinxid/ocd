// Tools registered without an input schema (core/src/tool.ts `commit`, 2.2.7): a missing or null `input` takes `{}`,
// and an `input` that is not a schema is rejected.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.inputless",
  setup: async (ctx) => {
    await ctx.tool.transform((tools: any) => {
      tools.add({
        name: "ping",
        options: { codemode: false },
        description: "Ping",
        execute: async (input: unknown) => ({ content: `pong ${JSON.stringify(input)}` }),
      })
      tools.add({ name: "nil", options: { codemode: false }, description: "Nil", input: null, execute: async () => ({ content: "nil" }) })
      tools.add({ name: "bad", options: { codemode: false }, description: "Bad", input: "text", execute: async () => ({ content: "bad" }) })
    })
  },
})
