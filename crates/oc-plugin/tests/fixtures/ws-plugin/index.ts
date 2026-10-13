// experimental.ws.handshake / experimental.ws.send / experimental.ws.receive around the session's
// Responses WebSocket, next to session.http.request (titles and the HTTP fallback). Every hook leaves a
// mark the mock provider records: a handshake header, a field in each sent frame, rewritten text in the
// received frames and a request header on HTTP requests.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.ws",
  setup: async (ctx) => {
    await ctx.session.hook("experimental.ws.handshake", async (event) => {
      const names = Object.keys(event.headers).sort().join(",")
      event.headers["x-fixture-ws"] = `${event.kind}|${event.agent}|${event.model.providerID}/${event.model.id}|${names}`
      event.url = event.url.replace("/v1/responses", "/v1/responses?fixture=1")
    })
    await ctx.session.hook("experimental.ws.send", async (event) => {
      const frame = JSON.parse(event.frame)
      frame.user = `fixture-${frame.previous_response_id === undefined ? "full" : "incremental"}`
      event.frame = JSON.stringify(frame)
    })
    await ctx.session.hook("experimental.ws.receive", async (event) => {
      event.frame = event.frame.replaceAll("ORIGINAL", "WSHOOKED")
    })
    await ctx.session.hook("http.request", async (event) => {
      const headers = new Headers(event.request.headers)
      headers.set("x-fixture-http", `${event.kind}|${new URL(event.request.url).pathname}`)
      event.request = new Request(event.request.url, {
        method: event.request.method,
        headers,
        body: await event.request.clone().text(),
      })
    })
  },
})
