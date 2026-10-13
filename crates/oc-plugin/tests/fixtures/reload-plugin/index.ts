// A provider transform whose result changes when the plugin calls `ctx.provider.reload()` (like
// opencode-models-discovery after a discovery round), driven through an RPC.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.reload",
  setup: async (ctx) => {
    let round = 0
    await ctx.provider.transform((providers) => {
      providers.update("reload-provider", (provider) => {
        provider.name = `Reload Provider ${round}`
        provider.activation = "enabled"
        provider.package = "@opencode/ai/providers/openai-compatible"
      })
      providers.models.update("reload-provider", `model-${round}`, (model) => {
        model.name = `Model ${round}`
      })
    })
    await ctx.rpc.register(
      {
        id: "fixture.reload",
        methods: {
          bump: {
            input: { type: "object", properties: {} },
            output: { type: "object", properties: { round: { type: "number" } }, required: ["round"] },
          },
        },
      },
      {
        bump: async () => {
          round++
          await ctx.provider.reload()
          return { round }
        },
      },
    )
  },
})
