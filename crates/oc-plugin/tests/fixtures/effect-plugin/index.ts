// Effect-style plugin: agent/skill transforms, a failing tool hook and a provider-scoped session hook.
import { Plugin } from "@opencode/plugin/effect"
import { Error as ToolError } from "@opencode/plugin/promise/tool"
import { Effect } from "effect"

export default Plugin.define({
  id: "fixture.effect",
  effect: Effect.fn(function* (ctx) {
    yield* ctx.agent.transform((agents) => {
      agents.update("build", (agent) => {
        agent.description = "Build things (from effect plugin)"
      })
      agents.update("reviewer", (agent) => {
        agent.mode = "subagent"
        agent.description = "Reviews code"
      })
      agents.remove("plan")
    })
    yield* ctx.skill.transform((skills) => {
      skills.add({
        id: "fixture-skill",
        name: "fixture-skill",
        description: "A skill added by a plugin",
        path: "/tmp/fixture-skill/SKILL.md",
        content: "Do the fixture thing.",
        source: { type: "embedded" },
      } as never)
    })
    yield* ctx.tool.hook("execute.before", (event) =>
      event.tool === "forbidden"
        ? Effect.fail(new ToolError({ message: "forbidden by fixture", metadata: { reason: "test" } }))
        : Effect.void,
    )
    yield* ctx.session.hook(
      "context",
      (event) =>
        Effect.sync(() => {
          event.system.push({ type: "text", text: "scoped to anthropic" } as never)
        }),
      { providerID: "anthropic" },
    )
    yield* ctx.session.hook("retry", (event) =>
      Effect.sync(() => {
        if (event.attempt >= 3) event.decision = { retry: false }
      }),
    )
  }),
})
