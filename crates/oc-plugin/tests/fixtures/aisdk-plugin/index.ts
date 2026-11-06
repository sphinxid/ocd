// aisdk.sdk / aisdk.language hooks (provider-scoped to `fixture`): builds the SDK itself, since the internal
// dynamic provider plugin only runs after plugins' `sdk` hooks (2.2.5, #60380), with the API model id gaining a
// suffix, and wraps the language model so every call carries headers naming what the hooks saw.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.aisdk",
  setup: async (ctx) => {
    await ctx.aisdk.hook(
      "sdk",
      async (event) => {
        // `undefined`: the dynamic npm fallback has not run yet.
        const before = typeof event.sdk
        const mod: Record<string, any> = await import(event.package)
        const create = mod[Object.keys(mod).find((key) => key.startsWith("create"))!]
        const inner = create(event.options)
        const seen = `${event.package}|${event.model.providerID}/${event.model.id}|${typeof inner}|${event.options.name}`
        event.sdk = Object.assign((id: string) => inner(`${id}-wrapped`), {
          languageModel: (id: string) => inner.languageModel(`${id}-wrapped`),
          seen,
          before,
        })
      },
      { providerID: "fixture" },
    )
    await ctx.aisdk.hook("language", async (event) => {
      if (event.model.providerID !== "fixture") return
      const language = event.language ?? event.sdk.languageModel(event.model.modelID ?? event.model.id)
      const { seen, before } = event.sdk
      event.language = {
        specificationVersion: language.specificationVersion,
        provider: language.provider,
        modelId: language.modelId,
        supportedUrls: language.supportedUrls,
        doGenerate: (options: any) => language.doGenerate(options),
        doStream: (options: any) =>
          language.doStream({
            ...options,
            headers: { ...options.headers, "x-fixture-aisdk": seen, "x-fixture-before": before },
          }),
      }
    })
  },
})
