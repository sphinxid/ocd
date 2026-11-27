// Collects the programs one of upstream's language tests runs (a template literal or string passed as the first
// argument of its helpers: value, error, runProgram, execute, ...), for tests/fixtures/language-2.4.1. The language
// tests left upstream's repository with the move to the psychoscript package (#65011); read them at its parent:
//
//   git -C <opencode> show eb2dfb2638^:packages/codemode/test/accessors.test.ts > accessors.test.ts
//   bun run extract-programs.ts <opencode>/packages/codemode accessors.test.ts tests/fixtures/language-2.4.1/accessors.json
//
// then record the results with cases-2.4.1.ts. Programs built with `${...}` interpolation are left out.
const [pkg, file, out] = process.argv.slice(2)
const acorn = await import(Bun.resolveSync("acorn", `${pkg}/src`))
const js = new Bun.Transpiler({ loader: "ts" }).transformSync(await Bun.file(file).text())
const ast = acorn.parse(js, { ecmaVersion: "latest", sourceType: "module", locations: true })
const helpers = new Set(["value", "error", "runProgram", "execute", "run", "result", "failure", "logs", "values", "fails", "program", "ok", "outcome", "expectValue", "expectError", "evaluate"])
const cases: Array<{ code: string; test: string }> = []
const stack: Array<string> = []
const walk = (node: any) => {
  if (!node || typeof node.type !== "string") return
  let pushed = false
  if (node.type === "CallExpression" && node.callee.type === "Identifier" && (node.callee.name === "test" || node.callee.name === "describe" || node.callee.name === "it") && node.arguments[0]?.type === "Literal") {
    stack.push(String(node.arguments[0].value)); pushed = true
  }
  if (node.type === "CallExpression") {
    const callee = node.callee
    const name = callee.type === "Identifier" ? callee.name : callee.type === "MemberExpression" && !callee.computed ? callee.property.name : undefined
    const arg = node.arguments[0]
    if (name && helpers.has(name) && arg) {
      let code: string | undefined
      if (arg.type === "TemplateLiteral" && arg.expressions.length === 0) code = arg.quasis[0].value.cooked
      else if (arg.type === "Literal" && typeof arg.value === "string") code = arg.value
      if (code !== undefined) cases.push({ code, test: stack.join(" > ") })
    }
  }
  for (const key of Object.keys(node)) {
    const child = node[key]
    if (Array.isArray(child)) child.forEach(walk)
    else if (child && typeof child.type === "string") walk(child)
  }
  if (pushed) stack.pop()
}
walk(ast)
const seen = new Set<string>()
const unique = cases.filter((c) => (seen.has(c.code) ? false : (seen.add(c.code), true)))
await Bun.write(out, JSON.stringify(unique.map((c) => ({ test: c.test, code: c.code, result: null })), null, 1) + "\n")
console.log(`${file}: ${unique.length} programs`)
