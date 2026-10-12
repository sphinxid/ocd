//! Parity tests for `Iterator` and the iterator helpers: each program's result JSON must equal what upstream `@opencode/codemode`
//! printed for it (cases ported from upstream stdlib, callbacks, parity, enumeration, and test262-derived tests,
//! plus edge cases for holes, integrity levels, coercion, and live iteration).

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
        const log = []
        function* g() { try { yield 1; yield 2; yield 3 } finally { log.push("closed") } }
        const seen = []
        g().forEach((v, i) => seen.push([v, i]))
        return {
          seen,
          sum: g().reduce((a, b) => a + b),
          sumFrom: g().reduce((a, b) => a + b, 10),
          some: g().some((v) => v === 2),
          every: g().every((v) => v < 2),
          find: g().find((v) => v > 1),
          missing: g().find((v) => v > 5),
          log,
        }
      "#,
        r#"{"ok":true,"value":{"seen":[[1,0],[2,1],[3,2]],"sum":6,"sumFrom":16,"some":true,"every":false,"find":2,"log":["closed","closed","closed","closed","closed","closed","closed"]},"toolCalls":[]}"#,
    ),
    (
        r#"
        const log = []
        function* g(name) { try { yield 1; yield 2 } finally { log.push(name) } }
        const throwing = g("throw").map((v) => { if (v === 2) throw new Error("boom"); return v })
        throwing.next()
        let message
        try { throwing.next() } catch (error) { message = error.message }
        const returned = g("return").map((v) => v)
        returned.next()
        const closed = returned.return()
        for (const v of g("loop").filter((v) => true)) break
        return { message, afterThrow: throwing.next(), closed, afterReturn: returned.next(), log }
      "#,
        r#"{"ok":true,"value":{"message":"boom","afterThrow":{"done":true},"closed":{"done":true},"afterReturn":{"done":true},"log":["throw","return","loop"]},"toolCalls":[]}"#,
    ),
    (
        r#"
        const it = [1].values()
        let n = 0
        const errors = []
        for (const attempt of [() => Iterator(), () => new Iterator(), () => Iterator.from(5)]) {
          try { attempt() } catch (error) { errors.push(error.name) }
        }
        return [
          Iterator.from(it) === it,
          Iterator.from("ab").toArray(),
          Iterator.from([1, 2]).map((v) => v * 2).toArray(),
          Iterator.from({ next: () => ({ done: n > 1, value: n++ }) }).toArray(),
          it instanceof Iterator,
          errors,
        ]
      "#,
        r#"{"ok":true,"value":[true,["a","b"],[2,4],[0,1],true,["TypeError","TypeError","TypeError"]],"toolCalls":[]}"#,
    ),
    (
        r#"
        function* g() { while (true) yield 1 }
        const it = g().map(() => it.next())
        return it.next()
      "#,
        r#"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Iterator helper is already running. (line 3, col 34)","location":{"line":3,"column":34}},"toolCalls":[]}"#,
    ),
    (
        r#"async function* g() { yield 1 } try { Array.from(g()) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Array.from expects a synchronous iterable or array-like value, received a generator."],"toolCalls":[]}"#,
    ),
    (
        r#"function* g() { yield 1; yield 2 } return Array.from(g(), (x, i) => [x, i])"#,
        r#"{"ok":true,"value":[[1,0],[2,1]],"toolCalls":[]}"#,
    ),
    (
        r#"function* g() { yield 1; yield 2; yield 3; yield 4 } return g().map((x) => x * 2).filter((x) => x > 2).toArray()"#,
        r#"{"ok":true,"value":[4,6,8],"toolCalls":[]}"#,
    ),
    (
        r#"function* g() { yield 1; yield 2; yield 3; yield 4 } return [g().take(2).toArray(), g().drop(3).toArray(), g().drop(10).toArray(), g().take(0).toArray()]"#,
        r#"{"ok":true,"value":[[1,2],[4],[],[]],"toolCalls":[]}"#,
    ),
    (
        r#"try { [1].values().flatMap((x) => "ab").toArray() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator.prototype.flatMap expects an iterable or iterator, received a string."],"toolCalls":[]}"#,
    ),
    (
        r#"try { [1].values().flatMap((x) => 5).toArray() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator.prototype.flatMap expects an iterable or iterator, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"const out = []; for (const bad of [-1, NaN, "x", -Infinity, undefined]) { try { [1].values().take(bad); out.push("ok") } catch (e) { out.push(e.name + ": " + e.message) } } return out"#,
        r#"{"ok":true,"value":["RangeError: Iterator.prototype.take expects a non-negative count, received -1.","RangeError: Iterator.prototype.take expects a non-negative count, received NaN.","RangeError: Iterator.prototype.take expects a non-negative count, received NaN.","RangeError: Iterator.prototype.take expects a non-negative count, received -Infinity.","RangeError: Iterator.prototype.take expects a non-negative count, received NaN."],"toolCalls":[]}"#,
    ),
    (
        r#"return [[1, 2, 3].values().take("2").toArray(), [1, 2, 3].values().drop(1.9).toArray(), [1, 2].values().take(Infinity).toArray(), [1, 2].values().drop(null).toArray()]"#,
        r#"{"ok":true,"value":[[1,2],[2,3],[1,2],[1,2]],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; function* g() { try { yield 1; yield 2; yield 3 } finally { closed++ } } const it = g().map((x) => x); it.next(); const r = it.return(); return [r, closed, it.next()]"#,
        r#"{"ok":true,"value":[{"done":true},1,{"done":true}],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; function* g() { try { yield 1; yield 2 } finally { closed++ } } try { g().map(() => { throw new Error("cb") }).next() } catch (e) { return [e.message, closed] }"#,
        r#"{"ok":true,"value":["cb",1],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; function* g() { try { yield 1; yield 2; yield 3 } finally { closed++ } } return [g().some((x) => x === 1), closed, g().every((x) => x > 5), closed, g().find((x) => x === 2), closed]"#,
        r#"{"ok":true,"value":[true,1,false,2,2,3],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; function* g() { try { yield 1; yield 2; yield 3 } finally { closed++ } } const r = g().take(1).toArray(); return [r, closed]"#,
        r#"{"ok":true,"value":[[1],1],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; function* g() { try { yield 1; yield 2 } finally { closed++ } } for (const x of g().map((x) => x)) { break } return closed"#,
        r#"{"ok":true,"value":1,"toolCalls":[]}"#,
    ),
    (
        r#"const it = [1, 2, 3].values().map((x) => { if (x === 2) it.next(); return x }); try { it.toArray() } catch (e) { return [e.name, e.message, it.next()] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator helper is already running.",{"done":true}],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Constructor Iterator requires 'new'."],"toolCalls":[]}"#,
    ),
    (
        r#"try { new Iterator() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator is abstract; use Iterator.from(...) or a built-in iterator."],"toolCalls":[]}"#,
    ),
    (
        r#"const it = Iterator.from({ next() { return { value: 1, done: false } } }); return [it.take(3).toArray(), typeof it.return]"#,
        r#"{"ok":true,"value":[[1,1,1],"function"],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.from(5) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator.from expects an iterable or iterator, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.from(null) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator.from expects an iterable or iterator, received null."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.from({}) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator next must be a function."],"toolCalls":[]}"#,
    ),
    (
        r#"let closed = 0; const src = { [Symbol.iterator]() { return { next() { return { value: 1, done: false } }, return() { closed++; return {} } } } }; const w = Iterator.from(src); w.next(); return [w.return(), closed]"#,
        r#"{"ok":true,"value":[{"done":true},1],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.prototype.next.call({}) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator.prototype.next called on incompatible receiver a data object."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.prototype.map.call(5, (x) => x) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","An iterator must be an object, received a number."],"toolCalls":[]}"#,
    ),
    (
        r#"try { Iterator.prototype.map.call({}, (x) => x) } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator next must be a function."],"toolCalls":[]}"#,
    ),
    (
        r#"const obj = { i: 0, next() { return { value: this.i++, done: this.i > 3 } } }; return Iterator.prototype.toArray.call(obj)"#,
        r#"{"ok":true,"value":[0,1,2],"toolCalls":[]}"#,
    ),
    (
        r#"function* inner() { yield "a"; yield "b" } return [1, 2].values().flatMap((x) => inner()).toArray()"#,
        r#"{"ok":true,"value":["a","b","a","b"],"toolCalls":[]}"#,
    ),
    (
        r#"let closedInner = 0; let closedOuter = 0; function* outer() { try { yield 1; yield 2 } finally { closedOuter++ } } function* inner() { try { yield "a"; yield "b" } finally { closedInner++ } } const h = outer().flatMap(() => inner()); h.next(); h.return(); return [closedInner, closedOuter]"#,
        r#"{"ok":true,"value":[1,1],"toolCalls":[]}"#,
    ),
    (
        r#"return [Object.getPrototypeOf([].values()) === Iterator.prototype, Object.getPrototypeOf(Object.getPrototypeOf([].values().map((x) => x))) === Iterator.prototype, [].values() instanceof Iterator]"#,
        r#"{"ok":true,"value":[true,true,true],"toolCalls":[]}"#,
    ),
    (
        r#"return [typeof Iterator.prototype[Symbol.iterator], [1].values()[Symbol.iterator]().next()]"#,
        r#"{"ok":true,"value":["function",{"value":1,"done":false}],"toolCalls":[]}"#,
    ),
    (
        r#"let count = 0; const src = { next() { count++; return { value: count, done: count > 5 } } }; const h = Iterator.prototype.drop.call(src, 2); return [h.next(), count]"#,
        r#"{"ok":true,"value":[{"value":3,"done":false},3],"toolCalls":[]}"#,
    ),
    (
        r#"const h = [1, 2, 3].values().take(1); return [h.next(), h.next(), h.next()]"#,
        r#"{"ok":true,"value":[{"value":1,"done":false},{"done":true},{"done":true}],"toolCalls":[]}"#,
    ),
    (
        r#"let closes = 0; const src = { next() { return { value: 1, done: false } }, return() { closes++; return {} } }; const h = Iterator.prototype.take.call(src, 1); return [h.next(), h.next(), closes, h.next(), closes]"#,
        r#"{"ok":true,"value":[{"value":1,"done":false},{"done":true},1,{"done":true},1],"toolCalls":[]}"#,
    ),
    (
        r#"let closes = 0; const src = { next() { return { value: 1, done: false } }, return() { closes++; return {} } }; const h = Iterator.prototype.map.call(src, (x) => x); h.return(); h.return(); return [closes, h.next()]"#,
        r#"{"ok":true,"value":[1,{"done":true}],"toolCalls":[]}"#,
    ),
    (
        r#"let closes = 0; const src = { next() { return { value: 1, done: false } }, return() { closes++; throw new Error("close fails") } }; try { Iterator.prototype.map.call(src, () => { throw new Error("cb fails") }).next() } catch (e) { return [e.message, closes] }"#,
        r#"{"ok":true,"value":["cb fails",1],"toolCalls":[]}"#,
    ),
    (
        r#"let closes = 0; const src = { next() { return { value: 1, done: false } }, return() { closes++; throw new Error("close fails") } }; try { Iterator.prototype.some.call(src, () => true) } catch (e) { return [e.message, closes] }"#,
        r#"{"ok":true,"value":["close fails",1],"toolCalls":[]}"#,
    ),
    (
        r#"let h; const src = { next() { h.return(); return { value: 1, done: false } } }; h = Iterator.prototype.map.call(src, (x) => x); try { h.next() } catch (e) { return [e.name, e.message] }"#,
        r#"{"ok":true,"value":["TypeError","Iterator helper is already running."],"toolCalls":[]}"#,
    ),
    (
        r#"return [1, 2, 3].values().filter((x, i) => i > 0).map((x, i) => x * 10 + i).toArray()"#,
        r#"{"ok":true,"value":[20,31],"toolCalls":[]}"#,
    ),
];

#[test]
fn iterator_matches_upstream() {
    check(CASES);
}
