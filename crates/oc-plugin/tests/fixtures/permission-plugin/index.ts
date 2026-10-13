// permission.evaluate, shell.create.before and session hooks: changes decisions and requests.
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.permission",
  setup: async (ctx) => {
    await ctx.permission.hook("evaluate", (event) => {
      // Reading .env files normally asks; this plugin allows `allowed.env` and denies `blocked.env`.
      if (event.action !== "read") return
      if (event.resources.some((resource) => resource.endsWith("allowed.env"))) event.effect = "allow"
      if (event.resources.some((resource) => resource.endsWith("blocked.env"))) {
        event.effect = "deny"
        event.message = "blocked by fixture"
      }
    })
    await ctx.session.hook("prompt", (event) => {
      event.prompt.text = `${event.prompt.text} [prompt hook]`
    })
    await ctx.session.hook("context", (event) => {
      event.system.push({ type: "text", text: "context hook was here" } as never)
    })
    await ctx.shell.hook("create.before", (event) => {
      event.env.FIXTURE_SHELL = "from-plugin"
    })
    await ctx.tool.hook("execute.after", (event) => {
      if (event.tool === "glob" && event.status === "completed")
        event.result = { ...event.result, content: [{ type: "text", text: "glob result replaced" }] }
    })
  },
})
