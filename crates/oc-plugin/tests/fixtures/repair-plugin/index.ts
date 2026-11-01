// Tool input repair: a direct and a Code Mode plugin tool that report the input they were given and the input
// this plugin's own tool.execute.before hook saw (after the built-in opencode.tool.input.repair hook).
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

let seen: unknown = undefined

const input = Schema.Struct({ count: Schema.Int, flag: Schema.optional(Schema.Boolean) })

export default Plugin.define({
  id: "fixture.repair",
  setup: async (ctx) => {
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "tally",
        options: { codemode: false },
        description: "Report the input",
        input,
        execute: async (value) => ({ content: JSON.stringify({ seen, input: value }) }),
      })
      tools.add({
        name: "nested_tally",
        description: "Report the input from Code Mode",
        input,
        execute: async (value) => ({ content: JSON.stringify({ seen, input: value }) }),
      })
    })
    await ctx.tool.hook("execute.before", (event) => {
      if (event.tool === "tally" || event.tool === "nested_tally") seen = event.input
    })
  },
})
