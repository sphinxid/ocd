// Regenerates mini's scrollback goldens (crates/oc-tui/src/mini/testdata/scrollback.json) from upstream's own
// RunScrollbackStream. Copy it into an upstream checkout's packages/tui/test/ (bun install done) and run it there with
// `bun test`, isolated, with:
//   OPENCODE_CONFIG_DIR  a directory whose themes/ocdold.json is ocd's assets/themes/v2/opencode-classic.json ("resolved" cases
//                        use it, so the colors follow ocd's theme rather than upstream's current one)
//   OCD_GEN_TS           a writable directory for the tree-sitter client
//   OCD_GEN_IN           the case file (each case: name, width, wrote, mono, shellOutput, steps)
//   OCD_GEN_OUT          where to write the cases with their commits
//   OCD_GEN_ONLY         optional, comma-separated case names to regenerate (the others are copied as they are)
//   OCD_GEN_EACH=1       claim the renderer's output after every step (its queue keeps only the latest commits)
// Each commit is dumped as the Rust test reads it: rows, then per row the runs of `fg/bg/attributes` (`i<slot>` for an
// indexed color, `d` before a default one, `:<alpha>` when not opaque), `-` for cells never written.
import { test } from "bun:test"
import { CliRenderEvents, RGBA, TreeSitterClient } from "@opentui/core"
import { monoSnapshot } from "../src/mini/mono"
import { createTestRenderer } from "@opentui/core/testing"
import { readFileSync, writeFileSync } from "node:fs"
import { RunScrollbackStream } from "../src/mini/scrollback.surface"
import { resolveRunTheme } from "../src/mini/theme"

const hex = (n: number) => Math.round(n * 255).toString(16).padStart(2, "0")
function color(c: RGBA): string {
  const h = `#${hex(c.r)}${hex(c.g)}${hex(c.b)}`
  const a = Math.round(c.a * 255)
  if (c.intent === "indexed") return `i${c.slot}`
  const base = a === 255 ? h : `${h}:${a}`
  return c.intent === "default" ? `d${base}` : base
}

function dump(commit: any) {
  const snap = commit.snapshot
  const w = snap.width
  const h = snap.height
  const rows: string[] = []
  const styles: string[][] = []
  const raw = new TextDecoder().decode(snap.getRealCharBytes(true)).split("\n")
  snap.withBuffers((cells: any) => {
    const { char, fg, bg, attributes } = cells
    const n = fg.length / (w * h)
    for (let y = 0; y < h; y++) {
      const keys: string[] = []
      for (let x = 0; x < w; x++) {
        const i = y * w + x
        if (char[i] === 0) {
          keys.push("-")
          continue
        }
        const f = RGBA.fromArray(fg.slice(i * n, i * n + n))
        const b = RGBA.fromArray(bg.slice(i * n, i * n + n))
        keys.push(`${color(f)}/${color(b)}/${attributes[i] & 0xff}`)
      }
      const runs: string[] = []
      let start = 0
      for (let x = 1; x <= w; x++) {
        if (x === w || keys[x] !== keys[start]) {
          runs.push(`${start}-${x}:${keys[start]}`)
          start = x
        }
      }
      styles.push(runs)
    }
  })
  for (let y = 0; y < h; y++) rows.push((raw[y] ?? "").replace(/\s+$/, ""))
  return { width: w, height: h, rowColumns: w, startOnNewLine: commit.startOnNewLine, trailingNewline: commit.trailingNewline, rows, styles }
}

test("generate", async () => {
  const cases = JSON.parse(readFileSync(process.env.OCD_GEN_IN!, "utf8"))
  const only = (process.env.OCD_GEN_ONLY ?? "").split(",").filter(Boolean)
  const out: any[] = []
  const treeSitterClient = new TreeSitterClient({ dataPath: process.env.OCD_GEN_TS! })
  await treeSitterClient.initialize()
  for (const c of cases) {
    if (only.length && !only.includes(c.name)) {
      out.push(c)
      continue
    }
    const r = await createTestRenderer({ width: c.width, screenMode: "split-footer", footerHeight: 6, externalOutputMode: "capture-stdout", consoleMode: "disabled" })
    if (c.mono) r.renderer.on(CliRenderEvents.EXTERNAL_OUTPUT, monoSnapshot)
    const theme = await resolveRunTheme(r.renderer, { name: "ocdold", mode: "dark" } as any, c.mono)
    const s = new RunScrollbackStream(r.renderer, theme, { wrote: c.wrote, shellOutput: () => c.shellOutput, mono: c.mono, treeSitterClient })
    const claimed: any[] = []
    for (const step of c.steps) {
      if (process.env.OCD_GEN_EACH) claimed.push(...(Reflect.get(r.renderer, "externalOutputQueue") as any).claim())
      if (step.commit) await s.append(step.commit)
      if (step.complete) await s.complete(false)
      if (step.summary) await s.writeTurnSummary(step.summary)
    }
    const queue = Reflect.get(r.renderer, "externalOutputQueue") as any
    const commits = [...claimed, ...queue.claim()]
    out.push({ ...c, commits: commits.map(dump) })
    for (const commit of commits) commit.snapshot.destroy()
    s.destroy()
    r.renderer.destroy()
  }
  await treeSitterClient.destroy()
  writeFileSync(process.env.OCD_GEN_OUT!, JSON.stringify(out))
}, 600000)
