// Regenerates crates/oc-codemode/tests/fixtures/catalog-golden.json from upstream core 2.2.7:
//   bun gen-catalog-golden.ts <opencode-2.2.7 checkout> <old golden> <out>
// Inventories are taken from the old golden; summaries, renders and update texts come from
// packages/core/src/codemode/{catalog,instructions}.ts.
const [root, oldPath, out] = process.argv.slice(2)
const { CodeModeCatalog } = await import(`${root}/packages/core/src/codemode/catalog.ts`)
const { CodeModeInstructions } = await import(`${root}/packages/core/src/codemode/instructions.ts`)
const old = JSON.parse(await Bun.file(oldPath).text())

type Entry = any
const inventories: Record<string, Entry[]> = {}
for (const [key, value] of Object.entries<any>(old.summaries)) {
  const name = key.split("/")[0]
  inventories[name] ??= value.inventory
}

const clone = <T>(v: T): T => JSON.parse(JSON.stringify(v))
const tool = (name: string, description: string, pinned?: boolean) => ({
  type: "tool",
  name,
  description,
  signature: `tools.${name}({ q: string }): Promise<string>`,
  ...(pinned === undefined ? {} : { pinned }),
})

// Mutations of an inventory: each names what changed.
const firstNamespace = (inv: Entry[]) => inv.find((e) => e.type === "namespace")
const variants = (inv: Entry[]): Record<string, Entry[]> => {
  const result: Record<string, Entry[]> = {}
  const ns = firstNamespace(inv)
  if (ns) {
    const added = clone(inv)
    firstNamespace(added).tools.push(tool("zz_added", "A tool that was added"))
    result.added = added
    const many = clone(inv)
    for (let i = 0; i < 60; i++) firstNamespace(many).tools.push(tool(`bulk_${String(i).padStart(2, "0")}`, `Bulk tool number ${i} with a fairly long description that costs tokens`))
    result.many = many
    const removed = clone(inv)
    firstNamespace(removed).tools.pop()
    result.removed = removed
    const described = clone(inv)
    const leaf = firstNamespace(described).tools.find((e: Entry) => e.type === "tool")
    if (leaf) {
      leaf.description = "A changed description"
      result.described = described
    }
    const nsDescribed = clone(inv)
    firstNamespace(nsDescribed).description = "A changed namespace description"
    result.nsDescribed = nsDescribed
    result.nsRemoved = clone(inv).filter((e: Entry) => e !== undefined && e.name !== ns.name)
  }
  const newNs = clone(inv)
  newNs.push({ type: "namespace", name: "zz_new", description: "A new namespace", tools: [tool("one", "First"), tool("two", "Second")] })
  result.newNamespace = newNs
  const pinned = clone(inv)
  pinned.push({ type: "namespace", name: "aa_pinned", tools: [tool("pin", "Pinned tool", true)] })
  result.pinned = pinned
  result.same = clone(inv)
  return result
}

const summaries: Record<string, any> = {}
for (const [name, inventory] of Object.entries(inventories)) {
  for (const budget of [undefined, 0, 50, 300]) {
    const summary = CodeModeCatalog.summarize({ tools: inventory }, budget === undefined ? {} : { budget })
    summaries[`${name}/${budget}`] = { inventory, budget: budget ?? null, summary, render: CodeModeInstructions.render(summary) }
  }
}

// Updates name their inventories, which are stored once.
const all: Record<string, Entry[]> = { ...inventories }
const updates: any[] = []
const update = (previous: string, current: string, budget?: number) => {
  const options = budget === undefined ? {} : { budget }
  const before = CodeModeCatalog.summarize({ tools: all[previous] }, options)
  const after = CodeModeCatalog.summarize({ tools: all[current] }, options)
  const text = CodeModeInstructions.update(before, after, CodeModeCatalog.lookup({ tools: all[current] }))
  updates.push({ previous, current, budget: budget ?? null, text: text ?? null })
}
const names = Object.keys(inventories)
for (const a of names) for (const b of names) if (a !== b) update(a, b)
for (const name of names) {
  for (const [label, variant] of Object.entries(variants(inventories[name]))) {
    const key = `${name}~${label}`
    all[key] = variant
    for (const budget of [undefined, 50, 300]) {
      update(name, key, budget)
      update(key, name, budget)
    }
  }
}

await Bun.write(out, JSON.stringify({ summaries, inventories: all, updates }, null, 1) + "\n")
console.log(`summaries ${Object.keys(summaries).length}, updates ${updates.length}, null updates ${updates.filter((u) => u.text === null).length}`)
