//! Parity tests for `Map`, `Set`, `WeakMap`, `WeakSet`, and `groupBy`: each program's result JSON must equal what upstream `@opencode/codemode`
//! printed for it (cases ported from upstream stdlib, callbacks, parity, enumeration, and test262-derived tests,
//! plus edge cases for holes, integrity levels, coercion, and live iteration).

#[path = "common/dump.rs"]
mod dump;

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;

fn normalize(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).expect("expected JSON");
    serde_json::to_string(&value).unwrap()
}

/// Runs every `(program, expected result JSON)` pair and reports all mismatches at once.
pub fn check(cases: &[(&str, &str)]) {
    let runtime = CodeMode::new(Vec::new(), Arc::new(NoTools), Vec::new(), ExecutionLimits::default()).unwrap();
    let mut failures = Vec::new();
    for (code, expected) in cases {
        dump::case("stdlib_collections", code, expected);
        let result = runtime.execute_blocking(code);
        let actual = serde_json::to_string(&result.to_json()).unwrap();
        if normalize(&actual) != normalize(expected) {
            failures.push(format!("program: {code}\n  expected: {expected}\n  actual:   {actual}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n\n{}", failures.len(), cases.len(), failures.join("\n\n"));
}

const CASES: &[(&str, &str)] = &[
    (
        r#"
        const headers = new Headers({ "Content-Type": "text/plain", "X-Count": 1, "X-Null": null })
        headers.append("Accept", "text/html")
        headers.append("accept", "application/json")
        headers.set("x-count", "2")
        headers.delete("x-null")
        const copy = new Headers(headers)
        copy.set("content-type", "text/html")
        return {
          get: headers.get("content-type"),
          missing: headers.get("x-missing"),
          combined: headers.get("ACCEPT"),
          has: [headers.has("Accept"), headers.has("x-null")],
          count: headers.get("x-count"),
          copied: [headers.get("content-type"), copy.get("content-type")],
          pairs: [...new Headers([["b", "2"], ["A", "1"]])],
          map: [...new Headers(new Map([["k", "v"]]))],
          keys: [...headers.keys()],
          values: [...headers.values()],
          entries: [...headers.entries()],
        }
      "#,
        r#"{"ok":true,"value":{"get":"text/plain","missing":null,"combined":"text/html, application/json","has":[true,false],"count":"2","copied":["text/plain","text/html"],"pairs":[["a","1"],["b","2"]],"map":[["k","v"]],"keys":["accept","content-type","x-count"],"values":["text/html, application/json","text/plain","2"],"entries":[["accept","text/html, application/json"],["content-type","text/plain"],["x-count","2"]]},"toolCalls":[]}"#,
    ),
    (
        r#"
        const headers = new Headers({ b: "2", a: "1" })
        headers.append("Set-Cookie", "x=1")
        headers.append("set-cookie", "y=2")
        const seen = []
        headers.forEach((value, name, self) => seen.push(name + "=" + value + ":" + (self === headers)))
        const [first] = headers
        function* pairs() { yield* headers }
        return {
          seen,
          first,
          spread: [...headers],
          from: Array.from(headers).length,
          generator: [...pairs()].length,
          object: Object.fromEntries(headers),
          cookies: headers.getSetCookie(),
        }
      "#,
        r#"{"ok":true,"value":{"seen":["a=1:true","b=2:true","set-cookie=x=1:true","set-cookie=y=2:true"],"first":["a","1"],"spread":[["a","1"],["b","2"],["set-cookie","x=1"],["set-cookie","y=2"]],"from":4,"generator":4,"object":{"a":"1","b":"2","set-cookie":"y=2"},"cookies":["x=1","y=2"]},"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map()
      m.set("a", 1).set("b", 2)
      return { a: m.get("a"), b: m.get("b"), has: m.has("a"), miss: m.get("zz") === undefined, size: m.size }
    "#,
        r#"{"ok":true,"value":{"a":1,"b":2,"has":true,"miss":true,"size":2},"toolCalls":[]}"#,
    ),
    (
        r#"
      const key = { id: 1 }
      const m = new Map()
      m.set(key, "hit")
      return [m.get(key), m.get({ id: 1 }) === undefined]
    "#,
        r#"{"ok":true,"value":["hit",true],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([["a", 1], ["b", 2]]); return m.get("b")"#,
        r#"{"ok":true,"value":2,"toolCalls":[]}"#,
    ),
    (
        r#"return new Map("nope")"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: new Map(...) expects [key, value] pairs as entry objects. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
    ),
    (
        r#"return new Map(["flat"])"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: new Map(...) expects [key, value] pairs as entry objects. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map([["a", 1], ["b", 2]])
      const keys = m.keys()
      const first = keys.next()
      m.set("c", 3)
      return { first, rest: [...keys], values: [...m.values()], entries: [...m.entries()], same: [...m[Symbol.iterator]()] }
    "#,
        r#"{"ok":true,"value":{"first":{"value":"a","done":false},"rest":["b","c"],"values":[1,2,3],"entries":[["a",1],["b",2],["c",3]],"same":[["a",1],["b",2],["c",3]]},"toolCalls":[]}"#,
    ),
    (
        r#"return Object.fromEntries(new Map([["a", 1], ["b", 2]]))"#,
        r#"{"ok":true,"value":{"a":1,"b":2},"toolCalls":[]}"#,
    ),
    (
        r#"return Array.from(new Map([["a", 1]]))"#,
        r#"{"ok":true,"value":[["a",1]],"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map([["a", 1], ["b", 2]])
      let total = 0
      let names = ""
      for (const [key, count] of m) { names += key; total += count }
      return names + total
    "#,
        r#"{"ok":true,"value":"ab3","toolCalls":[]}"#,
    ),
    (
        r#"return [...new Map([["a", 1]])]"#,
        r#"{"ok":true,"value":[["a",1]],"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map([["a", 1], ["b", 2]])
      const seen = []
      m.forEach((count, key) => seen.push(key + count))
      return seen
    "#,
        r#"{"ok":true,"value":["a1","b2"],"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map([["a", 1], ["b", 2]])
      const removed = m.delete("a")
      const missed = m.delete("zz")
      const sizeAfterDelete = m.size
      m.clear()
      return [removed, missed, sizeAfterDelete, m.size]
    "#,
        r#"{"ok":true,"value":[true,false,1,0],"toolCalls":[]}"#,
    ),
    (
        r#"
      const words = ["a", "b", "a", "c", "a"]
      const counts = new Map()
      for (const word of words) counts.set(word, (counts.get(word) ?? 0) + 1)
      return Object.fromEntries(counts)
    "#,
        r#"{"ok":true,"value":{"a":3,"b":1,"c":1},"toolCalls":[]}"#,
    ),
    (
        r#"
      const groups = new Map()
      groups.getOrInsert("a", []).push(1)
      groups.getOrInsert("a", []).push(2)
      let calls = 0
      const computed = (key) => { calls++; return key + "!" }
      const first = groups.getOrInsertComputed("b", computed)
      const second = groups.getOrInsertComputed("b", computed)
      const zero = groups.getOrInsertComputed(-0, (key) => 1 / key === Infinity)
      return [[...groups], first, second, calls, zero]
    "#,
        r#"{"ok":true,"value":[[["a",[1,2]],["b","b!"],[0,true]],"b!","b!",1,true],"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map()
      const outer = m.getOrInsertComputed("k", () => { m.set("k", "inner"); return "outer" })
      let thrown
      try { m.getOrInsertComputed("j", () => { throw new Error("boom") }) } catch (error) { thrown = error.message }
      return [outer, m.get("k"), thrown, m.has("j")]
    "#,
        r#"{"ok":true,"value":["outer","outer","boom",false],"toolCalls":[]}"#,
    ),
    (
        r#"new Map().getOrInsertComputed("k", 5)"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Map.getOrInsertComputed expects a function callback. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
    ),
    (
        r#"return new Map([["a", 1]])"#,
        r#"{"ok":true,"value":{},"toolCalls":[]}"#,
    ),
    (
        r#"return JSON.stringify(new Map([["a", 1]]))"#,
        r#"{"ok":true,"value":"{}","toolCalls":[]}"#,
    ),
    (
        r#"console.log(new Map([["a", 1]])); return null"#,
        r#"{"ok":true,"value":null,"logs":["Map(1) [[\"a\",1]]"],"toolCalls":[]}"#,
    ),
    (
        r#"
        const m = new Map([[1, "a"], [2, "b"]])
        const s = new Set([1, 2])
        const seen = []
        m.forEach((v, k) => { seen.push(k); if (k === 1) { m.delete(2); m.set(3, "c") } })
        s.forEach((v) => { seen.push(v); if (v === 1) { s.delete(2); s.add(3) } })
        return seen
      "#,
        r#"{"ok":true,"value":[1,3,1,3],"toolCalls":[]}"#,
    ),
    (
        r#"
      const s = new Set()
      s.add(1).add(2).add(1)
      const removed = s.delete(2)
      return [s.size, s.has(1), s.has(2), removed]
    "#,
        r#"{"ok":true,"value":[1,true,false,true],"toolCalls":[]}"#,
    ),
    (
        r#"return [...new Set([1, 2, 2, 3, 1])]"#,
        r#"{"ok":true,"value":[1,2,3],"toolCalls":[]}"#,
    ),
    (
        r#"return [...new Set("aba")]"#,
        r#"{"ok":true,"value":["a","b"],"toolCalls":[]}"#,
    ),
    (
        r#"return Array.from(new Set(new Set([1, 2])))"#,
        r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([NaN]); return s.has(NaN)"#,
        r#"{"ok":true,"value":true,"toolCalls":[]}"#,
    ),
    (
        r#"
      let total = 0
      for (const n of new Set([1, 2, 3])) total += n
      return total
    "#,
        r#"{"ok":true,"value":6,"toolCalls":[]}"#,
    ),
    (
        r#"return { s: new Set([1, "a", { n: 1 }, undefined]) }"#,
        r#"{"ok":true,"value":{"s":[1,"a",{"n":1},null]},"toolCalls":[]}"#,
    ),
    (
        r#"return JSON.stringify(new Set([1]))"#,
        r#"{"ok":true,"value":"{}","toolCalls":[]}"#,
    ),
    (
        r#"
      const a = new Uint8Array(2)
      const b = new Uint8Array([1, 2, 300])
      const c = Uint8Array.from(new Set([7, 8]))
      const d = new Uint8Array(b)
      d[0] = 9
      return [[...a], [...b], [...c], [...d], [...b], new Uint8Array("3").length, [...Uint8Array.of(1, "2")]]
    "#,
        r#"{"ok":true,"value":[[0,0],[1,2,44],[7,8],[9,2,44],[1,2,44],3,[1,2]],"toolCalls":[]}"#,
    ),
    (
        r#"
      const b = new Uint8Array([1, 2, 3])
      const [x, ...rest] = b
      const seen = []
      for (const byte of b) seen.push(byte)
      function* g() { yield* b }
      return [x, rest, seen, [...g()], Array.from(b, (n) => n * 2), new Set(b).size, Array.isArray(b)]
    "#,
        r#"{"ok":true,"value":[1,[2,3],[1,2,3],[1,2,3],[2,4,6],3,false],"toolCalls":[]}"#,
    ),
    (
        r#"
        const pulled = []
        function* naturals() { let n = 0; while (true) { pulled.push(n); yield n++ } }
        const squares = naturals().map((n) => n * n).filter((n) => n % 2 === 0).drop(1).take(3)
        const first = squares.next()
        return {
          first,
          rest: squares.toArray(),
          after: squares.next(),
          pulled,
          values: new Map([["a", 1], ["b", 2]]).values().map((v, i) => v * 10 + i).toArray(),
          flat: [1, 2].values().flatMap((n) => [n, [n]]).toArray(),
          entries: [...new Map([[1, 2]]).entries().map(([k, v]) => k + v)],
        }
      "#,
        r#"{"ok":true,"value":{"first":{"value":4,"done":false},"rest":[16,36],"after":{"done":true},"pulled":[0,1,2,3,4,5,6],"values":[10,21],"flat":[1,[1],2,[2]],"entries":[3]},"toolCalls":[]}"#,
    ),
    (
        r#"
        const s = new Set([1, 2])
        const u = new URLSearchParams("a=1&b=2")
        const h = new Headers({ b: "2", a: "1" })
        const bytes = new Uint8Array([7, 8])
        return [
          [...s.entries()], [...s[Symbol.iterator]()], s[Symbol.iterator] === s.values,
          [...u.keys()], [...u[Symbol.iterator]()], u[Symbol.iterator] === u.entries,
          [...h.values()], [...h[Symbol.iterator]()], h[Symbol.iterator] === h.entries,
          [...bytes.entries()], [...bytes[Symbol.iterator]()], bytes[Symbol.iterator] === bytes.values,
          [..."ab"[Symbol.iterator]()],
        ]
      "#,
        r#"{"ok":true,"value":[[[1,1],[2,2]],[1,2],true,["a","b"],[["a","1"],["b","2"]],true,["1","2"],[["a","1"],["b","2"]],true,[[0,7],[1,8]],[7,8],true,["a","b"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const m = new Map([["a", 1], ["b", 2]])
        return [
          Object.fromEntries(m.entries()), Array.from(m.keys(), (k) => k + "!"), new Set(m.values()).size,
          await Promise.all([Promise.resolve(1), 2].values()),
        ]
      "#,
        r#"{"ok":true,"value":[{"a":1,"b":2},["a!","b!"],2,[1,2]],"toolCalls":[]}"#,
    ),
    (
        r#"return new Set([1, 2]).union({ size: 1, has: () => false, keys: () => new Set([3]).keys() })"#,
        r#"{"ok":true,"value":[1,2,3],"toolCalls":[]}"#,
    ),
    (
        r#"
      return [
        new Map().toString(), new Set().toString(), new Headers().toString(), Promise.resolve(1).toString(),
        [1].values().toString(), ({}).toString(), String(new Map()), String(Promise.resolve(1)),
        `${new Set([1])}`, [new Map()] + "", new Map() == "[object Map]",
      ]
    "#,
        r#"{"ok":true,"value":["[object Map]","[object Set]","[object Headers]","[object Promise]","[object Iterator]","[object Object]","[object Map]","[object Promise]","[object Set]","[object Map]",true],"toolCalls":[]}"#,
    ),
    (
        r#"
        return [
          Object.fromEntries([["a", 1]]),
          Object.fromEntries(new Map([["b", 2]])),
          Object.fromEntries(new Set([["c", 3]])),
          Object.fromEntries(new URLSearchParams("d=4")),
          Object.fromEntries([{ 0: "e", 1: 5 }]),
          Object.fromEntries(new Set([[{}, 6], [new Date(0), 7], [null, 8], [undefined, 9]])),
        ]
      "#,
        r#"{"ok":true,"value":[{"a":1},{"b":2},{"c":3},{"d":"4"},{"e":5},{"[object Object]":6,"1970-01-01T00:00:00.000Z":7,"null":8,"undefined":9}],"toolCalls":[]}"#,
    ),
    (
        r#"try { Object.fromEntries(new Set([Math.max])); return false } catch { return true }"#,
        r#"{"ok":true,"value":true,"toolCalls":[]}"#,
    ),
    (
        r#"try { Object.fromEntries(new Map([["fn", Math.max]])); return false } catch { return true }"#,
        r#"{"ok":true,"value":true,"toolCalls":[]}"#,
    ),
    (
        r#"return !new Map()"#,
        r#"{"ok":true,"value":false,"toolCalls":[]}"#,
    ),
    (
        r#"return { ...new Map([["a", 1]]), kept: true }"#,
        r#"{"ok":true,"value":{"kept":true},"toolCalls":[]}"#,
    ),
    (
        r#"
      const m = new Map([["start", new Date(1000)]])
      return m.get("start").getTime()
    "#,
        r#"{"ok":true,"value":1000,"toolCalls":[]}"#,
    ),
    (
        r#"return [new Map() instanceof Set, "s" instanceof Date]"#,
        r#"{"ok":true,"value":[false,false],"toolCalls":[]}"#,
    ),
    (
        r#"
      const raw = '[{"at":"2024-01-01T05:00:00Z","tag":"a b"},{"at":"2024-01-01T09:00:00Z","tag":"b c"},{"at":"2024-01-02T01:00:00Z","tag":"a"}]'
      const rows = JSON.parse(raw)
      const tags = new Set()
      const byDay = new Map()
      for (const row of rows) {
        for (const m of row.tag.matchAll(/[a-z]+/g)) tags.add(m[0])
        const day = new Date(row.at).toISOString().slice(0, 10)
        byDay.set(day, (byDay.get(day) ?? 0) + 1)
      }
      return { tags: [...tags].sort((a, b) => (a < b ? -1 : 1)), byDay: Object.fromEntries(byDay) }
    "#,
        r#"{"ok":true,"value":{"tags":["a","b","c"],"byDay":{"2024-01-01":2,"2024-01-02":1}},"toolCalls":[]}"#,
    ),
    (
        r#"const merged = Object.assign({}, { m: new Map([["a", 1]]) }); return merged.m.get("a")"#,
        r#"{"ok":true,"value":1,"toolCalls":[]}"#,
    ),
    (
        r#"
      const src = { m: new Map([["a", 1]]) }
      const copy = { ...src }
      copy.m.set("b", 2)
      return [copy.m.get("a"), src.m.get("b")]
    "#,
        r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
    ),
    (
        r#"
      const child = { selected: false }
      const fromArrayLike = Array.from({ 0: child, length: 1 })
      const fromMap = Array.from(new Map([["child", child]]))
      const fromSet = Array.from(new Set([child]))
      fromArrayLike[0].selected = true
      return [fromMap[0][1] === child, fromSet[0] === child, child.selected]
    "#,
        r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
    ),
    (
        r#"return Object.keys(new Map([["a", 1]]))"#,
        r#"{"ok":true,"value":[],"toolCalls":[]}"#,
    ),
    (
        r#"return Object.entries(new Set([1]))"#,
        r#"{"ok":true,"value":[],"toolCalls":[]}"#,
    ),
    (
        r#"return Object.assign({}, new Map([["a", 1]]))"#,
        r#"{"ok":true,"value":{},"toolCalls":[]}"#,
    ),
    (
        r#"return { d: new Date(0), m: new Map([["a", 1]]) }"#,
        r#"{"ok":true,"value":{"d":"1970-01-01T00:00:00.000Z","m":{}},"toolCalls":[]}"#,
    ),
    (
        r#"return [1].map(Set)"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Constructor Set requires 'new'. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"#,
    ),
    (
        r#"return Array.from(new Set([1, 2]), (x) => x * 10)"#,
        r#"{"ok":true,"value":[10,20],"toolCalls":[]}"#,
    ),
    (
        r#"const o = []; new Map([["a", 1]]).forEach((v, k) => o.push(k), {}); return o"#,
        r#"{"ok":true,"value":["a"],"toolCalls":[]}"#,
    ),
    (
        r#"const o = []; new Set([1]).forEach((v) => o.push(v), "self"); return o"#,
        r#"{"ok":true,"value":[1],"toolCalls":[]}"#,
    ),
    (
        r#"return { ..."ab", ...5, ...true, ...(() => 1), ...new Map([[1, 2]]) }"#,
        r#"{"ok":true,"value":{"0":"a","1":"b"},"toolCalls":[]}"#,
    ),
    (
        r#"
        const [letter, ...letters] = "A😀B"
        const [[mapKey, mapValue]] = new Map([["key", 1]])
        const [setFirst, setSecond] = new Set([2, 3])
        const [[queryKey, queryValue]] = new URLSearchParams("q=test&page=2")
        return { letter, letters, mapKey, mapValue, setFirst, setSecond, queryKey, queryValue }
      "#,
        r#"{"ok":true,"value":{"letter":"A","letters":["😀","B"],"mapKey":"key","mapValue":1,"setFirst":2,"setSecond":3,"queryKey":"q","queryValue":"test"},"toolCalls":[]}"#,
    ),
    (
        r#"
        let first
        let rest
        ;[first, ...rest] = new Set([1, 2, 3])
        const read = ([[key, value]]) => key + value
        return { first, rest, entry: read(new Map([["a", 4]])) }
      "#,
        r#"{"ok":true,"value":{"first":1,"rest":[2,3],"entry":"a4"},"toolCalls":[]}"#,
    ),
    (
        r#"
        const empty = Object.freeze(new Uint8Array(0))
        const bytes = Object.preventExtensions(new Uint8Array([1]))
        const m = Object.freeze(new Map()); m.set(1, 2)
        const f = Object.freeze(() => 1)
        let named = "ok"
        try { f.x = 1 } catch (e) { named = e.name }
        return [Object.isFrozen(empty), Object.isFrozen(bytes), Object.isExtensible(bytes), m.size, Object.isFrozen(f), named, f()]
      "#,
        r#"{"ok":true,"value":[true,false,false,1,true,"TypeError",1],"toolCalls":[]}"#,
    ),
    (
        r#"
        const shared = { n: 1 }
        const source = { a: shared, b: shared, list: [1, , shared], map: new Map([[1, { a: [1, 2] }]]), set: new Set([shared]), when: new Date(5), bytes: new Uint8Array([1, 2]) }
        const c = structuredClone(source)
        c.a.n = 2
        return [
          c !== source, c.a === c.b, c.a !== shared, shared.n, c.list[2] === c.a, 1 in c.list, c.list.length,
          c.map.get(1).a, c.map !== source.map, c.set.has(c.a), c.when.getTime(), c.when instanceof Date,
          c.bytes instanceof Uint8Array, c.bytes[1], Array.isArray(c.list),
        ]
      "#,
        r#"{"ok":true,"value":[true,true,true,1,true,false,3,[1,2],true,true,5,true,true,2,true],"toolCalls":[]}"#,
    ),
    (
        r#"
        const c = { n: 0 }
        const count = function () { this.n++ }
        ;[1, 2].forEach(count, c)
        ;[1].map(count, c)
        ;[1].filter(count, c)
        ;[1].find(count, c)
        ;[1].findIndex(count, c)
        ;[1].findLast(count, c)
        ;[1].findLastIndex(count, c)
        ;[1].some(count, c)
        ;[1].every(count, c)
        ;[1].flatMap(count, c)
        Array.from([1], count, c)
        Array.from({ length: 1 }, count, c)
        new Map([[1, 1]]).forEach(count, c)
        new Set([1]).forEach(count, c)
        new URLSearchParams("a=1").forEach(count, c)
        new Headers({ a: "1" }).forEach(count, c)
        new Uint8Array([1]).forEach(count, c)
        return c.n
      "#,
        r#"{"ok":true,"value":18,"toolCalls":[]}"#,
    ),
    (
        r#"
        const k = {}
        const f = () => 1
        const wm = new WeakMap([[k, 1]])
        const ws = new WeakSet([k])
        return [
          wm.set(f, "fn") === wm, wm.get(k), wm.get(f), wm.has({}), wm.get(1), wm.has(1), wm.delete("s"),
          wm.getOrInsert(k, 9), wm.getOrInsertComputed({}, (key) => typeof key),
          ws.add(f) === ws, ws.has(k), ws.has(f), ws.has(1), ws.delete(k), ws.has(k),
          String(wm), wm.size, "clear" in wm, Symbol.iterator in ws, JSON.stringify(wm),
        ]
      "#,
        r#"{"ok":true,"value":[true,1,"fn",false,null,false,false,1,"object",true,true,true,false,true,false,"[object WeakMap]",null,false,false,"{}"],"toolCalls":[]}"#,
    ),
    (
        r#"new WeakMap().set(1, 1)"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Invalid value used as weak map key: a number cannot be held weakly. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
    ),
    (
        r#"new WeakSet([1])"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Invalid value used in weak set: a number cannot be held weakly. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
    ),
    (
        r#"WeakMap.prototype.get.call(new Map(), {})"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: WeakMap.prototype.get called on incompatible receiver a Map. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
    ),
    (
        r#"structuredClone(new WeakSet())"#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: DataCloneError: a WeakSet could not be cloned. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"#,
    ),
    (
        r#"
        const left = new Set([1, 2])
        const right = new Set([2, 3])
        const union = left.union(right)
        const intersection = left.intersection(right)
        const difference = left.difference(right)
        const symmetric = left.symmetricDifference(right)
        return [
          [...union], union instanceof Set,
          [...intersection], intersection instanceof Set,
          [...difference], difference instanceof Set,
          [...symmetric], symmetric instanceof Set,
          [...left], [...right],
          [...left.union(left)], left.union(left) !== left,
          [...left.intersection(left)], left.intersection(left) !== left,
          [...left.difference(left)], left.difference(left) !== left,
          [...left.symmetricDifference(left)], left.symmetricDifference(left) !== left,
          [...left.union(new Set())], [...new Set().union(left)],
        ]
      "#,
        r#"{"ok":true,"value":[[1,2,3],true,[2],true,[1],true,[1,3],true,[1,2],[2,3],[1,2],true,[1,2],true,[],true,[],true,[1,2],[1,2]],"toolCalls":[]}"#,
    ),
    (
        r#"
        return [
          [...new Set([1, 3, 5]).intersection(new Set([3, 2, 1]))],
          [...new Set([3, 2, 1, 0]).intersection(new Set([1, 3, 5]))],
          [...new Set([1, 2, 3, 4]).difference(new Set([6, 5, 3, 2]))],
          [...new Set([6, 5, 3, 2]).difference(new Set([1, 2, 3, 4]))],
          [...new Set([1, 2, 3, 4]).symmetricDifference(new Set([6, 5, 4, 3]))],
          [...new Set([2, 3]).union(new Set([1, 2]))],
          [...new Set([1, 2, -3]).union(new Set([-1, 0]))],
        ]
      "#,
        r#"{"ok":true,"value":[[1,3],[1,3],[1,4],[6,5],[1,2,6,5],[2,3,1],[1,2,-3,-1,0]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1, 2])
        const map = new Map([[2, "two"], [3, "three"]])
        return [
          [...set.union(map)],
          [...set.intersection(map)],
          [...set.difference(map)],
          [...set.symmetricDifference(map)],
        ]
      "#,
        r#"{"ok":true,"value":[[1,2,3],[2],[1],[1,3]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const keysOnly = {
          size: 1,
          has: () => { throw new Error("has should not be called") },
          keys: () => [2, 3],
        }
        const hasOnly = {
          size: 2,
          has: (value) => value === 2,
          keys: () => { throw new Error("keys should not be called") },
        }
        const set = new Set([1, 2])
        return [
          [...set.union(keysOnly)],
          [...set.intersection(hasOnly)],
          [...set.difference(hasOnly)],
          [...set.symmetricDifference(keysOnly)],
        ]
      "#,
        r#"{"ok":true,"value":[[1,2,3],[2],[1],[1,3]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const setlike = {
          size: 1,
          has: () => { throw new Error("has should not be called") },
          keys: () => [-0],
        }
        return [
          1 / [...new Set([1]).union(setlike)][1] === Infinity,
          1 / [...new Set([0, 1, 2]).intersection(setlike)][0] === Infinity,
          [...new Set([0, 1]).difference(setlike)],
          1 / [...new Set([1, 2]).symmetricDifference(setlike)][2] === Infinity,
        ]
      "#,
        r#"{"ok":true,"value":[true,true,[1],true],"toolCalls":[]}"#,
    ),
    (
        r#"
        const other = {
          size: 4,
          has: () => { throw new Error("has should not be called") },
          keys: () => [2, 2, 3, 3],
        }
        return [...new Set([1, 2]).symmetricDifference(other)]
      "#,
        r#"{"ok":true,"value":[1,3],"toolCalls":[]}"#,
    ),
    (
        r#"
        const unionBase = new Set(["a", "b", "c", "d", "e"])
        const unionOther = {
          size: 2,
          has: () => { throw new Error("has should not be called") },
          keys: () => {
            unionBase.delete("b")
            unionBase.delete("c")
            unionBase.add("b")
            return ["x", "y"]
          },
        }
        const union = unionBase.union(unionOther)

        const intersectionBase = new Set(["a", "b", "c", "d", "e"])
        const intersectionOther = {
          size: 3,
          has: () => { throw new Error("has should not be called") },
          keys: () => {
            intersectionBase.delete("b")
            intersectionBase.delete("c")
            intersectionBase.add("b")
            return ["x", "b", "b"]
          },
        }
        const intersection = intersectionBase.intersection(intersectionOther)

        const differenceBase = new Set(["a", "b", "c", "d", "e"])
        const differenceOther = {
          size: 3,
          has: () => { throw new Error("has should not be called") },
          keys: () => {
            differenceBase.delete("b")
            differenceBase.delete("c")
            differenceBase.add("b")
            return ["x", "b", "b"]
          },
        }
        const difference = differenceBase.difference(differenceOther)

        const symmetricBase = new Set(["a", "b", "c", "d", "e"])
        const symmetricOther = {
          size: 4,
          has: () => { throw new Error("has should not be called") },
          keys: () => {
            symmetricBase.delete("b")
            symmetricBase.delete("c")
            symmetricBase.add("b")
            return ["x", "b", "c", "c"]
          },
        }
        const symmetric = symmetricBase.symmetricDifference(symmetricOther)

        return [
          [...union], [...unionBase],
          [...intersection], [...intersectionBase],
          [...difference], [...differenceBase],
          [...symmetric], [...symmetricBase],
        ]
      "#,
        r#"{"ok":true,"value":[["a","b","c","d","e","x","y"],["a","d","e","b"],["b"],["a","d","e","b"],["a","c","d","e"],["a","d","e","b"],["a","c","d","e","x"],["a","d","e","b"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1, 2])
        return [
          set.isSubsetOf(new Set([2, 3])), set.isSubsetOf(new Set([1, 2, 3])),
          set.isSupersetOf(new Set([2, 3])), set.isSupersetOf(new Set([1])),
          set.isDisjointFrom(new Set([2, 3])), set.isDisjointFrom(new Set([3])),
          new Set().isSubsetOf(set), set.isSupersetOf(new Set()), new Set().isDisjointFrom(new Set()),
          set.isSubsetOf(set), set.isSupersetOf(set), set.isDisjointFrom(set),
          set.isSubsetOf(new Set([1, 2])), set.isSupersetOf(new Set([1, 2])),
        ]
      "#,
        r#"{"ok":true,"value":[false,true,false,true,false,true,true,true,true,true,true,false,true,true],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1, 2])
        const map = new Map([[1, "one"], [2, "two"], [3, "three"]])
        const hasOnly = {
          size: 2,
          has: (value) => value === 1 || value === 2,
          keys: () => { throw new Error("keys should not be called") },
        }
        const keysOnly = {
          size: 1,
          has: () => { throw new Error("has should not be called") },
          keys: () => [1],
        }
        return [
          set.isSubsetOf(map), set.isSupersetOf(map), set.isDisjointFrom(map),
          set.isSubsetOf(hasOnly), set.isSupersetOf(keysOnly), set.isDisjointFrom(hasOnly),
        ]
      "#,
        r#"{"ok":true,"value":[true,false,false,true,true,false],"toolCalls":[]}"#,
    ),
    (
        r#"
        const setlike = {
          size: 1,
          has: () => { throw new Error("has should not be called") },
          keys: () => [-0],
        }
        return [
          new Set([0, 1]).isSupersetOf(setlike),
          new Set([0, 1]).isDisjointFrom(setlike),
        ]
      "#,
        r#"{"ok":true,"value":[true,false],"toolCalls":[]}"#,
    ),
    (
        r#"
        const subset = new Set(["a", "b", "c"])
        const subsetOther = {
          size: 3,
          has: (item) => {
            if (item === "a") subset.delete("c")
            return ["x", "a", "b"].includes(item)
          },
          keys: () => { throw new Error("keys should not be called") },
        }
        const disjoint = new Set(["a", "b", "c"])
        const disjointOther = {
          size: 3,
          has: (item) => {
            if (item === "a") {
              disjoint.delete("b")
              disjoint.delete("c")
              disjoint.add("b")
            }
            if (item === "c") throw new Error("deleted value should not be visited")
            return false
          },
          keys: () => { throw new Error("keys should not be called") },
        }
        return [
          subset.isSubsetOf(subsetOther), [...subset],
          disjoint.isDisjointFrom(disjointOther), [...disjoint],
        ]
      "#,
        r#"{"ok":true,"value":[true,["a","b"],true,["a","b"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1])
        const names = []
        let coercionCalls = 0
        const invalid = [
          [],
          { size: undefined, has: () => false, keys: () => [] },
          { size: NaN, has: () => false, keys: () => [] },
          { size: { valueOf: () => { coercionCalls += 1; return NaN } }, has: () => false, keys: () => [] },
          { size: "string", has: () => false, keys: () => [] },
          { size: 0, has: undefined, keys: () => [] },
          { size: 0, has: {}, keys: () => [] },
          { size: 0, has: () => false, keys: undefined },
          { size: 0, has: () => false, keys: {} },
        ]
        for (const other of invalid) {
          try { set.union(other) } catch (error) { names.push(error.name) }
        }
        return [names, coercionCalls]
      "#,
        r#"{"ok":true,"value":[["TypeError","TypeError","TypeError","TypeError","TypeError","TypeError","TypeError","TypeError","TypeError"],1],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1])
        const invalid = { size: NaN, has: () => false, keys: () => [] }
        const names = []
        const operations = [
          () => set.union(invalid),
          () => set.intersection(invalid),
          () => set.difference(invalid),
          () => set.symmetricDifference(invalid),
          () => set.isSubsetOf(invalid),
          () => set.isSupersetOf(invalid),
          () => set.isDisjointFrom(invalid),
        ]
        for (const operation of operations) {
          try { operation() } catch (error) { names.push(error.name) }
        }
        return names
      "#,
        r#"{"ok":true,"value":["TypeError","TypeError","TypeError","TypeError","TypeError","TypeError","TypeError"],"toolCalls":[]}"#,
    ),
    (
        r#"
        const set = new Set([1])
        const names = []
        for (const keys of [() => "ab", () => new Set([2]), () => new Map([[2, true]])]) {
          try { set.union({ size: 1, has: () => false, keys }) } catch (error) { names.push(error.name) }
        }
        return names
      "#,
        r#"{"ok":true,"value":["TypeError","TypeError","TypeError"],"toolCalls":[]}"#,
    ),
    (
        r#"
        const values = [1, 2, 3]
        const grouped = Object.groupBy(values, (value) => value % 2 === 0 ? "even" : "odd")
        const stringable = { toString: () => 1 }
        const coerced = Object.groupBy([1, "1", stringable], (value) => value)
        const lengths = Object.groupBy(["hello", "test", "world"], (value) => value.length)
        return [
          Object.keys(grouped), grouped.even, grouped.odd, grouped.toString === undefined,
          Object.keys(coerced), coerced[1].length,
          Object.keys(lengths), lengths[5], lengths[4],
        ]
      "#,
        r#"{"ok":true,"value":[["odd","even"],[2],[1,3],true,["1"],3,["4","5"],["hello","world"],["test"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const values = [-0, 0, 1, 2, 3]
        const seen = []
        Object.groupBy(values, (value, index, ...extra) => {
          seen.push([value === values[index], index, extra.length])
          return null
        })
        return seen
      "#,
        r#"{"ok":true,"value":[[true,0,0],[true,1,0],[true,2,0],[true,3,0],[true,4,0]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const grouped = Object.groupBy("🥰💩🙏😈", (char) => char < "🙏" ? "before" : "after")
        const empty = Object.groupBy([], () => { throw new Error("not called") })
        return [Object.keys(grouped), grouped.before, grouped.after, Object.keys(empty)]
      "#,
        r#"{"ok":true,"value":[["after","before"],["💩","😈"],["🥰","🙏"],[]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const messages = []
        try { Object.groupBy({}, () => { throw new Error("not called") }) } catch (error) { messages.push(error.name) }
        for (const callback of [null, undefined, {}]) {
          try { Object.groupBy([], callback) } catch (error) { messages.push(error.name) }
        }
        try { Object.groupBy([1], () => { throw new Error("callback") }) } catch (error) { messages.push(error.message) }
        try {
          Object.groupBy([1], () => ({
            toString: () => { throw new Error("property key") },
          }))
        } catch (error) { messages.push(error.message) }
        return messages
      "#,
        r#"{"ok":true,"value":["TypeError","TypeError","TypeError","TypeError","callback","property key"],"toolCalls":[]}"#,
    ),
    (
        r#"
        const stringable = { toString: () => 1 }
        const grouped = Map.groupBy([1, "1", stringable], (value) => value)
        const parity = Map.groupBy([1, 2, 3], (value) => value % 2 === 0 ? "even" : "odd")
        const lengths = Map.groupBy(["hello", "test", "world"], (value) => value.length)
        return [
          grouped instanceof Map,
          grouped.size,
          grouped.get(1),
          grouped.get("1"),
          grouped.has(stringable),
          [...grouped.keys()].length,
          parity.get("even"),
          parity.get("odd"),
          [...lengths.keys()],
          lengths.get(5),
          lengths.get(4),
        ]
      "#,
        r#"{"ok":true,"value":[true,3,[1],["1"],true,3,[2],[1,3],[5,4],["hello","world"],["test"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const values = [-0, 0, 1, 2, 3]
        const seen = []
        const grouped = Map.groupBy(values, (value, index, ...extra) => {
          seen.push([value === values[index], index, extra.length])
          return value
        })
        return [grouped.size, grouped.get(0), seen]
      "#,
        r#"{"ok":true,"value":[4,[0,0],[[true,0,0],[true,1,0],[true,2,0],[true,3,0],[true,4,0]]],"toolCalls":[]}"#,
    ),
    (
        r#"
        const grouped = Map.groupBy("🥰💩🙏😈", (char) => char < "🙏" ? "before" : "after")
        const empty = Map.groupBy([], () => { throw new Error("not called") })
        return [[...grouped.keys()], grouped.get("before"), grouped.get("after"), empty.size]
      "#,
        r#"{"ok":true,"value":[["after","before"],["💩","😈"],["🥰","🙏"],0],"toolCalls":[]}"#,
    ),
    (
        r#"
        const results = []
        try { Map.groupBy({}, () => { throw new Error("not called") }) } catch (error) { results.push(error.name) }
        for (const callback of [null, undefined, {}]) {
          try { Map.groupBy([], callback) } catch (error) { results.push(error.name) }
        }
        try { Map.groupBy([1], () => { throw new Error("callback") }) }
        catch (error) { results.push(error.message) }
        return results
      "#,
        r#"{"ok":true,"value":["TypeError","TypeError","TypeError","TypeError","callback"],"toolCalls":[]}"#,
    ),
    (
        r#"const { a, size } = new Map(); return [a, size]"#,
        r#"{"ok":true,"value":[null,0],"toolCalls":[]}"#,
    ),
    (
        r#"
        const out = []
        for (const key in "ab") out.push(key)
        for (const key in 42) out.push(key)
        for (const key in null) out.push(key)
        for (const key in undefined) out.push(key)
        for (const key in new Map([[1, 2]])) out.push(key)
        for (const key in Math) out.push(key)
        return out
      "#,
        r#"{"ok":true,"value":["0","1"],"toolCalls":[]}"#,
    ),
    (
        r#"return [Map.prototype[Symbol.iterator] === Map.prototype.entries, Set.prototype[Symbol.iterator] === Set.prototype.values, Set.prototype.keys === Set.prototype.values]"#,
        r#"{"ok":true,"value":[true,true,false],"toolCalls":[]}"#,
    ),
    (
        r#"return [Map.length, Set.length, WeakMap.length, WeakSet.length, Iterator.length, Map.groupBy.length, Object.groupBy.length, Iterator.from.length]"#,
        r#"{"ok":true,"value":[0,0,0,0,0,2,2,1],"toolCalls":[]}"#,
    ),
    (
        r#"return [Array.from("a😀b"), Array.from(new Set([1, 1, 2])), Array.from(new Map([[1, 2]])), Array.from({ length: 3 }, (_, i) => i * i), Array.from({ length: "2", 0: "a", 1: "b" })]"#,
        r#"{"ok":true,"value":[["a","😀","b"],[1,2],[[1,2]],[0,1,4],["a","b"]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, "a"], [2, "b"]]); return [m.size, m.get(1), m.has(2), m.get(3), m.set(3, "c") === m, m.size, m.delete(1), m.delete(1), m.size, [...m]]"#,
        r#"{"ok":true,"value":[2,"a",true,null,true,3,true,false,2,[[2,"b"],[3,"c"]]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); m.set(-0, "z"); return [m.has(0), [...m.keys()][0] === 0, Object.is([...m.keys()][0], -0), m.get(+0)]"#,
        r#"{"ok":true,"value":[true,true,false,"z"],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[NaN, 1]]); return [m.get(NaN), m.has(NaN)]"#,
        r#"{"ok":true,"value":[1,true],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); const k = {}; m.set(k, 1); return [m.get(k), m.get({}), m.has(k)]"#,
        r#"{"ok":true,"value":[1,null,true],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([["a", 1], ["b", 2], ["c", 3]]); const seen = []; m.forEach((v, k, map) => { seen.push(k); if (k === "a") { map.delete("b"); map.set("d", 4) } }); return seen"#,
        r#"{"ok":true,"value":["a","c","d"],"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1, 2, 3]); const seen = []; s.forEach((v, k, set) => { seen.push([v, k]); if (v === 1) { set.delete(2); set.add(4) } }); return seen"#,
        r#"{"ok":true,"value":[[1,1],[3,3],[4,4]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, 2]]); m.forEach(function (v, k) { this.out = [v, k] }, this === undefined ? {} : {}); return "ok""#,
        r#"{"ok":true,"value":"ok","toolCalls":[]}"#,
    ),
    (
        r#"const o = { out: null }; new Map([[1, 2]]).forEach(function (v, k) { this.out = [v, k] }, o); return o.out"#,
        r#"{"ok":true,"value":[2,1],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, "a"]]); return [m.getOrInsert(1, "x"), m.getOrInsert(2, "y"), [...m]]"#,
        r#"{"ok":true,"value":["a","y",[[1,"a"],[2,"y"]]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); let calls = 0; const r1 = m.getOrInsertComputed("k", (key) => { calls++; return key + "!" }); const r2 = m.getOrInsertComputed("k", () => { calls++; return "no" }); return [r1, r2, calls, [...m]]"#,
        r#"{"ok":true,"value":["k!","k!",1,[["k","k!"]]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); return [m.getOrInsertComputed(-0, (k) => Object.is(k, -0)), [...m.keys()]]"#,
        r#"{"ok":true,"value":[false,[0]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); const r = m.getOrInsertComputed("k", () => { m.set("k", "inner"); return "outer" }); return [r, m.get("k")]"#,
        r#"{"ok":true,"value":["outer","outer"],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Map().getOrInsertComputed("k", 1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.getOrInsertComputed expects a function callback."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Map() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Constructor Map requires 'new'."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Set() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Constructor Set requires 'new'."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Map(5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new Map(...) expects an iterable of [key, value] pairs, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Map([1, 2]) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new Map(...) expects [key, value] pairs as entry objects."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Set(5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new Set(...) expects a synchronous iterable, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; const it = { [Symbol.iterator]() { return { next() { return { value: 1, done: false } }, return() { closed++; return {} } } } }; try { new Map(it) } catch (e) { return [e.message, closed] }"#,
        r#"{"ok":true,"value":["new Map(...) expects [key, value] pairs as entry objects.",1],"toolCalls":[]}"#,
    ),
    (
        r#"return [new Map(null).size, new Set(undefined).size, new Map([[1, 2], [1, 3]]).get(1), new Set("hello").size, [...new Set("aba")]]"#,
        r#"{"ok":true,"value":[0,0,3,4,["a","b"]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, 2]]); m.clear(); return [m.size, m.clear()]"#,
        r#"{"ok":true,"value":[0,null],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([["a", 1]]); const it = m.entries(); m.set("b", 2); return [...it]"#,
        r#"{"ok":true,"value":[["a",1],["b",2]],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([["a", 1], ["b", 2]]); const it = m.keys(); it.next(); m.delete("b"); m.set("c", 3); return [...it]"#,
        r#"{"ok":true,"value":["c"],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([["a", 1]]); const it = m.values(); m.clear(); m.set("z", 26); return [...it]"#,
        r#"{"ok":true,"value":[26],"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1]); return [[...s.entries()], [...s.keys()], [...s.values()], s.add(2) === s, s.size, s.has(2), s.delete(2), s.delete(2)]"#,
        r#"{"ok":true,"value":[[[1,1]],[1],[1],true,2,true,true,false],"toolCalls":[]}"#,
    ),
    (
        r#"try { Map.prototype.get.call({}, 1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.prototype.get called on incompatible receiver a data object."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Set.prototype.add.call(new Map(), 1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Set.prototype.add called on incompatible receiver a Map."],"toolCalls":[]}"#,
    ),
    (
        r#"try { return Map.prototype.size } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.prototype.size called on incompatible receiver a data object."],"toolCalls":[]}"#,
    ),
    (
        r#"try { return Object.getOwnPropertyDescriptor(Set.prototype, "size").get.call([]) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Object.getOwnPropertyDescriptor is not a function."],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2, 3]); const b = new Set([2, 3, 4]); return [[...a.union(b)], [...a.intersection(b)], [...a.difference(b)], [...a.symmetricDifference(b)], a.isSubsetOf(b), a.isSupersetOf(new Set([1])), a.isDisjointFrom(new Set([9]))]"#,
        r#"{"ok":true,"value":[[1,2,3,4],[2,3],[1],[1,4],false,true,true],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2]); const m = new Map([[2, "x"], [5, "y"]]); return [[...a.union(m)], [...a.intersection(m)], a.isDisjointFrom(m)]"#,
        r#"{"ok":true,"value":[[1,2,5],[2],false],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2, 3]); const like = { size: 2, has: (x) => x === 2, keys: () => [2, 7].values() }; return [[...a.union(like)], [...a.intersection(like)], [...a.difference(like)], [...a.symmetricDifference(like)], a.isSubsetOf(like), a.isSupersetOf(like), a.isDisjointFrom(like)]"#,
        r#"{"ok":true,"value":[[1,2,3,7],[2],[1,3],[1,3,7],false,false,false],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2, 3]); const like = { size: 1, has: (x) => x === 2, keys: () => [2, , 9] }; return [[...a.union(like)], [...a.intersection(like)], [...a.difference(like)]]"#,
        r#"{"ok":true,"value":[[1,2,3,null,9],[2],[1,3]],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1]); const out = []; for (const bad of [5, null, { size: NaN, has() {}, keys() {} }, { size: 1, has: 1, keys() {} }, { size: 1, has() {}, keys: null }, { size: "x", has() {}, keys() {} }, { has() {}, keys() {} }]) { try { a.union(bad); out.push("ok") } catch (e) { out.push(e.name + ": " + e.message) } } return out"#,
        r#"{"ok":true,"value":["TypeError: Set.union expects a Set-like object.","TypeError: Set.union expects a Set-like object.","TypeError: Set.union received a Set-like object with an invalid size.","TypeError: Set.union expects callable 'has' and 'keys' methods.","TypeError: Set.union expects callable 'has' and 'keys' methods.","TypeError: Set.union received a Set-like object with an invalid size.","TypeError: Set.union received a Set-like object with an invalid size."],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1]); return [...a.union({ size: { valueOf() { return 1 } }, has() { return false }, keys() { return [4][Symbol.iterator]() } })]"#,
        r#"{"ok":true,"value":[1,4],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1]); try { a.union({ size: 1, has() {}, keys() { return 5 } }) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","An iterator must be an object, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2, 3]); const calls = []; const like = { size: 10, has(x) { calls.push(x); return x !== 2 }, keys() { calls.push("keys"); return [].values() } }; return [[...a.intersection(like)], calls]"#,
        r#"{"ok":true,"value":[[1,3],[1,2,3]],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2, 3]); const calls = []; const like = { size: 1, has(x) { calls.push(x); return true }, keys() { calls.push("keys"); return [3].values() } }; return [[...a.intersection(like)], calls]"#,
        r#"{"ok":true,"value":[[3],["keys"]],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2]); const like = { size: -5, has: () => false, keys: () => [].values() }; return [[...a.difference(like)], a.isSupersetOf(like), a.isSubsetOf(like)]"#,
        r#"{"ok":true,"value":[[1,2],true,false],"toolCalls":[]}"#,
    ),
    (
        r#"const a = new Set([1, 2]); const like = { size: Infinity, has: () => true, keys: () => [].values() }; return [a.isSubsetOf(like), [...a.intersection(like)]]"#,
        r#"{"ok":true,"value":[true,[1,2]],"toolCalls":[]}"#,
    ),
    (
        r#"function* g() { yield 3; yield 1 } const a = new Set([1, 2]); return [...a.union({ size: 2, has: () => true, keys: g })]"#,
        r#"{"ok":true,"value":[1,2,3],"toolCalls":[]}"#,
    ),
    (
        r#"const wm = new WeakMap(); const k = {}; return [wm.set(k, 1) === wm, wm.get(k), wm.has(k), wm.has(1), wm.get("x"), wm.delete(1), wm.delete(k), wm.has(k)]"#,
        r#"{"ok":true,"value":[true,1,true,false,null,false,true,false],"toolCalls":[]}"#,
    ),
    (
        r#"const wm = new WeakMap(); try { wm.set(1, 1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used as weak map key: a number cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"const ws = new WeakSet(); try { ws.add("x") } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used in weak set: a string cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"const ws = new WeakSet(); try { ws.add(tools) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used in weak set: a tool reference cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"const k = {}; const ws = new WeakSet([k]); return [ws.has(k), ws.has({}), ws.add(k) === ws, ws.delete(k), ws.has(k), ws.has(null)]"#,
        r#"{"ok":true,"value":[true,false,true,true,false,false],"toolCalls":[]}"#,
    ),
    (
        r#"const k1 = {}; const k2 = []; const wm = new WeakMap([[k1, "a"], [k2, "b"]]); return [wm.get(k1), wm.get(k2)]"#,
        r#"{"ok":true,"value":["a","b"],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakMap([[1, 2]]) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used as weak map key: a number cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakMap([1]) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new WeakMap(...) expects [key, value] pairs as entry objects."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakMap(1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new WeakMap(...) expects an iterable of [key, value] pairs, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakSet([1]) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used in weak set: a number cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakSet(1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","new WeakSet(...) expects a synchronous iterable, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"try { WeakMap() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Constructor WeakMap requires 'new'."],"toolCalls":[]}"#,
    ),
    (
        r#"const wm = new WeakMap(); const k = {}; return [wm.getOrInsert(k, 1), wm.getOrInsert(k, 2), wm.getOrInsertComputed({}, (key) => typeof key), wm.getOrInsertComputed(k, () => 9)]"#,
        r#"{"ok":true,"value":[1,1,"object",1],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakMap().getOrInsertComputed(1, 5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Invalid value used as weak map key: a number cannot be held weakly."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new WeakMap().getOrInsertComputed({}, 5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","WeakMap.getOrInsertComputed expects a function callback."],"toolCalls":[]}"#,
    ),
    (
        r#"try { WeakMap.prototype.get.call(new Map(), {}) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","WeakMap.prototype.get called on incompatible receiver a Map."],"toolCalls":[]}"#,
    ),
    (
        r#"return [typeof new WeakMap().size, typeof new WeakSet().clear, Object.prototype.toString.call(new WeakMap())]"#,
        r#"{"ok":true,"value":["undefined","undefined","[object WeakMap]"],"toolCalls":[]}"#,
    ),
    (
        r#"return Map.groupBy([1, 2, 3, 4], (x) => (x % 2 ? "odd" : "even"))"#,
        r#"{"ok":true,"value":{},"toolCalls":[]}"#,
    ),
    (
        r#"const g = Map.groupBy([1, 2, 3], (x) => x % 2); return [g.get(1), g.get(0), [...g.keys()]]"#,
        r#"{"ok":true,"value":[[1,3],[2],[1,0]],"toolCalls":[]}"#,
    ),
    (
        r#"const k = {}; const g = Map.groupBy([1, 2], () => k); return g.get(k)"#,
        r#"{"ok":true,"value":[1,2],"toolCalls":[]}"#,
    ),
    (
        r#"const g = Object.groupBy([1, 2, 3, 4], (x, i) => (x % 2 ? "odd" : "even") + i % 2); return [g, Object.getPrototypeOf(g), Object.keys(g)]"#,
        r#"{"ok":true,"value":[{"odd0":[1,3],"even1":[2,4]},null,["odd0","even1"]],"toolCalls":[]}"#,
    ),
    (
        r#"return Object.groupBy("abca", (c) => c)"#,
        r#"{"ok":true,"value":{"a":["a","a"],"b":["b"],"c":["c"]},"toolCalls":[]}"#,
    ),
    (
        r#"return Object.groupBy([1, 2, 3], (x) => ({ toString() { return "k" + (x > 1) } }))"#,
        r#"{"ok":true,"value":{"kfalse":[1],"ktrue":[2,3]},"toolCalls":[]}"#,
    ),
    (
        r#"return Object.groupBy([1.5, -0, 0], (x) => x)"#,
        r#"{"ok":true,"value":{"0":[0,0],"1.5":[1.5]},"toolCalls":[]}"#,
    ),
    (
        r#"try { Object.groupBy([1], () => () => 1) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Object.groupBy callback must return a data value, received a function."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Object.groupBy(null, (x) => x) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Object.groupBy expects an iterable collection."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Map.groupBy(5, (x) => x) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.groupBy expects an iterable collection."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Map.groupBy([1], 5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.groupBy expects a function callback."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Map.groupBy(undefined, 5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Map.groupBy expects an iterable collection."],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; const it = { [Symbol.iterator]() { return { next() { return { value: 1, done: false } }, return() { closed++; return {} } } } }; try { Object.groupBy(it, () => { throw new Error("x") }) } catch (e) { return [e.message, closed] }"#,
        r#"{"ok":true,"value":["x",1],"toolCalls":[]}"#,
    ),
    (
        r#"function* g() { yield 1; yield 2; yield 3 } return Object.groupBy(g(), (x) => (x > 1 ? "big" : "small"))"#,
        r#"{"ok":true,"value":{"small":[1],"big":[2,3]},"toolCalls":[]}"#,
    ),
    (
        r#"return Object.groupBy([1, 2], (x) => Promise.resolve(x))"#,
        r#"{"ok":true,"value":{"[object Promise]":[1,2]},"toolCalls":[]}"#,
    ),
    (
        r#"return [[1, 2, 3].values().map((x, i) => [x, i]).toArray(), [1, 2].values().flatMap((x) => [x, x]).toArray(), [1, 2].values().flatMap((x) => new Set([x * 10])).toArray()]"#,
        r#"{"ok":true,"value":[[[1,0],[2,1],[3,2]],[1,1,2,2],[10,20]],"toolCalls":[]}"#,
    ),
    (
        r#"const it = [1].values(); function* g() {} const gen = g(); return [Iterator.from(it) === it, Iterator.from(gen) === gen, Iterator.from("ab").toArray(), Iterator.from([1, 2]).toArray(), Iterator.from(new Set([3])).toArray()]"#,
        r#"{"ok":true,"value":[true,true,["a","b"],[1,2],[3]],"toolCalls":[]}"#,
    ),
    (
        r#"return [new Map([[1, 2]]), new Set([1, "a"]), JSON.stringify(new Map([[1, 2]])), String(new Map()), String(new Set())]"#,
        r#"{"ok":true,"value":[{},[1,"a"],"{}","[object Map]","[object Set]"],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, { a: 1 }]]); return [...m.entries()]"#,
        r#"{"ok":true,"value":[[1,{"a":1}]],"toolCalls":[]}"#,
    ),
    (
        r#"return [...new Map([["a", 1]]).entries().map(([k, v]) => k + v)]"#,
        r#"{"ok":true,"value":["a1"],"toolCalls":[]}"#,
    ),
    (
        r#"console.log(new Map([[1, [2]]]), new Set([new Set([1])]), [1, , 3], new WeakMap()); return 1"#,
        r#"{"ok":true,"value":1,"logs":["Map(1) [[1,[2]]] Set(1) [Set(1) [1]] [1,,3] WeakMap { <items unknown> }"],"toolCalls":[]}"#,
    ),
    (
        r#"return Iterator.prototype.constructor === Iterator && Map.prototype.constructor === Map && Array.prototype.constructor === Array"#,
        r#"{"ok":true,"value":true,"toolCalls":[]}"#,
    ),
    (
        r#"class MyMap extends Map {} const m = new MyMap([[1, 2]]); return [m.get(1), m instanceof Map, m instanceof MyMap]"#,
        r#"{"ok":false,"error":{"kind":"UnsupportedSyntax","message":"SyntaxError: Syntax 'ClassDeclaration' is not supported. This is a restricted JavaScript-like language. Supported: plain and async functions, data literals, destructuring, standard control flow, await and Promise, and built-ins such as Array, Object, Math, JSON, Date, RegExp, Map, Set, and URL. Unsupported: classes, getters/setters, BigInt, and custom Symbols. Use plain functions and data objects instead. (line 1, col 1)","location":{"line":1,"column":1},"suggestions":["This is a restricted JavaScript-like language. Supported: plain and async functions, data literals, destructuring, standard control flow, await and Promise, and built-ins such as Array, Object, Math, JSON, Date, RegExp, Map, Set, and URL. Unsupported: classes, getters/setters, BigInt, and custom Symbols. Use plain functions and data objects instead."]},"toolCalls":[]}"#,
    ),
    (
        r#"class MySet extends Set {} return [new MySet([1]).has(1), new MySet().union(new Set([1])) instanceof MySet]"#,
        r#"{"ok":false,"error":{"kind":"UnsupportedSyntax","message":"SyntaxError: Syntax 'ClassDeclaration' is not supported. This is a restricted JavaScript-like language. Supported: plain and async functions, data literals, destructuring, standard control flow, await and Promise, and built-ins such as Array, Object, Math, JSON, Date, RegExp, Map, Set, and URL. Unsupported: classes, getters/setters, BigInt, and custom Symbols. Use plain functions and data objects instead. (line 1, col 1)","location":{"line":1,"column":1},"suggestions":["This is a restricted JavaScript-like language. Supported: plain and async functions, data literals, destructuring, standard control flow, await and Promise, and built-ins such as Array, Object, Math, JSON, Date, RegExp, Map, Set, and URL. Unsupported: classes, getters/setters, BigInt, and custom Symbols. Use plain functions and data objects instead."]},"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1, 2, 3]); s.forEach((v) => { if (v < 10) s.add(v + 10) }); return [...s]"#,
        r#"{"ok":true,"value":[1,2,3,11,12,13],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, 1]]); let n = 0; m.forEach((v, k) => { n++; m.delete(k); if (n < 5) m.set(k, v) }); return n"#,
        r#"{"ok":true,"value":5,"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1, 2, 3]); const it = s.values(); s.delete(1); s.add(1); return [...it]"#,
        r#"{"ok":true,"value":[2,3,1],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map(); m.set("a", 1).set("b", 2); m.delete("a"); m.set("a", 3); return [...m.keys()]"#,
        r#"{"ok":true,"value":["b","a"],"toolCalls":[]}"#,
    ),
    (
        r#"const k = {}; const out = []; const s = new Set([k, k, {}]); return [s.size, s.has(k)]"#,
        r#"{"ok":true,"value":[2,true],"toolCalls":[]}"#,
    ),
    (
        r#"return [new Set([0, -0, NaN, NaN]).size, [...new Set([-0])].map((x) => Object.is(x, -0))]"#,
        r#"{"ok":true,"value":[2,[false]],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Set([1]).union() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Set.union expects a Set-like object."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Set.prototype.union.call([], new Set()) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Set.prototype.union called on incompatible receiver an array."],"toolCalls":[]}"#,
    ),
    (
        r#"const like = { size: 3, has: (x) => true, keys() { return { next() { return { done: true } } } } }; return new Set([1, 2]).isSupersetOf(like)"#,
        r#"{"ok":true,"value":false,"toolCalls":[]}"#,
    ),
    (
        r#"return [new Set([1, 2]).isSubsetOf(new Set([1, 2, 3])), new Set([1, 4]).isSubsetOf(new Set([1, 2, 3])), new Set([1, 2, 3]).isSupersetOf(new Set([1, 2])), new Set([]).isDisjointFrom(new Set([1])), new Set([1]).isDisjointFrom(new Set([1, 2]))]"#,
        r#"{"ok":true,"value":[true,false,true,true,false],"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1, 2, 3, 4]); const like = { size: 1, has() { s.delete(3); return false }, keys: () => [9].values() }; return [[...s.difference({ size: 10, has(x) { s.add(99); return x === 1 }, keys: () => [].values() })], [...s]]"#,
        r#"{"ok":true,"value":[[2,3,4],[1,2,3,4,99]],"toolCalls":[]}"#,
    ),
    (
        r#"const s = new Set([1, 2, 3]); const seen = []; const r = s.intersection({ size: 10, has(x) { seen.push(x); if (x === 1) s.add(5); return true }, keys: () => [].values() }); return [[...r], seen]"#,
        r#"{"ok":true,"value":[[1,2,3,5],[1,2,3,5]],"toolCalls":[]}"#,
    ),
    (
        r#"const it = new Set([1, 2]).values(); return [it.next(), [...it], it.next()]"#,
        r#"{"ok":true,"value":[{"value":1,"done":false},[2],{"done":true}],"toolCalls":[]}"#,
    ),
    (
        r#"const m = new Map([[1, 2]]); return [m.keys().toArray(), m.values().toArray(), m.entries().toArray()]"#,
        r#"{"ok":true,"value":[[1],[2],[[1,2]]],"toolCalls":[]}"#,
    ),
];

#[test]
fn collections_matches_upstream() {
    check(CASES);
}
