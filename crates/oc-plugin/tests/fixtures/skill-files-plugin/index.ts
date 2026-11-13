// Adds an in-memory skill with extra files (`add(skill, files)`), as the Console plugin does with workspace skills.
// 2.2.7's plugin host hands third-party plugins an editor whose `add` takes only the skill, so the files are
// dropped: the skill loads with an empty file list and `skill://` reads of the files find nothing.
import { Plugin } from "@opencode/plugin/effect"
import { Effect } from "effect"

export default Plugin.define({
  id: "fixture.skill-files",
  effect: Effect.fn(function* (ctx) {
    yield* ctx.skill.transform((skills) => {
      ;(skills.add as (skill: unknown, files?: Record<string, string>) => void)(
        {
          id: "fixture-files",
          name: "Fixture Files",
          description: "A plugin skill with reference files",
          path: "skill://fixture-files/SKILL.md",
          content: "Read references/guide.md before answering.",
        },
        {
          "SKILL.md": "ignored",
          "references/guide.md": "one\ntwo\nthree\nfour\nfive\n",
          "scripts/run.sh": "echo run\n",
        },
      )
    })
  }),
})
