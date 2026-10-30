// Writes search-signature.json into the recording directory given as the first argument: CodeMode.searchSignature
// and toolExpression for curated edge cases followed by every tool path in the recorded trees.
import fs from "node:fs"
import path from "node:path"
import { searchSignature, toolExpression } from "../src/tool-runtime.js"

const dir = process.argv[2]
if (dir === undefined) throw new Error("usage: bun run script/record-extras.ts <recording dir>")

const curated = [
  "a",
  "a.b",
  "a.b.c",
  "_private",
  "$dollar",
  "camelCase.snake_case",
  "kebab-case",
  "ns.kebab-case",
  "123",
  "1abc",
  "a.1",
  "with space",
  "ünïcödé",
  "日本",
  'quote"d',
  "back\\slash",
  "new.line\nbreak",
  "tab\there",
  "",
  "tools",
  "search",
  "constructor",
  "__proto__",
  "a.b-c.d_e",
  "emoji😀",
  " ",
]

type Node = { children: Array<[string, Node]>; tool?: unknown }

const seen = new Set<string>()
const walk = (node: Node, segments: Array<string>) => {
  for (const [segment, child] of node.children) {
    const next = [...segments, segment]
    if (child.tool !== undefined) seen.add(next.join("."))
    walk(child, next)
  }
}

const files = (root: string): Array<string> =>
  fs.readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(root, entry.name)
    if (entry.isDirectory()) return files(full)
    return entry.name.endsWith(".jsonl") ? [full] : []
  })

for (const file of files(dir)) {
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    if (line === "") continue
    const record = JSON.parse(line) as { tree?: Node }
    if (record.tree !== undefined) walk(record.tree, [])
  }
}

const paths = [...curated, ...[...seen].filter((item) => !curated.includes(item)).sort()]
fs.writeFileSync(
  path.join(dir, "search-signature.json"),
  `${JSON.stringify({ searchSignature, toolExpression: paths.map((item) => [item, toolExpression(item)]) }, null, 2)}\n`,
)
console.log(`search-signature.json: ${paths.length} toolExpression samples`)
