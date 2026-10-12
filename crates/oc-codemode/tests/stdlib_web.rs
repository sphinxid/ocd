//! Parity tests for `URL`, `URLSearchParams`, the URI helpers, `Headers`, `Uint8Array`, `TextEncoder`, `TextDecoder`,
//! `atob`/`btoa`, `crypto`, and `structuredClone`: each program's result JSON must equal what upstream
//! `@opencode/codemode` (on Bun) printed for it. Cases are ported from upstream stdlib, parity, and web-wpt tests
//! (including the WPT forgiving-base64 fixture), plus URL input and setter matrices that pin Bun's WebKit URL
//! behavior, and edge cases for coercion, receivers, live iteration, and error texts.

use std::sync::Arc;

use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;

fn normalize(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("expected JSON")
}

/// Runs every `(program, expected result JSON)` pair and reports all mismatches at once.
fn check(cases: &[(&str, &str)]) {
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

/// `URL`, `URLSearchParams`, and the URI helpers.
#[test]
fn url_search_params_and_uri_helpers() {
    check(&[
        (
            r####"const url = new URL("../users?id=old#top", "https://user:pass@example.com:8443/api/v1/")
url.pathname = "/items/a b"
url.searchParams.set("id", "a b")
url.searchParams.append("tag", "x/y")
url.hash = "part 1"
return { href: url.href, origin: url.origin, host: url.host, pathname: url.pathname, search: url.search, id: url.searchParams.get("id"), string: `${url}`, json: url.toJSON(), instances: [url instanceof URL, url.searchParams instanceof URLSearchParams, url.searchParams === url.searchParams] }"####,
            r####"{"ok":true,"value":{"href":"https://user:pass@example.com:8443/items/a%20b?id=a+b&tag=x%2Fy#part%201","origin":"https://example.com:8443","host":"example.com:8443","pathname":"/items/a%20b","search":"?id=a+b&tag=x%2Fy","id":"a b","string":"https://user:pass@example.com:8443/items/a%20b?id=a+b&tag=x%2Fy#part%201","json":"https://user:pass@example.com:8443/items/a%20b?id=a+b&tag=x%2Fy#part%201","instances":[true,true,true]},"toolCalls":[]}"####,
        ),
        (
            r####"const parsed = URL.parse("/users", "https://example.test/api/")
let invalidIsTypeError = false
try { new URL("not relative without a base") } catch (error) { invalidIsTypeError = error instanceof TypeError }
return { canParse: URL.canParse("/users", "https://example.test/api/"), cannotParse: URL.canParse("not relative without a base"), parsed: parsed.href, invalidIsTypeError, boundary: [new URL("https://example.test/a"), new URLSearchParams("q=one")], json: JSON.stringify({ url: new URL("https://example.test/a"), params: new URLSearchParams("q=one") }) }"####,
            r####"{"ok":true,"value":{"canParse":true,"cannotParse":false,"parsed":"https://example.test/users","invalidIsTypeError":true,"boundary":["https://example.test/a","q=one"],"json":"{\"url\":\"https://example.test/a\",\"params\":{}}"},"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
return [m(()=>decodeURIComponent("%")), m(()=>encodeURIComponent("\ud800")), m(()=>new URL("x")), m(()=>new URL("x", "y")), m(()=>{const u = new URL("https://a.b/c"); u.href="nope"}), m(()=>new URL()), m(()=>URL.canParse()), m(()=>URLSearchParams()), m(()=>new URLSearchParams(Symbol.iterator)), m(()=>new URLSearchParams(()=>1)), m(()=>new URLSearchParams([["a"]])), m(()=>new URLSearchParams([1]))]"####,
            r####"{"ok":true,"value":["URIError: decodeURIComponent received malformed URI data: URI error","URIError: encodeURIComponent received malformed URI data: String contained an illegal UTF-16 sequence.","TypeError: new URL(...) received an invalid URL.","TypeError: new URL(...) received an invalid URL or base URL.","TypeError: URL.href received an invalid value.","TypeError: new URL(...) requires a URL string and an optional base URL.","TypeError: URL.canParse requires a URL argument.","TypeError: Constructor URLSearchParams requires 'new'.","TypeError: new URLSearchParams(...) expects a query string, data object, iterable pairs, or URLSearchParams.","TypeError: new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs."],"toolCalls":[]}"####,
        ),
        (
            r####"return [encodeURI("https://example.test/a b?q=a/b"), encodeURIComponent("a b/c?"), decodeURI("https://example.test/a%20b?q=a/b"), decodeURIComponent("a%20b%2Fc%3F"), decodeURI("%3B%2f%E2%9C%93%F0%9F%98%80"), encodeURIComponent("✓😀 ;/?#"), encodeURI("✓😀 ;/?#[]")]"####,
            r####"{"ok":true,"value":["https://example.test/a%20b?q=a/b","a%20b%2Fc%3F","https://example.test/a b?q=a/b","a b/c?","%3B%2f✓😀","%E2%9C%93%F0%9F%98%80%20%3B%2F%3F%23","%E2%9C%93%F0%9F%98%80%20;/?#%5B%5D"],"toolCalls":[]}"####,
        ),
        (
            r####"const params = new URLSearchParams([["tag", "b"], ["tag", "a"], ["q", "a b"]])
const seen = []
params.forEach((value, key) => { seen[seen.length] = key + "=" + value })
params.delete("tag", "b")
params.append("tag", "c")
params.sort()
return { text: params.toString(), size: params.size, tags: params.getAll("tag"), has: params.has("tag", "c"), entries: [...params], object: Object.fromEntries(params), record: new URLSearchParams({ page: 2, filter: "open" }).toString(), seen }"####,
            r####"{"ok":true,"value":{"text":"q=a+b&tag=a&tag=c","size":3,"tags":["a","c"],"has":true,"entries":[["q","a b"],["tag","a"],["tag","c"]],"object":{"q":"a b","tag":"c"},"record":"page=2&filter=open","seen":["tag=b","tag=a","q=a b"]},"toolCalls":[]}"####,
        ),
        (
            r####"function throwsTypeError(run) { try { run(); return false } catch (error) { return error instanceof TypeError } }
const params = new URLSearchParams()
const required = [throwsTypeError(() => params.append()), throwsTypeError(() => params.delete()), throwsTypeError(() => params.get()), throwsTypeError(() => params.getAll()), throwsTypeError(() => params.has()), throwsTypeError(() => params.set()), throwsTypeError(() => params.forEach())]
params.append(undefined, undefined)
return { construct: throwsTypeError(() => new URL()), canParse: throwsTypeError(() => URL.canParse()), parse: throwsTypeError(() => URL.parse()), explicitUndefined: new URL(undefined, "https://example.test/base/").href, params: params.toString(), required }"####,
            r####"{"ok":true,"value":{"construct":true,"canParse":true,"parse":true,"explicitUndefined":"https://example.test/base/undefined","params":"undefined=undefined","required":[true,true,true,true,true,true,true]},"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const p = new URLSearchParams()
return [m(() => p.append("a")), m(() => p.delete()), m(() => p.forEach()), m(() => p.forEach(1)), m(() => URLSearchParams.prototype.get.call(1, "a")), m(() => URLSearchParams.prototype.sort.call({})), m(() => URL.prototype.href), m(() => URL.prototype.toString.call({}))]"####,
            r####"{"ok":true,"value":["TypeError: URLSearchParams.append requires 2 arguments.","TypeError: URLSearchParams.delete requires 1 argument.","TypeError: URLSearchParams.forEach requires 1 argument.","TypeError: URLSearchParams.forEach expects a function callback.","TypeError: URLSearchParams.prototype.get called on incompatible receiver a number.","TypeError: URLSearchParams.prototype.sort called on incompatible receiver a data object.","TypeError: URL.prototype.href called on incompatible receiver a data object.","TypeError: URL.prototype.toString called on incompatible receiver a data object."],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://ex.com/p?a=1&b=2#f")
const p = u.searchParams
u.search = "?x=%20y&z"
const r1 = [...p]
u.href = "http://h.test/?q=1+2"
const r2 = [...p]
p.delete("q")
const r3 = u.href
p.append("k", "v w/é")
const r4 = u.href
u.search = ""
return [r1, r2, r3, r4, u.href, p.size, u.searchParams === p]"####,
            r####"{"ok":true,"value":[[["x"," y"],["z",""]],[["q","1 2"]],"http://h.test/","http://h.test/?k=v+w%2F%C3%A9","http://h.test/",0,true],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://user:pw@例え.テスト:443/a/../b/./c?x#y")
const r = [u.href, u.origin, u.protocol, u.username, u.password, u.host, u.hostname, u.port, u.pathname, u.search, u.hash]
u.protocol = "http"; u.port = "8080"; u.username = "a b"; u.password = "p@ss"; u.hostname = "example.org"; u.hash = ""; u.pathname = "x y"
r[r.length] = u.href
u.protocol = "ftp:"; u.host = "h:21"
r[r.length] = u.href
u.port = "99999"; u.protocol = "1nv"
r[r.length] = u.href
u.origin = "x"
r[r.length] = u.origin
return r"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Cannot assign to read only property 'origin'. (line 9, col 1)","location":{"line":9,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"return [new URL("file:///C:/x/../y").href, new URL("data:text/plain,hi there").href, new URL("data:text/plain,hi").origin, new URL("blob:https://a.b/x").origin, new URL("HTTP://EXAMPLE.com:80/").href, new URL("https://[::1]:8080/").host, new URL("https://192.168.0.257/").href, new URL("  https://a.b/\t\n x ").href, new URL("https://a.b/?q=a b'c").search, new URL("foo://bar/baz?a b'c").search, new URL("https://a.b/#a b`c").hash, new URL("https://0x7f.1/").hostname, URL.canParse("https://a b/"), URL.parse("nope"), new URL("?x", "https://a.b/p/q").href, new URL("//h2/x", "https://a.b/p").href, new URL(new URL("https://u.rl/z")).href, new URL("z", new URL("https://u.rl/a/b")).href]"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: new URL(...) received an invalid URL. (line 1, col 239)","location":{"line":1,"column":239}},"toolCalls":[]}"####,
        ),
        (
            r####"const p = new URLSearchParams("?a=1&a=2&b=%zz&c=%E2%9C%93&d+e=f+g&&=x&y=&z")
const r = [...p, p.size, p.toString()]
p.set("a", "3")
r[r.length] = p.toString()
const q = new URLSearchParams("z=1&a=2&z=0&b=3&a=1"); q.sort()
r[r.length] = q.toString()
r[r.length] = new URLSearchParams(q).toString()
r[r.length] = new URLSearchParams(null).toString()
r[r.length] = new URLSearchParams(12).toString()
r[r.length] = new URLSearchParams(true).toString()
r[r.length] = new URLSearchParams(new Map([["m", 1]])).toString()
r[r.length] = new URLSearchParams(["ab", "cd"]).toString()
r[r.length] = new URLSearchParams({ "é ü": "~!*'()" }).toString()
r[r.length] = new URLSearchParams("a=\ud800").toString()
r[r.length] = [...new URLSearchParams("a=1").keys(), ...new URLSearchParams("a=1").values(), ...new URLSearchParams("a=1").entries()]
r[r.length] = p.get("nope")
return r"####,
            r####"{"ok":true,"value":[["a","1"],["a","2"],["b","%zz"],["c","✓"],["d e","f g"],["","x"],["y",""],["z",""],8,"a=1&a=2&b=%25zz&c=%E2%9C%93&d+e=f+g&=x&y=&z=","a=3&b=%25zz&c=%E2%9C%93&d+e=f+g&=x&y=&z=","a=2&a=1&b=3&z=1&z=0","a=2&a=1&b=3&z=1&z=0","null=","12=","true=","m=1","a=b&c=d","%C3%A9+%C3%BC=%7E%21*%27%28%29","a=%EF%BF%BD",["a","1",["a","1"]],null],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://example.com/path?q=1#h")
const out = []
const props = ["protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"]
const vals = { protocol: ["http", "ftp:", "foo", "HTTPS:x", ""], username: ["us er", "a@b:c", ""], password: ["p w", "x/y", ""], host: ["other.org:81", "[::1]:8080", "bad host", "", "a.b:99999"], hostname: ["new.example", "with:port", "ex%41mple.com", "ÉXAMPLE.com"], port: ["8080", "443", "x", "12abc", "", "70000", "-1"], pathname: ["a/b c", "/%zz/ü", "", "?#"], search: ["a=b c", "?x", "", "#", "?ü=ö"], hash: ["#frag", "x y", "", "ü"] }
for (const p of props) for (const v of vals[p]) { const c = new URL(u.href); try { c[p] = v; out.push([p, v, c.href]) } catch (e) { out.push([p, v, e.name + ": " + e.message]) } }
return out"####,
            r####"{"ok":true,"value":[["protocol","http","http://example.com/path?q=1#h"],["protocol","ftp:","ftp://example.com/path?q=1#h"],["protocol","foo","https://example.com/path?q=1#h"],["protocol","HTTPS:x","https://example.com/path?q=1#h"],["protocol","","https://example.com/path?q=1#h"],["username","us er","https://us%20er@example.com/path?q=1#h"],["username","a@b:c","https://a%40b%3Ac@example.com/path?q=1#h"],["username","","https://example.com/path?q=1#h"],["password","p w","https://:p%20w@example.com/path?q=1#h"],["password","x/y","https://:x%2Fy@example.com/path?q=1#h"],["password","","https://example.com/path?q=1#h"],["host","other.org:81","https://other.org:81/path?q=1#h"],["host","[::1]:8080","https://[::1]:8080/path?q=1#h"],["host","bad host","https://example.com/path?q=1#h"],["host","","https://example.com/path?q=1#h"],["host","a.b:99999","https://a.b/path?q=1#h"],["hostname","new.example","https://new.example/path?q=1#h"],["hostname","with:port","https://example.com/path?q=1#h"],["hostname","ex%41mple.com","https://example.com/path?q=1#h"],["hostname","ÉXAMPLE.com","https://xn--xample-9ua.com/path?q=1#h"],["port","8080","https://example.com:8080/path?q=1#h"],["port","443","https://example.com/path?q=1#h"],["port","x","https://example.com/path?q=1#h"],["port","12abc","https://example.com:12/path?q=1#h"],["port","","https://example.com/path?q=1#h"],["port","70000","https://example.com/path?q=1#h"],["port","-1","https://example.com/path?q=1#h"],["pathname","a/b c","https://example.com/a/b%20c?q=1#h"],["pathname","/%zz/ü","https://example.com/%zz/%C3%BC?q=1#h"],["pathname","","https://example.com/?q=1#h"],["pathname","?#","https://example.com/%3F%23?q=1#h"],["search","a=b c","https://example.com/path?a=b%20c#h"],["search","?x","https://example.com/path?x#h"],["search","","https://example.com/path#h"],["search","#","https://example.com/path?%23#h"],["search","?ü=ö","https://example.com/path?%C3%BC=%C3%B6#h"],["hash","#frag","https://example.com/path?q=1#frag"],["hash","x y","https://example.com/path?q=1#x%20y"],["hash","","https://example.com/path?q=1"],["hash","ü","https://example.com/path?q=1#%C3%BC"]],"toolCalls":[]}"####,
        ),
        (
            r####"const out = []
for (const [input, base] of [["http://a/b/c/d;p?q", undefined], ["g", "http://a/b/c/d;p?q"], ["../../../g", "http://a/b/c/d;p?q"], ["//g", "http://a/b/c/d;p?q"], ["?y", "http://a/b/c/d;p?q"], ["#s", "http://a/b/c/d;p?q"], ["HTTP://EXAMPLE.COM:80/%7Efoo", undefined], ["http://192.168.0.257", undefined], ["http://0x7f.1", undefined], ["http://[::ffff:1.2.3.4]/", undefined], ["http://[1:0:0:0:0:0:0:1]", undefined], ["mailto:Joe@Example.COM", undefined], ["javascript:alert(1)", undefined], ["blob:https://a.com/uuid", undefined], ["ws://h:80/", undefined], ["wss://h:443/", undefined], ["ftp://h:21/x", undefined], ["http://h/a\\b", undefined], ["non-special://h/a\\b", undefined], ["http://user@/x", undefined], ["http://ex ample.com", undefined], ["http://%41.com", undefined], ["http://xn--nxasmq6b.com", undefined], ["https://☃.net/", undefined], ["http://h/ä?ä#ä", undefined], ["  http://h/  ", undefined], ["http://h/\t\nx", undefined], ["https://a.b/c?d=e f", undefined], ["about:blank", undefined], ["sc://ñ.test/", undefined], ["https://example.com:/", undefined], ["http://1.2.3", undefined], ["http://h/%2e%2E/x", undefined], ["file://localhost/etc", undefined], ["file:/x", undefined], ["", "https://a.b/c?d#e"]]) {
  try { const x = new URL(input, base); out.push([input, x.href, x.origin, x.host, x.pathname, x.search, x.hash]) } catch (e) { out.push([input, e.name + ": " + e.message]) }
}
return out"####,
            r####"{"ok":true,"value":[["http://a/b/c/d;p?q","http://a/b/c/d;p?q","http://a","a","/b/c/d;p","?q",""],["g","http://a/b/c/g","http://a","a","/b/c/g","",""],["../../../g","http://a/g","http://a","a","/g","",""],["//g","http://g/","http://g","g","/","",""],["?y","http://a/b/c/d;p?y","http://a","a","/b/c/d;p","?y",""],["#s","http://a/b/c/d;p?q#s","http://a","a","/b/c/d;p","?q","#s"],["HTTP://EXAMPLE.COM:80/%7Efoo","http://example.com/%7Efoo","http://example.com","example.com","/%7Efoo","",""],["http://192.168.0.257","TypeError: new URL(...) received an invalid URL."],["http://0x7f.1","http://127.0.0.1/","http://127.0.0.1","127.0.0.1","/","",""],["http://[::ffff:1.2.3.4]/","http://[::ffff:102:304]/","http://[::ffff:102:304]","[::ffff:102:304]","/","",""],["http://[1:0:0:0:0:0:0:1]","http://[1::1]/","http://[1::1]","[1::1]","/","",""],["mailto:Joe@Example.COM","mailto:Joe@Example.COM","null","","Joe@Example.COM","",""],["javascript:alert(1)","javascript:alert(1)","null","","alert(1)","",""],["blob:https://a.com/uuid","blob:https://a.com/uuid","https://a.com","","https://a.com/uuid","",""],["ws://h:80/","ws://h/","ws://h","h","/","",""],["wss://h:443/","wss://h/","wss://h","h","/","",""],["ftp://h:21/x","ftp://h/x","ftp://h","h","/x","",""],["http://h/a\\b","http://h/a/b","http://h","h","/a/b","",""],["non-special://h/a\\b","non-special://h/a\\b","null","h","/a\\b","",""],["http://user@/x","TypeError: new URL(...) received an invalid URL."],["http://ex ample.com","TypeError: new URL(...) received an invalid URL."],["http://%41.com","http://a.com/","http://a.com","a.com","/","",""],["http://xn--nxasmq6b.com","http://xn--nxasmq6b.com/","http://xn--nxasmq6b.com","xn--nxasmq6b.com","/","",""],["https://☃.net/","https://xn--n3h.net/","https://xn--n3h.net","xn--n3h.net","/","",""],["http://h/ä?ä#ä","http://h/%C3%A4?%C3%A4#%C3%A4","http://h","h","/%C3%A4","?%C3%A4","#%C3%A4"],["  http://h/  ","http://h/","http://h","h","/","",""],["http://h/\t\nx","http://h/x","http://h","h","/x","",""],["https://a.b/c?d=e f","https://a.b/c?d=e%20f","https://a.b","a.b","/c","?d=e%20f",""],["about:blank","about:blank","null","","blank","",""],["sc://ñ.test/","sc://%C3%B1.test/","null","%C3%B1.test","/","",""],["https://example.com:/","https://example.com/","https://example.com","example.com","/","",""],["http://1.2.3","http://1.2.0.3/","http://1.2.0.3","1.2.0.3","/","",""],["http://h/%2e%2E/x","http://h/x","http://h","h","/x","",""],["file://localhost/etc","file:///etc","null","","/etc","",""],["file:/x","file:///x","null","","/x","",""],["","https://a.b/c?d","https://a.b","a.b","/c","?d",""]],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://x.y/?a=1&b=2&a=3")
const sp = u.searchParams
sp.append("c", "4 5")
const h1 = u.href
sp.delete("a")
const h2 = u.href
sp.delete("b"); sp.delete("c")
const h3 = u.href
u.search = "?z=1"
const after = [...sp]
u.href = "https://x.y/n?k=v"
const after2 = [...sp]
u.hash = "#q"
sp.sort()
sp.set("k", "w")
return [h1, h2, h3, after, after2, u.href, u.search, sp.toString(), sp.size, u.searchParams === sp]"####,
            r####"{"ok":true,"value":["https://x.y/?a=1&b=2&a=3&c=4+5","https://x.y/?b=2&c=4+5","https://x.y/",[["z","1"]],[["k","v"]],"https://x.y/n?k=w#q","?k=w","k=w",1,true],"toolCalls":[]}"####,
        ),
        (
            r####"const p = new URLSearchParams({ b: 2, a: [1, 2], 1: "one", c: null, d: undefined, e: { x: 1 } })
const q = new URLSearchParams(p)
q.append("a", "x")
const r = new URLSearchParams(new Map([["m", 1], ["n", 2]]))
const s = new URLSearchParams([["x", 1], new Set(["y", 2])])
const t = new URLSearchParams(42)
const t2 = new URLSearchParams(null)
const t3 = new URLSearchParams(true)
const t4 = new URLSearchParams("??a=1")
const t5 = new URLSearchParams("a=%zz&b=%2&c=+%2B")
return [p.toString(), q.toString(), p.toString(), r.toString(), s.toString(), t.toString(), t2.toString(), t3.toString(), t4.toString(), [...t5], t5.toString()]"####,
            r####"{"ok":true,"value":["1=one&b=2&a=1%2C2&c=null&d=undefined&e=%5Bobject+Object%5D","1=one&b=2&a=1%2C2&c=null&d=undefined&e=%5Bobject+Object%5D&a=x","1=one&b=2&a=1%2C2&c=null&d=undefined&e=%5Bobject+Object%5D","m=1&n=2","x=1&y=2","42=","null=","true=","%3Fa=1",[["a","%zz"],["b","%2"],["c"," +"]],"a=%25zz&b=%252&c=+%2B"],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const p = new URLSearchParams("a=1&b=2&a=3&c=")
return [p.get("a"), p.get("z"), p.getAll("a"), p.getAll("z"), p.has("a"), p.has("a", "3"), p.has("a", "4"), p.has("c"), p.has("a", undefined), m(() => p.get()), m(() => p.append("x")), m(() => p.set()), m(() => p.getAll()), m(() => p.has()), m(() => p.delete()), m(() => p.forEach()), m(() => p.forEach(1)), m(() => URLSearchParams.prototype.get.call({}, "a")), m(() => new URLSearchParams([["a"]])), m(() => new URLSearchParams([["a", "b", "c"]])), m(() => new URLSearchParams([1])), m(() => new URLSearchParams(Symbol())), m(() => new URLSearchParams(() => 1)), m(() => new URLSearchParams(Promise.resolve(1))), (() => { p.delete("a", "1"); return p.toString() })(), (() => { p.delete("a", undefined); return p.toString() })()]"####,
            r####"{"ok":true,"value":["1",null,["1","3"],[],true,true,false,true,true,"TypeError: URLSearchParams.get requires 1 argument.","TypeError: URLSearchParams.append requires 2 arguments.","TypeError: URLSearchParams.set requires 2 arguments.","TypeError: URLSearchParams.getAll requires 1 argument.","TypeError: URLSearchParams.has requires 1 argument.","TypeError: URLSearchParams.delete requires 1 argument.","TypeError: URLSearchParams.forEach requires 1 argument.","TypeError: URLSearchParams.forEach expects a function callback.","TypeError: URLSearchParams.prototype.get called on incompatible receiver a data object.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs.","TypeError: Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.","TypeError: new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.","TypeError: new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.","b=2&a=3&c=","b=2&c="],"toolCalls":[]}"####,
        ),
        (
            r####"const p = new URLSearchParams("z=1&a=2&é=3&A=4&a=0&😀=5&｡=6")
p.sort()
const keys = [...p.keys()], values = [...p.values()], entries = [...p.entries()]
const it = p.keys()
const first = it.next()
p.append("late", "x")
const rest = [...it]
const seen = []
p.forEach(function (v, k, o) { seen.push([v, k, o === p, this.t]) }, { t: 1 })
return [keys, values, entries, first, rest, seen, p.toString(), Object.prototype.toString.call(p), Object.prototype.toString.call(it), p[Symbol.iterator] === p.entries]"####,
            r####"{"ok":true,"value":[["A","a","a","z","é","😀","｡"],["4","2","0","1","3","5","6"],[["A","4"],["a","2"],["a","0"],["z","1"],["é","3"],["😀","5"],["｡","6"]],{"value":"A","done":false},["a","a","z","é","😀","｡","late"],[["4","A",true,1],["2","a",true,1],["0","a",true,1],["1","z",true,1],["3","é",true,1],["5","😀",true,1],["6","｡",true,1],["x","late",true,1]],"A=4&a=2&a=0&z=1&%C3%A9=3&%F0%9F%98%80=5&%EF%BD%A1=6&late=x","[object URLSearchParams]","[object Iterator]",true],"toolCalls":[]}"####,
        ),
        (
            r####"const p = new URLSearchParams()
p.append("sp ace", "a+b&c=d")
p.append("ü", "\u{1F600}")
p.append("lone", "\uD800x")
p.append("*-._~!'()", "%")
return [p.toString(), p.get("lone"), [...new URLSearchParams(p.toString())], new URLSearchParams([["\uDC00", "v"]]).toString()]"####,
            r####"{"ok":true,"value":["sp+ace=a%2Bb%26c%3Dd&%C3%BC=%F0%9F%98%80&lone=%EF%BF%BDx&*-._%7E%21%27%28%29=%25","�x",[["sp ace","a+b&c=d"],["ü","😀"],["lone","�x"],["*-._~!'()","%"]],"%EF%BF%BD=v"],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
return [encodeURI("http://a.b/c d?e=f&g=h#i j ü 😀"), encodeURIComponent("http://a.b/c d?e=f&g=h#i j ü 😀;,/?:@&=+$-_.!~*'()#"), decodeURI("%3B%2F%3F%3A%40%26%3D%2B%24%2C%23%41%c3%bc"), decodeURIComponent("%3B%2F%3F%3A%40%26%3D%2B%24%2C%23%41%c3%bc%F0%9F%98%80"), m(() => decodeURIComponent("%")), m(() => decodeURIComponent("%zz")), m(() => decodeURIComponent("%C3")), m(() => decodeURIComponent("%C3%28")), m(() => decodeURIComponent("%C0%80")), m(() => decodeURIComponent("%ED%A0%80")), m(() => decodeURIComponent("%F4%90%80%80")), m(() => decodeURIComponent("%FF")), m(() => encodeURIComponent("\uD800")), m(() => encodeURIComponent("\uDC00x")), m(() => encodeURI("a\uD800b")), encodeURIComponent(), decodeURI(), encodeURI(null), encodeURIComponent(12.5), decodeURI("%E2%82%AC%e2%82%ac"), m(() => decodeURI("%E2%82")), decodeURIComponent("a%2"+"5b"), m(() => decodeURI("%E2%82%2")), m(() => decodeURI("%%41"))]"####,
            r####"{"ok":true,"value":["http://a.b/c%20d?e=f&g=h#i%20j%20%C3%BC%20%F0%9F%98%80","http%3A%2F%2Fa.b%2Fc%20d%3Fe%3Df%26g%3Dh%23i%20j%20%C3%BC%20%F0%9F%98%80%3B%2C%2F%3F%3A%40%26%3D%2B%24-_.!~*'()%23","%3B%2F%3F%3A%40%26%3D%2B%24%2C%23Aü",";/?:@&=+$,#Aü😀","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: decodeURIComponent received malformed URI data: URI error","URIError: encodeURIComponent received malformed URI data: String contained an illegal UTF-16 sequence.","URIError: encodeURIComponent received malformed URI data: String contained an illegal UTF-16 sequence.","URIError: encodeURI received malformed URI data: String contained an illegal UTF-16 sequence.","undefined","undefined","null","12.5","€€","URIError: decodeURI received malformed URI data: URI error","a%b","URIError: decodeURI received malformed URI data: URI error","URIError: decodeURI received malformed URI data: URI error"],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://h.com/")
u.foo = 1
const p = u.searchParams
p.bar = 2
return [Object.keys(u), u.foo, p.bar, JSON.stringify(u), { ...u }, { ...p }, Object.assign({}, u), "href" in u, Object.prototype.hasOwnProperty.call(u, "href"), delete u.href, u.href]"####,
            r####"{"ok":true,"value":[["foo"],1,2,"\"https://h.com/\"",{"foo":1},{"bar":2},{"foo":1},true,false,true,"https://h.com/"],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const u = new URL("https://a.b/c")
return [m(() => { u.href = "nope" }), u.href, m(() => { u.origin = "x" }), u.origin, m(() => URL.prototype.href), m(() => URL.prototype.origin), m(() => URL.prototype.searchParams), m(() => URL.prototype.toString.call(1)), m(() => URL.prototype.toJSON.call({})), m(() => URL.parse()), m(() => URL.canParse()), URL.parse("x"), URL.parse("/x", "nope"), URL.parse("/x", new URL("https://q.r/s/t")).href, URL.canParse("a:"), URL.canParse("/x", "https://ok/"), m(() => new URL("/x", "nope")), m(() => new URL("nope")), m(() => URL("x")), new URL(new URL("https://w.x/y")).href, m(() => new URL({ toString() { return "https://obj.x/" } }).href), m(() => new URL(Symbol())), m(() => new URL()), m(() => new URL(undefined)), m(() => new URL("https://a/", undefined).href), m(() => new URL("https://a/", null).href), m(() => new URL("x", null).href), m(() => new URL(12, "https://n.n/").href), typeof URL.canParse, URL.canParse.length, URL.parse.name, URL.parse.length, URL.prototype.toString.length, URL.prototype.toJSON.name]"####,
            r####"{"ok":true,"value":["TypeError: URL.href received an invalid value.","https://a.b/c","TypeError: Cannot assign to read only property 'origin'.","https://a.b","TypeError: URL.prototype.href called on incompatible receiver a data object.","TypeError: URL.prototype.origin called on incompatible receiver a data object.","TypeError: URL.prototype.searchParams called on incompatible receiver a data object.","TypeError: URL.prototype.toString called on incompatible receiver a number.","TypeError: URL.prototype.toJSON called on incompatible receiver a data object.","TypeError: URL.parse requires a URL argument.","TypeError: URL.canParse requires a URL argument.",null,null,"https://q.r/x",true,true,"TypeError: new URL(...) received an invalid URL or base URL.","TypeError: new URL(...) received an invalid URL.","TypeError: Constructor URL requires 'new'.","https://w.x/y","TypeError: new URL(...) received an invalid URL.","TypeError: Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.","TypeError: new URL(...) requires a URL string and an optional base URL.","TypeError: new URL(...) received an invalid URL.","https://a/","TypeError: new URL(...) received an invalid URL or base URL.","TypeError: new URL(...) received an invalid URL or base URL.","https://n.n/12","function",1,"parse",1,0,"toJSON"],"toolCalls":[]}"####,
        ),
        (
            r####"return [new URL("https://a/b"), new URLSearchParams("q=1&q=2"), new Headers([["set-cookie", "a"], ["set-cookie", "b"], ["x", "y"]]), { nested: { u: new URL("https://z/") } }]"####,
            r####"{"ok":true,"value":["https://a/b","q=1&q=2",{"x":"y","set-cookie":"b"},{"nested":{"u":"https://z/"}}],"toolCalls":[]}"####,
        ),
        (
            r####"return new URLSearchParams("a=1&a=2&b=x y")"####,
            r####"{"ok":true,"value":"a=1&a=2&b=x+y","toolCalls":[]}"####,
        ),
        (
            r####"return new URL("https://user@host/p?q#h")"####,
            r####"{"ok":true,"value":"https://user@host/p?q#h","toolCalls":[]}"####,
        ),
        (
            r####"return crypto"####,
            r####"{"ok":true,"value":{},"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://a/")
const out = []
for (const [p, v] of [["search", "?a=1&b=%zz+c"], ["search", ""], ["search", "?"], ["hash", "#"], ["hash", ""], ["pathname", ""], ["port", "0"], ["port", "443"], ["port", "080"], ["host", "x:443"], ["hostname", ""], ["protocol", "http:"], ["port", "8080"], ["protocol", "https"], ["username", "@"], ["password", ":"], ["host", "[::1"], ["hostname", "1.2.3.4"], ["hostname", "0x10.1"], ["protocol", "file"], ["href", "file:///x"], ["host", "h"], ["hostname", "h"], ["protocol", "http"], ["href", "mailto:x@y"], ["pathname", "/z"], ["host", "q"], ["protocol", "https"], ["search", "?x"], ["hash", "frag"]]) {
  u[p] = v
  out.push([p, v, u.href, u.searchParams.toString(), u.search, u[p]])
}
return out"####,
            r####"{"ok":true,"value":[["search","?a=1&b=%zz+c","https://a/?a=1&b=%zz+c","a=1&b=%25zz+c","?a=1&b=%zz+c","?a=1&b=%zz+c"],["search","","https://a/","","",""],["search","?","https://a/?","","",""],["hash","#","https://a/?#","","",""],["hash","","https://a/?","","",""],["pathname","","https://a/?","","","/"],["port","0","https://a:0/?","","","0"],["port","443","https://a/?","","",""],["port","080","https://a:80/?","","","80"],["host","x:443","https://x:80/?","","","x:80"],["hostname","","https://x:80/?","","","x"],["protocol","http:","http://x/?","","","http:"],["port","8080","http://x:8080/?","","","8080"],["protocol","https","https://x:8080/?","","","https:"],["username","@","https://%40@x:8080/?","","","%40"],["password",":","https://%40:%3A@x:8080/?","","","%3A"],["host","[::1","https://%40:%3A@x:8080/?","","","x:8080"],["hostname","1.2.3.4","https://%40:%3A@1.2.3.4:8080/?","","","1.2.3.4"],["hostname","0x10.1","https://%40:%3A@16.0.0.1:8080/?","","","16.0.0.1"],["protocol","file","https://%40:%3A@16.0.0.1:8080/?","","","https:"],["href","file:///x","file:///x","","","file:///x"],["host","h","file://h/x","","","h"],["hostname","h","file://h/x","","","h"],["protocol","http","http://h/x","","","http:"],["href","mailto:x@y","mailto:x@y","","","mailto:x@y"],["pathname","/z","mailto:x@y","","","x@y"],["host","q","mailto:x@y","","",""],["protocol","https","mailto:x@y","","","mailto:"],["search","?x","mailto:x@y?x","x=","?x","?x"],["hash","frag","mailto:x@y?x#frag","x=","?x","#frag"]],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://a/?q=1")
const sp = u.searchParams
sp.delete("q")
const r = [u.href, u.search]
sp.append("x", "")
r.push(u.href)
sp.set("x", "a b+c")
r.push(u.href, u.search)
u.searchParams.sort()
r.push(u.href)
const u2 = new URL("https://a/?b=2&a=1#h")
u2.searchParams.sort()
r.push(u2.href)
const u3 = new URL("https://a/?a=%20b&c")
r.push(u3.searchParams.toString(), u3.href)
u3.searchParams.append("d", "e")
r.push(u3.href)
const u4 = new URL("non-special://h/p?a b")
u4.searchParams.append("c", "d")
r.push(u4.href)
const u5 = new URL("https://a/?")
r.push(u5.href, u5.search, u5.searchParams.size)
u5.searchParams.sort()
r.push(u5.href)
return r"####,
            r####"{"ok":true,"value":["https://a/","","https://a/?x=","https://a/?x=a+b%2Bc","?x=a+b%2Bc","https://a/?x=a+b%2Bc","https://a/?a=1&b=2#h","a=+b&c=","https://a/?a=%20b&c","https://a/?a=+b&c=&d=e","non-special://h/p?a+b=&c=d","https://a/?","",0,"https://a/"],"toolCalls":[]}"####,
        ),
        (
            r####"const p = new URLSearchParams()
for (const [k, v] of [["a", "1"], ["b", "2"], ["a", "3"], ["c", "4"], ["a", "5"]]) p.append(k, v)
p.set("a", "X")
const r = [p.toString()]
p.set("new", "N")
r.push(p.toString())
const it = p.entries()
const seen = [it.next().value]
p.delete("a")
seen.push(it.next().value, it.next().value, it.next().value, it.next())
p.append("zz", "1")
seen.push(it.next())
r.push(seen, p.size)
const vals = []
for (const v of p.values()) { vals.push(v); if (vals.length < 3) p.append("more", "" + vals.length) }
r.push(vals)
const fe = []
p.forEach((v, k) => { fe.push(k); if (fe.length === 1) p.append("fe", "1") })
r.push(fe, p.has("fe"))
return r"####,
            r####"{"ok":true,"value":["a=X&b=2&c=4","a=X&b=2&c=4&new=N",[["a","X"],["c","4"],["new","N"],null,{"done":true},{"done":true}],4,["2","4","N","1","1","2"],["b","c","new","zz","more","more"],true],"toolCalls":[]}"####,
        ),
        (
            r####"const r = []
for (const s of ["a=b=c", "=", "&&&", "a", "a&b=", "%", "%2", "%zz", "%C3%A9=%E9", "a+b=c+d", "a%2Bb=1", "?", "??", "?a", "#a=1", "a=1#x", "%00=%00", "\uD800=1", "é=ü"]) {
  const p = new URLSearchParams(s)
  r.push([[...p], p.toString()])
}
return r"####,
            r####"{"ok":true,"value":[[[["a","b=c"]],"a=b%3Dc"],[[["",""]],"="],[[],""],[[["a",""]],"a="],[[["a",""],["b",""]],"a=&b="],[[["%",""]],"%25="],[[["%2",""]],"%252="],[[["%zz",""]],"%25zz="],[[["é","�"]],"%C3%A9=%EF%BF%BD"],[[["a b","c d"]],"a+b=c+d"],[[["a+b","1"]],"a%2Bb=1"],[[],""],[[["?",""]],"%3F="],[[["a",""]],"a="],[[["#a","1"]],"%23a=1"],[[["a","1#x"]],"a=1%23x"],[[["\u0000","\u0000"]],"%00=%00"],[[["�","1"]],"%EF%BF%BD=1"],[[["é","ü"]],"%C3%A9=%C3%BC"]],"toolCalls":[]}"####,
        ),
        (
            r####"async function main() {
  const m = async (f) => { try { return await f() } catch (e) { return e.name + ": " + e.message } }
  async function* gen() { yield ["a", "1"] }
  return [await m(() => new URLSearchParams(gen())), await m(() => new Headers(gen())), await m(() => new Uint8Array(gen())), await m(() => Uint8Array.from(gen())), await m(() => new URLSearchParams(gen)), await m(() => new Headers(main))]
}
return await main()"####,
            r####"{"ok":true,"value":["TypeError: new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.","TypeError: new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","TypeError: new Uint8Array(...) expects a Uint8Array, an array, or an iterable of numbers, received a generator.","TypeError: Uint8Array.from expects a Uint8Array, an array, or an iterable of numbers, received a generator.","TypeError: new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.","TypeError: new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers."],"toolCalls":[]}"####,
        ),
        (
            r####"const r = []
const h = new Headers({ 3: "three", b: "B", 1: "one", a: "A" })
r.push([...h])
const p = new URLSearchParams({ 3: "three", b: "B", 1: "one", a: "A" })
r.push(p.toString())
const proto = { inherited: "x" }
const obj = Object.create ? Object.create(proto) : { }
r.push(typeof Object.create)
const withSym = { a: "1", [Symbol.iterator]: undefined }
r.push((() => { try { return [...new Headers(withSym)] } catch (e) { return e.name + ": " + e.message } })())
const nonEnum = Object.freeze({ k: "v" })
r.push([...new Headers(nonEnum)], new URLSearchParams(nonEnum).toString())
const getterObj = { get g() { return "G" } }
r.push((() => { try { return [...new Headers(getterObj)] } catch (e) { return e.name + ": " + e.message } })(), (() => { try { return new URLSearchParams(getterObj).toString() } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return [...new Headers([1, 2].values())] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return [...new Headers(new Map([["a", "1"]]).entries())] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return [...new Headers("ab")] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return [...new Headers([["a", { toString() { return "x" } }]])] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return [...new Headers({ a: { toString() { return "x" } } })] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new URLSearchParams({ a: { toString() { return "x" } } }).toString() } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new URLSearchParams({ a: [1, [2, 3]], b: new Uint8Array([1, 2]) }).toString() } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new URLSearchParams({ a: Symbol("s") }).toString() } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new Headers({ a: Symbol("s") }) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new URLSearchParams([[Symbol("s"), "1"]]).toString() } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { const h = new Headers(); h.append(Symbol("s"), "x") } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { const h = new Headers(); h.append("a", { toString() { return "z" } }); return h.get("a") } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { const p = new URLSearchParams(); p.append("a", { toString() { return "z" } }); return p.get("a") } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return encodeURIComponent({ toString() { return "a b" } }) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return encodeURIComponent(Symbol()) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return atob(Symbol()) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return btoa({ toString() { return "hi" } }) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new TextEncoder().encode({ toString() { return "hi" } }).length } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new Uint8Array([{ valueOf() { return 7 } }])[0] } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new Uint8Array({ valueOf() { return 2 } }).length } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new Uint8Array([1, 2, 3]).at({ valueOf() { return 1 } }) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new Uint8Array([Symbol()]) } catch (e) { return e.name + ": " + e.message } })())
r.push((() => { try { return new URL("https://a/").searchParams.get({ toString() { return "x" } }) } catch (e) { return e.name + ": " + e.message } })())
return r"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Only init object properties are supported. (line 13, col 21)","location":{"line":13,"column":21}},"toolCalls":[]}"####,
        ),
    ]);
}

/// `Uint8Array`, `TextEncoder`, and `TextDecoder`.
#[test]
fn uint8_array_and_text_codecs() {
    check(&[
        (
            r####"const a = new Uint8Array(2)
const b = new Uint8Array([1, 2, 300])
const c = Uint8Array.from(new Set([7, 8]))
const d = new Uint8Array(b)
d[0] = 9
return [[...a], [...b], [...c], [...d], [...b], new Uint8Array("3").length, [...Uint8Array.of(1, "2")]]"####,
            r####"{"ok":true,"value":[[0,0],[1,2,44],[7,8],[9,2,44],[1,2,44],3,[1,2]],"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array(-1)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Invalid typed array length: -1 (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array(1.5)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Invalid typed array length: 1.5 (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array(20_000_000)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Invalid array length (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array(3)
b[0] = 300
b[1] = -1
b["2"] = "7"
b[5] = 9
b.tag = "x"
return [b[0], b[1], b[2], b[5], b.length, Object.keys(b), 1 in b, 5 in b, b.tag]"####,
            r####"{"ok":true,"value":[44,255,7,null,3,["0","1","2","tag"],true,false,"x"],"toolCalls":[]}"####,
        ),
        (
            r####"delete new Uint8Array(1)[0]"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Cannot delete property '0'. (line 1, col 8)","location":{"line":1,"column":8}},"toolCalls":[]}"####,
        ),
        (
            r####"Uint8Array.prototype.length"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Uint8Array.prototype.length called on incompatible receiver a data object. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3])
const [x, ...rest] = b
const seen = []
for (const byte of b) seen[seen.length] = byte
function* g() { yield* b }
return [x, rest, seen, [...g()], new Set(b).size]"####,
            r####"{"ok":true,"value":[1,[2,3],[1,2,3],[1,2,3],3],"toolCalls":[]}"####,
        ),
        (
            r####"const b = Uint8Array.from([1, 2, 3, 2])
const view = b.subarray(1, 3)
view.fill(9)
const copy = b.slice(0, 2)
copy[0] = 5
const target = new Uint8Array(4)
target.set([1, 2], 2)
target.set(b.subarray(0, 1))
return [b.at(-1), [...b], [...copy], [...target], b.indexOf(9), b.lastIndexOf(9), b.includes(3), b.includes(2, 1), b.join("-"), b.toString(), [...b.keys()], [...b.values()], [...b.entries()], [...Uint8Array.from([1, 2]).reverse()]]"####,
            r####"{"ok":true,"value":[2,[1,9,9,2],[5,9],[1,0,1,2],1,2,false,true,"1-9-9-2","1,9,9,2",[0,1,2,3],[1,9,9,2],[[0,1],[1,9],[2,9],[3,2]],[2,1]],"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array(2).set([1, 2, 3])"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Uint8Array.set: the source does not fit at that offset. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array(2).set([1], 2)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: Uint8Array.set: the source does not fit at that offset. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"return [new Uint8Array([104, 105]).toBase64(), new Uint8Array([255, 0]).toHex(), [...Uint8Array.fromBase64("aGk=")], [...Uint8Array.fromHex("ff00")]]"####,
            r####"{"ok":true,"value":["aGk=","ff00",[104,105],[255,0]],"toolCalls":[]}"####,
        ),
        (
            r####"Uint8Array.fromBase64("!!!")"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: Uint8Array.fromBase64: the string is not valid base64. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"Uint8Array.fromHex("zz")"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"SyntaxError: Uint8Array.fromHex: the string is not valid hex. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2])
console.log(b, new Uint8Array())
return [b + "", +new Uint8Array([5]), b == "1,2", JSON.stringify(b), typeof b, b instanceof Uint8Array]"####,
            r####"{"ok":true,"value":["1,2",5,true,"{\"0\":1,\"1\":2}","object",true],"logs":["Uint8Array(2) [1,2] Uint8Array(0) []"],"toolCalls":[]}"####,
        ),
        (
            r####"return new Uint8Array(1)"####,
            r####"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: A Uint8Array cannot cross to the host; pass text instead, e.g. `new TextDecoder().decode(bytes)` or `bytes.toBase64()`."},"toolCalls":[]}"####,
        ),
        (
            r####"return { deep: [new Uint8Array(1)] }"####,
            r####"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: A Uint8Array cannot cross to the host; pass text instead, e.g. `new TextDecoder().decode(bytes)` or `bytes.toBase64()`."},"toolCalls":[]}"####,
        ),
        (
            r####"return new Uint8Array([7, 8]).toBase64()"####,
            r####"{"ok":true,"value":"Bwg=","toolCalls":[]}"####,
        ),
        (
            r####"const encoder = new TextEncoder()
const bytes = encoder.encode("héllo ✓")
const decoder = new TextDecoder()
return [[...bytes], decoder.decode(bytes), decoder.decode(), [...encoder.encode()], encoder.encoding, decoder.encoding, decoder.fatal, decoder.ignoreBOM, new TextDecoder("UTF8", { fatal: true }).fatal, new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])), new TextDecoder("utf-8", { ignoreBOM: true }).decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x41])), new TextDecoder().decode(new Uint8Array([0xff])), encoder instanceof TextEncoder, decoder instanceof TextDecoder]"####,
            r####"{"ok":true,"value":[[104,195,169,108,108,111,32,226,156,147],"héllo ✓","",[],"utf-8","utf-8",false,false,true,"A","﻿A","�",true,true],"toolCalls":[]}"####,
        ),
        (
            r####"new TextDecoder("latin1")"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"RangeError: The \"latin1\" encoding is not supported; only UTF-8 is. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xff]))"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: TextDecoder.decode: the input is not valid utf-8. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new TextDecoder().decode("text")"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: TextDecoder.decode expects a Uint8Array, received a string. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"TextDecoder()"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Constructor TextDecoder requires 'new'. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array(16); const same = crypto.getRandomValues(b) === b; return [same, b.length, b.some((n) => n !== 0)]"####,
            r####"{"ok":true,"value":[true,16,true],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3])
const mapped = b.map((byte) => byte * 100)
const filtered = b.filter((byte) => byte > 1)
mapped[0] = 9
return [[...mapped], mapped instanceof Uint8Array, [...b], [...filtered], [...b.map(() => "7")], b.map((byte) => byte, {}).length, [...new Uint8Array().map((byte) => byte)]]"####,
            r####"{"ok":true,"value":[[9,200,44],true,[1,2,3],[2,3],[7,7,7],3,[]],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3])
const seen = []
b.forEach((byte, index, array) => { seen[seen.length] = [byte, index, array === b] })
return [b.find((byte) => byte > 1), b.find((byte) => byte > 5) === undefined, b.findIndex((byte) => byte > 1), b.findIndex((byte) => byte > 5), b.findLast((byte) => byte < 3), b.findLastIndex((byte) => byte < 3), b.findLastIndex((byte) => byte > 9), b.some((byte) => byte > 2), b.every((byte) => byte > 2), new Uint8Array().some(() => true), new Uint8Array().every(() => false), b.every((byte, index, array) => array === b), seen]"####,
            r####"{"ok":true,"value":[2,true,1,-1,2,1,-1,true,false,false,true,true,[[1,0,true],[2,1,true],[3,2,true]]],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3])
return [b.reduce((sum, byte) => sum + byte), b.reduce((sum, byte) => sum + byte, 10), b.reduceRight((text, byte) => text + byte, ""), new Uint8Array().reduce((sum, byte) => sum + byte, 5), b.reduce((_, byte, index, array) => array === b && index, 0)]"####,
            r####"{"ok":true,"value":[6,16,"321",5,2],"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array().reduce((sum, byte) => sum + byte)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Uint8Array.reduce of an empty array with no initial value. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array().reduceRight((sum, byte) => sum + byte)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Uint8Array.reduceRight of an empty array with no initial value. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array([1]).map(null)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Uint8Array.map expects a function callback. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([10, 9, 1])
const same = b.sort() === b
const desc = new Uint8Array([3, 1, 2]).sort((x, y) => y - x)
return [same, [...b], [...desc], desc instanceof Uint8Array, [...new Uint8Array([2, 1]).sort(() => NaN)]]"####,
            r####"{"ok":true,"value":[true,[1,9,10],[3,2,1],true,[2,1]],"toolCalls":[]}"####,
        ),
        (
            r####"new Uint8Array([2, 1]).sort(null)"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Uint8Array.sort expects a function callback. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 1])
return [b.lastIndexOf(1, undefined), b.lastIndexOf(1), b.indexOf(2, "1"), new Uint8Array([1, 2]).join(null), b.lastIndexOf(1, -1), b.lastIndexOf(1, -2), b.lastIndexOf(1, -4), b.indexOf(1, -1), b.indexOf(1, -10), b.includes(1, Infinity), b.at(1.7), b.at(-4), b.at(), b.at("x")]"####,
            r####"{"ok":true,"value":[0,2,1,"1null2",2,0,-1,2,0,false,2,null,1,1],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const out = []
for (const s of ["", "AA==", "AA", "AA=", "A", "AAAA", " A A A A ", "AAA=", "AB==", "AQ==", "AR==", "AAAAAA==", "AAAA=", "-_8=", "+/8=", "AA==\n", "AA== =", "A=A=", "SGVsbG8gV29ybGQ", "\tSGk=\n"]) out.push([s, m(() => [...Uint8Array.fromBase64(s)])])
for (const s of ["", "00", "0f", "0F", "ff00AB", "f", "fg", " 00", "0x00"]) out.push([s, m(() => [...Uint8Array.fromHex(s)])])
out.push(m(() => Uint8Array.fromBase64(1)), m(() => Uint8Array.fromHex()), Uint8Array.fromBase64.length, Uint8Array.fromHex.name)
out.push(new Uint8Array([0, 1, 2, 250, 251, 252, 253, 254, 255]).toBase64(), new Uint8Array([]).toBase64(), new Uint8Array([1]).toBase64(), new Uint8Array([1, 2]).toBase64(), new Uint8Array([171, 205, 239]).toHex())
return out"####,
            r####"{"ok":true,"value":[["",[]],["AA==",[0]],["AA",[0]],["AA=","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["A","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["AAAA",[0,0,0]],[" A A A A ",[0,0,0]],["AAA=",[0,0]],["AB==",[0]],["AQ==",[1]],["AR==",[1]],["AAAAAA==",[0,0,0,0]],["AAAA=","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["-_8=","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["+/8=",[251,255]],["AA==\n",[0]],["AA== =","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["A=A=","SyntaxError: Uint8Array.fromBase64: the string is not valid base64."],["SGVsbG8gV29ybGQ",[72,101,108,108,111,32,87,111,114,108,100]],["\tSGk=\n",[72,105]],["",[]],["00",[0]],["0f",[15]],["0F",[15]],["ff00AB",[255,0,171]],["f","SyntaxError: Uint8Array.fromHex: the string is not valid hex."],["fg","SyntaxError: Uint8Array.fromHex: the string is not valid hex."],[" 00","SyntaxError: Uint8Array.fromHex: the string is not valid hex."],["0x00","SyntaxError: Uint8Array.fromHex: the string is not valid hex."],"TypeError: Uint8Array.fromBase64 expects a string.","TypeError: Uint8Array.fromHex expects a string.",1,"fromHex","AAEC+vv8/f7/","","AQ==","AQI=","abcdef"],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://u:p@h.com:8080/a/b?c=d#e")
const p = new URLSearchParams("a=1&b=2")
const h = new Headers({ x: "1" })
const b = new Uint8Array([1, 2])
console.log(u, p, h, b)
console.log([u], { p }, new TextEncoder(), new TextDecoder(), crypto)
return { u, p, h, json: JSON.stringify({ u, p, h }), arr: [u, p, h], s: [u + "", p + "", h + "", `${u}`], keys: [Object.keys(u), Object.keys(p), Object.keys(h)], entries: Object.entries(u) }"####,
            r####"{"ok":true,"value":{"u":"https://u:p@h.com:8080/a/b?c=d#e","p":"a=1&b=2","h":{"x":"1"},"json":"{\"u\":\"https://u:p@h.com:8080/a/b?c=d#e\",\"p\":{},\"h\":{\"x\":\"1\"}}","arr":["https://u:p@h.com:8080/a/b?c=d#e","a=1&b=2",{"x":"1"}],"s":["https://u:p@h.com:8080/a/b?c=d#e","a=1&b=2","[object Headers]","https://u:p@h.com:8080/a/b?c=d#e"],"keys":[[],[],[]],"entries":[]},"logs":["https://u:p@h.com:8080/a/b?c=d#e a=1&b=2 Headers {\"x\":\"1\"} Uint8Array(2) [1,2]","[https://u:p@h.com:8080/a/b?c=d#e] {\"p\":a=1&b=2} {} {} {}"],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const b = new Uint8Array([3, 1, 2])
const seen = []
const r1 = b.sort((x, y) => { seen.push([x, y]); return y - x })
const r2 = [...new Uint8Array([5, 1, 4]).sort(() => NaN)]
const r3 = [...new Uint8Array([1, 2, 3]).sort((a, b) => b > a ? 1 : -1)]
const r4 = [...new Uint8Array([1, 2, 3]).sort((a, b) => 300)]
return [[...r1], r1 === b, seen, r2, r3, r4, m(() => new Uint8Array([1, 2]).sort("x")), m(() => new Uint8Array([1, 2]).sort((a, b) => { throw new Error("cmp") })), m(() => Uint8Array.prototype.sort.call([2, 1]))]"####,
            r####"{"ok":true,"value":[[3,2,1],true,[[1,2],[3,2]],[5,1,4],[3,2,1],[3,2,1],"TypeError: Uint8Array.sort expects a function callback.","Error: cmp","TypeError: Uint8Array.prototype.sort called on incompatible receiver an array."],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([10, 20, 30, 40, 50])
return [b.at(0), b.at(-1), b.at(5), b.at(-6), b.at(1.7), b.at("2"), b.at(), b.includes(30), b.includes(30, 3), b.includes(30, -3), b.includes(300), b.indexOf(40, -2), b.indexOf(10, -100), b.indexOf(10, Infinity), b.lastIndexOf(10, -5), b.lastIndexOf(10, -6), b.lastIndexOf(50, 100), b.join(), b.join(""), b.join(undefined), b.join(null), [...b.slice(-2)], [...b.slice(1, -1)], [...b.slice(3, 1)], [...b.slice()], b.subarray(-2).length, b.subarray(4, 1).length, [...b.keys()], [...b.entries()], [...b.fill(7, -2)], [...b.fill(1, 1, 2)], [...b.fill(2, 3, 1)], [...b.reverse()], b.length, b.toString(), b.fill(256) === b, b.join(), [...b.fill(-1, NaN, 1)], b.indexOf("255"), b.includes(), new Uint8Array([0]).includes(), new Uint8Array([0]).indexOf(-0), b.at(Infinity), b.at(-Infinity)]"####,
            r####"{"ok":true,"value":[10,50,null,null,20,30,10,true,false,true,false,3,0,-1,0,-1,4,"10,20,30,40,50","1020304050","10,20,30,40,50","10null20null30null40null50",[40,50],[20,30,40],[],[10,20,30,40,50],2,0,[0,1,2,3,4],[[0,10],[1,20],[2,30],[3,40],[4,50]],[10,20,30,7,7],[10,1,30,7,7],[10,1,30,7,7],[7,7,30,1,10],5,"7,7,30,1,10",true,"0,0,0,0,0",[255,0,0,0,0],0,false,false,0,null,null],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const j = (b) => b instanceof Uint8Array ? [...b] : b
return [Uint8Array.of(1, 256, -1, 1.9, "3", null, undefined, NaN, Infinity, -1.5, 255.9).join(), m(() => Uint8Array.of(Symbol())), m(() => Uint8Array.from()), m(() => Uint8Array.from(5)), m(() => j(Uint8Array.from("abc"))), j(Uint8Array.from("123")), m(() => Uint8Array.from({ length: 2 })), m(() => new Uint8Array({ length: 2, 0: 1 })), j(new Uint8Array(new Set([3, 4]))), new Uint8Array(null).length, new Uint8Array("3").length, m(() => new Uint8Array("x")), m(() => new Uint8Array(NaN)), m(() => new Uint8Array(Infinity)), m(() => new Uint8Array(-0).length), new Uint8Array(true).length, m(() => new Uint8Array(() => 1)), m(() => new Uint8Array(Symbol())), m(() => new Uint8Array(Promise.resolve([1]))), new Uint8Array([1, 2], 5).length, m(() => Uint8Array(1)), m(() => Uint8Array.prototype.at.call([1], 0)), m(() => Uint8Array.prototype.length), m(() => new Uint8Array([1]).set(1)), m(() => new Uint8Array([1]).set([1], -1)), m(() => new Uint8Array([1]).set([1], 1.5)), new Uint8Array([1, 2]).set([9], 1.5), m(() => new Uint8Array([1]).set([1, 2])), m(() => new Uint8Array([1]).set([1], Infinity)), m(() => j(Uint8Array.from([1, [2], "x", { valueOf() { return 3 } }]))), m(() => j(Uint8Array.from(new Map([[1, 2]])))), m(() => j(Uint8Array.from(new Uint8Array([4, 5]).values()))), m(() => j(Uint8Array.from([1].values(), (x) => x * 2))), m(() => Uint8Array.from(null)), m(() => j(Uint8Array.from(new Headers({ a: "1" }))))]"####,
            r####"{"ok":true,"value":["1,0,255,1,3,0,0,0,0,255,255","TypeError: Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.","TypeError: Uint8Array.from expects a Uint8Array, an array, or an iterable of numbers, received undefined.","TypeError: Uint8Array.from expects a Uint8Array, an array, or an iterable of numbers, received a number.",[0,0,0],[1,2,3],"TypeError: Uint8Array.from expects a Uint8Array, an array, or an iterable of numbers, received a data object.","TypeError: new Uint8Array(...) expects a Uint8Array, an array, or an iterable of numbers, received a data object.",[3,4],0,3,"RangeError: Invalid typed array length: x","RangeError: Invalid typed array length: NaN","RangeError: Invalid typed array length: Infinity",0,1,"TypeError: new Uint8Array(...) expects a Uint8Array, an array, or an iterable of numbers, received a function.","TypeError: Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.","TypeError: new Uint8Array(...) expects a Uint8Array, an array, or an iterable of numbers, received an un-awaited Promise.",2,"TypeError: Constructor Uint8Array requires 'new'.","TypeError: Uint8Array.prototype.at called on incompatible receiver an array.","TypeError: Uint8Array.prototype.length called on incompatible receiver a data object.","TypeError: Uint8Array.set expects a Uint8Array, an array, or an iterable of numbers, received a number.","RangeError: Uint8Array.set: the source does not fit at that offset.","RangeError: Uint8Array.set: the source does not fit at that offset.",null,"RangeError: Uint8Array.set: the source does not fit at that offset.","RangeError: Uint8Array.set: the source does not fit at that offset.",[1,2,0,0],[0],[4,5],[1],"TypeError: Uint8Array.from expects a Uint8Array, an array, or an iterable of numbers, received null.",[0]],"toolCalls":[]}"####,
        ),
        (
            r####"const out = [Uint8Array.fromBase64("AQID") instanceof Uint8Array]
const b = new Uint8Array([104, 105])
out.push(b.toBase64(), b.toHex(), [...new Uint8Array(3)], new Uint8Array(3).length)
const c = new Uint8Array([1, 2, 3])
c[1] = 258
c[10] = 1
c[-1] = 5
c.foo = "bar"
out.push([...c], c[10], c[-1], c.foo, Object.keys(c), 1 in c, 10 in c, c["1"], c[1.5], JSON.stringify(c), JSON.stringify({ c }))
return out"####,
            r####"{"ok":true,"value":[true,"aGk=","6869",[0,0,0],3,[1,2,3],null,5,"bar",["0","1","2","-1","foo"],true,false,2,null,"{\"0\":1,\"1\":2,\"2\":3,\"-1\":5,\"foo\":\"bar\"}","{\"c\":{\"0\":1,\"1\":2,\"2\":3,\"-1\":5,\"foo\":\"bar\"}}"],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3, 4, 5, 6])
const v = b.subarray(2, 5)
v[0] = 99
const v2 = v.subarray(1)
v2.set([7, 8])
b.set(new Uint8Array([200]), 5)
b.set(v.subarray(0, 1), 0)
const parts = [[...b], [...v], [...v2], v.length, v2.length, v.toHex(), v.toBase64(), v.at(-1), v.indexOf(8), v.includes(1), v.join("-"), v.slice(1).length]
v.reverse()
parts.push([...b])
v.sort()
parts.push([...b])
v.fill(0)
parts.push([...b], structuredClone(v).length, new Uint8Array(v).length, Uint8Array.from(v).length, [...new TextEncoder().encode("hi").subarray(1)])
const overlap = new Uint8Array([1, 2, 3, 4, 5])
overlap.set(overlap.subarray(0, 3), 2)
parts.push([...overlap])
const ov2 = new Uint8Array([1, 2, 3, 4, 5])
ov2.subarray(0, 4).set(ov2.subarray(1), 0)
parts.push([...ov2])
const it = b.values()
const first = it.next()
b[1] = 77
parts.push(first, it.next(), new TextDecoder().decode(new TextEncoder().encode("xyz").subarray(1, 2)))
return parts"####,
            r####"{"ok":true,"value":[[99,2,99,7,8,200],[99,7,8],[7,8],3,2,"630708","YwcI",8,2,false,"99-7-8",2,[99,2,8,7,99,200],[99,2,7,8,99,200],[99,2,0,0,0,200],3,3,3,[105],[1,2,1,2,3],[2,3,4,5,5],{"value":99,"done":false},{"value":77,"done":false},"y"],"toolCalls":[]}"####,
        ),
        (
            r####"const enc = new TextEncoder()
const dec = new TextDecoder()
const j = (b) => [...b]
const out = [enc.encoding, j(enc.encode()), j(enc.encode(undefined)), j(enc.encode(null)), j(enc.encode(12)), j(enc.encode("\uD800")), j(enc.encode("a\uDC00\uD800b")), j(enc.encode("😀é")), dec.encoding, dec.fatal, dec.ignoreBOM, dec.decode(), dec.decode(undefined)]
out.push(dec.decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x68, 0x69])), new TextDecoder("utf-8", { ignoreBOM: true }).decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x68])).length, dec.decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF])).length)
out.push(dec.decode(new Uint8Array([0xff, 0x41, 0xC3, 0x28, 0xE2, 0x82, 0xF0, 0x9F, 0x98, 0x80, 0xED, 0xA0, 0x80, 0xC0, 0xAF, 0xF4, 0x90, 0x80, 0x80])))
out.push(dec.decode(new Uint8Array([0xF0, 0x9F, 0x98])), dec.decode(new Uint8Array([0xEF, 0xBB])), dec.decode(new Uint8Array([0xF8, 0x88, 0x80, 0x80, 0x80])))
for (const label of [" UTF-8 ", "utf8", "UNICODE-1-1-UTF-8", "unicode11utf8", "x-unicode20utf8", "Unicode20UTF8", "\nutf-8\t"]) out.push(new TextDecoder(label).encoding)
const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
out.push(m(() => new TextDecoder("utf-16le")), m(() => new TextDecoder("  Latin1 ")), m(() => new TextDecoder(null)), m(() => new TextDecoder("")), m(() => new TextDecoder("utf-8 ")), m(() => new TextDecoder({})), m(() => dec.decode([1])), m(() => dec.decode(null)), m(() => dec.decode("x")), m(() => new TextDecoder("utf-8", { fatal: 1 }).decode(new Uint8Array([0xC3]))), new TextDecoder("utf-8", { fatal: 1 }).fatal, new TextDecoder(undefined, "x").fatal, new TextDecoder("utf8", { fatal: true }).decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x61])), new TextDecoder("utf8", { fatal: true, ignoreBOM: true }).ignoreBOM)
out.push(m(() => TextDecoder.prototype.decode.call({})), m(() => TextDecoder.prototype.encoding), m(() => TextDecoder.prototype.fatal), m(() => TextEncoder()), TextEncoder.prototype.encoding, m(() => TextEncoder.prototype.encode.call(5, "a").length), m(() => TextDecoder.prototype.decode.call(new TextEncoder(), new Uint8Array(1))))
out.push(new TextEncoder(), new TextDecoder(), JSON.stringify(new TextDecoder()), Object.keys(new TextDecoder("utf8", { fatal: true })), TextEncoder.prototype.encode.length, TextDecoder.prototype.decode.length, TextDecoder.name)
return out"####,
            r####"{"ok":true,"value":["utf-8",[],[],[],[49,50],[239,191,189],[97,239,191,189,239,191,189,98],[240,159,152,128,195,169],"utf-8",false,false,"","","hi",2,1,"�A�(�😀���������","�","�","�����","utf-8","utf-8","utf-8","utf-8","utf-8","utf-8","utf-8","RangeError: The \"utf-16le\" encoding is not supported; only UTF-8 is.","RangeError: The \"latin1\" encoding is not supported; only UTF-8 is.","RangeError: The \"null\" encoding is not supported; only UTF-8 is.","RangeError: The \"\" encoding is not supported; only UTF-8 is.",{},"RangeError: The \"[object object]\" encoding is not supported; only UTF-8 is.","TypeError: TextDecoder.decode expects a Uint8Array, received an array.","TypeError: TextDecoder.decode expects a Uint8Array, received null.","TypeError: TextDecoder.decode expects a Uint8Array, received a string.","TypeError: TextDecoder.decode: the input is not valid utf-8.",true,false,"a",true,"TypeError: TextDecoder.prototype.decode called on incompatible receiver a data object.","TypeError: TextDecoder.prototype.encoding called on incompatible receiver a data object.","TypeError: TextDecoder.prototype.fatal called on incompatible receiver a data object.","TypeError: Constructor TextEncoder requires 'new'.","utf-8",1,"TypeError: TextDecoder.prototype.decode called on incompatible receiver a data object.",{},{},"{}",[],0,0,"TextDecoder"],"toolCalls":[]}"####,
        ),
        (
            r####"const b = new Uint8Array([1, 2, 3, 4])
const log = []
const r = [[...b.map((x, i, a) => { log.push([x, i, a === b]); if (i === 0) a[3] = 9; return x + 255 })], [...b.filter((x) => x % 2)], b.find((x) => x > 2), b.findIndex((x) => x > 100), b.findLast((x) => x < 3), b.findLastIndex((x) => x < 3), b.some((x) => x === 9), b.every((x) => x > 0), b.reduce((a, x) => a + x, ""), b.reduceRight((a, x) => a + x, ""), b.reduceRight((a, x) => a + x)]
const thisArg = []
b.forEach(function () { thisArg.push(this.t) }, { t: "T" })
const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
r.push(log, thisArg, b.map(() => "7").join(), b.map(() => 1.9).join(), b.filter(() => true) !== b, m(() => b.map()), m(() => b.forEach(1)), m(() => b.reduce()), m(() => Uint8Array.prototype.map.call([1], (x) => x)), b.map.length, b.reduce.length, b.reduce.name)
return r"####,
            r####"{"ok":true,"value":[[0,1,2,8],[1,3,9],3,-1,2,1,true,true,"1239","9321",15,[[1,0,true],[2,1,true],[3,2,true],[9,3,true]],["T","T","T","T"],"7,7,7,7","1,1,1,1",true,"TypeError: Uint8Array.map expects a function callback.","TypeError: Uint8Array.forEach expects a function callback.","TypeError: Uint8Array.reduce expects a function callback.","TypeError: Uint8Array.prototype.map called on incompatible receiver an array.",1,1,"reduce"],"toolCalls":[]}"####,
        ),
        (
            r####"async function main() {
  const b = new Uint8Array([1, 2, 3])
  const out = []
  const mapped = b.map(async (x) => x * 2)
  out.push([...mapped])
  const found = await b.find(async (x) => x > 1)
  out.push(found)
  return out
}
const m = async (f) => { try { return await f() } catch (e) { return e.name + ": " + e.message } }
return [await m(main), await m(async () => [...new Uint8Array([2, 1]).sort(async () => 1)])]"####,
            r####"{"ok":true,"value":[[[0,0,0],1],[2,1]],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const p = new URLSearchParams("a=1")
return [m(() => URLSearchParams.prototype.size), m(() => URLSearchParams.prototype.toString.call(new URL("https://a/"))), m(() => Headers.prototype.get.call(p, "a")), m(() => URL.prototype.toJSON.call(p)), m(() => Uint8Array.prototype.toHex.call(p)), m(() => TextDecoder.prototype.decode.call(p)), Object.prototype.toString.call(new Headers()), Object.prototype.toString.call(new URL("https://a/")), Object.prototype.toString.call(new Uint8Array(1)), Object.prototype.toString.call(new TextDecoder()), Object.prototype.toString.call(new TextEncoder()), Object.prototype.toString.call(crypto), Object.prototype.toString.call(p), typeof crypto, Object.keys(crypto), crypto.randomUUID.length, crypto.getRandomValues.name, URL.name, URLSearchParams.name, Headers.name, Uint8Array.name, [URL, URLSearchParams, Headers, Uint8Array, TextEncoder, TextDecoder].map((C) => [C.length, Object.keys(C), Object.keys(C.prototype), typeof C.prototype.constructor, C.prototype.constructor === C]), [new URL("https://a/"), p, new Headers(), new Uint8Array(1), new TextEncoder(), new TextDecoder()].map((x) => x.constructor.name)]"####,
            r####"{"ok":true,"value":["TypeError: URLSearchParams.prototype.size called on incompatible receiver a data object.","TypeError: URLSearchParams.prototype.toString called on incompatible receiver a URL.","TypeError: Headers.prototype.get called on incompatible receiver a URLSearchParams.","TypeError: URL.prototype.toJSON called on incompatible receiver a URLSearchParams.","TypeError: Uint8Array.prototype.toHex called on incompatible receiver a URLSearchParams.","TypeError: TextDecoder.prototype.decode called on incompatible receiver a URLSearchParams.","[object Headers]","[object URL]","[object Uint8Array]","[object Object]","[object Object]","[object Object]","[object URLSearchParams]","object",[],0,"getRandomValues","URL","URLSearchParams","Headers","Uint8Array",[[1,[],[],"function",true],[0,[],[],"function",true],[0,[],[],"function",true],[3,[],[],"function",true],[0,[],[],"function",true],[0,[],[],"function",true]],["URL","URLSearchParams","Headers","Uint8Array","TextEncoder","TextDecoder"]],"toolCalls":[]}"####,
        ),
        (
            r####"const r = []
for (const [C, names] of [[URLSearchParams, ["append", "delete", "get", "getAll", "has", "set", "sort", "keys", "values", "entries", "toString", "forEach"]], [Headers, ["append", "delete", "get", "getSetCookie", "has", "set", "keys", "values", "entries", "forEach"]], [Uint8Array, ["at", "slice", "subarray", "set", "fill", "reverse", "sort", "indexOf", "lastIndexOf", "includes", "join", "toString", "toBase64", "toHex", "keys", "values", "entries", "map", "filter", "find", "findIndex", "findLast", "findLastIndex", "some", "every", "forEach", "reduce", "reduceRight"]]]) {
  r.push(names.map((n) => [n, typeof C.prototype[n], C.prototype[n] && C.prototype[n].length, C.prototype[n] && C.prototype[n].name]))
}
r.push([Uint8Array.from.length, Uint8Array.of.length, Uint8Array.fromBase64.length, Uint8Array.fromHex.length], [Headers.prototype[Symbol.iterator] === Headers.prototype.entries, URLSearchParams.prototype[Symbol.iterator] === URLSearchParams.prototype.entries, Uint8Array.prototype[Symbol.iterator] === Uint8Array.prototype.values])
r.push(["concat", "push", "copyWithin", "toReversed", "with", "flat", "toSorted", "buffer", "byteLength", "BYTES_PER_ELEMENT"].map((n) => typeof new Uint8Array(1)[n]))
return r"####,
            r####"{"ok":true,"value":[[["append","function",2,"append"],["delete","function",1,"delete"],["get","function",1,"get"],["getAll","function",1,"getAll"],["has","function",1,"has"],["set","function",2,"set"],["sort","function",0,"sort"],["keys","function",0,"keys"],["values","function",0,"values"],["entries","function",0,"entries"],["toString","function",0,"toString"],["forEach","function",1,"forEach"]],[["append","function",2,"append"],["delete","function",1,"delete"],["get","function",1,"get"],["getSetCookie","function",0,"getSetCookie"],["has","function",1,"has"],["set","function",2,"set"],["keys","function",0,"keys"],["values","function",0,"values"],["entries","function",0,"entries"],["forEach","function",1,"forEach"]],[["at","function",1,"at"],["slice","function",2,"slice"],["subarray","function",2,"subarray"],["set","function",1,"set"],["fill","function",1,"fill"],["reverse","function",0,"reverse"],["sort","function",1,"sort"],["indexOf","function",1,"indexOf"],["lastIndexOf","function",1,"lastIndexOf"],["includes","function",1,"includes"],["join","function",1,"join"],["toString","function",0,"toString"],["toBase64","function",0,"toBase64"],["toHex","function",0,"toHex"],["keys","function",0,"keys"],["values","function",0,"values"],["entries","function",0,"entries"],["map","function",1,"map"],["filter","function",1,"filter"],["find","function",1,"find"],["findIndex","function",1,"findIndex"],["findLast","function",1,"findLast"],["findLastIndex","function",1,"findLastIndex"],["some","function",1,"some"],["every","function",1,"every"],["forEach","function",1,"forEach"],["reduce","function",1,"reduce"],["reduceRight","function",1,"reduceRight"]],[1,0,1,1],[true,true,true],["undefined","undefined","undefined","undefined","undefined","undefined","undefined","undefined","undefined","undefined"]],"toolCalls":[]}"####,
        ),
        (
            r####"const u = new URL("https://u:p@h.com:8080/a/b?c=d#e")
const p = new URLSearchParams("a=1&b=2")
const h = new Headers({ x: "1" })
const b = new Uint8Array([1, 2])
console.log(u, p, h, b)
console.log([u], { p }, new TextEncoder(), new TextDecoder(), crypto, [b])
console.log("" + b, `${p}`)
return { u, p, h, json: JSON.stringify({ u, p, h, b: [...b] }), arr: [u, p, h], s: [u + "", p + "", h + "", `${u}`], keys: [Object.keys(u), Object.keys(p), Object.keys(h)], entries: Object.entries(u) }"####,
            r####"{"ok":true,"value":{"u":"https://u:p@h.com:8080/a/b?c=d#e","p":"a=1&b=2","h":{"x":"1"},"json":"{\"u\":\"https://u:p@h.com:8080/a/b?c=d#e\",\"p\":{},\"h\":{\"x\":\"1\"},\"b\":[1,2]}","arr":["https://u:p@h.com:8080/a/b?c=d#e","a=1&b=2",{"x":"1"}],"s":["https://u:p@h.com:8080/a/b?c=d#e","a=1&b=2","[object Headers]","https://u:p@h.com:8080/a/b?c=d#e"],"keys":[[],[],[]],"entries":[]},"logs":["https://u:p@h.com:8080/a/b?c=d#e a=1&b=2 Headers {\"x\":\"1\"} Uint8Array(2) [1,2]","[https://u:p@h.com:8080/a/b?c=d#e] {\"p\":a=1&b=2} {} {} {} [Uint8Array(2) [1,2]]","1,2 a=1&b=2"],"toolCalls":[]}"####,
        ),
        (
            r####"return new TextDecoder()"####,
            r####"{"ok":true,"value":{},"toolCalls":[]}"####,
        ),
        (
            r####"return { e: new TextEncoder(), d: new TextDecoder("utf8") }"####,
            r####"{"ok":true,"value":{"e":{},"d":{}},"toolCalls":[]}"####,
        ),
    ]);
}

/// `atob`/`btoa`, `crypto`, and `structuredClone`.
#[test]
fn base64_crypto_and_structured_clone() {
    check(&[
        (
            r####"crypto.getRandomValues([1])"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: crypto.getRandomValues expects a Uint8Array, received an array. (line 1, col 1)","location":{"line":1,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const results = []
for (const s of ["", "f", "fo", "foo", "foob", "fooba", "foobar", "\xff\xfe", "Ā", "😀", undefined, null, 7, 1.5, true, false, NaN, Infinity, -0, "\0a"]) {
  try { results.push(btoa(s)) } catch (e) { results.push(e.name + ": " + e.message) }
}
for (const s of ["", "Zg==", "Zg", "Zg=", "Zm9v", " Zm 9v\n", "Zm9v=", "Zm9vYg", "Zm9vYg=", "Zm9vYg==", "Zm9vYmFy", "=", "==", "Z", "ZZZZZ", "Zg===", "Z===", "!!!!", "Zm 9v", "Zm9v\f", "YQ", "YR", "YR==", "/+==", "ab=c", "ab==c", undefined, null, 12, true, NaN, Infinity]) {
  try { const r = atob(s); results.push([s, r, r.length]) } catch (e) { results.push([s, e.name + ": " + e.message]) }
}
let missing
try { atob() } catch (e) { missing = e.name + ": " + e.message }
let missing2
try { btoa() } catch (e) { missing2 = e.name + ": " + e.message }
return [results, missing, missing2, atob.name, btoa.length]"####,
            r####"{"ok":true,"value":[["","Zg==","Zm8=","Zm9v","Zm9vYg==","Zm9vYmE=","Zm9vYmFy","//4=","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","dW5kZWZpbmVk","bnVsbA==","Nw==","MS41","dHJ1ZQ==","ZmFsc2U=","TmFO","SW5maW5pdHk=","MA==","AGE=",["","",0],["Zg==","f",1],["Zg","f",1],["Zg=","TypeError: The string contains invalid characters."],["Zm9v","foo",3],[" Zm 9v\n","foo",3],["Zm9v=","TypeError: The string contains invalid characters."],["Zm9vYg","foob",4],["Zm9vYg=","TypeError: The string contains invalid characters."],["Zm9vYg==","foob",4],["Zm9vYmFy","foobar",6],["=","TypeError: The string contains invalid characters."],["==","TypeError: The string contains invalid characters."],["Z","TypeError: The string contains invalid characters."],["ZZZZZ","TypeError: The string contains invalid characters."],["Zg===","TypeError: The string contains invalid characters."],["Z===","TypeError: The string contains invalid characters."],["!!!!","TypeError: The string contains invalid characters."],["Zm 9v","TypeError: The string contains invalid characters."],["Zm9v\f","foo",3],["YQ","a",1],["YR","a",1],["YR==","a",1],["/+==","ÿ",1],["ab=c","TypeError: The string contains invalid characters."],["ab==c","TypeError: The string contains invalid characters."],[null,"TypeError: The string contains invalid characters."],[null,"ée",3],[12,"×",1],[true,"¶»",3],[null,"5£",2],[null,"\"wâ+r",6]],"TypeError: atob requires 1 argument (a string)","TypeError: btoa requires 1 argument (a string)","atob",1],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const uuid = crypto.randomUUID()
const b = new Uint8Array(70000)
return [uuid.length, uuid[8], uuid[14], uuid[13], uuid !== crypto.randomUUID(), m(() => crypto.getRandomValues()), m(() => crypto.getRandomValues(new Uint8Array(65536)).length), m(() => crypto.getRandomValues(b).length), m(() => crypto.getRandomValues({})), Object.keys(crypto), typeof crypto, crypto.randomUUID.length, crypto.randomUUID.name, crypto.getRandomValues(new Uint8Array(0)).length, crypto.getRandomValues(new Uint8Array(8).subarray(2)).length]"####,
            r####"{"ok":true,"value":[36,"-","4","-",true,"TypeError: crypto.getRandomValues expects a Uint8Array, received undefined.",65536,70000,"TypeError: crypto.getRandomValues expects a Uint8Array, received a data object.",[],"object",0,"randomUUID",0,6],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const shared = { s: 1 }
const src = { a: [1, , 3], m: new Map([[shared, shared]]), set: new Set([shared, 2]), b: new Uint8Array([1, 2]), e: new TypeError("bad", { cause: shared }), n: null, u: undefined, x: -0, big: "s" }
const c = structuredClone(src)
return [c !== src, c.a, 1 in c.a, c.a.length, c.m.size, [...c.m.keys()][0] === [...c.m.values()][0], [...c.set][0] === [...c.m.keys()][0], c.b instanceof Uint8Array, [...c.b], c.b !== src.b, c.e instanceof TypeError, c.e.message, c.e.cause === [...c.m.keys()][0], c.e.cause !== shared, Object.keys(c), Object.is(c.x, -0), "u" in c, m(() => structuredClone()), m(() => structuredClone(() => 1)), m(() => structuredClone({ f() {} })), m(() => structuredClone(Symbol("q"))), m(() => structuredClone([Symbol.iterator])), m(() => structuredClone(new WeakMap())), m(() => structuredClone(new WeakSet())), m(() => structuredClone(Promise.resolve(1))), m(() => structuredClone(new URL("https://a.b"))), m(() => structuredClone(new Headers())), m(() => structuredClone(new URLSearchParams("a=1"))), m(() => structuredClone([1].values())), structuredClone(5), structuredClone("s"), structuredClone(null), structuredClone(undefined), m(() => structuredClone(crypto)), m(() => structuredClone(new TextEncoder())), m(() => structuredClone(Math))]"####,
            r####"{"ok":true,"value":[true,[1,null,3],false,3,1,true,true,true,[1,2],true,true,"bad",true,true,["a","m","set","b","e","n","u","x","big"],true,true,"TypeError: structuredClone requires 1 argument","TypeError: DataCloneError: a function could not be cloned.","TypeError: DataCloneError: a function could not be cloned.","TypeError: Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.","TypeError: DataCloneError: a symbol could not be cloned.","TypeError: DataCloneError: a WeakMap could not be cloned.","TypeError: DataCloneError: a WeakSet could not be cloned.","TypeError: DataCloneError: an un-awaited Promise could not be cloned.","TypeError: DataCloneError: a URL could not be cloned.","TypeError: DataCloneError: a Headers could not be cloned.","TypeError: DataCloneError: a URLSearchParams could not be cloned.","TypeError: DataCloneError: an iterator could not be cloned.",5,"s",null,null,{},{},{}],"toolCalls":[]}"####,
        ),
        (
            r####"const src = { k: 1 }
src.self = src
src.list = [src, { back: src }]
const c = structuredClone(src)
const m = new Map()
m.set("me", m)
const cm = structuredClone(m)
const s = new Set()
s.add(s)
const cs = structuredClone(s)
return [c.self === c, c.list[0] === c, c.list[1].back === c, c !== src, cm.get("me") === cm, [...cs][0] === cs]"####,
            r####"{"ok":false,"error":{"kind":"InvalidDataValue","message":"TypeError: Object assignment result contains a circular value. (line 2, col 1)","location":{"line":2,"column":1}},"toolCalls":[]}"####,
        ),
        (
            r####"const e = new RangeError("r")
e.extra = 1
const c = structuredClone(e)
const custom = new Error("m")
custom.name = "CustomError"
const c2 = structuredClone(custom)
const noMsg = structuredClone(new Error())
const frozen = structuredClone(Object.freeze({ a: 1 }))
frozen.b = 2
const arr = [1, 2]
arr.extra = "x"
const ca = structuredClone(arr)
const sub = new Uint8Array([1, 2, 3, 4]).subarray(1, 3)
const cs = structuredClone(sub)
const getter = structuredClone({ get g() { return 5 } })
const types = [EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError, Error].map((T) => { const x = structuredClone(new T("q")); return [x.name, x instanceof T, x.message] })
const named = new Error("n")
named.name = "TypeError"
const cn = structuredClone(named)
const numName = new Error("n")
numName.name = 5
const cnn = structuredClone(numName)
const msgObj = new Error("x")
msgObj.message = 42
const cmo = structuredClone(msgObj)
return [c instanceof RangeError, c.name, c.message, c.extra, Object.keys(c), c2.name, c2.message, c2 instanceof Error, noMsg.message, "message" in noMsg, Object.hasOwn(noMsg, "message"), frozen, Object.isFrozen(frozen), ca, ca.extra, [...cs], cs.length, getter, typeof c.stack, types, cn instanceof TypeError, cn.name, cnn.name, cmo.message, typeof cmo.message, Object.hasOwn(structuredClone(new Error("x", { cause: undefined })), "cause"), structuredClone([1, , 3]).length]"####,
            r####"{"ok":false,"error":{"kind":"ExecutionFailure","message":"TypeError: Only init object properties are supported. (line 15, col 34)","location":{"line":15,"column":34}},"toolCalls":[]}"####,
        ),
    ]);
}

/// `Headers`.
#[test]
fn headers() {
    check(&[
        (
            r####"const headers = new Headers({ "Content-Type": "text/plain", "X-Count": 1, "X-Null": null })
headers.append("Accept", "text/html")
headers.append("accept", "application/json")
headers.set("x-count", "2")
headers.delete("x-null")
const copy = new Headers(headers)
copy.set("content-type", "text/html")
return { get: headers.get("content-type"), missing: headers.get("x-missing"), combined: headers.get("ACCEPT"), has: [headers.has("Accept"), headers.has("x-null")], count: headers.get("x-count"), copied: [headers.get("content-type"), copy.get("content-type")], pairs: [...new Headers([["b", "2"], ["A", "1"]])], keys: [...headers.keys()], values: [...headers.values()], entries: [...headers.entries()] }"####,
            r####"{"ok":true,"value":{"get":"text/plain","missing":null,"combined":"text/html, application/json","has":[true,false],"count":"2","copied":["text/plain","text/html"],"pairs":[["a","1"],["b","2"]],"keys":["accept","content-type","x-count"],"values":["text/html, application/json","text/plain","2"],"entries":[["accept","text/html, application/json"],["content-type","text/plain"],["x-count","2"]]},"toolCalls":[]}"####,
        ),
        (
            r####"const headers = new Headers({ b: "2", a: "1" })
headers.append("Set-Cookie", "x=1")
headers.append("set-cookie", "y=2")
const seen = []
headers.forEach((value, name, self) => { seen[seen.length] = name + "=" + value + ":" + (self === headers) })
const [first] = headers
function* pairs() { yield* headers }
return { seen, first, spread: [...headers], generator: [...pairs()].length, object: Object.fromEntries(headers), cookies: headers.getSetCookie() }"####,
            r####"{"ok":true,"value":{"seen":["a=1:true","b=2:true","set-cookie=x=1:true","set-cookie=y=2:true"],"first":["a","1"],"spread":[["a","1"],["b","2"],["set-cookie","x=1"],["set-cookie","y=2"]],"generator":4,"object":{"a":"1","b":"2","set-cookie":"y=2"},"cookies":["x=1","y=2"]},"toolCalls":[]}"####,
        ),
        (
            r####"const headers = new Headers({ "X-A": "1", b: "2" })
console.log(headers)
return { headers, json: JSON.stringify({ headers }), text: `${headers}`, type: typeof headers, is: headers instanceof Headers }"####,
            r####"{"ok":true,"value":{"headers":{"b":"2","x-a":"1"},"json":"{\"headers\":{\"b\":\"2\",\"x-a\":\"1\"}}","text":"[object Headers]","type":"object","is":true},"logs":["Headers {\"b\":\"2\",\"x-a\":\"1\"}"],"toolCalls":[]}"####,
        ),
        (
            r####"function message(run) { try { run(); return null } catch (error) { return error instanceof TypeError ? error.message : error } }
const headers = new Headers()
return [message(() => Headers()), message(() => new Headers(null)), message(() => new Headers(1)), message(() => new Headers("a=1")), message(() => new Headers(new Set(["ab"]))), message(() => new Headers(() => 1)), message(() => new Headers([["name"]])), message(() => new Headers([["a", "b", "c"]])), message(() => new Headers({ "bad name": "x" })), message(() => new Headers({ name: "bad\u0000value" })), message(() => headers.get("invalid\u0100")), message(() => headers.has({})), message(() => headers.set("a", "invalid\u0100")), message(() => headers.append("a")), message(() => headers.forEach()), message(() => headers.forEach(1)), message(() => { const get = headers.get; return get("a") }), message(() => new Headers(tools)), message(() => new Headers(Symbol.iterator)), message(() => headers.delete("a b")), message(() => headers.set("A-B", " x\ny ")), message(() => new Headers([["x", "a\u0100"]]))]"####,
            r####"{"ok":true,"value":["Constructor Headers requires 'new'.","new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.",null,"new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","new Headers(...) expects iterable [name, value] pairs.","new Headers(...) expects iterable [name, value] pairs.","Invalid header name: 'bad name'","Header 'name' has invalid value: 'bad\u0000value'","Invalid header name: 'invalidĀ'","Invalid header name: '[object Object]\"","Header 'a' has invalid value: 'invalidĀ'","Headers.append requires 2 arguments.","Headers.forEach requires 1 argument.","Headers.forEach expects a function callback.","Headers.prototype.get called on incompatible receiver undefined.","new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.","Invalid header name: 'a b'","Header 'A-B' has invalid value: 'x\ny'","Header 'x' has invalid value: 'aĀ'"],"toolCalls":[]}"####,
        ),
        (
            r####"const h = new Headers([["Set-Cookie","a=1"],["z","1"],["a","2"],["set-cookie","b=2"],["Cookie","c"],["cookie","d"], ["A","3"], ["x-a", "  \t v  "]])
h.set("e", "")
h.append("e", "")
h.append("e", "z")
return [[...h], h.get("set-cookie"), h.getSetCookie(), Object.fromEntries(h), h.get("cookie"), h.get("x-a"), h.get("e")]"####,
            r####"{"ok":true,"value":[[["a","2, 3"],["cookie","c; d"],["e",", , z"],["x-a","v"],["z","1"],["set-cookie","a=1"],["set-cookie","b=2"]],"a=1, b=2",["a=1","b=2"],{"a":"2, 3","cookie":"c; d","e":", , z","x-a":"v","z":"1","set-cookie":"b=2"},"c; d","v",", , z"],"toolCalls":[]}"####,
        ),
        (
            r####"function run(init, f) {
  const h = new Headers(init); const out = []; let i = 0
  for (const [k, v] of h) { out[out.length] = k + "=" + v; f(h, k, i++) ; if (i > 20) break }
  return out
}
return [
run([["set-cookie","1"],["set-cookie","2"],["a","x"]], (h,k,i) => { if (i==1) h.append("set-cookie","3") }),
run([["set-cookie","1"],["set-cookie","2"],["a","x"]], (h,k,i) => { if (i==1) h.append("b","3") }),
run([["set-cookie","1"],["set-cookie","2"],["a","x"]], (h,k,i) => { if (i==1) h.delete("a") }),
run([["set-cookie","1"],["set-cookie","2"],["a","x"]], (h,k,i) => { if (i==2) h.delete("set-cookie") }),
run([["set-cookie","1"],["set-cookie","2"],["set-cookie","3"],["a","x"]], (h,k,i) => { if (i==2) h.set("b", "y") }),
run([["b","1"],["c","2"],["d","3"]], (h,k,i) => { if (i==0) h.set("a", "9") }),
run({"foo": "2", "baz": "1", "BAR": "0", "quux": "3"}, (h,k) => { if (k === "baz") h.append("abc", "-1") }),
run({"foo": "2", "baz": "1", "BAR": "0"}, (h) => h.delete("foo")),
run([["set-cookie","1"],["set-cookie","2"]], (h,k,i) => { if (i==0) h.append("a", "9") }),
]"####,
            r####"{"ok":true,"value":[["a=x","set-cookie=1","set-cookie=2","set-cookie=3"],["a=x","set-cookie=1","set-cookie=1","set-cookie=2"],["a=x","set-cookie=1"],["a=x","set-cookie=1","set-cookie=2"],["a=x","set-cookie=1","set-cookie=2","set-cookie=2","set-cookie=3"],["b=1","b=1","c=2","d=3"],["bar=0","baz=1","baz=1","foo=2","quux=3"],["bar=0","baz=1"],["set-cookie=1","set-cookie=1","set-cookie=2"]],"toolCalls":[]}"####,
        ),
        (
            r####"const h2 = new Headers([["a","1"]]); const it = h2.keys(); const r = [it.next(), it.next()]; h2.append("b","2"); r[2] = it.next(); return r"####,
            r####"{"ok":true,"value":[{"value":"a","done":false},{"done":true},{"done":true}],"toolCalls":[]}"####,
        ),
        (
            r####"const failures = []
function test(run, name) { try { run() } catch (error) { failures.push(name + ": " + (error && error.message ? error.message : error)) } }
function assert_equals(actual, expected, message) { if (actual !== expected) throw new Error((message || "") + " expected " + JSON.stringify(expected) + " got " + JSON.stringify(actual)) }
function assert_true(actual, message) { assert_equals(actual, true, message) }
function assert_false(actual, message) { assert_equals(actual, false, message) }
function assert_array_equals(actual, expected, message) { assert_equals(JSON.stringify(actual), JSON.stringify(expected), message) }
function assert_throws_js(type, run) { try { run() } catch (error) { if (error instanceof type) return; throw new Error("threw " + error.name) } throw new Error("did not throw") }
test(function() { new Headers() }, "Create headers from no parameter")
test(function() { new Headers(undefined) }, "Create headers from undefined parameter")
test(function() { new Headers({}) }, "Create headers from empty object")
var parameters = [null, 1]
parameters.forEach(function(parameter) {
  test(function() { assert_throws_js(TypeError, function() { new Headers(parameter) }) }, "Create headers with " + parameter + " should throw")
})
var headerDict = {"name1": "value1", "name2": "value2", "name3": "value3", "name4": null, "name5": undefined, "name6": 1, "Content-Type": "value4"}
var headerSeq = []
for (var name in headerDict) headerSeq.push([name, headerDict[name]])
test(function() {
  var headers = new Headers(headerSeq)
  for (name in headerDict) assert_equals(headers.get(name), "" + headerDict[name], "name: " + name)
  assert_equals(headers.get("length"), null, "init should be treated as a sequence, not as a dictionary")
}, "Create headers with sequence")
test(function() {
  var headers = new Headers(headerDict)
  for (name in headerDict) assert_equals(headers.get(name), "" + headerDict[name], "name: " + name)
}, "Create headers with record")
test(function() {
  var headers = new Headers(headerDict)
  var headers2 = new Headers(headers)
  for (name in headerDict) assert_equals(headers2.get(name), "" + headerDict[name], "name: " + name)
}, "Create headers with existing headers")
test(function() {
  var headers = new Headers()
  for (name in headerDict) { headers.append(name, headerDict[name]); assert_equals(headers.get(name), "" + headerDict[name], "name: " + name) }
}, "Check append method")
test(function() {
  var headers = new Headers()
  for (name in headerDict) { headers.set(name, headerDict[name]); assert_equals(headers.get(name), "" + headerDict[name], "name: " + name) }
}, "Check set method")
test(function() {
  var headers = new Headers(headerDict)
  for (name in headerDict) assert_true(headers.has(name), "headers has name " + name)
  assert_false(headers.has("nameNotInHeaders"), "headers do not have header: nameNotInHeaders")
}, "Check has method")
test(function() {
  var headers = new Headers(headerDict)
  for (name in headerDict) { assert_true(headers.has(name)); headers.delete(name); assert_true(!headers.has(name)) }
}, "Check delete method")
var headerEntriesDict = {"name1": "value1", "Name2": "value2", "name": "value3", "content-Type": "value4", "Content-Typ": "value5", "Content-Types": "value6"}
var sortedHeaderKeys = ["content-typ", "content-type", "content-types", "name", "name1", "name2"]
var sortedHeaderDict = {"name1": "value1", "name2": "value2", "name": "value3", "content-type": "value4", "content-typ": "value5", "content-types": "value6"}
test(function() {
  var headers = new Headers(headerEntriesDict)
  var actual = headers.keys()
  sortedHeaderKeys.forEach(function(key) { const entry = actual.next(); assert_false(entry.done); assert_equals(entry.value, key) })
  assert_true(actual.next().done)
  assert_true(actual.next().done)
}, "Check keys method")
test(function() {
  var headers = new Headers(headerEntriesDict)
  var actual = headers.entries()
  sortedHeaderKeys.forEach(function(key) { const entry = actual.next(); assert_false(entry.done); assert_equals(entry.value[0], key); assert_equals(entry.value[1], sortedHeaderDict[key]) })
  assert_true(actual.next().done)
}, "Check entries method")
test(function() {
  var headers = new Headers(headerEntriesDict)
  var reference = sortedHeaderKeys[Symbol.iterator]()
  headers.forEach(function(value, key, container) { assert_equals(headers, container); const entry = reference.next(); assert_false(entry.done); assert_equals(key, entry.value); assert_equals(value, sortedHeaderDict[entry.value]) })
  assert_true(reference.next().done)
}, "Check forEach method")
test(() => {
  const headers = new Headers({"foo": "2", "baz": "1", "BAR": "0"})
  const actualKeys = [], actualValues = []
  for (const [header, value] of headers) { actualKeys.push(header); actualValues.push(value); headers.delete("foo") }
  assert_array_equals(actualKeys, ["bar", "baz"]); assert_array_equals(actualValues, ["0", "1"])
}, "Iteration skips elements removed while iterating")
test(() => {
  const headers = new Headers({"foo": "2", "baz": "1", "BAR": "0", "quux": "3"})
  const actualKeys = [], actualValues = []
  for (const [header, value] of headers) { actualKeys.push(header); actualValues.push(value); if (header === "baz") headers.delete("bar") }
  assert_array_equals(actualKeys, ["bar", "baz", "quux"]); assert_array_equals(actualValues, ["0", "1", "3"])
}, "Removing elements already iterated over")
test(() => {
  const headers = new Headers({"foo": "2", "baz": "1", "BAR": "0", "quux": "3"})
  const actualKeys = [], actualValues = []
  for (const [header, value] of headers) { actualKeys.push(header); actualValues.push(value); if (header === "baz") headers.append("X-yZ", "4") }
  assert_array_equals(actualKeys, ["bar", "baz", "foo", "quux", "x-yz"]); assert_array_equals(actualValues, ["0", "1", "2", "3", "4"])
}, "Appending during iteration")
test(() => {
  const headers = new Headers({"foo": "2", "baz": "1", "BAR": "0", "quux": "3"})
  const actualKeys = [], actualValues = []
  for (const [header, value] of headers) { actualKeys.push(header); actualValues.push(value); if (header === "baz") headers.append("abc", "-1") }
  assert_array_equals(actualKeys, ["bar", "baz", "baz", "foo", "quux"]); assert_array_equals(actualValues, ["0", "1", "1", "2", "3"])
}, "Prepending during iteration")
test(function() { assert_throws_js(TypeError, function() { new Headers([["name"]]) }) }, "one string")
test(function() { assert_throws_js(TypeError, function() { new Headers([["invalid", "invalidValue1", "invalidValue2"]]) }) }, "three strings")
test(function() { assert_throws_js(TypeError, function() { new Headers([["invalidĀ", "Value1"]]) }) }, "bad name")
test(function() { assert_throws_js(TypeError, function() { new Headers([["name", "invalidValueĀ"]]) }) }, "bad value")
var badNames = ["invalidĀ", {}]
badNames.forEach(function(name) {
  test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.get(name) }) }, "get invalid " + name)
  test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.delete(name) }) }, "delete invalid " + name)
  test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.has(name) }) }, "has invalid " + name)
  test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.set(name, "Value1") }) }, "set invalid " + name)
})
test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.set("name", "invalidĀ") }) }, "set invalid value")
test(function() { var headers = new Headers(); assert_throws_js(TypeError, function() { headers.append("name", "invalidĀ") }) }, "append invalid value")
test(function() {
  var headers = new Headers([["name", "value"]])
  assert_throws_js(TypeError, function() { headers.forEach() })
  assert_throws_js(TypeError, function() { headers.forEach(undefined) })
  assert_throws_js(TypeError, function() { headers.forEach(1) })
}, "forEach not callable")
test(function() {
  var headers = new Headers([["name1", "value1"], ["name2", "value2"], ["name3", "value3"]])
  var counter = 0
  try { headers.forEach(function(value, name) { counter++; if (name == "name2") throw "error" }) } catch (e) { assert_equals(counter, 2); assert_equals(e, "error"); return }
  throw new Error("unreachable")
}, "forEach stops on throw")
return failures"####,
            r####"{"ok":true,"value":[],"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
const h = new Headers()
const out = []
for (const v of ["  a  ", "\ta\t", "\na\r", " ", "a b", "aÿ", "\u0080", "a\rb", "a\u0000", "a\u000bb", "xĀ"]) out.push(m(() => { h.set("x", v); return h.get("x") }))
for (const n of ["", " a", "a:", "aé", "ok-Name_1!#$%&'*+.^`|~", "(", "a/b", "[x]", "a\"b", "{"]) out.push(m(() => { h.set(n, "v"); return h.get(n) }))
out.push(m(() => h.get("a b")), m(() => h.has("a b")), m(() => h.delete("a b")), m(() => h.has("")), m(() => h.get("")), m(() => h.append("", "x")), m(() => new Headers({ "": "x" })), m(() => new Headers([["", "x"]])), m(() => new Headers({ a: "\n" }).get("a")))
return out"####,
            r####"{"ok":true,"value":["a","a","a","","a b","aÿ","","TypeError: Header 'x' has invalid value: 'a\rb'","TypeError: Header 'x' has invalid value: 'a\u0000'","a\u000bb","TypeError: Header 'x' has invalid value: 'xĀ'","TypeError: Invalid header name: ''","TypeError: Invalid header name: ' a'","TypeError: Invalid header name: 'a:'","TypeError: Invalid header name: 'aé'","v","TypeError: Invalid header name: '('","TypeError: Invalid header name: 'a/b'","TypeError: Invalid header name: '[x]'","TypeError: Invalid header name: 'a\"b'","TypeError: Invalid header name: '{'","TypeError: Invalid header name: 'a b'","TypeError: Invalid header name: 'a b\"","TypeError: Invalid header name: 'a b'","TypeError: Invalid header name: '\"","TypeError: Invalid header name: ''","TypeError: Invalid header name: ''","TypeError: Invalid header name: ''","TypeError: Invalid header name: ''",""],"toolCalls":[]}"####,
        ),
        (
            r####"const h = new Headers()
h.append("Set-Cookie", "a=1")
h.append("set-cookie", "b=2")
h.append("Cookie", "c=1")
h.append("cookie", "d=2")
h.append("Accept", "x")
h.append("accept", "y")
h.append("Accept", "")
h.append("x-empty", "")
const out = [h.get("set-cookie"), h.getSetCookie(), h.get("cookie"), h.get("accept"), h.get("x-empty"), h.has("x-empty"), [...h], Object.fromEntries(h)]
h.set("set-cookie", "z=9")
out.push(h.getSetCookie(), [...h.keys()])
h.delete("Set-Cookie")
out.push(h.getSetCookie(), h.has("set-cookie"), h.get("set-cookie"), new Headers().getSetCookie(), [...new Headers([["set-cookie", "q"], ["a", "1"], ["set-cookie", "r"], ["zz", "2"]])])
const it = h.entries()
out.push(Object.prototype.toString.call(it), it[Symbol.iterator]() === it, typeof it.map, h[Symbol.iterator] === h.entries, JSON.stringify(h), h + "", Object.keys(h))
return out"####,
            r####"{"ok":true,"value":["a=1, b=2",["a=1","b=2"],"c=1; d=2","x, y, ","",true,[["accept","x, y, "],["cookie","c=1; d=2"],["x-empty",""],["set-cookie","a=1"],["set-cookie","b=2"]],{"accept":"x, y, ","cookie":"c=1; d=2","x-empty":"","set-cookie":"b=2"},["z=9"],["accept","cookie","x-empty","set-cookie"],[],false,null,[],[["a","1"],["zz","2"],["set-cookie","q"],["set-cookie","r"]],"[object Iterator]",true,"function",true,"{\"accept\":\"x, y, \",\"cookie\":\"c=1; d=2\",\"x-empty\":\"\"}","[object Headers]",[]],"toolCalls":[]}"####,
        ),
        (
            r####"const h = new Headers({ b: "1", a: "2" })
const it = h.values()
const seen = [it.next().value]
h.set("aa", "3")
seen.push(it.next().value, it.next().value, it.next().done)
h.append("zzz", "4")
seen.push(it.next().done)
const k = [...h.keys()]
const fe = []
h.forEach(function (value, key, target) { fe.push([value, key, target === h, this === undefined ? "u" : this.x]); if (key === "a") h.append("ab", "late") }, { x: 7 })
const iter = h.keys().map((x) => x.toUpperCase ? x : x)
return [seen, k, fe, [...h.keys()], Array.from(h.entries()).length, iter.toArray ? "iter" : "no"]"####,
            r####"{"ok":true,"value":[["2","3","1",true,true],["a","aa","b","zzz"],[["2","a",true,7],["3","aa",true,7],["1","b",true,7],["4","zzz",true,7]],["a","aa","ab","b","zzz"],5,"iter"],"toolCalls":[]}"####,
        ),
        (
            r####"return new Headers([["set-cookie", "a=1"], ["Set-Cookie", "b=2"], ["Z", "z"]])"####,
            r####"{"ok":true,"value":{"z":"z","set-cookie":"b=2"},"toolCalls":[]}"####,
        ),
        (
            r####"const m = (f) => { try { return f() } catch (e) { return e.name + ": " + e.message } }
let calls = 0
const iterable = { [Symbol.iterator]() { let i = 0; return { next() { calls++; i++; return i <= 2 ? { value: ["k" + i, "v" + i], done: false } : { done: true } } } } }
const p = new URLSearchParams(iterable)
const bad = { [Symbol.iterator]() { return { next() { return { value: 1, done: false } }, return() { calls += 100; return {} } } } }
const r = [p.toString(), calls, m(() => new URLSearchParams(bad)), calls]
const closes = []
const inner = { [Symbol.iterator]() { let i = 0; return { next() { i++; return i === 1 ? { value: "a", done: false } : (() => { throw new Error("inner boom") })() }, return() { closes.push("inner") } } } }
const outer = { [Symbol.iterator]() { let i = 0; return { next() { i++; return i === 1 ? { value: inner, done: false } : { done: true } }, return() { closes.push("outer"); return {} } } } }
r.push(m(() => new URLSearchParams(outer)), closes)
const h = new Headers(iterable)
r.push([...h], m(() => new Headers(bad)), m(() => new Headers([["a", "1"], ["b"]])), m(() => new Headers([["a", "1"], 5])), m(() => new Headers([["a b", "1"], ["c"]])), m(() => new URLSearchParams([["a", "1"], ["b"], 5])))
return r"####,
            r####"{"ok":true,"value":["k1=v1&k2=v2",3,"TypeError: new URLSearchParams(...) expects iterable [name, value] pairs.",103,"Error: inner boom",["outer"],[["k1","v1"],["k2","v2"]],"TypeError: new Headers(...) expects iterable [name, value] pairs.","TypeError: new Headers(...) expects iterable [name, value] pairs.","TypeError: new Headers(...) expects iterable [name, value] pairs.","TypeError: new Headers(...) expects iterable [name, value] pairs.","TypeError: new URLSearchParams(...) expects iterable [name, value] pairs."],"toolCalls":[]}"####,
        ),
    ]);
}

/// URL parsing, serialization, and setters over input matrices (Bun's WebKit URL).
#[test]
fn url_parsing_and_setters_match_webkit() {
    check(&[
        (
            r####"// host setter: ports after the last colon, default ports, out-of-range ports (WebKit)
const t = (href, f) => { const u = new URL(href); try { f(u) } catch (e) { return e.message } return u.href }
const cases = ["x:abc", "x:-1", "x:65535", "x:65536", "x:99999", "x:443", "x:0443", "x:00", "x:0", "x:1a", "x:443a", "x:80", "[::1]:443", "[::1]:99999", "x:1:2", "x:443/p", "x:99999/p", "x:443?q", ":443", "x:+1", "x: 1", "x:\t1", "x:4\t43"]
const out = []
for (const base of ["https://a:8080/p?q#h", "https://a/p", "http://a:8080/", "foo://a:8080/"]) out.push(base, cases.map(c => [c, t(base, u => u.host = c)]))
return out"####,
            r####"{"ok":true,"value":["https://a:8080/p?q#h",[["x:abc","https://x:8080/p?q#h"],["x:-1","https://x:8080/p?q#h"],["x:65535","https://x:65535/p?q#h"],["x:65536","https://x/p?q#h"],["x:99999","https://x/p?q#h"],["x:443","https://x:8080/p?q#h"],["x:0443","https://x:8080/p?q#h"],["x:00","https://x:0/p?q#h"],["x:0","https://x:0/p?q#h"],["x:1a","https://x:1/p?q#h"],["x:443a","https://x:8080/p?q#h"],["x:80","https://x:80/p?q#h"],["[::1]:443","https://[::1]:8080/p?q#h"],["[::1]:99999","https://[::1]/p?q#h"],["x:1:2","https://a:8080/p?q#h"],["x:443/p","https://x:8080/p?q#h"],["x:99999/p","https://x/p?q#h"],["x:443?q","https://x:8080/p?q#h"],[":443","https://a:8080/p?q#h"],["x:+1","https://x:8080/p?q#h"],["x: 1","https://x:8080/p?q#h"],["x:\t1","https://x:8080/p?q#h"],["x:4\t43","https://x:4/p?q#h"]],"https://a/p",[["x:abc","https://x/p"],["x:-1","https://x/p"],["x:65535","https://x:65535/p"],["x:65536","https://x/p"],["x:99999","https://x/p"],["x:443","https://x/p"],["x:0443","https://x/p"],["x:00","https://x:0/p"],["x:0","https://x:0/p"],["x:1a","https://x:1/p"],["x:443a","https://x/p"],["x:80","https://x:80/p"],["[::1]:443","https://[::1]/p"],["[::1]:99999","https://[::1]/p"],["x:1:2","https://a/p"],["x:443/p","https://x/p"],["x:99999/p","https://x/p"],["x:443?q","https://x/p"],[":443","https://a/p"],["x:+1","https://x/p"],["x: 1","https://x/p"],["x:\t1","https://x/p"],["x:4\t43","https://x:4/p"]],"http://a:8080/",[["x:abc","http://x:8080/"],["x:-1","http://x:8080/"],["x:65535","http://x:65535/"],["x:65536","http://x/"],["x:99999","http://x/"],["x:443","http://x:443/"],["x:0443","http://x:443/"],["x:00","http://x:0/"],["x:0","http://x:0/"],["x:1a","http://x:1/"],["x:443a","http://x:443/"],["x:80","http://x:8080/"],["[::1]:443","http://[::1]:443/"],["[::1]:99999","http://[::1]/"],["x:1:2","http://a:8080/"],["x:443/p","http://x:443/"],["x:99999/p","http://x/"],["x:443?q","http://x:443/"],[":443","http://a:8080/"],["x:+1","http://x:8080/"],["x: 1","http://x:8080/"],["x:\t1","http://x:8080/"],["x:4\t43","http://x:4/"]],"foo://a:8080/",[["x:abc","foo://x:8080/"],["x:-1","foo://x:8080/"],["x:65535","foo://x:65535/"],["x:65536","foo://x/"],["x:99999","foo://x/"],["x:443","foo://x:443/"],["x:0443","foo://x:443/"],["x:00","foo://x:0/"],["x:0","foo://x:0/"],["x:1a","foo://x:1/"],["x:443a","foo://x:443/"],["x:80","foo://x:80/"],["[::1]:443","foo://[::1]:443/"],["[::1]:99999","foo://[::1]/"],["x:1:2","foo://a:8080/"],["x:443/p","foo://x:443/"],["x:99999/p","foo://x/"],["x:443?q","foo://x:443/"],[":443","foo://a:8080/"],["x:+1","foo://x:8080/"],["x: 1","foo://x:8080/"],["x:\t1","foo://x:8080/"],["x:4\t43","foo://x:4/"]]],"toolCalls":[]}"####,
        ),
        (
            r####"// every setter against odd values on special, file, non-special, and opaque URLs (0 = unchanged)
const t = (href, p, v) => { const u = new URL(href); try { u[p] = v } catch (e) { return "ERR " + e.message } return u.href === href ? 0 : u.href }
const values = ["", "\t", "a\tb", " a ", "x y", "%41", "ü", "a/b", "a\\b", "a?b", "a#b", "a@b", "a:b", "[::1]", "0x7f.1", "A.B", "xn--a", "ß", "C:", "/C:/x", "./a/../b", "443", "65536", "http:", "file", "foo", "ws"]
const props = ["protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"]
const bases = ["https://u:p@a.com:8080/p/q?x=1#h", "http://a/", "file:///C:/dir/f", "file://srv/share", "foo://h:1/p", "foo:opaque?q#h", "mailto:x@y", "data:text/plain,hi", "ws://h/", "blob:https://a.com/u"]
const out = []
for (const b of bases) { const normalized = new URL(b).href; for (const p of props) out.push([b, p, values.map((v) => t(normalized, p, v))]) }
return out"####,
            r####"{"ok":true,"value":[["https://u:p@a.com:8080/p/q?x=1#h","protocol",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,"http://u:p@a.com:8080/p/q?x=1#h",0,0,"ws://u:p@a.com:8080/p/q?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","username",["https://:p@a.com:8080/p/q?x=1#h","https://%09:p@a.com:8080/p/q?x=1#h","https://a%09b:p@a.com:8080/p/q?x=1#h","https://%20a%20:p@a.com:8080/p/q?x=1#h","https://x%20y:p@a.com:8080/p/q?x=1#h","https://%41:p@a.com:8080/p/q?x=1#h","https://%C3%BC:p@a.com:8080/p/q?x=1#h","https://a%2Fb:p@a.com:8080/p/q?x=1#h","https://a%5Cb:p@a.com:8080/p/q?x=1#h","https://a%3Fb:p@a.com:8080/p/q?x=1#h","https://a%23b:p@a.com:8080/p/q?x=1#h","https://a%40b:p@a.com:8080/p/q?x=1#h","https://a%3Ab:p@a.com:8080/p/q?x=1#h","https://%5B%3A%3A1%5D:p@a.com:8080/p/q?x=1#h","https://0x7f.1:p@a.com:8080/p/q?x=1#h","https://A.B:p@a.com:8080/p/q?x=1#h","https://xn--a:p@a.com:8080/p/q?x=1#h","https://%C3%9F:p@a.com:8080/p/q?x=1#h","https://C%3A:p@a.com:8080/p/q?x=1#h","https://%2FC%3A%2Fx:p@a.com:8080/p/q?x=1#h","https://.%2Fa%2F..%2Fb:p@a.com:8080/p/q?x=1#h","https://443:p@a.com:8080/p/q?x=1#h","https://65536:p@a.com:8080/p/q?x=1#h","https://http%3A:p@a.com:8080/p/q?x=1#h","https://file:p@a.com:8080/p/q?x=1#h","https://foo:p@a.com:8080/p/q?x=1#h","https://ws:p@a.com:8080/p/q?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","password",["https://u@a.com:8080/p/q?x=1#h","https://u:%09@a.com:8080/p/q?x=1#h","https://u:a%09b@a.com:8080/p/q?x=1#h","https://u:%20a%20@a.com:8080/p/q?x=1#h","https://u:x%20y@a.com:8080/p/q?x=1#h","https://u:%41@a.com:8080/p/q?x=1#h","https://u:%C3%BC@a.com:8080/p/q?x=1#h","https://u:a%2Fb@a.com:8080/p/q?x=1#h","https://u:a%5Cb@a.com:8080/p/q?x=1#h","https://u:a%3Fb@a.com:8080/p/q?x=1#h","https://u:a%23b@a.com:8080/p/q?x=1#h","https://u:a%40b@a.com:8080/p/q?x=1#h","https://u:a%3Ab@a.com:8080/p/q?x=1#h","https://u:%5B%3A%3A1%5D@a.com:8080/p/q?x=1#h","https://u:0x7f.1@a.com:8080/p/q?x=1#h","https://u:A.B@a.com:8080/p/q?x=1#h","https://u:xn--a@a.com:8080/p/q?x=1#h","https://u:%C3%9F@a.com:8080/p/q?x=1#h","https://u:C%3A@a.com:8080/p/q?x=1#h","https://u:%2FC%3A%2Fx@a.com:8080/p/q?x=1#h","https://u:.%2Fa%2F..%2Fb@a.com:8080/p/q?x=1#h","https://u:443@a.com:8080/p/q?x=1#h","https://u:65536@a.com:8080/p/q?x=1#h","https://u:http%3A@a.com:8080/p/q?x=1#h","https://u:file@a.com:8080/p/q?x=1#h","https://u:foo@a.com:8080/p/q?x=1#h","https://u:ws@a.com:8080/p/q?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","host",[0,0,"https://u:p@ab:8080/p/q?x=1#h",0,0,"https://u:p@a:8080/p/q?x=1#h","https://u:p@xn--tda:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h",0,"https://u:p@a:8080/p/q?x=1#h","https://u:p@[::1]:8080/p/q?x=1#h","https://u:p@127.0.0.1:8080/p/q?x=1#h","https://u:p@a.b:8080/p/q?x=1#h",0,"https://u:p@xn--zca:8080/p/q?x=1#h","https://u:p@c:8080/p/q?x=1#h",0,"https://u:p@.:8080/p/q?x=1#h","https://u:p@0.0.1.187:8080/p/q?x=1#h","https://u:p@0.1.0.0:8080/p/q?x=1#h","https://u:p@http:8080/p/q?x=1#h","https://u:p@file:8080/p/q?x=1#h","https://u:p@foo:8080/p/q?x=1#h","https://u:p@ws:8080/p/q?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","hostname",[0,0,"https://u:p@ab:8080/p/q?x=1#h",0,0,"https://u:p@a:8080/p/q?x=1#h","https://u:p@xn--tda:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h","https://u:p@a:8080/p/q?x=1#h",0,0,"https://u:p@[::1]:8080/p/q?x=1#h","https://u:p@127.0.0.1:8080/p/q?x=1#h","https://u:p@a.b:8080/p/q?x=1#h",0,"https://u:p@xn--zca:8080/p/q?x=1#h",0,0,"https://u:p@.:8080/p/q?x=1#h","https://u:p@0.0.1.187:8080/p/q?x=1#h","https://u:p@0.1.0.0:8080/p/q?x=1#h",0,"https://u:p@file:8080/p/q?x=1#h","https://u:p@foo:8080/p/q?x=1#h","https://u:p@ws:8080/p/q?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","port",["https://u:p@a.com/p/q?x=1#h","https://u:p@a.com/p/q?x=1#h",0,0,0,0,0,0,0,0,0,0,0,0,"https://u:p@a.com:0/p/q?x=1#h",0,0,0,0,0,0,"https://u:p@a.com/p/q?x=1#h",0,0,0,0,0]],["https://u:p@a.com:8080/p/q?x=1#h","pathname",["https://u:p@a.com:8080/?x=1#h","https://u:p@a.com:8080/?x=1#h","https://u:p@a.com:8080/ab?x=1#h","https://u:p@a.com:8080/%20a%20?x=1#h","https://u:p@a.com:8080/x%20y?x=1#h","https://u:p@a.com:8080/%41?x=1#h","https://u:p@a.com:8080/%C3%BC?x=1#h","https://u:p@a.com:8080/a/b?x=1#h","https://u:p@a.com:8080/a/b?x=1#h","https://u:p@a.com:8080/a%3Fb?x=1#h","https://u:p@a.com:8080/a%23b?x=1#h","https://u:p@a.com:8080/a@b?x=1#h","https://u:p@a.com:8080/a:b?x=1#h","https://u:p@a.com:8080/[::1]?x=1#h","https://u:p@a.com:8080/0x7f.1?x=1#h","https://u:p@a.com:8080/A.B?x=1#h","https://u:p@a.com:8080/xn--a?x=1#h","https://u:p@a.com:8080/%C3%9F?x=1#h","https://u:p@a.com:8080/C:?x=1#h","https://u:p@a.com:8080/C:/x?x=1#h","https://u:p@a.com:8080/b?x=1#h","https://u:p@a.com:8080/443?x=1#h","https://u:p@a.com:8080/65536?x=1#h","https://u:p@a.com:8080/http:?x=1#h","https://u:p@a.com:8080/file?x=1#h","https://u:p@a.com:8080/foo?x=1#h","https://u:p@a.com:8080/ws?x=1#h"]],["https://u:p@a.com:8080/p/q?x=1#h","search",["https://u:p@a.com:8080/p/q#h","https://u:p@a.com:8080/p/q?#h","https://u:p@a.com:8080/p/q?ab#h","https://u:p@a.com:8080/p/q?%20a%20#h","https://u:p@a.com:8080/p/q?x%20y#h","https://u:p@a.com:8080/p/q?%41#h","https://u:p@a.com:8080/p/q?%C3%BC#h","https://u:p@a.com:8080/p/q?a/b#h","https://u:p@a.com:8080/p/q?a\\b#h","https://u:p@a.com:8080/p/q?a?b#h","https://u:p@a.com:8080/p/q?a%23b#h","https://u:p@a.com:8080/p/q?a@b#h","https://u:p@a.com:8080/p/q?a:b#h","https://u:p@a.com:8080/p/q?[::1]#h","https://u:p@a.com:8080/p/q?0x7f.1#h","https://u:p@a.com:8080/p/q?A.B#h","https://u:p@a.com:8080/p/q?xn--a#h","https://u:p@a.com:8080/p/q?%C3%9F#h","https://u:p@a.com:8080/p/q?C:#h","https://u:p@a.com:8080/p/q?/C:/x#h","https://u:p@a.com:8080/p/q?./a/../b#h","https://u:p@a.com:8080/p/q?443#h","https://u:p@a.com:8080/p/q?65536#h","https://u:p@a.com:8080/p/q?http:#h","https://u:p@a.com:8080/p/q?file#h","https://u:p@a.com:8080/p/q?foo#h","https://u:p@a.com:8080/p/q?ws#h"]],["https://u:p@a.com:8080/p/q?x=1#h","hash",["https://u:p@a.com:8080/p/q?x=1","https://u:p@a.com:8080/p/q?x=1#","https://u:p@a.com:8080/p/q?x=1#ab","https://u:p@a.com:8080/p/q?x=1#%20a%20","https://u:p@a.com:8080/p/q?x=1#x%20y","https://u:p@a.com:8080/p/q?x=1#%41","https://u:p@a.com:8080/p/q?x=1#%C3%BC","https://u:p@a.com:8080/p/q?x=1#a/b","https://u:p@a.com:8080/p/q?x=1#a\\b","https://u:p@a.com:8080/p/q?x=1#a?b","https://u:p@a.com:8080/p/q?x=1#a#b","https://u:p@a.com:8080/p/q?x=1#a@b","https://u:p@a.com:8080/p/q?x=1#a:b","https://u:p@a.com:8080/p/q?x=1#[::1]","https://u:p@a.com:8080/p/q?x=1#0x7f.1","https://u:p@a.com:8080/p/q?x=1#A.B","https://u:p@a.com:8080/p/q?x=1#xn--a","https://u:p@a.com:8080/p/q?x=1#%C3%9F","https://u:p@a.com:8080/p/q?x=1#C:","https://u:p@a.com:8080/p/q?x=1#/C:/x","https://u:p@a.com:8080/p/q?x=1#./a/../b","https://u:p@a.com:8080/p/q?x=1#443","https://u:p@a.com:8080/p/q?x=1#65536","https://u:p@a.com:8080/p/q?x=1#http:","https://u:p@a.com:8080/p/q?x=1#file","https://u:p@a.com:8080/p/q?x=1#foo","https://u:p@a.com:8080/p/q?x=1#ws"]],["http://a/","protocol",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,"file://a/",0,"ws://a/"]],["http://a/","username",[0,"http://%09@a/","http://a%09b@a/","http://%20a%20@a/","http://x%20y@a/","http://%41@a/","http://%C3%BC@a/","http://a%2Fb@a/","http://a%5Cb@a/","http://a%3Fb@a/","http://a%23b@a/","http://a%40b@a/","http://a%3Ab@a/","http://%5B%3A%3A1%5D@a/","http://0x7f.1@a/","http://A.B@a/","http://xn--a@a/","http://%C3%9F@a/","http://C%3A@a/","http://%2FC%3A%2Fx@a/","http://.%2Fa%2F..%2Fb@a/","http://443@a/","http://65536@a/","http://http%3A@a/","http://file@a/","http://foo@a/","http://ws@a/"]],["http://a/","password",[0,"http://:%09@a/","http://:a%09b@a/","http://:%20a%20@a/","http://:x%20y@a/","http://:%41@a/","http://:%C3%BC@a/","http://:a%2Fb@a/","http://:a%5Cb@a/","http://:a%3Fb@a/","http://:a%23b@a/","http://:a%40b@a/","http://:a%3Ab@a/","http://:%5B%3A%3A1%5D@a/","http://:0x7f.1@a/","http://:A.B@a/","http://:xn--a@a/","http://:%C3%9F@a/","http://:C%3A@a/","http://:%2FC%3A%2Fx@a/","http://:.%2Fa%2F..%2Fb@a/","http://:443@a/","http://:65536@a/","http://:http%3A@a/","http://:file@a/","http://:foo@a/","http://:ws@a/"]],["http://a/","host",[0,0,"http://ab/",0,0,0,"http://xn--tda/",0,0,0,0,0,0,"http://[::1]/","http://127.0.0.1/","http://a.b/",0,"http://xn--zca/","http://c/",0,"http://./","http://0.0.1.187/","http://0.1.0.0/","http://http/","http://file/","http://foo/","http://ws/"]],["http://a/","hostname",[0,0,"http://ab/",0,0,0,"http://xn--tda/",0,0,0,0,0,0,"http://[::1]/","http://127.0.0.1/","http://a.b/",0,"http://xn--zca/",0,0,"http://./","http://0.0.1.187/","http://0.1.0.0/",0,"http://file/","http://foo/","http://ws/"]],["http://a/","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,"http://a:0/",0,0,0,0,0,0,"http://a:443/",0,0,0,0,0]],["http://a/","pathname",[0,0,"http://a/ab","http://a/%20a%20","http://a/x%20y","http://a/%41","http://a/%C3%BC","http://a/a/b","http://a/a/b","http://a/a%3Fb","http://a/a%23b","http://a/a@b","http://a/a:b","http://a/[::1]","http://a/0x7f.1","http://a/A.B","http://a/xn--a","http://a/%C3%9F","http://a/C:","http://a/C:/x","http://a/b","http://a/443","http://a/65536","http://a/http:","http://a/file","http://a/foo","http://a/ws"]],["http://a/","search",[0,"http://a/?","http://a/?ab","http://a/?%20a%20","http://a/?x%20y","http://a/?%41","http://a/?%C3%BC","http://a/?a/b","http://a/?a\\b","http://a/?a?b","http://a/?a%23b","http://a/?a@b","http://a/?a:b","http://a/?[::1]","http://a/?0x7f.1","http://a/?A.B","http://a/?xn--a","http://a/?%C3%9F","http://a/?C:","http://a/?/C:/x","http://a/?./a/../b","http://a/?443","http://a/?65536","http://a/?http:","http://a/?file","http://a/?foo","http://a/?ws"]],["http://a/","hash",[0,"http://a/#","http://a/#ab","http://a/#%20a%20","http://a/#x%20y","http://a/#%41","http://a/#%C3%BC","http://a/#a/b","http://a/#a\\b","http://a/#a?b","http://a/#a#b","http://a/#a@b","http://a/#a:b","http://a/#[::1]","http://a/#0x7f.1","http://a/#A.B","http://a/#xn--a","http://a/#%C3%9F","http://a/#C:","http://a/#/C:/x","http://a/#./a/../b","http://a/#443","http://a/#65536","http://a/#http:","http://a/#file","http://a/#foo","http://a/#ws"]],["file:///C:/dir/f","protocol",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file:///C:/dir/f","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file:///C:/dir/f","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file:///C:/dir/f","host",[0,0,"file://ab/C:/dir/f",0,0,"file://a/C:/dir/f","file://xn--tda/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f",0,"file://a/C:/dir/f","file://[::1]/C:/dir/f","file://127.0.0.1/C:/dir/f","file://a.b/C:/dir/f",0,"file://xn--zca/C:/dir/f","file://c/C:/dir/f",0,"file://./C:/dir/f","file://0.0.1.187/C:/dir/f","file://0.1.0.0/C:/dir/f","file://http/C:/dir/f","file://file/C:/dir/f","file://foo/C:/dir/f","file://ws/C:/dir/f"]],["file:///C:/dir/f","hostname",[0,0,"file://ab/C:/dir/f",0,0,"file://a/C:/dir/f","file://xn--tda/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f","file://a/C:/dir/f",0,0,"file://[::1]/C:/dir/f","file://127.0.0.1/C:/dir/f","file://a.b/C:/dir/f",0,"file://xn--zca/C:/dir/f",0,0,"file://./C:/dir/f","file://0.0.1.187/C:/dir/f","file://0.1.0.0/C:/dir/f",0,"file://file/C:/dir/f","file://foo/C:/dir/f","file://ws/C:/dir/f"]],["file:///C:/dir/f","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file:///C:/dir/f","pathname",["file:///","file:///","file:///ab","file:///%20a%20","file:///x%20y","file:///%41","file:///%C3%BC","file:///a/b","file:///a/b","file:///a%3Fb","file:///a%23b","file:///a@b","file:///a:b","file:///[::1]","file:///0x7f.1","file:///A.B","file:///xn--a","file:///%C3%9F","file:///C:","file:///C:/x","file:///b","file:///443","file:///65536","file:///http:","file:///file","file:///foo","file:///ws"]],["file:///C:/dir/f","search",[0,"file:///C:/dir/f?","file:///C:/dir/f?ab","file:///C:/dir/f?%20a%20","file:///C:/dir/f?x%20y","file:///C:/dir/f?%41","file:///C:/dir/f?%C3%BC","file:///C:/dir/f?a/b","file:///C:/dir/f?a\\b","file:///C:/dir/f?a?b","file:///C:/dir/f?a%23b","file:///C:/dir/f?a@b","file:///C:/dir/f?a:b","file:///C:/dir/f?[::1]","file:///C:/dir/f?0x7f.1","file:///C:/dir/f?A.B","file:///C:/dir/f?xn--a","file:///C:/dir/f?%C3%9F","file:///C:/dir/f?C:","file:///C:/dir/f?/C:/x","file:///C:/dir/f?./a/../b","file:///C:/dir/f?443","file:///C:/dir/f?65536","file:///C:/dir/f?http:","file:///C:/dir/f?file","file:///C:/dir/f?foo","file:///C:/dir/f?ws"]],["file:///C:/dir/f","hash",[0,"file:///C:/dir/f#","file:///C:/dir/f#ab","file:///C:/dir/f#%20a%20","file:///C:/dir/f#x%20y","file:///C:/dir/f#%41","file:///C:/dir/f#%C3%BC","file:///C:/dir/f#a/b","file:///C:/dir/f#a\\b","file:///C:/dir/f#a?b","file:///C:/dir/f#a#b","file:///C:/dir/f#a@b","file:///C:/dir/f#a:b","file:///C:/dir/f#[::1]","file:///C:/dir/f#0x7f.1","file:///C:/dir/f#A.B","file:///C:/dir/f#xn--a","file:///C:/dir/f#%C3%9F","file:///C:/dir/f#C:","file:///C:/dir/f#/C:/x","file:///C:/dir/f#./a/../b","file:///C:/dir/f#443","file:///C:/dir/f#65536","file:///C:/dir/f#http:","file:///C:/dir/f#file","file:///C:/dir/f#foo","file:///C:/dir/f#ws"]],["file://srv/share","protocol",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,"http://srv/share",0,0,"ws://srv/share"]],["file://srv/share","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file://srv/share","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file://srv/share","host",["file:///share","file:///share","file://ab/share",0,0,"file://a/share","file://xn--tda/share","file://a/share","file://a/share","file://a/share","file://a/share",0,"file://a/share","file://[::1]/share","file://127.0.0.1/share","file://a.b/share",0,"file://xn--zca/share","file://c/share","file:///share","file://./share","file://0.0.1.187/share","file://0.1.0.0/share","file://http/share","file://file/share","file://foo/share","file://ws/share"]],["file://srv/share","hostname",["file:///share","file:///share","file://ab/share",0,0,"file://a/share","file://xn--tda/share","file://a/share","file://a/share","file://a/share","file://a/share",0,0,"file://[::1]/share","file://127.0.0.1/share","file://a.b/share",0,"file://xn--zca/share",0,"file:///share","file://./share","file://0.0.1.187/share","file://0.1.0.0/share",0,"file://file/share","file://foo/share","file://ws/share"]],["file://srv/share","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["file://srv/share","pathname",["file://srv/","file://srv/","file://srv/ab","file://srv/%20a%20","file://srv/x%20y","file://srv/%41","file://srv/%C3%BC","file://srv/a/b","file://srv/a/b","file://srv/a%3Fb","file://srv/a%23b","file://srv/a@b","file://srv/a:b","file://srv/[::1]","file://srv/0x7f.1","file://srv/A.B","file://srv/xn--a","file://srv/%C3%9F","file://srv/C:","file://srv/C:/x","file://srv/b","file://srv/443","file://srv/65536","file://srv/http:","file://srv/file","file://srv/foo","file://srv/ws"]],["file://srv/share","search",[0,"file://srv/share?","file://srv/share?ab","file://srv/share?%20a%20","file://srv/share?x%20y","file://srv/share?%41","file://srv/share?%C3%BC","file://srv/share?a/b","file://srv/share?a\\b","file://srv/share?a?b","file://srv/share?a%23b","file://srv/share?a@b","file://srv/share?a:b","file://srv/share?[::1]","file://srv/share?0x7f.1","file://srv/share?A.B","file://srv/share?xn--a","file://srv/share?%C3%9F","file://srv/share?C:","file://srv/share?/C:/x","file://srv/share?./a/../b","file://srv/share?443","file://srv/share?65536","file://srv/share?http:","file://srv/share?file","file://srv/share?foo","file://srv/share?ws"]],["file://srv/share","hash",[0,"file://srv/share#","file://srv/share#ab","file://srv/share#%20a%20","file://srv/share#x%20y","file://srv/share#%41","file://srv/share#%C3%BC","file://srv/share#a/b","file://srv/share#a\\b","file://srv/share#a?b","file://srv/share#a#b","file://srv/share#a@b","file://srv/share#a:b","file://srv/share#[::1]","file://srv/share#0x7f.1","file://srv/share#A.B","file://srv/share#xn--a","file://srv/share#%C3%9F","file://srv/share#C:","file://srv/share#/C:/x","file://srv/share#./a/../b","file://srv/share#443","file://srv/share#65536","file://srv/share#http:","file://srv/share#file","file://srv/share#foo","file://srv/share#ws"]],["foo://h:1/p","protocol",[0,0,"ab://h:1/p",0,0,0,0,0,0,0,0,0,"a://h:1/p",0,0,"a.b://h:1/p","xn--a://h:1/p",0,"c://h:1/p",0,0,0,0,0,0,0,0]],["foo://h:1/p","username",[0,"foo://%09@h:1/p","foo://a%09b@h:1/p","foo://%20a%20@h:1/p","foo://x%20y@h:1/p","foo://%41@h:1/p","foo://%C3%BC@h:1/p","foo://a%2Fb@h:1/p","foo://a%5Cb@h:1/p","foo://a%3Fb@h:1/p","foo://a%23b@h:1/p","foo://a%40b@h:1/p","foo://a%3Ab@h:1/p","foo://%5B%3A%3A1%5D@h:1/p","foo://0x7f.1@h:1/p","foo://A.B@h:1/p","foo://xn--a@h:1/p","foo://%C3%9F@h:1/p","foo://C%3A@h:1/p","foo://%2FC%3A%2Fx@h:1/p","foo://.%2Fa%2F..%2Fb@h:1/p","foo://443@h:1/p","foo://65536@h:1/p","foo://http%3A@h:1/p","foo://file@h:1/p","foo://foo@h:1/p","foo://ws@h:1/p"]],["foo://h:1/p","password",[0,"foo://:%09@h:1/p","foo://:a%09b@h:1/p","foo://:%20a%20@h:1/p","foo://:x%20y@h:1/p","foo://:%41@h:1/p","foo://:%C3%BC@h:1/p","foo://:a%2Fb@h:1/p","foo://:a%5Cb@h:1/p","foo://:a%3Fb@h:1/p","foo://:a%23b@h:1/p","foo://:a%40b@h:1/p","foo://:a%3Ab@h:1/p","foo://:%5B%3A%3A1%5D@h:1/p","foo://:0x7f.1@h:1/p","foo://:A.B@h:1/p","foo://:xn--a@h:1/p","foo://:%C3%9F@h:1/p","foo://:C%3A@h:1/p","foo://:%2FC%3A%2Fx@h:1/p","foo://:.%2Fa%2F..%2Fb@h:1/p","foo://:443@h:1/p","foo://:65536@h:1/p","foo://:http%3A@h:1/p","foo://:file@h:1/p","foo://:foo@h:1/p","foo://:ws@h:1/p"]],["foo://h:1/p","host",[0,0,"foo://ab:1/p",0,0,"foo://%41:1/p","foo://%C3%BC:1/p","foo://a:1/p",0,"foo://a:1/p","foo://a:1/p",0,"foo://a:1/p","foo://[::1]:1/p","foo://0x7f.1:1/p","foo://A.B:1/p","foo://xn--a:1/p","foo://%C3%9F:1/p","foo://C:1/p",0,"foo://.:1/p","foo://443:1/p","foo://65536:1/p","foo://http:1/p","foo://file:1/p","foo://foo:1/p","foo://ws:1/p"]],["foo://h:1/p","hostname",[0,0,"foo://ab:1/p",0,0,"foo://%41:1/p","foo://%C3%BC:1/p","foo://a:1/p",0,"foo://a:1/p","foo://a:1/p",0,0,"foo://[::1]:1/p","foo://0x7f.1:1/p","foo://A.B:1/p","foo://xn--a:1/p","foo://%C3%9F:1/p",0,0,"foo://.:1/p","foo://443:1/p","foo://65536:1/p",0,"foo://file:1/p","foo://foo:1/p","foo://ws:1/p"]],["foo://h:1/p","port",["foo://h/p","foo://h/p",0,0,0,0,0,0,0,0,0,0,0,0,"foo://h:0/p",0,0,0,0,0,0,"foo://h:443/p",0,0,0,0,0]],["foo://h:1/p","pathname",["foo://h:1","foo://h:1/","foo://h:1/ab","foo://h:1/%20a%20","foo://h:1/x%20y","foo://h:1/%41","foo://h:1/%C3%BC","foo://h:1/a/b","foo://h:1/a\\b","foo://h:1/a%3Fb","foo://h:1/a%23b","foo://h:1/a@b","foo://h:1/a:b","foo://h:1/[::1]","foo://h:1/0x7f.1","foo://h:1/A.B","foo://h:1/xn--a","foo://h:1/%C3%9F","foo://h:1/C:","foo://h:1/C:/x","foo://h:1/b","foo://h:1/443","foo://h:1/65536","foo://h:1/http:","foo://h:1/file","foo://h:1/foo","foo://h:1/ws"]],["foo://h:1/p","search",[0,"foo://h:1/p?","foo://h:1/p?ab","foo://h:1/p?%20a%20","foo://h:1/p?x%20y","foo://h:1/p?%41","foo://h:1/p?%C3%BC","foo://h:1/p?a/b","foo://h:1/p?a\\b","foo://h:1/p?a?b","foo://h:1/p?a%23b","foo://h:1/p?a@b","foo://h:1/p?a:b","foo://h:1/p?[::1]","foo://h:1/p?0x7f.1","foo://h:1/p?A.B","foo://h:1/p?xn--a","foo://h:1/p?%C3%9F","foo://h:1/p?C:","foo://h:1/p?/C:/x","foo://h:1/p?./a/../b","foo://h:1/p?443","foo://h:1/p?65536","foo://h:1/p?http:","foo://h:1/p?file","foo://h:1/p?foo","foo://h:1/p?ws"]],["foo://h:1/p","hash",[0,"foo://h:1/p#","foo://h:1/p#ab","foo://h:1/p#%20a%20","foo://h:1/p#x%20y","foo://h:1/p#%41","foo://h:1/p#%C3%BC","foo://h:1/p#a/b","foo://h:1/p#a\\b","foo://h:1/p#a?b","foo://h:1/p#a#b","foo://h:1/p#a@b","foo://h:1/p#a:b","foo://h:1/p#[::1]","foo://h:1/p#0x7f.1","foo://h:1/p#A.B","foo://h:1/p#xn--a","foo://h:1/p#%C3%9F","foo://h:1/p#C:","foo://h:1/p#/C:/x","foo://h:1/p#./a/../b","foo://h:1/p#443","foo://h:1/p#65536","foo://h:1/p#http:","foo://h:1/p#file","foo://h:1/p#foo","foo://h:1/p#ws"]],["foo:opaque?q#h","protocol",[0,0,"ab:opaque?q#h",0,0,0,0,0,0,0,0,0,"a:opaque?q#h",0,0,"a.b:opaque?q#h","xn--a:opaque?q#h",0,"c:opaque?q#h",0,0,0,0,0,0,0,0]],["foo:opaque?q#h","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","host",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","hostname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","pathname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["foo:opaque?q#h","search",["foo:opaque#h","foo:opaque?#h","foo:opaque?ab#h","foo:opaque?%20a%20#h","foo:opaque?x%20y#h","foo:opaque?%41#h","foo:opaque?%C3%BC#h","foo:opaque?a/b#h","foo:opaque?a\\b#h","foo:opaque?a?b#h","foo:opaque?a%23b#h","foo:opaque?a@b#h","foo:opaque?a:b#h","foo:opaque?[::1]#h","foo:opaque?0x7f.1#h","foo:opaque?A.B#h","foo:opaque?xn--a#h","foo:opaque?%C3%9F#h","foo:opaque?C:#h","foo:opaque?/C:/x#h","foo:opaque?./a/../b#h","foo:opaque?443#h","foo:opaque?65536#h","foo:opaque?http:#h","foo:opaque?file#h","foo:opaque?foo#h","foo:opaque?ws#h"]],["foo:opaque?q#h","hash",["foo:opaque?q","foo:opaque?q#","foo:opaque?q#ab","foo:opaque?q#%20a%20","foo:opaque?q#x%20y","foo:opaque?q#%41","foo:opaque?q#%C3%BC","foo:opaque?q#a/b","foo:opaque?q#a\\b","foo:opaque?q#a?b","foo:opaque?q#a#b","foo:opaque?q#a@b","foo:opaque?q#a:b","foo:opaque?q#[::1]","foo:opaque?q#0x7f.1","foo:opaque?q#A.B","foo:opaque?q#xn--a","foo:opaque?q#%C3%9F","foo:opaque?q#C:","foo:opaque?q#/C:/x","foo:opaque?q#./a/../b","foo:opaque?q#443","foo:opaque?q#65536","foo:opaque?q#http:","foo:opaque?q#file","foo:opaque?q#foo","foo:opaque?q#ws"]],["mailto:x@y","protocol",[0,0,"ab:x@y",0,0,0,0,0,0,0,0,0,"a:x@y",0,0,"a.b:x@y","xn--a:x@y",0,"c:x@y",0,0,0,0,0,0,"foo:x@y",0]],["mailto:x@y","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","host",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","hostname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","pathname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["mailto:x@y","search",[0,"mailto:x@y?","mailto:x@y?ab","mailto:x@y?%20a%20","mailto:x@y?x%20y","mailto:x@y?%41","mailto:x@y?%C3%BC","mailto:x@y?a/b","mailto:x@y?a\\b","mailto:x@y?a?b","mailto:x@y?a%23b","mailto:x@y?a@b","mailto:x@y?a:b","mailto:x@y?[::1]","mailto:x@y?0x7f.1","mailto:x@y?A.B","mailto:x@y?xn--a","mailto:x@y?%C3%9F","mailto:x@y?C:","mailto:x@y?/C:/x","mailto:x@y?./a/../b","mailto:x@y?443","mailto:x@y?65536","mailto:x@y?http:","mailto:x@y?file","mailto:x@y?foo","mailto:x@y?ws"]],["mailto:x@y","hash",[0,"mailto:x@y#","mailto:x@y#ab","mailto:x@y#%20a%20","mailto:x@y#x%20y","mailto:x@y#%41","mailto:x@y#%C3%BC","mailto:x@y#a/b","mailto:x@y#a\\b","mailto:x@y#a?b","mailto:x@y#a#b","mailto:x@y#a@b","mailto:x@y#a:b","mailto:x@y#[::1]","mailto:x@y#0x7f.1","mailto:x@y#A.B","mailto:x@y#xn--a","mailto:x@y#%C3%9F","mailto:x@y#C:","mailto:x@y#/C:/x","mailto:x@y#./a/../b","mailto:x@y#443","mailto:x@y#65536","mailto:x@y#http:","mailto:x@y#file","mailto:x@y#foo","mailto:x@y#ws"]],["data:text/plain,hi","protocol",[0,0,"ab:text/plain,hi",0,0,0,0,0,0,0,0,0,"a:text/plain,hi",0,0,"a.b:text/plain,hi","xn--a:text/plain,hi",0,"c:text/plain,hi",0,0,0,0,0,0,"foo:text/plain,hi",0]],["data:text/plain,hi","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","host",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","hostname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","pathname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["data:text/plain,hi","search",[0,"data:text/plain,hi?","data:text/plain,hi?ab","data:text/plain,hi?%20a%20","data:text/plain,hi?x%20y","data:text/plain,hi?%41","data:text/plain,hi?%C3%BC","data:text/plain,hi?a/b","data:text/plain,hi?a\\b","data:text/plain,hi?a?b","data:text/plain,hi?a%23b","data:text/plain,hi?a@b","data:text/plain,hi?a:b","data:text/plain,hi?[::1]","data:text/plain,hi?0x7f.1","data:text/plain,hi?A.B","data:text/plain,hi?xn--a","data:text/plain,hi?%C3%9F","data:text/plain,hi?C:","data:text/plain,hi?/C:/x","data:text/plain,hi?./a/../b","data:text/plain,hi?443","data:text/plain,hi?65536","data:text/plain,hi?http:","data:text/plain,hi?file","data:text/plain,hi?foo","data:text/plain,hi?ws"]],["data:text/plain,hi","hash",[0,"data:text/plain,hi#","data:text/plain,hi#ab","data:text/plain,hi#%20a%20","data:text/plain,hi#x%20y","data:text/plain,hi#%41","data:text/plain,hi#%C3%BC","data:text/plain,hi#a/b","data:text/plain,hi#a\\b","data:text/plain,hi#a?b","data:text/plain,hi#a#b","data:text/plain,hi#a@b","data:text/plain,hi#a:b","data:text/plain,hi#[::1]","data:text/plain,hi#0x7f.1","data:text/plain,hi#A.B","data:text/plain,hi#xn--a","data:text/plain,hi#%C3%9F","data:text/plain,hi#C:","data:text/plain,hi#/C:/x","data:text/plain,hi#./a/../b","data:text/plain,hi#443","data:text/plain,hi#65536","data:text/plain,hi#http:","data:text/plain,hi#file","data:text/plain,hi#foo","data:text/plain,hi#ws"]],["ws://h/","protocol",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,"http://h/","file://h/",0,0]],["ws://h/","username",[0,"ws://%09@h/","ws://a%09b@h/","ws://%20a%20@h/","ws://x%20y@h/","ws://%41@h/","ws://%C3%BC@h/","ws://a%2Fb@h/","ws://a%5Cb@h/","ws://a%3Fb@h/","ws://a%23b@h/","ws://a%40b@h/","ws://a%3Ab@h/","ws://%5B%3A%3A1%5D@h/","ws://0x7f.1@h/","ws://A.B@h/","ws://xn--a@h/","ws://%C3%9F@h/","ws://C%3A@h/","ws://%2FC%3A%2Fx@h/","ws://.%2Fa%2F..%2Fb@h/","ws://443@h/","ws://65536@h/","ws://http%3A@h/","ws://file@h/","ws://foo@h/","ws://ws@h/"]],["ws://h/","password",[0,"ws://:%09@h/","ws://:a%09b@h/","ws://:%20a%20@h/","ws://:x%20y@h/","ws://:%41@h/","ws://:%C3%BC@h/","ws://:a%2Fb@h/","ws://:a%5Cb@h/","ws://:a%3Fb@h/","ws://:a%23b@h/","ws://:a%40b@h/","ws://:a%3Ab@h/","ws://:%5B%3A%3A1%5D@h/","ws://:0x7f.1@h/","ws://:A.B@h/","ws://:xn--a@h/","ws://:%C3%9F@h/","ws://:C%3A@h/","ws://:%2FC%3A%2Fx@h/","ws://:.%2Fa%2F..%2Fb@h/","ws://:443@h/","ws://:65536@h/","ws://:http%3A@h/","ws://:file@h/","ws://:foo@h/","ws://:ws@h/"]],["ws://h/","host",[0,0,"ws://ab/",0,0,"ws://a/","ws://xn--tda/","ws://a/","ws://a/","ws://a/","ws://a/",0,"ws://a/","ws://[::1]/","ws://127.0.0.1/","ws://a.b/",0,"ws://xn--zca/","ws://c/",0,"ws://./","ws://0.0.1.187/","ws://0.1.0.0/","ws://http/","ws://file/","ws://foo/","ws://ws/"]],["ws://h/","hostname",[0,0,"ws://ab/",0,0,"ws://a/","ws://xn--tda/","ws://a/","ws://a/","ws://a/","ws://a/",0,0,"ws://[::1]/","ws://127.0.0.1/","ws://a.b/",0,"ws://xn--zca/",0,0,"ws://./","ws://0.0.1.187/","ws://0.1.0.0/",0,"ws://file/","ws://foo/","ws://ws/"]],["ws://h/","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,"ws://h:0/",0,0,0,0,0,0,"ws://h:443/",0,0,0,0,0]],["ws://h/","pathname",[0,0,"ws://h/ab","ws://h/%20a%20","ws://h/x%20y","ws://h/%41","ws://h/%C3%BC","ws://h/a/b","ws://h/a/b","ws://h/a%3Fb","ws://h/a%23b","ws://h/a@b","ws://h/a:b","ws://h/[::1]","ws://h/0x7f.1","ws://h/A.B","ws://h/xn--a","ws://h/%C3%9F","ws://h/C:","ws://h/C:/x","ws://h/b","ws://h/443","ws://h/65536","ws://h/http:","ws://h/file","ws://h/foo","ws://h/ws"]],["ws://h/","search",[0,"ws://h/?","ws://h/?ab","ws://h/?%20a%20","ws://h/?x%20y","ws://h/?%41","ws://h/?%C3%BC","ws://h/?a/b","ws://h/?a\\b","ws://h/?a?b","ws://h/?a%23b","ws://h/?a@b","ws://h/?a:b","ws://h/?[::1]","ws://h/?0x7f.1","ws://h/?A.B","ws://h/?xn--a","ws://h/?%C3%9F","ws://h/?C:","ws://h/?/C:/x","ws://h/?./a/../b","ws://h/?443","ws://h/?65536","ws://h/?http:","ws://h/?file","ws://h/?foo","ws://h/?ws"]],["ws://h/","hash",[0,"ws://h/#","ws://h/#ab","ws://h/#%20a%20","ws://h/#x%20y","ws://h/#%41","ws://h/#%C3%BC","ws://h/#a/b","ws://h/#a\\b","ws://h/#a?b","ws://h/#a#b","ws://h/#a@b","ws://h/#a:b","ws://h/#[::1]","ws://h/#0x7f.1","ws://h/#A.B","ws://h/#xn--a","ws://h/#%C3%9F","ws://h/#C:","ws://h/#/C:/x","ws://h/#./a/../b","ws://h/#443","ws://h/#65536","ws://h/#http:","ws://h/#file","ws://h/#foo","ws://h/#ws"]],["blob:https://a.com/u","protocol",[0,0,"ab:https://a.com/u",0,0,0,0,0,0,0,0,0,"a:https://a.com/u",0,0,"a.b:https://a.com/u","xn--a:https://a.com/u",0,"c:https://a.com/u",0,0,0,0,0,0,"foo:https://a.com/u",0]],["blob:https://a.com/u","username",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","password",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","host",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","hostname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","port",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","pathname",[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]],["blob:https://a.com/u","search",[0,"blob:https://a.com/u?","blob:https://a.com/u?ab","blob:https://a.com/u?%20a%20","blob:https://a.com/u?x%20y","blob:https://a.com/u?%41","blob:https://a.com/u?%C3%BC","blob:https://a.com/u?a/b","blob:https://a.com/u?a\\b","blob:https://a.com/u?a?b","blob:https://a.com/u?a%23b","blob:https://a.com/u?a@b","blob:https://a.com/u?a:b","blob:https://a.com/u?[::1]","blob:https://a.com/u?0x7f.1","blob:https://a.com/u?A.B","blob:https://a.com/u?xn--a","blob:https://a.com/u?%C3%9F","blob:https://a.com/u?C:","blob:https://a.com/u?/C:/x","blob:https://a.com/u?./a/../b","blob:https://a.com/u?443","blob:https://a.com/u?65536","blob:https://a.com/u?http:","blob:https://a.com/u?file","blob:https://a.com/u?foo","blob:https://a.com/u?ws"]],["blob:https://a.com/u","hash",[0,"blob:https://a.com/u#","blob:https://a.com/u#ab","blob:https://a.com/u#%20a%20","blob:https://a.com/u#x%20y","blob:https://a.com/u#%41","blob:https://a.com/u#%C3%BC","blob:https://a.com/u#a/b","blob:https://a.com/u#a\\b","blob:https://a.com/u#a?b","blob:https://a.com/u#a#b","blob:https://a.com/u#a@b","blob:https://a.com/u#a:b","blob:https://a.com/u#[::1]","blob:https://a.com/u#0x7f.1","blob:https://a.com/u#A.B","blob:https://a.com/u#xn--a","blob:https://a.com/u#%C3%9F","blob:https://a.com/u#C:","blob:https://a.com/u#/C:/x","blob:https://a.com/u#./a/../b","blob:https://a.com/u#443","blob:https://a.com/u#65536","blob:https://a.com/u#http:","blob:https://a.com/u#file","blob:https://a.com/u#foo","blob:https://a.com/u#ws"]]],"toolCalls":[]}"####,
        ),
        (
            r####"// percent-encoding of each printable ASCII and some non-ASCII characters in each URL component
const chars = []
const printable = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~"
for (let i = 0; i < printable.length; i++) chars.push(printable[i])
chars.push("\u00e9", "\u2603", "\ud83d\ude00", "\u007f", "\u0001", "\u001f", "\u00a0", "%2e", "%2E%2e", "\ufeff")
const out = []
for (const c of chars) {
  const row = []
  for (const [tpl, part] of [["https://h/a" + c + "b", "pathname"], ["https://h/?a" + c + "b", "search"], ["https://h/#a" + c + "b", "hash"], ["https://u" + c + "v@h/", "username"], ["https://u:p" + c + "q@h/", "password"], ["foo://h/a" + c + "b", "pathname"], ["foo:a" + c + "b", "pathname"], ["https://a" + c + "b/", "host"]]) {
    try { row.push(new URL(tpl)[part]) } catch (e) { row.push(0) }
  }
  for (const p of ["pathname", "search", "hash", "username"]) { const u = new URL("https://h/"); u[p] = "a" + c + "b"; row.push(u[p]) }
  out.push(row)
}
return out"####,
            r####"{"ok":true,"value":[["/a%20b","?a%20b","#a%20b","u%20v","p%20q","/a%20b","a b",0,"/a%20b","?a%20b","#a%20b","a%20b"],["/a!b","?a!b","#a!b","u!v","p!q","/a!b","a!b","a!b","/a!b","?a!b","#a!b","a!b"],["/a%22b","?a%22b","#a%22b","u%22v","p%22q","/a%22b","a\"b","a\"b","/a%22b","?a%22b","#a%22b","a%22b"],["/a","?a","#a#b","",0,"/a","a","a","/a%23b","?a%23b","#a#b","a%23b"],["/a$b","?a$b","#a$b","u$v","p$q","/a$b","a$b","a$b","/a$b","?a$b","#a$b","a$b"],["/a%b","?a%b","#a%b","u%v","p%q","/a%b","a%b",0,"/a%b","?a%b","#a%b","a%b"],["/a&b","?a&b","#a&b","u&v","p&q","/a&b","a&b","a&b","/a&b","?a&b","#a&b","a&b"],["/a'b","?a%27b","#a'b","u'v","p'q","/a'b","a'b","a'b","/a'b","?a%27b","#a'b","a'b"],["/a(b","?a(b","#a(b","u(v","p(q","/a(b","a(b","a(b","/a(b","?a(b","#a(b","a(b"],["/a)b","?a)b","#a)b","u)v","p)q","/a)b","a)b","a)b","/a)b","?a)b","#a)b","a)b"],["/a*b","?a*b","#a*b","u*v","p*q","/a*b","a*b","a*b","/a*b","?a*b","#a*b","a*b"],["/a+b","?a+b","#a+b","u+v","p+q","/a+b","a+b","a+b","/a+b","?a+b","#a+b","a+b"],["/a,b","?a,b","#a,b","u,v","p,q","/a,b","a,b","a,b","/a,b","?a,b","#a,b","a,b"],["/a-b","?a-b","#a-b","u-v","p-q","/a-b","a-b","a-b","/a-b","?a-b","#a-b","a-b"],["/a.b","?a.b","#a.b","u.v","p.q","/a.b","a.b","a.b","/a.b","?a.b","#a.b","a.b"],["/a/b","?a/b","#a/b","",0,"/a/b","a/b","a","/a/b","?a/b","#a/b","a%2Fb"],["/a0b","?a0b","#a0b","u0v","p0q","/a0b","a0b","a0b","/a0b","?a0b","#a0b","a0b"],["/a1b","?a1b","#a1b","u1v","p1q","/a1b","a1b","a1b","/a1b","?a1b","#a1b","a1b"],["/a2b","?a2b","#a2b","u2v","p2q","/a2b","a2b","a2b","/a2b","?a2b","#a2b","a2b"],["/a3b","?a3b","#a3b","u3v","p3q","/a3b","a3b","a3b","/a3b","?a3b","#a3b","a3b"],["/a4b","?a4b","#a4b","u4v","p4q","/a4b","a4b","a4b","/a4b","?a4b","#a4b","a4b"],["/a5b","?a5b","#a5b","u5v","p5q","/a5b","a5b","a5b","/a5b","?a5b","#a5b","a5b"],["/a6b","?a6b","#a6b","u6v","p6q","/a6b","a6b","a6b","/a6b","?a6b","#a6b","a6b"],["/a7b","?a7b","#a7b","u7v","p7q","/a7b","a7b","a7b","/a7b","?a7b","#a7b","a7b"],["/a8b","?a8b","#a8b","u8v","p8q","/a8b","a8b","a8b","/a8b","?a8b","#a8b","a8b"],["/a9b","?a9b","#a9b","u9v","p9q","/a9b","a9b","a9b","/a9b","?a9b","#a9b","a9b"],["/a:b","?a:b","#a:b","u","p%3Aq","/a:b","a:b",0,"/a:b","?a:b","#a:b","a%3Ab"],["/a;b","?a;b","#a;b","u%3Bv","p%3Bq","/a;b","a;b","a;b","/a;b","?a;b","#a;b","a%3Bb"],["/a%3Cb","?a%3Cb","#a%3Cb","u%3Cv","p%3Cq","/a%3Cb","a<b",0,"/a%3Cb","?a%3Cb","#a%3Cb","a%3Cb"],["/a=b","?a=b","#a=b","u%3Dv","p%3Dq","/a=b","a=b","a=b","/a=b","?a=b","#a=b","a%3Db"],["/a%3Eb","?a%3Eb","#a%3Eb","u%3Ev","p%3Eq","/a%3Eb","a>b",0,"/a%3Eb","?a%3Eb","#a%3Eb","a%3Eb"],["/a","?a?b","#a?b","",0,"/a","a","a","/a%3Fb","?a?b","#a?b","a%3Fb"],["/a@b","?a@b","#a@b","u%40v","p%40q","/a@b","a@b","b","/a@b","?a@b","#a@b","a%40b"],["/aAb","?aAb","#aAb","uAv","pAq","/aAb","aAb","aab","/aAb","?aAb","#aAb","aAb"],["/aBb","?aBb","#aBb","uBv","pBq","/aBb","aBb","abb","/aBb","?aBb","#aBb","aBb"],["/aCb","?aCb","#aCb","uCv","pCq","/aCb","aCb","acb","/aCb","?aCb","#aCb","aCb"],["/aDb","?aDb","#aDb","uDv","pDq","/aDb","aDb","adb","/aDb","?aDb","#aDb","aDb"],["/aEb","?aEb","#aEb","uEv","pEq","/aEb","aEb","aeb","/aEb","?aEb","#aEb","aEb"],["/aFb","?aFb","#aFb","uFv","pFq","/aFb","aFb","afb","/aFb","?aFb","#aFb","aFb"],["/aGb","?aGb","#aGb","uGv","pGq","/aGb","aGb","agb","/aGb","?aGb","#aGb","aGb"],["/aHb","?aHb","#aHb","uHv","pHq","/aHb","aHb","ahb","/aHb","?aHb","#aHb","aHb"],["/aIb","?aIb","#aIb","uIv","pIq","/aIb","aIb","aib","/aIb","?aIb","#aIb","aIb"],["/aJb","?aJb","#aJb","uJv","pJq","/aJb","aJb","ajb","/aJb","?aJb","#aJb","aJb"],["/aKb","?aKb","#aKb","uKv","pKq","/aKb","aKb","akb","/aKb","?aKb","#aKb","aKb"],["/aLb","?aLb","#aLb","uLv","pLq","/aLb","aLb","alb","/aLb","?aLb","#aLb","aLb"],["/aMb","?aMb","#aMb","uMv","pMq","/aMb","aMb","amb","/aMb","?aMb","#aMb","aMb"],["/aNb","?aNb","#aNb","uNv","pNq","/aNb","aNb","anb","/aNb","?aNb","#aNb","aNb"],["/aOb","?aOb","#aOb","uOv","pOq","/aOb","aOb","aob","/aOb","?aOb","#aOb","aOb"],["/aPb","?aPb","#aPb","uPv","pPq","/aPb","aPb","apb","/aPb","?aPb","#aPb","aPb"],["/aQb","?aQb","#aQb","uQv","pQq","/aQb","aQb","aqb","/aQb","?aQb","#aQb","aQb"],["/aRb","?aRb","#aRb","uRv","pRq","/aRb","aRb","arb","/aRb","?aRb","#aRb","aRb"],["/aSb","?aSb","#aSb","uSv","pSq","/aSb","aSb","asb","/aSb","?aSb","#aSb","aSb"],["/aTb","?aTb","#aTb","uTv","pTq","/aTb","aTb","atb","/aTb","?aTb","#aTb","aTb"],["/aUb","?aUb","#aUb","uUv","pUq","/aUb","aUb","aub","/aUb","?aUb","#aUb","aUb"],["/aVb","?aVb","#aVb","uVv","pVq","/aVb","aVb","avb","/aVb","?aVb","#aVb","aVb"],["/aWb","?aWb","#aWb","uWv","pWq","/aWb","aWb","awb","/aWb","?aWb","#aWb","aWb"],["/aXb","?aXb","#aXb","uXv","pXq","/aXb","aXb","axb","/aXb","?aXb","#aXb","aXb"],["/aYb","?aYb","#aYb","uYv","pYq","/aYb","aYb","ayb","/aYb","?aYb","#aYb","aYb"],["/aZb","?aZb","#aZb","uZv","pZq","/aZb","aZb","azb","/aZb","?aZb","#aZb","aZb"],["/a[b","?a[b","#a[b","u%5Bv","p%5Bq","/a[b","a[b",0,"/a[b","?a[b","#a[b","a%5Bb"],["/a/b","?a\\b","#a\\b","",0,"/a\\b","a\\b","a","/a/b","?a\\b","#a\\b","a%5Cb"],["/a]b","?a]b","#a]b","u%5Dv","p%5Dq","/a]b","a]b",0,"/a]b","?a]b","#a]b","a%5Db"],["/a%5Eb","?a^b","#a^b","u%5Ev","p%5Eq","/a%5Eb","a^b",0,"/a%5Eb","?a^b","#a^b","a%5Eb"],["/a_b","?a_b","#a_b","u_v","p_q","/a_b","a_b","a_b","/a_b","?a_b","#a_b","a_b"],["/a%60b","?a`b","#a%60b","u%60v","p%60q","/a%60b","a`b","a`b","/a%60b","?a`b","#a%60b","a%60b"],["/aab","?aab","#aab","uav","paq","/aab","aab","aab","/aab","?aab","#aab","aab"],["/abb","?abb","#abb","ubv","pbq","/abb","abb","abb","/abb","?abb","#abb","abb"],["/acb","?acb","#acb","ucv","pcq","/acb","acb","acb","/acb","?acb","#acb","acb"],["/adb","?adb","#adb","udv","pdq","/adb","adb","adb","/adb","?adb","#adb","adb"],["/aeb","?aeb","#aeb","uev","peq","/aeb","aeb","aeb","/aeb","?aeb","#aeb","aeb"],["/afb","?afb","#afb","ufv","pfq","/afb","afb","afb","/afb","?afb","#afb","afb"],["/agb","?agb","#agb","ugv","pgq","/agb","agb","agb","/agb","?agb","#agb","agb"],["/ahb","?ahb","#ahb","uhv","phq","/ahb","ahb","ahb","/ahb","?ahb","#ahb","ahb"],["/aib","?aib","#aib","uiv","piq","/aib","aib","aib","/aib","?aib","#aib","aib"],["/ajb","?ajb","#ajb","ujv","pjq","/ajb","ajb","ajb","/ajb","?ajb","#ajb","ajb"],["/akb","?akb","#akb","ukv","pkq","/akb","akb","akb","/akb","?akb","#akb","akb"],["/alb","?alb","#alb","ulv","plq","/alb","alb","alb","/alb","?alb","#alb","alb"],["/amb","?amb","#amb","umv","pmq","/amb","amb","amb","/amb","?amb","#amb","amb"],["/anb","?anb","#anb","unv","pnq","/anb","anb","anb","/anb","?anb","#anb","anb"],["/aob","?aob","#aob","uov","poq","/aob","aob","aob","/aob","?aob","#aob","aob"],["/apb","?apb","#apb","upv","ppq","/apb","apb","apb","/apb","?apb","#apb","apb"],["/aqb","?aqb","#aqb","uqv","pqq","/aqb","aqb","aqb","/aqb","?aqb","#aqb","aqb"],["/arb","?arb","#arb","urv","prq","/arb","arb","arb","/arb","?arb","#arb","arb"],["/asb","?asb","#asb","usv","psq","/asb","asb","asb","/asb","?asb","#asb","asb"],["/atb","?atb","#atb","utv","ptq","/atb","atb","atb","/atb","?atb","#atb","atb"],["/aub","?aub","#aub","uuv","puq","/aub","aub","aub","/aub","?aub","#aub","aub"],["/avb","?avb","#avb","uvv","pvq","/avb","avb","avb","/avb","?avb","#avb","avb"],["/awb","?awb","#awb","uwv","pwq","/awb","awb","awb","/awb","?awb","#awb","awb"],["/axb","?axb","#axb","uxv","pxq","/axb","axb","axb","/axb","?axb","#axb","axb"],["/ayb","?ayb","#ayb","uyv","pyq","/ayb","ayb","ayb","/ayb","?ayb","#ayb","ayb"],["/azb","?azb","#azb","uzv","pzq","/azb","azb","azb","/azb","?azb","#azb","azb"],["/a%7Bb","?a{b","#a{b","u%7Bv","p%7Bq","/a%7Bb","a{b","a{b","/a%7Bb","?a{b","#a{b","a%7Bb"],["/a|b","?a|b","#a|b","u%7Cv","p%7Cq","/a|b","a|b",0,"/a|b","?a|b","#a|b","a%7Cb"],["/a%7Db","?a}b","#a}b","u%7Dv","p%7Dq","/a%7Db","a}b","a}b","/a%7Db","?a}b","#a}b","a%7Db"],["/a~b","?a~b","#a~b","u~v","p~q","/a~b","a~b","a~b","/a~b","?a~b","#a~b","a~b"],["/a%C3%A9b","?a%C3%A9b","#a%C3%A9b","u%C3%A9v","p%C3%A9q","/a%C3%A9b","a%C3%A9b","xn--ab-bja","/a%C3%A9b","?a%C3%A9b","#a%C3%A9b","a%C3%A9b"],["/a%E2%98%83b","?a%E2%98%83b","#a%E2%98%83b","u%E2%98%83v","p%E2%98%83q","/a%E2%98%83b","a%E2%98%83b","xn--ab-fsx","/a%E2%98%83b","?a%E2%98%83b","#a%E2%98%83b","a%E2%98%83b"],["/a%F0%9F%98%80b","?a%F0%9F%98%80b","#a%F0%9F%98%80b","u%F0%9F%98%80v","p%F0%9F%98%80q","/a%F0%9F%98%80b","a%F0%9F%98%80b","xn--ab-no82a","/a%F0%9F%98%80b","?a%F0%9F%98%80b","#a%F0%9F%98%80b","a%F0%9F%98%80b"],["/a%7Fb","?a%7Fb","#a%7Fb","u%7Fv","p%7Fq","/a%7Fb","a%7Fb",0,"/a%7Fb","?a%7Fb","#a%7Fb","a%7Fb"],["/a%01b","?a%01b","#a%01b","u%01v","p%01q","/a%01b","a%01b",0,"/a%01b","?a%01b","#a%01b","a%01b"],["/a%1Fb","?a%1Fb","#a%1Fb","u%1Fv","p%1Fq","/a%1Fb","a%1Fb",0,"/a%1Fb","?a%1Fb","#a%1Fb","a%1Fb"],["/a%C2%A0b","?a%C2%A0b","#a%C2%A0b","u%C2%A0v","p%C2%A0q","/a%C2%A0b","a%C2%A0b",0,"/a%C2%A0b","?a%C2%A0b","#a%C2%A0b","a%C2%A0b"],["/a%2eb","?a%2eb","#a%2eb","u%2ev","p%2eq","/a%2eb","a%2eb","a.b","/a%2eb","?a%2eb","#a%2eb","a%2eb"],["/a%2E%2eb","?a%2E%2eb","#a%2E%2eb","u%2E%2ev","p%2E%2eq","/a%2E%2eb","a%2E%2eb","a..b","/a%2E%2eb","?a%2E%2eb","#a%2E%2eb","a%2E%2eb"],["/a%EF%BB%BFb","?a%EF%BB%BFb","#a%EF%BB%BFb","u%EF%BB%BFv","p%EF%BB%BFq","/a%EF%BB%BFb","a%EF%BB%BFb","ab","/a%EF%BB%BFb","?a%EF%BB%BFb","#a%EF%BB%BFb","a%EF%BB%BFb"]],"toolCalls":[]}"####,
        ),
        (
            r####"// file host setters, IPv6 hosts, and protocol changes to and from file:
const t = (href, p, v) => { const u = new URL(href); try { u[p] = v } catch (e) { return "ERR " + e.message } return u.href }
const out = []
for (const b of ["file:///x", "file://h/x", "https://a:8080/", "https://a/", "file://h:/x"]) for (const v of ["[::1]:5", "[::1", "a]:5", "h:5:", "a:1/b", "a\\b:5", "x:5#", "x\t:5", "x:\n5", "localhost", "LOCALHOST:1", "a%3A5", "[::1]", "", ":", "::"]) for (const p of ["host", "hostname"]) out.push([b, v, p, t(b, p, v)])
for (const b of ["http://a/", "https://a/p?q#h", "ws://a:1/", "http://u@a/", "ftp://a/", "foo://a/", "http://localhost/"]) for (const v of ["file", "FILE:", "fi\tle", "file:x", "file+x", " file", "https", "foo", "ws"]) out.push([b, v, "protocol", t(b, "protocol", v)])
return out"####,
            r####"{"ok":true,"value":[["file:///x","[::1]:5","host","file:///x"],["file:///x","[::1]:5","hostname","file:///x"],["file:///x","[::1","host","file:///x"],["file:///x","[::1","hostname","file:///x"],["file:///x","a]:5","host","file:///x"],["file:///x","a]:5","hostname","file:///x"],["file:///x","h:5:","host","file:///x"],["file:///x","h:5:","hostname","file:///x"],["file:///x","a:1/b","host","file:///x"],["file:///x","a:1/b","hostname","file:///x"],["file:///x","a\\b:5","host","file://a/x"],["file:///x","a\\b:5","hostname","file://a/x"],["file:///x","x:5#","host","file:///x"],["file:///x","x:5#","hostname","file:///x"],["file:///x","x\t:5","host","file:///x"],["file:///x","x\t:5","hostname","file:///x"],["file:///x","x:\n5","host","file://x/x"],["file:///x","x:\n5","hostname","file:///x"],["file:///x","localhost","host","file:///x"],["file:///x","localhost","hostname","file:///x"],["file:///x","LOCALHOST:1","host","file:///x"],["file:///x","LOCALHOST:1","hostname","file:///x"],["file:///x","a%3A5","host","file:///x"],["file:///x","a%3A5","hostname","file:///x"],["file:///x","[::1]","host","file://[::1]/x"],["file:///x","[::1]","hostname","file://[::1]/x"],["file:///x","","host","file:///x"],["file:///x","","hostname","file:///x"],["file:///x",":","host","file:///x"],["file:///x",":","hostname","file:///x"],["file:///x","::","host","file:///x"],["file:///x","::","hostname","file:///x"],["file://h/x","[::1]:5","host","file://h/x"],["file://h/x","[::1]:5","hostname","file://h/x"],["file://h/x","[::1","host","file://h/x"],["file://h/x","[::1","hostname","file://h/x"],["file://h/x","a]:5","host","file://h/x"],["file://h/x","a]:5","hostname","file://h/x"],["file://h/x","h:5:","host","file://h/x"],["file://h/x","h:5:","hostname","file://h/x"],["file://h/x","a:1/b","host","file://h/x"],["file://h/x","a:1/b","hostname","file://h/x"],["file://h/x","a\\b:5","host","file://a/x"],["file://h/x","a\\b:5","hostname","file://a/x"],["file://h/x","x:5#","host","file://h/x"],["file://h/x","x:5#","hostname","file://h/x"],["file://h/x","x\t:5","host","file://h/x"],["file://h/x","x\t:5","hostname","file://h/x"],["file://h/x","x:\n5","host","file://x/x"],["file://h/x","x:\n5","hostname","file://h/x"],["file://h/x","localhost","host","file:///x"],["file://h/x","localhost","hostname","file:///x"],["file://h/x","LOCALHOST:1","host","file://h/x"],["file://h/x","LOCALHOST:1","hostname","file://h/x"],["file://h/x","a%3A5","host","file://h/x"],["file://h/x","a%3A5","hostname","file://h/x"],["file://h/x","[::1]","host","file://[::1]/x"],["file://h/x","[::1]","hostname","file://[::1]/x"],["file://h/x","","host","file:///x"],["file://h/x","","hostname","file:///x"],["file://h/x",":","host","file://h/x"],["file://h/x",":","hostname","file://h/x"],["file://h/x","::","host","file://h/x"],["file://h/x","::","hostname","file://h/x"],["https://a:8080/","[::1]:5","host","https://[::1]:5/"],["https://a:8080/","[::1]:5","hostname","https://a:8080/"],["https://a:8080/","[::1","host","https://a:8080/"],["https://a:8080/","[::1","hostname","https://a:8080/"],["https://a:8080/","a]:5","host","https://a:8080/"],["https://a:8080/","a]:5","hostname","https://a:8080/"],["https://a:8080/","h:5:","host","https://a:8080/"],["https://a:8080/","h:5:","hostname","https://a:8080/"],["https://a:8080/","a:1/b","host","https://a:1/"],["https://a:8080/","a:1/b","hostname","https://a:8080/"],["https://a:8080/","a\\b:5","host","https://a:8080/"],["https://a:8080/","a\\b:5","hostname","https://a:8080/"],["https://a:8080/","x:5#","host","https://x:5/"],["https://a:8080/","x:5#","hostname","https://a:8080/"],["https://a:8080/","x\t:5","host","https://x:5/"],["https://a:8080/","x\t:5","hostname","https://a:8080/"],["https://a:8080/","x:\n5","host","https://x:8080/"],["https://a:8080/","x:\n5","hostname","https://a:8080/"],["https://a:8080/","localhost","host","https://localhost:8080/"],["https://a:8080/","localhost","hostname","https://localhost:8080/"],["https://a:8080/","LOCALHOST:1","host","https://localhost:1/"],["https://a:8080/","LOCALHOST:1","hostname","https://a:8080/"],["https://a:8080/","a%3A5","host","https://a:8080/"],["https://a:8080/","a%3A5","hostname","https://a:8080/"],["https://a:8080/","[::1]","host","https://[::1]:8080/"],["https://a:8080/","[::1]","hostname","https://[::1]:8080/"],["https://a:8080/","","host","https://a:8080/"],["https://a:8080/","","hostname","https://a:8080/"],["https://a:8080/",":","host","https://a:8080/"],["https://a:8080/",":","hostname","https://a:8080/"],["https://a:8080/","::","host","https://a:8080/"],["https://a:8080/","::","hostname","https://a:8080/"],["https://a/","[::1]:5","host","https://[::1]:5/"],["https://a/","[::1]:5","hostname","https://[::1]:5/"],["https://a/","[::1","host","https://a/"],["https://a/","[::1","hostname","https://a/"],["https://a/","a]:5","host","https://a/"],["https://a/","a]:5","hostname","https://a/"],["https://a/","h:5:","host","https://a/"],["https://a/","h:5:","hostname","https://a/"],["https://a/","a:1/b","host","https://a:1/"],["https://a/","a:1/b","hostname","https://a/"],["https://a/","a\\b:5","host","https://a/"],["https://a/","a\\b:5","hostname","https://a/"],["https://a/","x:5#","host","https://x:5/"],["https://a/","x:5#","hostname","https://a/"],["https://a/","x\t:5","host","https://x:5/"],["https://a/","x\t:5","hostname","https://a/"],["https://a/","x:\n5","host","https://x/"],["https://a/","x:\n5","hostname","https://a/"],["https://a/","localhost","host","https://localhost/"],["https://a/","localhost","hostname","https://localhost/"],["https://a/","LOCALHOST:1","host","https://localhost:1/"],["https://a/","LOCALHOST:1","hostname","https://a/"],["https://a/","a%3A5","host","https://a/"],["https://a/","a%3A5","hostname","https://a/"],["https://a/","[::1]","host","https://[::1]/"],["https://a/","[::1]","hostname","https://[::1]/"],["https://a/","","host","https://a/"],["https://a/","","hostname","https://a/"],["https://a/",":","host","https://a/"],["https://a/",":","hostname","https://a/"],["https://a/","::","host","https://a/"],["https://a/","::","hostname","https://a/"],["file://h:/x","[::1]:5","host","file:///h:/x"],["file://h:/x","[::1]:5","hostname","file:///h:/x"],["file://h:/x","[::1","host","file:///h:/x"],["file://h:/x","[::1","hostname","file:///h:/x"],["file://h:/x","a]:5","host","file:///h:/x"],["file://h:/x","a]:5","hostname","file:///h:/x"],["file://h:/x","h:5:","host","file:///h:/x"],["file://h:/x","h:5:","hostname","file:///h:/x"],["file://h:/x","a:1/b","host","file:///h:/x"],["file://h:/x","a:1/b","hostname","file:///h:/x"],["file://h:/x","a\\b:5","host","file://a/h:/x"],["file://h:/x","a\\b:5","hostname","file://a/h:/x"],["file://h:/x","x:5#","host","file:///h:/x"],["file://h:/x","x:5#","hostname","file:///h:/x"],["file://h:/x","x\t:5","host","file:///h:/x"],["file://h:/x","x\t:5","hostname","file:///h:/x"],["file://h:/x","x:\n5","host","file://x/h:/x"],["file://h:/x","x:\n5","hostname","file:///h:/x"],["file://h:/x","localhost","host","file:///h:/x"],["file://h:/x","localhost","hostname","file:///h:/x"],["file://h:/x","LOCALHOST:1","host","file:///h:/x"],["file://h:/x","LOCALHOST:1","hostname","file:///h:/x"],["file://h:/x","a%3A5","host","file:///h:/x"],["file://h:/x","a%3A5","hostname","file:///h:/x"],["file://h:/x","[::1]","host","file://[::1]/h:/x"],["file://h:/x","[::1]","hostname","file://[::1]/h:/x"],["file://h:/x","","host","file:///h:/x"],["file://h:/x","","hostname","file:///h:/x"],["file://h:/x",":","host","file:///h:/x"],["file://h:/x",":","hostname","file:///h:/x"],["file://h:/x","::","host","file:///h:/x"],["file://h:/x","::","hostname","file:///h:/x"],["http://a/","file","protocol","file://a/"],["http://a/","FILE:","protocol","file://a/"],["http://a/","fi\tle","protocol","file://a/"],["http://a/","file:x","protocol","file://a/"],["http://a/","file+x","protocol","http://a/"],["http://a/"," file","protocol","http://a/"],["http://a/","https","protocol","https://a/"],["http://a/","foo","protocol","http://a/"],["http://a/","ws","protocol","ws://a/"],["https://a/p?q#h","file","protocol","file://a/p?q#h"],["https://a/p?q#h","FILE:","protocol","file://a/p?q#h"],["https://a/p?q#h","fi\tle","protocol","file://a/p?q#h"],["https://a/p?q#h","file:x","protocol","file://a/p?q#h"],["https://a/p?q#h","file+x","protocol","https://a/p?q#h"],["https://a/p?q#h"," file","protocol","https://a/p?q#h"],["https://a/p?q#h","https","protocol","https://a/p?q#h"],["https://a/p?q#h","foo","protocol","https://a/p?q#h"],["https://a/p?q#h","ws","protocol","ws://a/p?q#h"],["ws://a:1/","file","protocol","ws://a:1/"],["ws://a:1/","FILE:","protocol","ws://a:1/"],["ws://a:1/","fi\tle","protocol","ws://a:1/"],["ws://a:1/","file:x","protocol","ws://a:1/"],["ws://a:1/","file+x","protocol","ws://a:1/"],["ws://a:1/"," file","protocol","ws://a:1/"],["ws://a:1/","https","protocol","https://a:1/"],["ws://a:1/","foo","protocol","ws://a:1/"],["ws://a:1/","ws","protocol","ws://a:1/"],["http://u@a/","file","protocol","http://u@a/"],["http://u@a/","FILE:","protocol","http://u@a/"],["http://u@a/","fi\tle","protocol","http://u@a/"],["http://u@a/","file:x","protocol","http://u@a/"],["http://u@a/","file+x","protocol","http://u@a/"],["http://u@a/"," file","protocol","http://u@a/"],["http://u@a/","https","protocol","https://u@a/"],["http://u@a/","foo","protocol","http://u@a/"],["http://u@a/","ws","protocol","ws://u@a/"],["ftp://a/","file","protocol","file://a/"],["ftp://a/","FILE:","protocol","file://a/"],["ftp://a/","fi\tle","protocol","file://a/"],["ftp://a/","file:x","protocol","file://a/"],["ftp://a/","file+x","protocol","ftp://a/"],["ftp://a/"," file","protocol","ftp://a/"],["ftp://a/","https","protocol","https://a/"],["ftp://a/","foo","protocol","ftp://a/"],["ftp://a/","ws","protocol","ws://a/"],["foo://a/","file","protocol","foo://a/"],["foo://a/","FILE:","protocol","foo://a/"],["foo://a/","fi\tle","protocol","foo://a/"],["foo://a/","file:x","protocol","foo://a/"],["foo://a/","file+x","protocol","file+x://a/"],["foo://a/"," file","protocol","foo://a/"],["foo://a/","https","protocol","foo://a/"],["foo://a/","foo","protocol","foo://a/"],["foo://a/","ws","protocol","foo://a/"],["http://localhost/","file","protocol","file:///"],["http://localhost/","FILE:","protocol","file:///"],["http://localhost/","fi\tle","protocol","file:///"],["http://localhost/","file:x","protocol","file:///"],["http://localhost/","file+x","protocol","http://localhost/"],["http://localhost/"," file","protocol","http://localhost/"],["http://localhost/","https","protocol","https://localhost/"],["http://localhost/","foo","protocol","http://localhost/"],["http://localhost/","ws","protocol","ws://localhost/"]],"toolCalls":[]}"####,
        ),
        (
            r####"// hostname/host setters with bracketed IPv6 hosts and ports (WebKit)
const t = (href, p, v) => { const u = new URL(href); try { u[p] = v } catch (e) { return "ERR " + e.message } return u.href }
const out = ["https://a:8080/","https://a/","foo://a:1/","http://a/", "http://u:p@a/x?y#z"].map(b => ["[::1]:5","[::1]:443","[::1]:x","[::1]:","[::1]:99999","[::1]x","[::1]:5:6"].map(v => [v, t(b,"hostname",v), t(b, "host", v)]))
return out"####,
            r####"{"ok":true,"value":[[["[::1]:5","https://a:8080/","https://[::1]:5/"],["[::1]:443","https://a:8080/","https://[::1]:8080/"],["[::1]:x","https://a:8080/","https://[::1]:8080/"],["[::1]:","https://a:8080/","https://[::1]:8080/"],["[::1]:99999","https://a:8080/","https://[::1]/"],["[::1]x","https://a:8080/","https://a:8080/"],["[::1]:5:6","https://a:8080/","https://a:8080/"]],[["[::1]:5","https://[::1]:5/","https://[::1]:5/"],["[::1]:443","https://[::1]/","https://[::1]/"],["[::1]:x","https://a/","https://[::1]/"],["[::1]:","https://[::1]/","https://[::1]/"],["[::1]:99999","https://a/","https://[::1]/"],["[::1]x","https://a/","https://a/"],["[::1]:5:6","https://a/","https://a/"]],[["[::1]:5","foo://a:1/","foo://[::1]:5/"],["[::1]:443","foo://a:1/","foo://[::1]:443/"],["[::1]:x","foo://a:1/","foo://[::1]:1/"],["[::1]:","foo://a:1/","foo://[::1]:1/"],["[::1]:99999","foo://a:1/","foo://[::1]/"],["[::1]x","foo://a:1/","foo://a:1/"],["[::1]:5:6","foo://a:1/","foo://a:1/"]],[["[::1]:5","http://[::1]:5/","http://[::1]:5/"],["[::1]:443","http://[::1]:443/","http://[::1]:443/"],["[::1]:x","http://a/","http://[::1]/"],["[::1]:","http://[::1]/","http://[::1]/"],["[::1]:99999","http://a/","http://[::1]/"],["[::1]x","http://a/","http://a/"],["[::1]:5:6","http://a/","http://a/"]],[["[::1]:5","http://u:p@[::1]:5/x?y#z","http://u:p@[::1]:5/x?y#z"],["[::1]:443","http://u:p@[::1]:443/x?y#z","http://u:p@[::1]:443/x?y#z"],["[::1]:x","http://u:p@a/x?y#z","http://u:p@[::1]/x?y#z"],["[::1]:","http://u:p@[::1]/x?y#z","http://u:p@[::1]/x?y#z"],["[::1]:99999","http://u:p@a/x?y#z","http://u:p@[::1]/x?y#z"],["[::1]x","http://u:p@a/x?y#z","http://u:p@a/x?y#z"],["[::1]:5:6","http://u:p@a/x?y#z","http://u:p@a/x?y#z"]]],"toolCalls":[]}"####,
        ),
        (
            r####"// URL parsing over WPT urltestdata-style inputs
const inputs = [["http://example\t.\norg", null], ["http://user:pass@foo:21/bar;par?b#c", null], ["https://test:@test", null], ["https://:@test", null], ["non-special://test:@test/x", null], ["non-special://:@test/x", null], ["http:foo.com", "http://example.org/foo/bar"], ["\t   :foo.com   \n", "http://example.org/foo/bar"], [" foo.com  ", "http://example.org/foo/bar"], ["a:\t foo.com", "http://example.org/foo/bar"], ["http://f:21/ b ? d # e ", "http://example.org/foo/bar"], ["lolscheme:x x#x x", null], ["http://f:/c", null], ["http://f:0/c", null], ["http://f:00000000000000/c", null], ["http://f:00000000000000000000080/c", null], ["http://f:b/c", null], ["http://f: /c", null], ["http://f:\n/c", null], ["http://f:fifty-two/c", null], ["http://f:999999/c", null], ["non-special://f:999999/c", null], ["http://f: 21 / b ? d # e ", null], ["", "http://example.org/foo/bar"], ["  \t", "http://example.org/foo/bar"], [":foo.com/", "http://example.org/foo/bar"], [":", "http://example.org/foo/bar"], [":a", "http://example.org/foo/bar"], [":/", "http://example.org/foo/bar"], [":#", "http://example.org/foo/bar"], ["#", "http://example.org/foo/bar"], ["#/", "http://example.org/foo/bar"], ["#\\", "http://example.org/foo/bar"], ["#;?", "http://example.org/foo/bar"], ["?", "http://example.org/foo/bar"], ["/", "http://example.org/foo/bar"], [":23", "http://example.org/foo/bar"], ["/:23", "http://example.org/foo/bar"], ["\\x", "http://example.org/foo/bar"], ["\\\\x\\hello", "http://example.org/foo/bar"], ["::", "http://example.org/foo/bar"], ["::23", "http://example.org/foo/bar"], ["foo://", "http://example.org/foo/bar"], ["http://a:b@c:29/d", "http://example.org/foo/bar"], ["http::@c:29", "http://example.org/foo/bar"], ["http://&a:foo(b]c@d:2/", "http://example.org/foo/bar"], ["http://::@c@d:2", "http://example.org/foo/bar"], ["http://foo.com:b@d/", "http://example.org/foo/bar"], ["http://foo.com/\\@", "http://example.org/foo/bar"], ["http:\\\\foo.com\\", "http://example.org/foo/bar"], ["http:\\\\a\\b:c\\d@foo.com\\", "http://example.org/foo/bar"], ["foo:/", "http://example.org/foo/bar"], ["foo:/bar.com/", "http://example.org/foo/bar"], ["foo://///////", "http://example.org/foo/bar"], ["foo://///////bar.com/", "http://example.org/foo/bar"], ["foo:////://///", "http://example.org/foo/bar"], ["c:/foo", "http://example.org/foo/bar"], ["//foo/bar", "http://example.org/foo/bar"], ["http://foo/path;a??e#f#g", "http://example.org/foo/bar"], ["http://foo/abcd?efgh?ijkl", "http://example.org/foo/bar"], ["http://foo/abcd#foo?bar", "http://example.org/foo/bar"], ["[61:24:74]:98", "http://example.org/foo/bar"], ["http:[61:27]/:foo", "http://example.org/foo/bar"], ["http://[1::2]:3:4", "http://example.org/foo/bar"], ["http://2001::1", "http://example.org/foo/bar"], ["http://2001::1]", "http://example.org/foo/bar"], ["http://2001::1]:80", "http://example.org/foo/bar"], ["http://[2001::1]", "http://example.org/foo/bar"], ["http://[::127.0.0.1]", "http://example.org/foo/bar"], ["http://[0:0:0:0:0:0:13.1.68.3]", "http://example.org/foo/bar"], ["http://[2001::1]:80", "http://example.org/foo/bar"], ["http:/example.com/", "http://example.org/foo/bar"], ["ftp:/example.com/", "http://example.org/foo/bar"], ["https:/example.com/", "http://example.org/foo/bar"], ["madeupscheme:/example.com/", "http://example.org/foo/bar"], ["file:/example.com/", "http://example.org/foo/bar"], ["ftps:/example.com/", "http://example.org/foo/bar"], ["gopher:/example.com/", "http://example.org/foo/bar"], ["ws:/example.com/", "http://example.org/foo/bar"], ["data:/example.com/", "http://example.org/foo/bar"], ["javascript:/example.com/", "http://example.org/foo/bar"], ["mailto:/example.com/", "http://example.org/foo/bar"], ["http:example.com/", "http://example.org/foo/bar"], ["ftp:example.com/", "http://example.org/foo/bar"], ["/a/b/c", "http://example.org/foo/bar"], ["/a/ /c", "http://example.org/foo/bar"], ["/a%2fc", "http://example.org/foo/bar"], ["/a/%2f/c", "http://example.org/foo/bar"], ["#β", "http://example.org/foo/bar"], ["data:text/html,test#test", "http://example.org/foo/bar"], ["tel:1234567890", "http://example.org/foo/bar"], ["ssh://example.com/foo/bar.git", "http://example.org/"], ["file:c:\\foo\\bar.html", "file:///tmp/mock/path"], ["  File:c|////foo\\bar.html", "file:///tmp/mock/path"], ["C|/foo/bar", "file:///tmp/mock/path"], ["/C|\\foo\\bar", "file:///tmp/mock/path"], ["//C|/foo/bar", "file:///tmp/mock/path"], ["//server/file", "file:///tmp/mock/path"], ["\\\\server\\file", "file:///tmp/mock/path"], ["/\\server/file", "file:///tmp/mock/path"], ["file:///foo/bar.txt", "file:///tmp/mock/path"], ["file:///home/me", "file:///tmp/mock/path"], ["//", "file:///tmp/mock/path"], ["///", "file:///tmp/mock/path"], ["///test", "file:///tmp/mock/path"], ["file://test", "file:///tmp/mock/path"], ["file://localhost", "file:///tmp/mock/path"], ["file://localhost/", "file:///tmp/mock/path"], ["file://localhost/test", "file:///tmp/mock/path"], ["test", "file:///tmp/mock/path"], ["file:test", "file:///tmp/mock/path"], ["http://example.com/././foo", null], ["http://example.com/./.foo", null], ["http://example.com/foo/.", null], ["http://example.com/foo/./", null], ["http://example.com/foo/bar/..", null], ["http://example.com/foo/bar/../", null], ["http://example.com/foo/..bar", null], ["http://example.com/foo/bar/../ton/../../a", null], ["http://example.com/foo/../../..", null], ["http://example.com/foo/../../../ton", null], ["http://example.com/foo/%2e", null], ["http://example.com/foo/%2e%2", null], ["http://example.com/foo/%2e./%2e%2e/.%2e/%2e.bar", null], ["http://example.com////../..", null], ["http://example.com/foo/bar//../..", null], ["http://example.com/foo/bar//..", null], ["http://example.com/foo", null], ["http://example.com/%20foo", null], ["http://example.com/foo%", null], ["http://example.com/foo%2", null], ["http://example.com/foo%2zbar", null], ["http://example.com/foo%2Â©zbar", null], ["http://example.com/foo%41%7a", null], ["http://example.com/foo\t\u0091%91", null], ["http://example.com/foo%00%51", null], ["http://example.com/(%28:%3A%29)", null], ["http://example.com/%3A%3a%3C%3c", null], ["http://example.com/foo\tbar", null], ["http://example.com\\\\foo\\\\bar", null], ["http://example.com/%%20%%20", null], ["http://example.com/\u4f60\u597d\u4f60\u597d", null], ["http://example.com/\u2025/foo", null], ["http://example.com/\ufeff/foo", null], ["http://example.com/\u202e/foo/\u202d/bar", null], ["http://www.google.com/foo?bar=baz#", null], ["http://www.google.com/foo?bar=baz# »", null], ["data:test# »", null], ["http://www.google.com", null], ["http://192.0x00A80001", null], ["http://www/foo%2Ehtml", null], ["http://user:pass@/", null], ["http://%25DOMAIN:foobar@foodomain.com/", null], ["http:\\\\www.google.com\\foo", null], ["http://foo:80/", null], ["http://foo:81/", null], ["httpa://foo:80/", null], ["http://foo:-80/", null], ["https://foo:443/", null], ["https://foo:80/", null], ["ftp://foo:21/", null], ["ftp://foo:80/", null], ["gopher://foo:70/", null], ["gopher://foo:443/", null], ["ws://foo:80/", null], ["ws://foo:81/", null], ["ws://foo:443/", null], ["ws://foo:815/", null], ["wss://foo:80/", null], ["wss://foo:81/", null], ["wss://foo:443/", null], ["wss://foo:815/", null], ["http:@www.example.com", null], ["http:/@www.example.com", null], ["http://@www.example.com", null], ["http:a:b@www.example.com", null], ["http:/a:b@www.example.com", null], ["http://a:b@www.example.com", null], ["http://@pple.com", null], ["http::b@www.example.com", null], ["http:/:b@www.example.com", null], ["http://:b@www.example.com", null], ["http:/:@/www.example.com", null], ["http://user@/www.example.com", null], ["http:@/www.example.com", null], ["http:/@/www.example.com", null], ["http://@/www.example.com", null], ["https:@/www.example.com", null], ["http:a:b@/www.example.com", null], ["http:/a:b@/www.example.com", null], ["http://a:b@/www.example.com", null], ["http::@/www.example.com", null], ["http:a:@www.example.com", null], ["http:/a:@www.example.com", null], ["http://a:@www.example.com", null], ["http://www.@pple.com", null], ["http:@:www.example.com", null], ["http:/@:www.example.com", null], ["http://@:www.example.com", null], ["http://:@www.example.com", null], ["/", "http://www.example.com/test"], ["/test.txt", "http://www.example.com/test"], [".", "http://www.example.com/test"], ["..", "http://www.example.com/test"], ["test.txt", "http://www.example.com/test"], ["./test.txt", "http://www.example.com/test"], ["../test.txt", "http://www.example.com/test"], ["../aaa/test.txt", "http://www.example.com/test"], ["../../test.txt", "http://www.example.com/test"], ["\u4e2d/test.txt", "http://www.example.com/test"], ["http://www.example2.com", "http://www.example.com/test"], ["//www.example2.com", "http://www.example.com/test"], ["file:...", "http://www.example.com/test"], ["file:..", "http://www.example.com/test"], ["file:a", "http://www.example.com/test"], ["http://ExAmPlE.CoM", null], ["http://example example.com", null], ["http://Goo%20 goo%7C|.com", null], ["http://[]", null], ["http://[:]", null], ["http://GOO\u00a0\u3000goo.com", null], ["http://GOO\u200b\u2060\ufeffgoo.com", null], ["\u0000\u001b\u0004\u0012 http://example.com/\u001f \u000d ", null], ["http://www.foo\u3002bar.com", null], ["http://\ufdd0zyx.com", null], ["http://%ef%b7%90zyx.com", null], ["http://\uff27\uff4f.com", null], ["http://\uff05\uff14\uff11.com", null], ["http://%ef%bc%85%ef%bc%94%ef%bc%91.com", null], ["http://\uff05\uff10\uff10.com", null], ["http://%ef%bc%85%ef%bc%90%ef%bc%90.com", null], ["http://\u4f60\u597d\u4f60\u597d", null], ["https://faß.ExAmPlE/", null], ["sc://faß.ExAmPlE/", null], ["http://%zz%66%a.com", null], ["http://%25", null], ["http://hello%00", null], ["http://%30%78%63%30%2e%30%32%35%30.01", null], ["http://%30%78%63%30%2e%30%32%35%30.01%2e", null], ["http://192.168.0.257", null], ["http://%3g%78%63%30%2e%30%32%35%30%2E.01", null], ["http://192.168.0.1 hello", null], ["https://x x:12", null], ["http://\uff10\uff38\uff43\uff10\uff0e\uff10\uff12\uff15\uff10\uff0e\uff10\uff11", null], ["http://./", null], ["http://../", null], ["h://.", null], ["http://[www.google.com]/", null], ["http://host/?'", null], ["notspecial://host/?'", null], ["/some/path", "http://user@example.org/smth"], ["", "http://user:pass@example.org:21/smth"], ["/some/path", "http://user:pass@example.org:21/smth"], ["i", "sc:sd"], ["i", "sc:sd/sd"], ["i", "sc:/pa/pa"], ["i", "sc://ho/pa"], ["i", "sc:///pa/pa"], ["../i", "sc:sd"], ["../i", "sc:/pa/pa"], ["/i", "sc:sd"], ["?i", "sc:sd"], ["#i", "sc:sd"], ["about:/../", null], ["data:/../", null], ["javascript:/../", null], ["mailto:/../", null], ["sc://ñ.test/", null], ["sc://%/", null], ["sc://@/", null], ["sc://te@s:t@/", null], ["sc://:/", null], ["sc://:12/", null], ["x", "sc://ñ"], ["sc:\\../", null], ["sc::a@example.net", null], ["sc:a@example.net", null], ["sc:\\a@example.net", null], ["sc::@example.net", null], ["sc:\\@example.net", null], ["sc://\u001f!\"$&'()*+,-.;=_`{}~/", null], ["sc://\u0000/", null], ["sc:// /", null], ["sc://</", null], ["sc://>/", null], ["sc://^/", null], ["sc://|/", null], ["http://1.2.3.4/", null], ["http://0x100.2.3.4/", null], ["http://4294967295/", null], ["http://4294967296/", null], ["http://0xffffffff1/", null], ["http://256.256.256.256/", null], ["https://0x.0x.0", null], ["https://0x100000000/test", null], ["https://256.0.0.1/test", null], ["http://[0:1:0:1:0:1:0:1]", null], ["http://[1:0:1:0:1:0:1:0]", null], ["http://example.com/\ud800\udfff", null], ["http://example.com/\ud800", null], ["http://[1:0::]", null], ["http://[0:0:0:0:0:0:0:0:0]", null], ["http://[1:2:3:4:5:6:7:8:9]", null], ["http://[1::2::3]", null], ["http://[::1.2.3.]", null], ["blob:https://example.com:443/", null], ["blob:d3958f5c-0777-0845-9dcf-2cb28783acaf", null], ["http://0..0x300/", null], ["http://0..0x300./", null], ["http://256.256.256.256.256", null], ["http://1.2.3.08", null], ["http://1.2.3.09", null], ["http://09.2.3.4", null], ["http://1.2.3.4.5", null], ["http://01.2.3.4.5", null], ["http://0x100.2.3.4.5", null], ["http://foo.0x4", null], ["http://foo.09", null], ["http://0999999999999999999/", null], ["http://foo.0x", null], ["http://foo.0XFfFfFfFfFfFfFfFfFfAcE123", null], ["http://\ud83d\udca9.123/", null], ["https://\u0000y", null], ["https://x/\u0000y", null], ["https://x/?\u0000y", null], ["https://x/?#\u0000y", null], ["non-special://host\\a", null], ["non-special:opaque  ", null], ["non-special:opaque  ?hi", null], ["non-special:opaque  #hi", null], ["non-special:opaque  x?hi", null], ["http://a.b.c.xn--pokxncvks", null], ["http://10.0.0.xn--pokxncvks", null], ["http://a.b.c.XN--pokxncvks", null], ["https://xn--/", null], ["https://xn--a/", null], ["https://a.xn--/", null], ["https://x/x#x x", null], ["https://example.com/ \u00e9", null], ["https://www.example.com/path{\u007fpath.html?query'\u007f=query#fragment<\u007ffragment", null], ["https://user:pass[\u007f@foo/bar", null], ["https://\u00ad/", null], ["https://xn--ls8h.la/", null], ["http://a\u00adb/", null], ["https://example.com/foo?bar#baz", "about:blank"], ["#x", "about:blank"], ["#x", "data:,"], ["#x", "sc:\\a"], ["?x", "sc:\\a"]]
const out = inputs.map(([i, b]) => { try { const u = b === null ? new URL(i) : new URL(i, b); return [u.href, u.origin, u.host, u.pathname] } catch (e) { return "ERR" } })
return out"####,
            r####"{"ok":true,"value":[["http://example.org/","http://example.org","example.org","/"],["http://user:pass@foo:21/bar;par?b#c","http://foo:21","foo:21","/bar;par"],["https://test@test/","https://test","test","/"],["https://test/","https://test","test","/"],["non-special://test@test/x","null","test","/x"],["non-special://test/x","null","test","/x"],["http://example.org/foo/foo.com","http://example.org","example.org","/foo/foo.com"],["http://example.org/foo/:foo.com","http://example.org","example.org","/foo/:foo.com"],["http://example.org/foo/foo.com","http://example.org","example.org","/foo/foo.com"],["a: foo.com","null",""," foo.com"],["http://f:21/%20b%20?%20d%20#%20e","http://f:21","f:21","/%20b%20"],["lolscheme:x x#x%20x","null","","x x"],["http://f/c","http://f","f","/c"],["http://f:0/c","http://f:0","f:0","/c"],["http://f:0/c","http://f:0","f:0","/c"],["http://f/c","http://f","f","/c"],"ERR","ERR",["http://f/c","http://f","f","/c"],"ERR","ERR","ERR","ERR",["http://example.org/foo/bar","http://example.org","example.org","/foo/bar"],["http://example.org/foo/bar","http://example.org","example.org","/foo/bar"],["http://example.org/foo/:foo.com/","http://example.org","example.org","/foo/:foo.com/"],["http://example.org/foo/:","http://example.org","example.org","/foo/:"],["http://example.org/foo/:a","http://example.org","example.org","/foo/:a"],["http://example.org/foo/:/","http://example.org","example.org","/foo/:/"],["http://example.org/foo/:#","http://example.org","example.org","/foo/:"],["http://example.org/foo/bar#","http://example.org","example.org","/foo/bar"],["http://example.org/foo/bar#/","http://example.org","example.org","/foo/bar"],["http://example.org/foo/bar#\\","http://example.org","example.org","/foo/bar"],["http://example.org/foo/bar#;?","http://example.org","example.org","/foo/bar"],["http://example.org/foo/bar?","http://example.org","example.org","/foo/bar"],["http://example.org/","http://example.org","example.org","/"],["http://example.org/foo/:23","http://example.org","example.org","/foo/:23"],["http://example.org/:23","http://example.org","example.org","/:23"],["http://example.org/x","http://example.org","example.org","/x"],["http://x/hello","http://x","x","/hello"],["http://example.org/foo/::","http://example.org","example.org","/foo/::"],["http://example.org/foo/::23","http://example.org","example.org","/foo/::23"],["foo://","null","",""],["http://a:b@c:29/d","http://c:29","c:29","/d"],["http://example.org/foo/:@c:29","http://example.org","example.org","/foo/:@c:29"],["http://&a:foo(b%5Dc@d:2/","http://d:2","d:2","/"],["http://:%3A%40c@d:2/","http://d:2","d:2","/"],["http://foo.com:b@d/","http://d","d","/"],["http://foo.com//@","http://foo.com","foo.com","//@"],["http://foo.com/","http://foo.com","foo.com","/"],["http://a/b:c/d@foo.com/","http://a","a","/b:c/d@foo.com/"],["foo:/","null","","/"],["foo:/bar.com/","null","","/bar.com/"],["foo://///////","null","","///////"],["foo://///////bar.com/","null","","///////bar.com/"],["foo:////://///","null","","//://///"],["c:/foo","null","","/foo"],["http://foo/bar","http://foo","foo","/bar"],["http://foo/path;a??e#f#g","http://foo","foo","/path;a"],["http://foo/abcd?efgh?ijkl","http://foo","foo","/abcd"],["http://foo/abcd#foo?bar","http://foo","foo","/abcd"],["http://example.org/foo/[61:24:74]:98","http://example.org","example.org","/foo/[61:24:74]:98"],["http://example.org/foo/[61:27]/:foo","http://example.org","example.org","/foo/[61:27]/:foo"],"ERR","ERR","ERR","ERR",["http://[2001::1]/","http://[2001::1]","[2001::1]","/"],["http://[::7f00:1]/","http://[::7f00:1]","[::7f00:1]","/"],["http://[::d01:4403]/","http://[::d01:4403]","[::d01:4403]","/"],["http://[2001::1]/","http://[2001::1]","[2001::1]","/"],["http://example.org/example.com/","http://example.org","example.org","/example.com/"],["ftp://example.com/","ftp://example.com","example.com","/"],["https://example.com/","https://example.com","example.com","/"],["madeupscheme:/example.com/","null","","/example.com/"],["file:///example.com/","null","","/example.com/"],["ftps:/example.com/","ftps:","","/example.com/"],["gopher:/example.com/","null","","/example.com/"],["ws://example.com/","ws://example.com","example.com","/"],["data:/example.com/","null","","/example.com/"],["javascript:/example.com/","null","","/example.com/"],["mailto:/example.com/","null","","/example.com/"],["http://example.org/foo/example.com/","http://example.org","example.org","/foo/example.com/"],["ftp://example.com/","ftp://example.com","example.com","/"],["http://example.org/a/b/c","http://example.org","example.org","/a/b/c"],["http://example.org/a/%20/c","http://example.org","example.org","/a/%20/c"],["http://example.org/a%2fc","http://example.org","example.org","/a%2fc"],["http://example.org/a/%2f/c","http://example.org","example.org","/a/%2f/c"],["http://example.org/foo/bar#%CE%B2","http://example.org","example.org","/foo/bar"],["data:text/html,test#test","null","","text/html,test"],["tel:1234567890","null","","1234567890"],["ssh://example.com/foo/bar.git","null","example.com","/foo/bar.git"],["file:///c:/foo/bar.html","null","","/c:/foo/bar.html"],["file:///c:////foo/bar.html","null","","/c:////foo/bar.html"],["file:///C:/foo/bar","null","","/C:/foo/bar"],["file:///C:/foo/bar","null","","/C:/foo/bar"],["file:///C:/foo/bar","null","","/C:/foo/bar"],["file://server/file","null","server","/file"],["file://server/file","null","server","/file"],["file://server/file","null","server","/file"],["file:///foo/bar.txt","null","","/foo/bar.txt"],["file:///home/me","null","","/home/me"],["file:///","null","","/"],["file:///","null","","/"],["file:///test","null","","/test"],["file://test/","null","test","/"],["file:///","null","","/"],["file:///","null","","/"],["file:///test","null","","/test"],["file:///tmp/mock/test","null","","/tmp/mock/test"],["file:///tmp/mock/test","null","","/tmp/mock/test"],["http://example.com/foo","http://example.com","example.com","/foo"],["http://example.com/.foo","http://example.com","example.com","/.foo"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/..bar","http://example.com","example.com","/foo/..bar"],["http://example.com/a","http://example.com","example.com","/a"],["http://example.com/","http://example.com","example.com","/"],["http://example.com/ton","http://example.com","example.com","/ton"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/%2e%2","http://example.com","example.com","/foo/%2e%2"],["http://example.com/%2e.bar","http://example.com","example.com","/%2e.bar"],["http://example.com//","http://example.com","example.com","//"],["http://example.com/foo/","http://example.com","example.com","/foo/"],["http://example.com/foo/bar/","http://example.com","example.com","/foo/bar/"],["http://example.com/foo","http://example.com","example.com","/foo"],["http://example.com/%20foo","http://example.com","example.com","/%20foo"],["http://example.com/foo%","http://example.com","example.com","/foo%"],["http://example.com/foo%2","http://example.com","example.com","/foo%2"],["http://example.com/foo%2zbar","http://example.com","example.com","/foo%2zbar"],["http://example.com/foo%2%C3%82%C2%A9zbar","http://example.com","example.com","/foo%2%C3%82%C2%A9zbar"],["http://example.com/foo%41%7a","http://example.com","example.com","/foo%41%7a"],["http://example.com/foo%C2%91%91","http://example.com","example.com","/foo%C2%91%91"],["http://example.com/foo%00%51","http://example.com","example.com","/foo%00%51"],["http://example.com/(%28:%3A%29)","http://example.com","example.com","/(%28:%3A%29)"],["http://example.com/%3A%3a%3C%3c","http://example.com","example.com","/%3A%3a%3C%3c"],["http://example.com/foobar","http://example.com","example.com","/foobar"],["http://example.com//foo//bar","http://example.com","example.com","//foo//bar"],["http://example.com/%%20%%20","http://example.com","example.com","/%%20%%20"],["http://example.com/%E4%BD%A0%E5%A5%BD%E4%BD%A0%E5%A5%BD","http://example.com","example.com","/%E4%BD%A0%E5%A5%BD%E4%BD%A0%E5%A5%BD"],["http://example.com/%E2%80%A5/foo","http://example.com","example.com","/%E2%80%A5/foo"],["http://example.com/%EF%BB%BF/foo","http://example.com","example.com","/%EF%BB%BF/foo"],["http://example.com/%E2%80%AE/foo/%E2%80%AD/bar","http://example.com","example.com","/%E2%80%AE/foo/%E2%80%AD/bar"],["http://www.google.com/foo?bar=baz#","http://www.google.com","www.google.com","/foo"],["http://www.google.com/foo?bar=baz#%20%C2%BB","http://www.google.com","www.google.com","/foo"],["data:test#%20%C2%BB","null","","test"],["http://www.google.com/","http://www.google.com","www.google.com","/"],["http://192.168.0.1/","http://192.168.0.1","192.168.0.1","/"],["http://www/foo%2Ehtml","http://www","www","/foo%2Ehtml"],"ERR",["http://%25DOMAIN:foobar@foodomain.com/","http://foodomain.com","foodomain.com","/"],["http://www.google.com/foo","http://www.google.com","www.google.com","/foo"],["http://foo/","http://foo","foo","/"],["http://foo:81/","http://foo:81","foo:81","/"],["httpa://foo:80/","null","foo:80","/"],"ERR",["https://foo/","https://foo","foo","/"],["https://foo:80/","https://foo:80","foo:80","/"],["ftp://foo/","ftp://foo","foo","/"],["ftp://foo:80/","ftp://foo:80","foo:80","/"],["gopher://foo:70/","null","foo:70","/"],["gopher://foo:443/","null","foo:443","/"],["ws://foo/","ws://foo","foo","/"],["ws://foo:81/","ws://foo:81","foo:81","/"],["ws://foo:443/","ws://foo:443","foo:443","/"],["ws://foo:815/","ws://foo:815","foo:815","/"],["wss://foo:80/","wss://foo:80","foo:80","/"],["wss://foo:81/","wss://foo:81","foo:81","/"],["wss://foo/","wss://foo","foo","/"],["wss://foo:815/","wss://foo:815","foo:815","/"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://a:b@www.example.com/","http://www.example.com","www.example.com","/"],["http://a:b@www.example.com/","http://www.example.com","www.example.com","/"],["http://a:b@www.example.com/","http://www.example.com","www.example.com","/"],["http://pple.com/","http://pple.com","pple.com","/"],["http://:b@www.example.com/","http://www.example.com","www.example.com","/"],["http://:b@www.example.com/","http://www.example.com","www.example.com","/"],["http://:b@www.example.com/","http://www.example.com","www.example.com","/"],"ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR",["http://a@www.example.com/","http://www.example.com","www.example.com","/"],["http://a@www.example.com/","http://www.example.com","www.example.com","/"],["http://a@www.example.com/","http://www.example.com","www.example.com","/"],["http://www.@pple.com/","http://pple.com","pple.com","/"],"ERR","ERR","ERR",["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/test.txt","http://www.example.com","www.example.com","/test.txt"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/","http://www.example.com","www.example.com","/"],["http://www.example.com/test.txt","http://www.example.com","www.example.com","/test.txt"],["http://www.example.com/test.txt","http://www.example.com","www.example.com","/test.txt"],["http://www.example.com/test.txt","http://www.example.com","www.example.com","/test.txt"],["http://www.example.com/aaa/test.txt","http://www.example.com","www.example.com","/aaa/test.txt"],["http://www.example.com/test.txt","http://www.example.com","www.example.com","/test.txt"],["http://www.example.com/%E4%B8%AD/test.txt","http://www.example.com","www.example.com","/%E4%B8%AD/test.txt"],["http://www.example2.com/","http://www.example2.com","www.example2.com","/"],["http://www.example2.com/","http://www.example2.com","www.example2.com","/"],["file:///...","null","","/..."],["file:///","null","","/"],["file:///a","null","","/a"],["http://example.com/","http://example.com","example.com","/"],"ERR","ERR","ERR","ERR","ERR",["http://googoo.com/","http://googoo.com","googoo.com","/"],["http://example.com/","http://example.com","example.com","/"],["http://www.foo.bar.com/","http://www.foo.bar.com","www.foo.bar.com","/"],"ERR","ERR",["http://go.com/","http://go.com","go.com","/"],"ERR","ERR","ERR","ERR",["http://xn--6qqa088eba/","http://xn--6qqa088eba","xn--6qqa088eba","/"],["https://xn--fa-hia.example/","https://xn--fa-hia.example","xn--fa-hia.example","/"],["sc://fa%C3%9F.ExAmPlE/","null","fa%C3%9F.ExAmPlE","/"],"ERR","ERR","ERR",["http://192.168.0.1/","http://192.168.0.1","192.168.0.1","/"],["http://192.168.0.1/","http://192.168.0.1","192.168.0.1","/"],"ERR","ERR","ERR","ERR",["http://192.168.0.1/","http://192.168.0.1","192.168.0.1","/"],["http://./","http://.",".","/"],["http://../","http://..","..","/"],["h://.","null",".",""],"ERR",["http://host/?%27","http://host","host","/"],["notspecial://host/?'","null","host","/"],["http://user@example.org/some/path","http://example.org","example.org","/some/path"],["http://user:pass@example.org:21/smth","http://example.org:21","example.org:21","/smth"],["http://user:pass@example.org:21/some/path","http://example.org:21","example.org:21","/some/path"],"ERR","ERR",["sc:/pa/i","null","","/pa/i"],["sc://ho/i","null","ho","/i"],["sc:///pa/i","null","","/pa/i"],"ERR",["sc:/i","null","","/i"],"ERR","ERR",["sc:sd#i","null","","sd"],["about:/","null","","/"],["data:/","null","","/"],["javascript:/","null","","/"],["mailto:/","null","","/"],["sc://%C3%B1.test/","null","%C3%B1.test","/"],["sc://%/","null","%","/"],"ERR","ERR","ERR","ERR",["sc://%C3%B1/x","null","%C3%B1","/x"],["sc:\\../","null","","\\../"],["sc::a@example.net","null","",":a@example.net"],["sc:a@example.net","null","","a@example.net"],["sc:\\a@example.net","null","","\\a@example.net"],["sc::@example.net","null","",":@example.net"],["sc:\\@example.net","null","","\\@example.net"],["sc://%1F!\"$&'()*+,-.;=_`{}~/","null","%1F!\"$&'()*+,-.;=_`{}~","/"],"ERR","ERR","ERR","ERR","ERR","ERR",["http://1.2.3.4/","http://1.2.3.4","1.2.3.4","/"],"ERR",["http://255.255.255.255/","http://255.255.255.255","255.255.255.255","/"],"ERR","ERR","ERR",["https://0.0.0.0/","https://0.0.0.0","0.0.0.0","/"],"ERR","ERR",["http://[0:1:0:1:0:1:0:1]/","http://[0:1:0:1:0:1:0:1]","[0:1:0:1:0:1:0:1]","/"],["http://[1:0:1:0:1:0:1:0]/","http://[1:0:1:0:1:0:1:0]","[1:0:1:0:1:0:1:0]","/"],["http://example.com/%F0%90%8F%BF","http://example.com","example.com","/%F0%90%8F%BF"],["http://example.com/%EF%BF%BD","http://example.com","example.com","/%EF%BF%BD"],["http://[1::]/","http://[1::]","[1::]","/"],"ERR","ERR","ERR","ERR",["blob:https://example.com:443/","https://example.com","","https://example.com:443/"],["blob:d3958f5c-0777-0845-9dcf-2cb28783acaf","null","","d3958f5c-0777-0845-9dcf-2cb28783acaf"],"ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR","ERR",["https://x/%00y","https://x","x","/%00y"],["https://x/?%00y","https://x","x","/"],["https://x/?#%00y","https://x","x","/"],"ERR",["non-special:opaque","null","","opaque"],["non-special:opaque %20?hi","null","","opaque %20"],["non-special:opaque %20#hi","null","","opaque %20"],["non-special:opaque  x?hi","null","","opaque  x"],"ERR","ERR","ERR","ERR","ERR","ERR",["https://x/x#x%20x","https://x","x","/x"],["https://example.com/%20%C3%A9","https://example.com","example.com","/%20%C3%A9"],["https://www.example.com/path%7B%7Fpath.html?query%27%7F=query#fragment%3C%7Ffragment","https://www.example.com","www.example.com","/path%7B%7Fpath.html"],["https://user:pass%5B%7F@foo/bar","https://foo","foo","/bar"],"ERR",["https://xn--ls8h.la/","https://xn--ls8h.la","xn--ls8h.la","/"],["http://ab/","http://ab","ab","/"],["https://example.com/foo?bar#baz","https://example.com","example.com","/foo"],["about:blank#x","null","","blank"],["data:,#x","null","",","],["sc:\\a#x","null","","\\a"],"ERR"],"toolCalls":[]}"####,
        ),
        (
            r####"// origins (ftps, blob) and opaque paths with trailing spaces
const out = ["ftps://h/","ftps:/x","ftps:x","sftp://h/","gopher://h/","ftp:/x", "blob:ftps://h/", "foo:/x", "ftps://h:21/", "FTPS://H/", "blob:https://a.com/x", "blob:http://a:80/", "blob:ws://a/", "blob:ftp://a/", "blob:file:///x", "blob:blob:https://a/", "blob:foo://a/", "blob:", "blob:x", "blob:https://u:p@a.com:8443/x?y#z", "blob:HTTPS://A.com/", "file:///x", "file://h/x", "ws://h:80", "wss://h:444", "https://[::1]:8/", "https://xn--nxasmq6b.com/", "https://ü.com/", "data:,x", "javascript:x", "about:blank", "foo:a  ?q#h", "foo:a  #h", "foo:a  ", "foo:a \t?q"].map(x=>{try{const u = new URL(x); return [u.href,u.origin,u.pathname]}catch(e){return "ERR"}})
const u = new URL("foo:a  ?q#h"); out.push(u.href); u.search=""; out.push(u.href); u.hash=""; out.push(u.href)
return out"####,
            r####"{"ok":true,"value":[["ftps://h/","ftps://h","/"],["ftps:/x","ftps:","/x"],["ftps:x","ftps:","x"],["sftp://h/","null","/"],["gopher://h/","null","/"],["ftp://x/","ftp://x","/"],["blob:ftps://h/","ftps://h","ftps://h/"],["foo:/x","null","/x"],["ftps://h:21/","ftps://h:21","/"],["ftps://H/","ftps://H","/"],["blob:https://a.com/x","https://a.com","https://a.com/x"],["blob:http://a:80/","http://a","http://a:80/"],["blob:ws://a/","ws://a","ws://a/"],["blob:ftp://a/","ftp://a","ftp://a/"],["blob:file:///x","file://","file:///x"],["blob:blob:https://a/","null","blob:https://a/"],["blob:foo://a/","null","foo://a/"],["blob:","null",""],["blob:x","null","x"],["blob:https://u:p@a.com:8443/x?y#z","https://a.com:8443","https://u:p@a.com:8443/x"],["blob:HTTPS://A.com/","https://a.com","HTTPS://A.com/"],["file:///x","null","/x"],["file://h/x","null","/x"],["ws://h/","ws://h","/"],["wss://h:444/","wss://h:444","/"],["https://[::1]:8/","https://[::1]:8","/"],["https://xn--nxasmq6b.com/","https://xn--nxasmq6b.com","/"],["https://xn--tda.com/","https://xn--tda.com","/"],["data:,x","null",",x"],["javascript:x","null","x"],["about:blank","null","blank"],["foo:a %20?q#h","null","a %20"],["foo:a %20#h","null","a %20"],["foo:a","null","a"],["foo:a%20?q","null","a%20"],"foo:a %20?q#h","foo:a %20#h","foo:a %20"],"toolCalls":[]}"####,
        ),
    ]);
}

/// web-platform-tests ports (atob/btoa, randomUUID, URI coding), as in upstream web-wpt.test.ts.
#[test]
fn web_platform_tests() {
    check(&[
        (
            r####"// atob over the WPT forgiving-base64 fixture (fetch/data-urls/resources/base64.json)
const cases = ["", "abcd", " abcd", "abcd ", " abcd===", "abcd=== ", "abcd ===", "a", "ab", "abc", "abcde", "\ud800\udc00", "=", "==", "===", "====", "=====", "a=", "a==", "a===", "a====", "a=====", "ab=", "ab==", "ab===", "ab====", "ab=====", "abc=", "abc==", "abc===", "abc====", "abc=====", "abcd=", "abcd==", "abcd===", "abcd====", "abcd=====", "abcde=", "abcde==", "abcde===", "abcde====", "abcde=====", "=a", "=a=", "a=b", "a=b=", "ab=c", "ab=c=", "abc=d", "abc=d=", "ab\u000bcd", "ab\u3000cd", "ab\u3001cd", "ab\tcd", "ab\ncd", "ab\fcd", "ab\rcd", "ab cd", "ab\u00a0cd", "ab\t\n\f\r cd", " \t\n\f\r ab\t\n\f\r cd\t\n\f\r ", "ab\t\n\f\r =\t\n\f\r =\t\n\f\r ", "A", "/A", "//A", "///A", "////A", "/", "A/", "AA/", "AAAA/", "AAA/", "\u0000nonsense", "abcd\u0000nonsense", "YQ", "YR", "~~", "..", "--", "__"]
return cases.map((input) => { try { return atob(input) } catch (error) { return error instanceof TypeError ? "TypeError: " + error.message : error.name } })"####,
            r####"{"ok":true,"value":["","i·\u001d","i·\u001d","i·\u001d","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","i","i·","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","i","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","i·","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","i·\u001d","i·\u001d","i·\u001d","i·\u001d","i·\u001d","TypeError: The string contains invalid characters.","i·\u001d","i·\u001d","i","TypeError: The string contains invalid characters.","ü","ÿð","ÿÿÀ","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","\u0003","\u0000\u000f","TypeError: The string contains invalid characters.","\u0000\u0000?","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","a","a","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters."],"toolCalls":[]}"####,
        ),
        (
            r####"// atob WebIDL argument conversion (html/webappapis/atob/base64.any.js)
const cases = [undefined, null, 7, 12, 1.5, true, false, NaN, Infinity, -Infinity, 0, -0]
return cases.map((input) => { try { return atob(input) } catch (error) { return error instanceof TypeError ? "TypeError" : error.name } })"####,
            r####"{"ok":true,"value":["TypeError","ée","TypeError","×","TypeError","¶»","TypeError","5£","\"wâ+r","TypeError","TypeError","TypeError"],"toolCalls":[]}"####,
        ),
        (
            r####"// btoa over the base64.any.js inputs, each round-tripped through atob
var tests = ["\u05e2\u05d1\u05e8\u05d9\u05ea", "", "ab", "abc", "abcd", "abcde", "\u00ff\u00ff\u00c0", "\u0000a", "a\u0000b", "\u0000", "\u0001", "\u0002", "\u0003", "\u0004", "\u0005", "\u0006", "\u0007", "\b", "\t", "\n", "\u000b", "\f", "\r", "\u000e", "\u000f", "\u0010", "\u0011", "\u0012", "\u0013", "\u0014", "\u0015", "\u0016", "\u0017", "\u0018", "\u0019", "\u001a", "\u001b", "\u001c", "\u001d", "\u001e", "\u001f", " ", "!", "\"", "#", "$", "%", "&", "'", "(", ")", "*", "+", ",", "-", ".", "/", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", ":", ";", "<", "=", ">", "?", "@", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z", "[", "\\", "]", "^", "_", "`", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "{", "|", "}", "~", "\u007f", "\u0080", "\u0081", "\u0082", "\u0083", "\u0084", "\u0085", "\u0086", "\u0087", "\u0088", "\u0089", "\u008a", "\u008b", "\u008c", "\u008d", "\u008e", "\u008f", "\u0090", "\u0091", "\u0092", "\u0093", "\u0094", "\u0095", "\u0096", "\u0097", "\u0098", "\u0099", "\u009a", "\u009b", "\u009c", "\u009d", "\u009e", "\u009f", "\u00a0", "\u00a1", "\u00a2", "\u00a3", "\u00a4", "\u00a5", "\u00a6", "\u00a7", "\u00a8", "\u00a9", "\u00aa", "\u00ab", "\u00ac", "\u00ad", "\u00ae", "\u00af", "\u00b0", "\u00b1", "\u00b2", "\u00b3", "\u00b4", "\u00b5", "\u00b6", "\u00b7", "\u00b8", "\u00b9", "\u00ba", "\u00bb", "\u00bc", "\u00bd", "\u00be", "\u00bf", "\u00c0", "\u00c1", "\u00c2", "\u00c3", "\u00c4", "\u00c5", "\u00c6", "\u00c7", "\u00c8", "\u00c9", "\u00ca", "\u00cb", "\u00cc", "\u00cd", "\u00ce", "\u00cf", "\u00d0", "\u00d1", "\u00d2", "\u00d3", "\u00d4", "\u00d5", "\u00d6", "\u00d7", "\u00d8", "\u00d9", "\u00da", "\u00db", "\u00dc", "\u00dd", "\u00de", "\u00df", "\u00e0", "\u00e1", "\u00e2", "\u00e3", "\u00e4", "\u00e5", "\u00e6", "\u00e7", "\u00e8", "\u00e9", "\u00ea", "\u00eb", "\u00ec", "\u00ed", "\u00ee", "\u00ef", "\u00f0", "\u00f1", "\u00f2", "\u00f3", "\u00f4", "\u00f5", "\u00f6", "\u00f7", "\u00f8", "\u00f9", "\u00fa", "\u00fb", "\u00fc", "\u00fd", "\u00fe", "\u00ff", "\u0100", "\u0101", "\u2710", "\ufffe", "\uffff"]
tests.push(undefined, null, 7, 12, 1.5, true, false, NaN, +Infinity, -Infinity, 0, -0, "\ud800\udc00", "\u0000\u0001\u0002\u0003\u0004\u0005\u0006\u0007\b\t\n\u000b\f\r\u000e\u000f\u0010\u0011\u0012\u0013\u0014\u0015\u0016\u0017\u0018\u0019\u001a\u001b\u001c\u001d\u001e\u001f !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\u007f\u0080\u0081\u0082\u0083\u0084\u0085\u0086\u0087\u0088\u0089\u008a\u008b\u008c\u008d\u008e\u008f\u0090\u0091\u0092\u0093\u0094\u0095\u0096\u0097\u0098\u0099\u009a\u009b\u009c\u009d\u009e\u009f\u00a0\u00a1\u00a2\u00a3\u00a4\u00a5\u00a6\u00a7\u00a8\u00a9\u00aa\u00ab\u00ac\u00ad\u00ae\u00af\u00b0\u00b1\u00b2\u00b3\u00b4\u00b5\u00b6\u00b7\u00b8\u00b9\u00ba\u00bb\u00bc\u00bd\u00be\u00bf\u00c0\u00c1\u00c2\u00c3\u00c4\u00c5\u00c6\u00c7\u00c8\u00c9\u00ca\u00cb\u00cc\u00cd\u00ce\u00cf\u00d0\u00d1\u00d2\u00d3\u00d4\u00d5\u00d6\u00d7\u00d8\u00d9\u00da\u00db\u00dc\u00dd\u00de\u00df\u00e0\u00e1\u00e2\u00e3\u00e4\u00e5\u00e6\u00e7\u00e8\u00e9\u00ea\u00eb\u00ec\u00ed\u00ee\u00ef\u00f0\u00f1\u00f2\u00f3\u00f4\u00f5\u00f6\u00f7\u00f8\u00f9\u00fa\u00fb\u00fc\u00fd\u00fe\u00ff")
return tests.map((input) => { try { const encoded = btoa(input); return [encoded, atob(encoded) === "" + input] } catch (error) { return error instanceof TypeError ? "TypeError: " + error.message : error.name } })"####,
            r####"{"ok":true,"value":["TypeError: The string contains invalid characters.",["",true],["YWI=",true],["YWJj",true],["YWJjZA==",true],["YWJjZGU=",true],["///A",true],["AGE=",true],["YQBi",true],["AA==",true],["AQ==",true],["Ag==",true],["Aw==",true],["BA==",true],["BQ==",true],["Bg==",true],["Bw==",true],["CA==",true],["CQ==",true],["Cg==",true],["Cw==",true],["DA==",true],["DQ==",true],["Dg==",true],["Dw==",true],["EA==",true],["EQ==",true],["Eg==",true],["Ew==",true],["FA==",true],["FQ==",true],["Fg==",true],["Fw==",true],["GA==",true],["GQ==",true],["Gg==",true],["Gw==",true],["HA==",true],["HQ==",true],["Hg==",true],["Hw==",true],["IA==",true],["IQ==",true],["Ig==",true],["Iw==",true],["JA==",true],["JQ==",true],["Jg==",true],["Jw==",true],["KA==",true],["KQ==",true],["Kg==",true],["Kw==",true],["LA==",true],["LQ==",true],["Lg==",true],["Lw==",true],["MA==",true],["MQ==",true],["Mg==",true],["Mw==",true],["NA==",true],["NQ==",true],["Ng==",true],["Nw==",true],["OA==",true],["OQ==",true],["Og==",true],["Ow==",true],["PA==",true],["PQ==",true],["Pg==",true],["Pw==",true],["QA==",true],["QQ==",true],["Qg==",true],["Qw==",true],["RA==",true],["RQ==",true],["Rg==",true],["Rw==",true],["SA==",true],["SQ==",true],["Sg==",true],["Sw==",true],["TA==",true],["TQ==",true],["Tg==",true],["Tw==",true],["UA==",true],["UQ==",true],["Ug==",true],["Uw==",true],["VA==",true],["VQ==",true],["Vg==",true],["Vw==",true],["WA==",true],["WQ==",true],["Wg==",true],["Ww==",true],["XA==",true],["XQ==",true],["Xg==",true],["Xw==",true],["YA==",true],["YQ==",true],["Yg==",true],["Yw==",true],["ZA==",true],["ZQ==",true],["Zg==",true],["Zw==",true],["aA==",true],["aQ==",true],["ag==",true],["aw==",true],["bA==",true],["bQ==",true],["bg==",true],["bw==",true],["cA==",true],["cQ==",true],["cg==",true],["cw==",true],["dA==",true],["dQ==",true],["dg==",true],["dw==",true],["eA==",true],["eQ==",true],["eg==",true],["ew==",true],["fA==",true],["fQ==",true],["fg==",true],["fw==",true],["gA==",true],["gQ==",true],["gg==",true],["gw==",true],["hA==",true],["hQ==",true],["hg==",true],["hw==",true],["iA==",true],["iQ==",true],["ig==",true],["iw==",true],["jA==",true],["jQ==",true],["jg==",true],["jw==",true],["kA==",true],["kQ==",true],["kg==",true],["kw==",true],["lA==",true],["lQ==",true],["lg==",true],["lw==",true],["mA==",true],["mQ==",true],["mg==",true],["mw==",true],["nA==",true],["nQ==",true],["ng==",true],["nw==",true],["oA==",true],["oQ==",true],["og==",true],["ow==",true],["pA==",true],["pQ==",true],["pg==",true],["pw==",true],["qA==",true],["qQ==",true],["qg==",true],["qw==",true],["rA==",true],["rQ==",true],["rg==",true],["rw==",true],["sA==",true],["sQ==",true],["sg==",true],["sw==",true],["tA==",true],["tQ==",true],["tg==",true],["tw==",true],["uA==",true],["uQ==",true],["ug==",true],["uw==",true],["vA==",true],["vQ==",true],["vg==",true],["vw==",true],["wA==",true],["wQ==",true],["wg==",true],["ww==",true],["xA==",true],["xQ==",true],["xg==",true],["xw==",true],["yA==",true],["yQ==",true],["yg==",true],["yw==",true],["zA==",true],["zQ==",true],["zg==",true],["zw==",true],["0A==",true],["0Q==",true],["0g==",true],["0w==",true],["1A==",true],["1Q==",true],["1g==",true],["1w==",true],["2A==",true],["2Q==",true],["2g==",true],["2w==",true],["3A==",true],["3Q==",true],["3g==",true],["3w==",true],["4A==",true],["4Q==",true],["4g==",true],["4w==",true],["5A==",true],["5Q==",true],["5g==",true],["5w==",true],["6A==",true],["6Q==",true],["6g==",true],["6w==",true],["7A==",true],["7Q==",true],["7g==",true],["7w==",true],["8A==",true],["8Q==",true],["8g==",true],["8w==",true],["9A==",true],["9Q==",true],["9g==",true],["9w==",true],["+A==",true],["+Q==",true],["+g==",true],["+w==",true],["/A==",true],["/Q==",true],["/g==",true],["/w==",true],"TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.","TypeError: The string contains invalid characters.",["dW5kZWZpbmVk",true],["bnVsbA==",true],["Nw==",true],["MTI=",true],["MS41",true],["dHJ1ZQ==",true],["ZmFsc2U=",true],["TmFO",true],["SW5maW5pdHk=",true],["LUluZmluaXR5",true],["MA==",true],["MA==",true],"TypeError: The string contains invalid characters.",["AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==",true]],"toolCalls":[]}"####,
        ),
        (
            r####"// crypto.randomUUID (WebCryptoAPI/randomUUID.https.any.js): format, version, and variant over 768 UUIDs
const uuids = new Set()
let format = true, version = true, variant = true
for (let i = 0; i < 768; i++) {
  const uuid = crypto.randomUUID()
  if (uuids.has(uuid)) throw new Error("uuid collision " + uuid)
  uuids.add(uuid)
  if (uuid.length !== 36) format = false
  for (let k = 0; k < 36; k++) {
    const c = uuid[k]
    if (k === 8 || k === 13 || k === 18 || k === 23) { if (c !== "-") format = false }
    else if (!(c >= "0" && c <= "9") && !(c >= "a" && c <= "f")) format = false
  }
  if (uuid[14] !== "4") version = false
  if (!(uuid[19] === "8" || uuid[19] === "9" || uuid[19] === "a" || uuid[19] === "b")) variant = false
}
return [format, version, variant, uuids.size]"####,
            r####"{"ok":true,"value":[true,true,true,768],"toolCalls":[]}"####,
        ),
        (
            r####"// decodeURIComponent and decodeURI over every %XX; encodeURIComponent and encodeURI over "\u0000\u0001\u0002\u0003\u0004\u0005\u0006\u0007\b\t\n\u000b\f\r\u000e\u000f\u0010\u0011\u0012\u0013\u0014\u0015\u0016\u0017\u0018\u0019\u001a\u001b\u001c\u001d\u001e\u001f !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\u007f"
const h = "0123456789ABCDEF"
const out = []
for (let i = 0; i < 256; i++) {
  const escape = "%" + h[i >> 4] + h[i & 15]
  const row = []
  for (const f of [decodeURIComponent, decodeURI]) { try { row.push(f(escape)) } catch (e) { row.push(e instanceof URIError && e.message === f.name + " received malformed URI data: URI error" ? 0 : e.name) } }
  out.push(row)
}
const ascii = "\u0000\u0001\u0002\u0003\u0004\u0005\u0006\u0007\b\t\n\u000b\f\r\u000e\u000f\u0010\u0011\u0012\u0013\u0014\u0015\u0016\u0017\u0018\u0019\u001a\u001b\u001c\u001d\u001e\u001f !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\u007f"
const enc = []
for (let i = 0; i < ascii.length; i++) enc.push([encodeURIComponent(ascii[i]), encodeURI(ascii[i])])
return [out, enc, encodeURI(ascii), encodeURIComponent(ascii)]"####,
            r####"{"ok":true,"value":[[["\u0000","\u0000"],["\u0001","\u0001"],["\u0002","\u0002"],["\u0003","\u0003"],["\u0004","\u0004"],["\u0005","\u0005"],["\u0006","\u0006"],["\u0007","\u0007"],["\b","\b"],["\t","\t"],["\n","\n"],["\u000b","\u000b"],["\f","\f"],["\r","\r"],["\u000e","\u000e"],["\u000f","\u000f"],["\u0010","\u0010"],["\u0011","\u0011"],["\u0012","\u0012"],["\u0013","\u0013"],["\u0014","\u0014"],["\u0015","\u0015"],["\u0016","\u0016"],["\u0017","\u0017"],["\u0018","\u0018"],["\u0019","\u0019"],["\u001a","\u001a"],["\u001b","\u001b"],["\u001c","\u001c"],["\u001d","\u001d"],["\u001e","\u001e"],["\u001f","\u001f"],[" "," "],["!","!"],["\"","\""],["#","%23"],["$","%24"],["%","%"],["&","%26"],["'","'"],["(","("],[")",")"],["*","*"],["+","%2B"],[",","%2C"],["-","-"],[".","."],["/","%2F"],["0","0"],["1","1"],["2","2"],["3","3"],["4","4"],["5","5"],["6","6"],["7","7"],["8","8"],["9","9"],[":","%3A"],[";","%3B"],["<","<"],["=","%3D"],[">",">"],["?","%3F"],["@","%40"],["A","A"],["B","B"],["C","C"],["D","D"],["E","E"],["F","F"],["G","G"],["H","H"],["I","I"],["J","J"],["K","K"],["L","L"],["M","M"],["N","N"],["O","O"],["P","P"],["Q","Q"],["R","R"],["S","S"],["T","T"],["U","U"],["V","V"],["W","W"],["X","X"],["Y","Y"],["Z","Z"],["[","["],["\\","\\"],["]","]"],["^","^"],["_","_"],["`","`"],["a","a"],["b","b"],["c","c"],["d","d"],["e","e"],["f","f"],["g","g"],["h","h"],["i","i"],["j","j"],["k","k"],["l","l"],["m","m"],["n","n"],["o","o"],["p","p"],["q","q"],["r","r"],["s","s"],["t","t"],["u","u"],["v","v"],["w","w"],["x","x"],["y","y"],["z","z"],["{","{"],["|","|"],["}","}"],["~","~"],["",""],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0],[0,0]],[["%00","%00"],["%01","%01"],["%02","%02"],["%03","%03"],["%04","%04"],["%05","%05"],["%06","%06"],["%07","%07"],["%08","%08"],["%09","%09"],["%0A","%0A"],["%0B","%0B"],["%0C","%0C"],["%0D","%0D"],["%0E","%0E"],["%0F","%0F"],["%10","%10"],["%11","%11"],["%12","%12"],["%13","%13"],["%14","%14"],["%15","%15"],["%16","%16"],["%17","%17"],["%18","%18"],["%19","%19"],["%1A","%1A"],["%1B","%1B"],["%1C","%1C"],["%1D","%1D"],["%1E","%1E"],["%1F","%1F"],["%20","%20"],["!","!"],["%22","%22"],["%23","#"],["%24","$"],["%25","%25"],["%26","&"],["'","'"],["(","("],[")",")"],["*","*"],["%2B","+"],["%2C",","],["-","-"],[".","."],["%2F","/"],["0","0"],["1","1"],["2","2"],["3","3"],["4","4"],["5","5"],["6","6"],["7","7"],["8","8"],["9","9"],["%3A",":"],["%3B",";"],["%3C","%3C"],["%3D","="],["%3E","%3E"],["%3F","?"],["%40","@"],["A","A"],["B","B"],["C","C"],["D","D"],["E","E"],["F","F"],["G","G"],["H","H"],["I","I"],["J","J"],["K","K"],["L","L"],["M","M"],["N","N"],["O","O"],["P","P"],["Q","Q"],["R","R"],["S","S"],["T","T"],["U","U"],["V","V"],["W","W"],["X","X"],["Y","Y"],["Z","Z"],["%5B","%5B"],["%5C","%5C"],["%5D","%5D"],["%5E","%5E"],["_","_"],["%60","%60"],["a","a"],["b","b"],["c","c"],["d","d"],["e","e"],["f","f"],["g","g"],["h","h"],["i","i"],["j","j"],["k","k"],["l","l"],["m","m"],["n","n"],["o","o"],["p","p"],["q","q"],["r","r"],["s","s"],["t","t"],["u","u"],["v","v"],["w","w"],["x","x"],["y","y"],["z","z"],["%7B","%7B"],["%7C","%7C"],["%7D","%7D"],["~","~"],["%7F","%7F"]],"%00%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F%10%11%12%13%14%15%16%17%18%19%1A%1B%1C%1D%1E%1F%20!%22#$%25&'()*+,-./0123456789:;%3C=%3E?@ABCDEFGHIJKLMNOPQRSTUVWXYZ%5B%5C%5D%5E_%60abcdefghijklmnopqrstuvwxyz%7B%7C%7D~%7F","%00%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F%10%11%12%13%14%15%16%17%18%19%1A%1B%1C%1D%1E%1F%20!%22%23%24%25%26'()*%2B%2C-.%2F0123456789%3A%3B%3C%3D%3E%3F%40ABCDEFGHIJKLMNOPQRSTUVWXYZ%5B%5C%5D%5E_%60abcdefghijklmnopqrstuvwxyz%7B%7C%7D~%7F"],"toolCalls":[]}"####,
        ),
    ]);
}
