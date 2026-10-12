#!/usr/bin/env node
// Generates the expected results the acorn-port test (`tests/parser_acorn.rs`) compares against, by running the real
// acorn 8.15.0 with the options upstream `@opencode/codemode` uses.
//
//   node crates/oc-codemode/tests/fixtures/parser/gen.js [--acorn <path/to/acorn.js>] [--test262 <dir>]
//
// Defaults: acorn from $ACORN or the scratchpad bun install, test262 from $OC_TEST262_DIR or the scratchpad checkout.
// Inputs:  snippets.js (hand-written corpus, in this directory).
// Outputs: snippets.json  — [{src, expected}] with the full normalized ESTree (or the error), compared exactly.
//          test262.json   — {"<relative path>": "<fnv1a64 hex of canonical JSON>" | {error}} for every .js file.
//
// Normalization (mirrored by `to_estree` in src/parser/estree.rs and by `canonical` in tests/parser_acorn.rs):
// - every node: type, start, end, loc = {line, column} of loc.start, then its fields; `raw` is dropped;
// - ClassDeclaration, DebuggerStatement, WithStatement, ClassExpression, ImportExpression, MetaProperty keep only
//   type/start/end/loc;
// - number literal value -> {"f64": "<16 lowercase hex digits of the IEEE-754 bits>"};
// - BigInt literal -> `bigint` only (no value); regex literal -> `regex: {pattern, flags}` only (no value);
// - strings (literal values, template cooked/raw) -> JSON string, or an array of UTF-16 code units when the string
//   holds a lone surrogate;
// - Program: body, sourceType.
// The test262 hash is FNV-1a 64 over the UTF-8 bytes of `canonical(result)`: object keys sorted (all ASCII), no
// whitespace, integers in decimal, strings escaped only for `"`, `\` and code units < 0x20 (as \u00xx, lowercase).
// A parse result is {"ast": <program>} or {"error": {message, pos, line, column}}; other exceptions become
// {"exception": "<name>: <message>"}.
"use strict"
const fs = require("fs")
const path = require("path")

const SCRATCH = "$OCD_SCRATCH"
const args = process.argv.slice(2)
function arg(name) {
  const i = args.indexOf(name)
  return i >= 0 ? args[i + 1] : undefined
}
const acornPath =
  arg("--acorn") ||
  process.env.ACORN ||
  SCRATCH + "/build/opencode/node_modules/.bun/acorn@8.15.0/node_modules/acorn/dist/acorn.js"
const test262Dir =
  arg("--test262") || process.env.OC_TEST262_DIR || SCRATCH + "/src/opencode/packages/codemode/test/test262"
const acorn = require(acornPath)
if (acorn.version !== "8.15.0") throw new Error("expected acorn 8.15.0, got " + acorn.version)

const OPTIONS = {
  ecmaVersion: "latest",
  sourceType: "script",
  allowReturnOutsideFunction: true,
  allowAwaitOutsideFunction: true,
  locations: true,
}
const UNSUPPORTED = new Set([
  "ClassDeclaration",
  "DebuggerStatement",
  "WithStatement",
  "ClassExpression",
  "ImportExpression",
  "MetaProperty",
])

function hasLoneSurrogate(s) {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c >= 0xd800 && c <= 0xdbff) {
      const n = s.charCodeAt(i + 1)
      if (n >= 0xdc00 && n <= 0xdfff) i++
      else return true
    } else if (c >= 0xdc00 && c <= 0xdfff) return true
  }
  return false
}
function str(s) {
  if (!hasLoneSurrogate(s)) return s
  const out = []
  for (let i = 0; i < s.length; i++) out.push(s.charCodeAt(i))
  return out
}
function f64(v) {
  const b = Buffer.alloc(8)
  b.writeDoubleBE(v)
  return { f64: b.toString("hex") }
}

function norm(node) {
  if (node === null || node === undefined) return null
  if (Array.isArray(node)) return node.map(norm)
  if (typeof node !== "object" || typeof node.type !== "string") throw new Error("unexpected value " + node)
  const out = { type: node.type, start: node.start, end: node.end, loc: { line: node.loc.start.line, column: node.loc.start.column } }
  if (UNSUPPORTED.has(node.type)) return out
  for (const key of Object.keys(node)) {
    if (key === "type" || key === "start" || key === "end" || key === "loc" || key === "raw") continue
    const v = node[key]
    if (node.type === "Literal" && key === "value") {
      if (node.regex || node.bigint !== undefined) continue
      if (typeof v === "number") out.value = f64(v)
      else if (typeof v === "string") out.value = str(v)
      else out.value = v // null, boolean
      continue
    }
    if (node.type === "Literal" && key === "regex") {
      out.regex = { pattern: str(v.pattern), flags: str(v.flags) }
      continue
    }
    if (node.type === "TemplateElement" && key === "value") {
      out.value = { raw: str(v.raw), cooked: v.cooked === null || v.cooked === undefined ? null : str(v.cooked) }
      continue
    }
    if (v === null || v === undefined) out[key] = null
    else if (typeof v === "object") out[key] = norm(v)
    else if (typeof v === "string") out[key] = str(v)
    else out[key] = v
  }
  return out
}

function run(src) {
  let ast
  try {
    ast = acorn.parse(src, OPTIONS)
  } catch (e) {
    if (e instanceof SyntaxError && e.loc)
      return { error: { message: e.message, pos: e.pos, line: e.loc.line, column: e.loc.column } }
    return { exception: e.name + ": " + e.message }
  }
  return { ast: norm(ast) }
}

function canonical(v) {
  if (v === null) return "null"
  if (typeof v === "boolean") return v ? "true" : "false"
  if (typeof v === "number") {
    if (!Number.isInteger(v) || v < 0) throw new Error("non-integer number in canonical JSON: " + v)
    return String(v)
  }
  if (typeof v === "string") {
    let s = '"'
    for (let i = 0; i < v.length; i++) {
      const c = v.charCodeAt(i)
      if (c === 0x22) s += '\\"'
      else if (c === 0x5c) s += "\\\\"
      else if (c < 0x20) s += "\\u" + c.toString(16).padStart(4, "0")
      else s += v[i]
    }
    return s + '"'
  }
  if (Array.isArray(v)) return "[" + v.map(canonical).join(",") + "]"
  const keys = Object.keys(v).sort()
  return "{" + keys.map((k) => canonical(k) + ":" + canonical(v[k])).join(",") + "}"
}

function fnv1a64(str) {
  const bytes = Buffer.from(str, "utf8")
  let hi = 0xcbf29ce4 >>> 0
  let lo = 0x84222325 >>> 0
  for (let i = 0; i < bytes.length; i++) {
    lo = (lo ^ bytes[i]) >>> 0
    // (hi:lo) * 0x100000001b3 = (hi:lo) * 0x1b3 + ((hi:lo) << 40)
    const p = lo * 0x1b3
    const carry = Math.floor(p / 4294967296)
    const nlo = p >>> 0
    hi = (hi * 0x1b3 + carry + (lo << 8)) >>> 0
    lo = nlo
  }
  return hi.toString(16).padStart(8, "0") + lo.toString(16).padStart(8, "0")
}

const here = __dirname
let exceptions = 0

// Snippets.
const snippets = require(path.join(here, "snippets.js"))
const seen = new Set()
const snippetOut = []
let accepted = 0
for (const src of snippets) {
  if (seen.has(src)) continue
  seen.add(src)
  const expected = run(src)
  if (expected.exception) {
    exceptions++
    console.error("non-SyntaxError exception for snippet", JSON.stringify(src), expected.exception)
  }
  if (expected.ast) accepted++
  snippetOut.push({ src, expected })
}
fs.writeFileSync(path.join(here, "snippets.json"), JSON.stringify(snippetOut) + "\n")
console.log(`snippets: ${snippetOut.length} (${accepted} accepted, ${snippetOut.length - accepted} rejected)`)

// test262.
if (fs.existsSync(test262Dir)) {
  const files = []
  ;(function walk(dir) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name)
      if (e.isDirectory()) walk(p)
      else if (e.isFile() && e.name.endsWith(".js")) files.push(path.relative(test262Dir, p).split(path.sep).join("/"))
    }
  })(test262Dir)
  files.sort()
  const out = {}
  let ok = 0
  let err = 0
  for (const rel of files) {
    const src = fs.readFileSync(path.join(test262Dir, rel), "utf8")
    const r = run(src)
    if (r.ast) {
      out[rel] = fnv1a64(canonical(r))
      ok++
    } else if (r.error) {
      out[rel] = r.error
      err++
    } else {
      out[rel] = r
      exceptions++
      console.error("non-SyntaxError exception for", rel, r.exception)
    }
  }
  fs.writeFileSync(path.join(here, "test262.json"), JSON.stringify(out, null, 0).replace(/,"/g, ',\n"') + "\n")
  console.log(`test262: ${files.length} files (${ok} accepted, ${err} rejected)`)
} else {
  console.log("test262 directory not found, test262.json left unchanged:", test262Dir)
}
if (exceptions) console.log(`WARNING: ${exceptions} non-SyntaxError exceptions`)
