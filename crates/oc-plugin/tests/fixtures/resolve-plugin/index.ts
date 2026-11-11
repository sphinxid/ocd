// session.experimental.model.resolve (2.2.7): serves primary requests on mock/m1 with mock/m2, and reports every
// resolution it saw (kind, agent, model) through a request header set by session.model.request.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.resolve",
  setup: async (ctx) => {
    const seen: string[] = []
    await ctx.session.hook("experimental.model.resolve", async (event: any) => {
      seen.push(`${event.kind}|${event.agent}|${event.model.providerID}/${event.model.id}`)
      if (event.kind === "primary" && event.model.id === "m1") event.model = { providerID: "mock", id: "m2" }
    })
    await ctx.session.hook("model.request", async (event: any) => {
      event.headers["x-fixture-resolved"] = `${event.model.id}|${seen.join(",")}`
    })
  },
})
