// A transform that throws: the plugin must be disabled with upstream's message.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.bad",
  setup: async (ctx) => {
    await ctx.command.transform((editor) => {
      editor.add({ name: "bad-first", execute: async () => {} })
    })
    await ctx.skill.transform(() => {
      throw new Error("boom")
    })
  },
})
