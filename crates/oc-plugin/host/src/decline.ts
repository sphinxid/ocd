// Declines crossing the sidecar boundary.
//
// Upstream's Permission.assert and question tool throw declines as defects (`Permission.DeclinedError`,
// `QuestionTool.CancelledError`) so tools cannot catch them and turn a "no" into model-visible output, and
// the session runner recovers them as declines, including a decline a tool rethrew and its wrapper attached to
// a `Tool.Error` (packages/core/src/session/model-request.ts, 143807327). Here a decline arrives from Rust as
// an error named after its tag, becomes one of these defects, and leaves again as a `declined` failure.

import { Effect } from "effect"

export class PermissionDeclined extends Error {
  readonly _tag = "Permission.DeclinedError"
  constructor() {
    super("The user declined this tool call")
    this.name = "Permission.DeclinedError"
  }
}

export class QuestionCancelled extends Error {
  readonly _tag = "QuestionTool.CancelledError"
  constructor() {
    super("The user dismissed this question")
    this.name = "QuestionTool.CancelledError"
  }
}

const tagOf = (value: unknown) => {
  if (typeof value !== "object" || value === null) return undefined
  const record = value as { _tag?: unknown; name?: unknown }
  return typeof record._tag === "string" ? record._tag : typeof record.name === "string" ? record.name : undefined
}

/** The decline an error carries: itself, or the cause a `Tool.Error` wrapper attached. */
export function declineOf(error: unknown): { kind: "permission" | "question"; message: string } | undefined {
  const cause = typeof error === "object" && error !== null ? (error as { error?: unknown }).error : undefined
  for (const value of [error, cause]) {
    const tag = tagOf(value)
    if (tag === "Permission.DeclinedError") return { kind: "permission", message: new PermissionDeclined().message }
    if (tag === "QuestionTool.CancelledError") return { kind: "question", message: new QuestionCancelled().message }
  }
  return undefined
}

/** A Rust error named after a decline becomes the defect upstream's code would die with. */
export function declineDefect(error: unknown): Error | undefined {
  const tag = tagOf(error)
  if (tag === "Permission.DeclinedError") return new PermissionDeclined()
  if (tag === "QuestionTool.CancelledError") return new QuestionCancelled()
  return undefined
}

/** Fails `effect` like upstream: declines die, everything else keeps its error. */
export const withDeclines = <A, E, R>(effect: Effect.Effect<A, E, R>) =>
  effect.pipe(
    Effect.catch((error) => {
      const defect = declineDefect(error)
      return defect ? Effect.die(defect) : Effect.fail(error)
    }),
  )
