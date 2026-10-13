// provider/model/integration/websearch transforms and a Code Mode tool with a before hook.
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

export default Plugin.define({
  id: "fixture.catalog",
  setup: async (ctx) => {
    await ctx.provider.transform((providers) => {
      providers.update("fixture-provider", (provider) => {
        provider.name = "Fixture Provider"
        provider.activation = "enabled"
        provider.package = "@opencode/ai/providers/openai-compatible"
      })
      providers.models.update("fixture-provider", "fixture-model", (model) => {
        model.name = "Fixture Model"
      })
    })
    await ctx.model.transform((models) => {
      // Configured providers (`opencode.config.provider`) apply after plugins; edit a built-in one.
      models.update("opencode", "big-pickle", (model) => {
        model.name = "Big Pickle (renamed)"
      })
    })
    await ctx.integration.transform((integrations) => {
      integrations.update("fixture-integration", (ref) => {
        ref.name = "Fixture Integration"
      })
      integrations.method.update({ integrationID: "fixture-integration", method: { type: "key", label: "API key" } } as never)
    })
    await ctx.websearch.transform((search) => {
      search.add({
        id: "fixture-search",
        name: "Fixture Search",
        execute: async (input: { query: string }) => [
          { url: `https://example.com/${input.query}`, title: "Fixture result", time: {} },
        ],
      } as never)
    })
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "shout",
        description: "Shout text",
        input: Schema.Struct({ text: Schema.String }),
        execute: async ({ text }) => ({ content: text.toUpperCase() }),
      })
    })
    await ctx.tool.hook("execute.before", (event) => {
      if (event.tool === "shout") event.input = { text: `${(event.input as { text: string }).text}!` }
    })
  },
})
