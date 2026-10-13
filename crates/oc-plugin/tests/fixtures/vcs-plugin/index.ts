// vcs and worktree transforms: a selected fake VCS provider and a fake worktree strategy.
import { Plugin } from "@opencode/plugin"
import { mkdir, rm } from "node:fs/promises"

export default Plugin.define({
  id: "fixture.vcs",
  setup: async (ctx) => {
    await ctx.vcs.transform((vcs) => {
      vcs.add({
        id: "fake",
        name: "Fake VCS",
        info: async (input) => ({
          branch: { current: `fake:${input.directory === input.worktree}:${input.store === undefined}`, default: "trunk" },
        }),
        base: async () => ({ name: "trunk", ref: "abc123", source: "default" }),
        branches: async (input) => ["trunk", `limit:${input.limit}`, ...(input.search ? [`search:${input.search}`] : [])],
        status: async () => [{ file: "a.txt", additions: 1, deletions: 0, status: "added" }],
        diff: async (input) => {
          if (input.mode === "committed") throw new Error("no commits in fake")
          return [
            {
              file: "a.txt",
              patch: `mode=${input.mode} base=${input.base} context=${input.context} max=${input.maxOutputBytes}`,
              additions: 1,
              deletions: 0,
              status: "added",
            },
          ]
        },
      })
      vcs.default.set("fake")
    })
    await ctx.worktree.transform((worktree) => {
      worktree.add({
        id: "fake",
        create: async (input) => {
          await mkdir(input.directory, { recursive: true })
          return { directory: input.directory }
        },
        remove: async (input) => {
          if (!input.force) throw new Error("force required by fake")
          await rm(input.directory, { recursive: true, force: true })
        },
        list: async (source) => [{ directory: source, type: "root" }],
      })
    })
  },
})
