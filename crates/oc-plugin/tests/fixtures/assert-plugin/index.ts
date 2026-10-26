// The 2.1 plugin API additions: `ctx.permission.assert`, `ctx.session.list` and `ctx.credential.activate`
// (upstream's permission-assert.test.ts and session-list.test.ts), and declines rethrown by Promise tools,
// including one that wraps a native tool (143807327).
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

export default Plugin.define({
  id: "fixture.assert",
  setup: async (ctx) => {
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "deploy",
        options: { codemode: false },
        description: "Deploy a target after asking",
        input: Schema.Struct({ target: Schema.String }),
        output: Schema.Struct({ target: Schema.String }),
        execute: async ({ target }, context) => {
          try {
            await ctx.permission.assert({ sessionID: context.sessionID, action: "deploy", resources: [target] })
          } catch (error) {
            // A plain rejection must be rethrown, not returned as output; it stays a decline.
            throw error
          }
          return { output: { target }, content: `deployed ${target}` }
        },
      })
      tools.add({
        name: "sessions",
        options: { codemode: false },
        description: "List sessions",
        input: Schema.Struct({}),
        output: Schema.Struct({}),
        execute: async () => {
          const first = await ctx.session.list({ limit: 1 })
          const second = first.cursor.next ? await ctx.session.list({ cursor: first.cursor.next, limit: 1 }) : undefined
          const invalid = await ctx.session.list({ cursor: "invalid" }).then(
            () => "resolved",
            () => "rejected",
          )
          const activate = await ctx.credential.activate({ credentialID: "crd_missing" }).then(
            () => "resolved",
            (error: unknown) => (error instanceof Error ? error.message : String(error)),
          )
          const ids = [...first.data, ...(second?.data ?? [])].map((session) => session.id)
          return { output: {}, content: JSON.stringify({ ids, invalid, activate }) }
        },
      })
      tools.add({
        name: "activate",
        options: { codemode: false },
        description: "Activate a saved credential",
        input: Schema.Struct({ credentialID: Schema.String }),
        output: Schema.Struct({}),
        execute: async ({ credentialID }) => {
          await ctx.credential.activate({ credentialID })
          return { output: {}, content: `activated ${credentialID}` }
        },
      })
      // Wrap a native tool and rethrow whatever it throws.
      tools.update("read", (tool) => {
        const original = tool.execute
        tool.execute = async (input, context) => {
          try {
            return await original(input, context)
          } catch (error) {
            throw error
          }
        }
      })
    })
  },
})
