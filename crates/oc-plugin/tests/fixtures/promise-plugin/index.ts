// Promise-style plugin: a tool, a command and a tool.execute.before hook.
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

export default Plugin.define({
  id: "fixture.promise",
  setup: async (ctx) => {
    const suffix = typeof ctx.options.suffix === "string" ? ctx.options.suffix : ""
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "echo",
        options: { codemode: false },
        description: `Echo text${suffix}`,
        input: Schema.Struct({ text: Schema.String }),
        output: Schema.Struct({ text: Schema.String }),
        execute: async ({ text }, context) => {
          await context.progress({ stage: "echoing" })
          return { output: { text }, content: `echo: ${text}`, metadata: { session: context.sessionID } }
        },
      })
      // Wrap a native tool: the update replaces its executor with one that calls the original.
      tools.update("read", (tool) => {
        tool.description = `${tool.description} (wrapped)`
      })
    })
    await ctx.command.transform((editor) => {
      editor.add({
        name: "hello",
        description: "Say hello",
        execute: async (input) => {
          const agents = await ctx.agent.list()
          await ctx.storage.set("hello", { sessionID: input.sessionID, agents: agents.data.length })
        },
      })
    })
    await ctx.tool.hook("execute.before", (event) => {
      if (event.tool === "echo") event.input = { text: `[${(event.input as { text: string }).text}]` }
      if (event.tool === "legacy_echo") event.tool = "echo"
      // Simulates the plugin host dying while a hook runs.
      if (event.tool === "crash_host") process.exit(1)
    })
    return async () => {
      await ctx.storage.set("cleanup", true)
    }
  },
})
