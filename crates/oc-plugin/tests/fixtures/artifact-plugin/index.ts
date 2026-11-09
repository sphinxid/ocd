// Promise-style artifact providers (`ctx.experimental.artifact.transform`): "pages" keeps files in memory and
// reports its own URL, "broken" fails every call, and the default moves to "pages" when the plugin's
// `default` option says so.
import { Plugin } from "@opencode/plugin"

const store = new Map<string, Record<string, Uint8Array>>()
const decoder = new TextDecoder()

export default Plugin.define({
  id: "fixture.artifact",
  setup: async (ctx) => {
    await ctx.experimental.artifact.transform((editor) => {
      editor.add({
        id: "pages",
        name: "Pages",
        capabilities: ["static", "server-code"],
        prompt: "Artifacts are published to the Pages fixture.",
        create: async (input) => {
          store.set(input.id, { ...input.files })
          const names = Object.keys(input.files).join(",")
          const sizes = Object.values(input.files).map((bytes) => decoder.decode(bytes).length).join(",")
          return { url: `https://pages.example/${input.id}/${input.entry}?files=${names}&sizes=${sizes}&session=${input.sessionID ?? "none"}` }
        },
        read: async (id) => {
          const files = store.get(id)
          if (!files) throw new Error(`no pages artifact ${id}`)
          return files
        },
        update: async (id, input) => {
          store.set(id, { ...input.files })
          return input.title === "keep url" ? {} : { url: `https://pages.example/${id}/v2/${input.entry}` }
        },
        delete: async (id) => {
          store.delete(id)
        },
      })
      editor.add({
        id: "broken",
        name: "Broken",
        capabilities: ["static"],
        prompt: "Always fails.",
        create: async () => {
          throw new Error("quota exceeded")
        },
        read: async () => {
          throw "not an error"
        },
        update: async () => {
          throw new Error("")
        },
        delete: async () => {
          throw new Error("cannot delete")
        },
      })
      if (ctx.options.default === "pages") editor.default.set("pages")
      if (ctx.options.default === "missing") editor.default.set("missing")
    })
  },
})
