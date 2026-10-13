// session.http.request / session.http.response: rewrites model requests and responses and reports
// what the hooks saw through a request header and the response text.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.http",
  setup: async (ctx) => {
    await ctx.session.hook("http.request", async (event) => {
      const request = event.request
      const text = await request.clone().text()
      const body = JSON.parse(text)
      body.user = "fixture-user"
      const headers = new Headers(request.headers)
      const names = Array.from(request.headers.keys()).sort().join(",")
      headers.set(
        "x-fixture-seen",
        `${event.kind}|${event.agent}|${event.model.providerID}/${event.model.modelID}|${request.method}|${new URL(request.url).pathname}|${names}|${request.headers.get("content-length") === String(new TextEncoder().encode(text).length)}`,
      )
      event.request = new Request(request.url, { method: request.method, headers, body: JSON.stringify(body) })
    })
    await ctx.session.hook("http.response", async (event) => {
      if (event.kind !== "primary") return
      const response = event.response
      const text = await response.clone().text()
      const marker = `HOOKED ${response.status} ${response.statusText} ${response.headers.get("content-type")} ${event.request.headers.get("x-fixture-seen") !== null}`
      event.response = new Response(text.replaceAll("ORIGINAL", marker), {
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
      })
    })
  },
})
