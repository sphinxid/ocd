// opencode 2.4.1's plugin API: Code Mode overrides on a tool (`options.codemode` with its own description and
// schemas), `info` and `codemode` on tool hooks, `experimental.automation`, and an OAuth method that revokes the
// credential on sign-out.
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

export default Plugin.define({
  id: "fixture.v241",
  setup: async (ctx) => {
    const seen: Array<{ tool: string; codemode: boolean; info: string | null; override: boolean }> = []
    await ctx.tool.hook("execute.before", async (event) => {
      seen.push({
        tool: event.tool,
        codemode: event.codemode,
        info: event.info ? event.info.name : null,
        override: typeof event.info?.options?.codemode === "object",
      })
      await ctx.storage.set("seen", seen)
    })
    await ctx.tool.transform((editor) => {
      editor.add({
        name: "shell_like",
        description: "Direct shell",
        input: Schema.Struct({ command: Schema.String, background: Schema.optionalKey(Schema.Boolean) }),
        output: Schema.String,
        options: {
          codemode: {
            description: "Code Mode shell",
            input: Schema.Struct({ command: Schema.String }),
            output: Schema.Struct({ summary: Schema.String }),
          },
        },
        execute: async (input, context) => {
          const background = "background" in input ? input.background : undefined
          const text = `${input.command}:${String(background ?? "none")}`
          return { output: context.codemode ? { summary: text } : text }
        },
      })
      editor.add({
        name: "plain_typed",
        description: "Unchanged plain tool",
        input: Schema.Struct({ count: Schema.Number }),
        execute: async (input) => ({ content: String(input.count) }),
      })
      editor.add({
        name: "bad_codemode",
        description: "Rejected registration",
        input: Schema.Struct({}),
        options: { codemode: "yes" } as never,
        execute: async () => ({ content: "never" }),
      })
    })
    await ctx.command.transform((editor) => {
      editor.add({
        name: "automate",
        description: "Create, list and delete an automation",
        execute: async (input) => {
          const created = await ctx.experimental.automation.create({
            directory: ctx.location.directory,
            name: "promise inbox",
            trigger: { type: "cron", cron: "0 9 * * *" },
            action: { type: "prompt", prompt: "Check the balance now.", to: { session: input.sessionID } },
          })
          const listed = await ctx.experimental.automation.list({ directory: ctx.location.directory })
          await ctx.experimental.automation.delete({ automationID: created.id })
          const again = await ctx.experimental.automation.delete({ automationID: created.id }).then(
            () => null,
            (error: unknown) => (error instanceof Error ? error.message : String(error)),
          )
          const after = await ctx.experimental.automation.list({ directory: ctx.location.directory })
          await ctx.storage.set("automation", {
            created,
            listed: listed.map((item) => item.id),
            again,
            after: after.map((item) => item.id),
          })
        },
      })
    })
    await ctx.integration.transform((editor) => {
      editor.update("fixture-oauth", (ref) => {
        ref.name = "Fixture OAuth"
      })
      editor.method.update({
        integrationID: "fixture-oauth",
        method: { id: "device", type: "oauth", label: "Fixture device" },
        authorize: async () => {
          throw new Error("unused authorize")
        },
        revoke: async (credential) => {
          const revoked = ((await ctx.storage.get("revoked")) as string[] | undefined) ?? []
          await ctx.storage.set("revoked", [...revoked, credential.refresh])
          if (credential.refresh === "broken") throw new Error("offline")
        },
      } as never)
    })
  },
})
