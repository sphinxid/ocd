// Promise tools that throw (upstream's promise-tool.test.ts "Promise tool throws become tool failures").
import { Plugin } from "@opencode/plugin"
import { Error as ToolError } from "@opencode/plugin/promise/tool"
import { Schema } from "effect"

export default Plugin.define({
  id: "fixture.throwing",
  setup: async (ctx) => {
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "plain",
        options: { codemode: false },
        description: "Throw an Error",
        input: Schema.Struct({}),
        output: Schema.Struct({}),
        execute: async () => {
          throw new Error("plain failure")
        },
      })
      tools.add({
        name: "typed",
        options: { codemode: false },
        description: "Throw a Tool.Error",
        input: Schema.Struct({}),
        output: Schema.Struct({}),
        execute: async () => {
          throw new ToolError({ message: "typed failure", metadata: { code: 7 } })
        },
      })
    })
  },
})
