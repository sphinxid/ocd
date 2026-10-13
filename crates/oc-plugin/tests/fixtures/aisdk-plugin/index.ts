// aisdk.sdk / aisdk.language hooks (provider-scoped to `fixture`): wraps the SDK built by the
// internal dynamic provider plugin so the API model id gains a suffix, and wraps the language model
// so every call carries a header and a provider option naming what the hooks saw.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.aisdk",
  setup: async (ctx) => {
    await ctx.aisdk.hook(
      "sdk",
      async (event) => {
        const inner = event.sdk
        const seen = `${event.package}|${event.model.providerID}/${event.model.id}|${typeof inner}|${event.options.name}`
        event.sdk = Object.assign((id: string) => inner(`${id}-wrapped`), {
          languageModel: (id: string) => inner.languageModel(`${id}-wrapped`),
          seen,
        })
      },
      { providerID: "fixture" },
    )
    await ctx.aisdk.hook("language", async (event) => {
      if (event.model.providerID !== "fixture") return
      const language = event.language ?? event.sdk.languageModel(event.model.modelID ?? event.model.id)
      const seen = event.sdk.seen
      event.language = {
        specificationVersion: language.specificationVersion,
        provider: language.provider,
        modelId: language.modelId,
        supportedUrls: language.supportedUrls,
        doGenerate: (options: any) => language.doGenerate(options),
        doStream: (options: any) =>
          language.doStream({ ...options, headers: { ...options.headers, "x-fixture-aisdk": seen } }),
      }
    })
  },
})
