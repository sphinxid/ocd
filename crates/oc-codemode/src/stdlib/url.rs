//! `URL`, `URLSearchParams`, and the URI helpers. Port of `stdlib/url.ts`.
//!
//! Upstream wraps the host's WHATWG `URL` and `URLSearchParams`; here the `url` crate is the WHATWG parser (its
//! `quirks` module is the JS getter/setter API) and the search-parameter list is kept as USV strings, serialized
//! as `application/x-www-form-urlencoded` exactly like JS. A URL's `searchParams` is linked both ways: mutating the
//! parameters rewrites the URL's query, and setting `href` or `search` re-parses the parameter list.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use url::{Position, Url};

use crate::interp::callback::apply_collection_callback;
use crate::interp::coerce::coerce_to_string;
use crate::interp::errors::{JsResult, type_error, uri_error};
use crate::interp::function::{
    Method, NResult, NativeFn, arg, constructor, fn_, method, methods, prototype_from, receiver, requires_new,
};
use crate::interp::iteration::{Cursor, Mode, Step, iterate, iterator_object, preserve_consumer_error};
use crate::interp::references::is_runtime_reference;
use crate::interp::{Rt, Site};
use crate::object::{Getter, HIDDEN, Obj, ObjKind, Setter, define, define_accessor, entries, get};
use crate::value::{JsStr, PropKey, Sym, Value};

// ---------------------------------------------------------------------------------------------------------------
// URLSearchParams storage

/// The list behind a `URLSearchParams`, optionally linked to the URL whose query it mirrors.
pub struct SearchParams {
    /// Name/value pairs as USV strings (lone surrogates already replaced, as WebIDL's USVString conversion does).
    pub list: RefCell<Vec<(String, String)>>,
    url: RefCell<Option<Rc<RefCell<Url>>>>,
}

impl SearchParams {
    pub fn new(list: Vec<(String, String)>) -> Rc<SearchParams> {
        Rc::new(SearchParams { list: RefCell::new(list), url: RefCell::new(None) })
    }

    /// `application/x-www-form-urlencoded` serialization (`params.toString()`).
    pub fn serialize(&self) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in self.list.borrow().iter() {
            serializer.append_pair(name, value);
        }
        serializer.finish()
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        self.list.borrow().clone()
    }

    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.url.borrow_mut().take())
    }

    /// The URLSearchParams update steps: write the serialization back to the linked URL's query.
    fn update(&self) {
        let linked = self.url.borrow().clone();
        if let Some(url) = linked {
            let serialized = self.serialize();
            url.borrow_mut().set_query(if serialized.is_empty() { None } else { Some(&serialized) });
        }
    }

    fn reparse(&self, url: &Url) {
        *self.list.borrow_mut() = url.query().map(parse_query).unwrap_or_default();
    }
}

/// The `application/x-www-form-urlencoded` parser.
fn parse_query(input: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(input.as_bytes()).into_owned().collect()
}

/// Code-unit order, as JS compares strings (`URLSearchParams.sort` sorts by UTF-16 code units).
fn compare_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

// ---------------------------------------------------------------------------------------------------------------
// URL storage

/// A `URL`: the parsed record and its linked `searchParams` object.
pub struct UrlData {
    url: Rc<RefCell<Url>>,
    params: Rc<SearchParams>,
    params_obj: RefCell<Option<Obj>>,
}

impl UrlData {
    pub fn href(&self) -> String {
        self.url.borrow().as_str().to_string()
    }
    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.params_obj.borrow_mut().take())
    }
}

fn make_url(rt: &Rt, proto: Obj, parsed: Url) -> Obj {
    let list = parsed.query().map(parse_query).unwrap_or_default();
    let url = Rc::new(RefCell::new(parsed));
    let params = Rc::new(SearchParams { list: RefCell::new(list), url: RefCell::new(Some(url.clone())) });
    let params_obj = Obj::new(Some(rt.builtins.url_search_params.clone()), ObjKind::URLSearchParams(params.clone()));
    Obj::new(Some(proto), ObjKind::URL(UrlData { url, params, params_obj: RefCell::new(Some(params_obj)) }))
}

fn parse_url(input: &str, base: Option<&str>) -> Option<Url> {
    let base = match base {
        Some(b) => Some(Url::parse(b).ok()?),
        None => None,
    };
    let mut url = Url::options().base_url(base.as_ref()).parse(input).ok()?;
    encode_path_carets(&mut url);
    Some(url)
}

/// A URL argument: a URL is its href, anything else stringifies without program code.
fn url_argument(value: &Value) -> String {
    if let Value::Object(o) = value
        && let ObjKind::URL(u) = o.kind()
    {
        return u.href();
    }
    coerce_to_string(value).to_rust()
}

/// A URL value for a host href (extensions); an unparsable href is the constructor's TypeError.
pub fn url_value(rt: &Rt, href: &str) -> JsResult<Value> {
    match parse_url(href, None) {
        Some(parsed) => Ok(Value::Object(make_url(rt, rt.builtins.url.clone(), parsed))),
        None => Err(type_error("new URL(...) received an invalid URL.")),
    }
}

/// A URLSearchParams value for host pairs (extensions).
pub fn search_params_value(rt: &Rt, pairs: Vec<(String, String)>) -> Value {
    Value::Object(Obj::new(
        Some(rt.builtins.url_search_params.clone()),
        ObjKind::URLSearchParams(SearchParams::new(pairs)),
    ))
}

const URL_PROPERTIES: [&str; 11] =
    ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"];

fn url_receiver(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("URL.prototype.{name}"), |o| matches!(o.kind(), ObjKind::URL(_)))
}

fn with_url<T>(o: &Obj, f: impl FnOnce(&UrlData) -> T) -> T {
    match o.kind() {
        ObjKind::URL(u) => f(u),
        _ => unreachable!("receiver checked"),
    }
}

fn read_property(url: &Url, name: &str) -> String {
    use url::quirks;
    match name {
        "href" => quirks::href(url).to_string(),
        "origin" => origin(url),
        "protocol" => quirks::protocol(url).to_string(),
        "username" => quirks::username(url).to_string(),
        "password" => quirks::password(url).to_string(),
        "host" => quirks::host(url).to_string(),
        "hostname" => quirks::hostname(url).to_string(),
        "port" => quirks::port(url).to_string(),
        "pathname" => quirks::pathname(url).to_string(),
        "search" => quirks::search(url).to_string(),
        _ => quirks::hash(url).to_string(),
    }
}

/// Runs a JS URL setter; only `href` rejects a value (the others ignore what they cannot apply, as in JS). The
/// `host`, `hostname`, and `protocol` setters follow WebKit (Bun's URL) where it departs from the `url` crate.
fn write_property(url: &mut Url, name: &str, value: &str) -> Result<(), ()> {
    use url::quirks;
    match name {
        "href" => quirks::set_href(url, value).map_err(|_| ())?,
        "protocol" => set_protocol(url, value),
        "username" => {
            let _ = quirks::set_username(url, value);
        }
        "password" => {
            let _ = quirks::set_password(url, value);
        }
        "host" => set_host(url, value, true),
        "hostname" => set_host(url, value, false),
        "port" => {
            let _ = quirks::set_port(url, value);
        }
        "pathname" => quirks::set_pathname(url, value),
        "search" => quirks::set_search(url, value),
        _ => quirks::set_hash(url, value),
    }
    encode_path_carets(url);
    Ok(())
}

/// The origin serialization, with WebKit's departures: `ftps:` URLs have a tuple origin, and a `blob:` URL around a
/// `file:` URL is `file://` while one around another `blob:` URL is opaque.
fn origin(url: &Url) -> String {
    if url.scheme() == "ftps" {
        return match url.host_str() {
            Some(host) => match url.port() {
                Some(port) => format!("ftps://{host}:{port}"),
                None => format!("ftps://{host}"),
            },
            None => "ftps:".to_string(),
        };
    }
    if url.scheme() == "blob"
        && let Ok(inner) = Url::parse(url.path())
    {
        match inner.scheme() {
            "ftps" => return origin(&inner),
            "file" => return "file://".to_string(),
            "blob" => return "null".to_string(),
            _ => {}
        }
    }
    url::quirks::origin(url)
}

fn is_special(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

/// Where WebKit (Bun's URL) serializes differently from the `url` crate: it percent-encodes `^` in hierarchical
/// paths, and it encodes the last trailing space of an opaque path followed by a query or fragment instead of
/// keeping it.
fn encode_path_carets(url: &mut Url) {
    if url.cannot_be_a_base() {
        if url.path().ends_with(' ') && (url.query().is_some() || url.fragment().is_some()) {
            let end = url[..Position::AfterPath].len();
            let href = url.as_str();
            if let Ok(parsed) = Url::parse(&format!("{}%20{}", &href[..end - 1], &href[end..])) {
                *url = parsed;
            }
        }
    } else if url.path().contains('^') {
        let path = url.path().replace('^', "%5E");
        url.set_path(&path);
    }
}

/// The `protocol` setter. Unlike the `url` crate, WebKit lets a special URL without credentials or a port become a
/// `file:` URL.
fn set_protocol(url: &mut Url, value: &str) {
    let _ = url::quirks::set_protocol(url, value);
    let cleaned: String = value.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let scheme = cleaned.split(':').next().unwrap_or("").to_ascii_lowercase();
    if scheme == "file"
        && url.scheme() != "file"
        && is_special(url.scheme())
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
    {
        let rest = &url.as_str()[url.scheme().len()..];
        if let Ok(parsed) = Url::parse(&format!("file{rest}")) {
            *url = parsed;
        }
    }
}

/// The `host` (`with_port`) and `hostname` setters as WebKit applies them: the value ends at the first path, query,
/// or fragment delimiter; for `host`, a port is the run of digits after the last colon (an IPv6 address aside),
/// a default port leaves the current port alone, an out-of-range one clears it, and stray colons ignore the value.
fn set_host(url: &mut Url, value: &str, with_port: bool) {
    use url::quirks;
    if url.cannot_be_a_base() {
        return;
    }
    let special = is_special(url.scheme());
    let end = value.find(|c| matches!(c, '/' | '?' | '#') || (special && c == '\\')).unwrap_or(value.len());
    let value = &value[..end];
    if url.scheme() == "file" {
        // A file URL has no port; its host may be empty.
        let mut host = value;
        if with_port
            && let Some(colon) = value.rfind(':')
            && value.rfind(']').is_none_or(|b| b < colon)
        {
            // A file URL cannot take a port: a numeric one (or a missing host) ignores the value.
            if colon == 0 || value[colon + 1..].starts_with(|c: char| c.is_ascii_digit()) {
                return;
            }
            host = &value[..colon];
        }
        let host: String = host.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
        let _ = quirks::set_hostname(url, &host);
        return;
    }
    if !with_port && value.starts_with('[') && value.contains("]:") {
        // WebKit splices a bracketed host with a port into a port-less URL and re-parses it.
        if url.port().is_none() {
            let spliced = format!("{}{value}{}", &url[..Position::BeforeHost], &url[Position::AfterHost..]);
            if let Ok(parsed) = Url::parse(&spliced) {
                *url = parsed;
            }
        }
        return;
    }
    if !with_port {
        let _ = quirks::set_hostname(url, value);
        return;
    }
    let Some(colon) = value.rfind(':') else {
        let _ = quirks::set_host(url, value);
        return;
    };
    let bracket = value.rfind(']');
    if bracket.is_some_and(|b| b > colon) {
        let _ = quirks::set_host(url, value);
        return;
    }
    let host = &value[..colon];
    if host.contains(':') && bracket.is_none() {
        return;
    }
    if quirks::set_hostname(url, host).is_err() {
        return;
    }
    let digits: String = value[colon + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return;
    }
    let trimmed = digits.trim_start_matches('0');
    let port = if trimmed.len() > 5 { None } else { trimmed.parse::<u32>().ok().or(Some(0)) };
    match port {
        Some(port) if port <= 65535 => {
            if default_port(url.scheme()) != Some(port as u16) {
                let _ = url.set_port(Some(port as u16));
            }
        }
        _ => {
            let _ = url.set_port(None);
        }
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

pub fn url_global(rt: &Rt) -> Value {
    let proto = rt.builtins.url.clone();
    let fp = rt.builtins.function.clone();
    let construct_proto = proto.clone();
    let ctor = constructor(
        &fp,
        &proto,
        "URL",
        1,
        NativeFn {
            call: requires_new("URL"),
            construct: Some(Rc::new(move |rt, _site, args, new_target| {
                if args.is_empty() {
                    return NResult::err(type_error("new URL(...) requires a URL string and an optional base URL."));
                }
                let input = url_argument(&args[0]);
                let base = match arg(&args, 1) {
                    Value::Undefined => None,
                    other => Some(url_argument(&other)),
                };
                match parse_url(&input, base.as_deref()) {
                    Some(parsed) => {
                        NResult::ok(Value::Object(make_url(rt, prototype_from(&new_target, &construct_proto), parsed)))
                    }
                    None => NResult::err(type_error(format!(
                        "new URL(...) received an invalid URL{}.",
                        if base.is_none() { "" } else { " or base URL" }
                    ))),
                }
            })),
            callback: true,
        },
    );
    let parse = |name: &'static str| -> Method {
        method(name, 1, move |rt, _site, _this, args| {
            if args.is_empty() {
                return NResult::err(type_error(format!("URL.{name} requires a URL argument.")));
            }
            let input = url_argument(&args[0]);
            let base = match arg(&args, 1) {
                Value::Undefined => None,
                other => Some(url_argument(&other)),
            };
            match parse_url(&input, base.as_deref()) {
                Some(parsed) => {
                    if name == "canParse" {
                        NResult::ok(true)
                    } else {
                        NResult::ok(Value::Object(make_url(rt, rt.builtins.url.clone(), parsed)))
                    }
                }
                None => NResult::ok(if name == "canParse" { Value::Bool(false) } else { Value::Null }),
            }
        })
    };
    methods(&fp, &ctor, vec![parse("canParse"), parse("parse")]);

    for name in URL_PROPERTIES {
        let getter: Getter = Rc::new(move |this| {
            let o = url_receiver(this, name)?;
            Ok(Value::from(with_url(&o, |u| read_property(&u.url.borrow(), name))))
        });
        let setter: Option<Setter> = if name == "origin" {
            None
        } else {
            Some(Rc::new(move |this, value| {
                let o = url_receiver(this, name)?;
                let text = coerce_to_string(&value).to_rust();
                with_url(&o, |u| {
                    let result = write_property(&mut u.url.borrow_mut(), name, &text);
                    if result.is_err() {
                        return Err(type_error(format!("URL.{name} received an invalid value.")));
                    }
                    if name == "href" || name == "search" {
                        u.params.reparse(&u.url.borrow());
                    }
                    Ok(())
                })
            }))
        };
        define_accessor(&proto, name, Some(getter), setter);
    }
    let search_params: Getter = Rc::new(|this| {
        let o = url_receiver(this, "searchParams")?;
        Ok(with_url(&o, |u| u.params_obj.borrow().clone().map(Value::Object).unwrap_or_default()))
    });
    define_accessor(&proto, "searchParams", Some(search_params), None);
    methods(
        &fp,
        &proto,
        vec![
            method("toString", 0, |_, _, this, _| {
                NResult::from(url_receiver(&this, "toString").map(|o| Value::from(with_url(&o, |u| u.href()))))
            }),
            method("toJSON", 0, |_, _, this, _| {
                NResult::from(url_receiver(&this, "toJSON").map(|o| Value::from(with_url(&o, |u| u.href()))))
            }),
        ],
    );
    Value::Object(ctor)
}

// ---------------------------------------------------------------------------------------------------------------
// Pairs (shared with Headers)

async fn read_pair(rt: &Rt, value: &Value, label: &str, site: Site) -> JsResult<Vec<JsStr>> {
    let Some(cursor) = iterate(rt, value, None, site).await? else {
        return Err(type_error(format!("{label} expects iterable [name, value] pairs.")));
    };
    let mut items = Vec::new();
    loop {
        let step = cursor.next().await?;
        if step.done {
            return Ok(items);
        }
        items.push(coerce_to_string(&step.value));
    }
}

/// Reads a synchronous iterable of `[name, value]` pairs as strings; `None` when `init` is not iterable. As in
/// WebIDL, the whole sequence is converted before any pair's length is checked.
pub async fn read_pairs(rt: &Rt, init: &Value, label: &str, site: Site) -> JsResult<Option<Vec<(JsStr, JsStr)>>> {
    let Some(cursor) = iterate(rt, init, None, site).await? else { return Ok(None) };
    let mut pairs: Vec<Vec<JsStr>> = Vec::new();
    loop {
        let step = cursor.next().await?;
        if step.done {
            break;
        }
        let pair = read_pair(rt, &step.value, label, site).await;
        pairs.push(preserve_consumer_error(&cursor, pair).await?);
    }
    if pairs.iter().any(|p| p.len() != 2) {
        return Err(type_error(format!("{label} expects iterable [name, value] pairs.")));
    }
    Ok(Some(
        pairs
            .into_iter()
            .map(|mut p| {
                let value = p.pop().unwrap_or_else(JsStr::empty);
                let name = p.pop().unwrap_or_else(JsStr::empty);
                (name, value)
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// URLSearchParams

fn parse_init(text: &str) -> Vec<(String, String)> {
    parse_query(text.strip_prefix('?').unwrap_or(text))
}

async fn construct_search_params(rt: Rt, init: Value, proto: Obj, site: Site) -> JsResult<Value> {
    let wrap = |list: Vec<(String, String)>| {
        Value::Object(Obj::new(Some(proto.clone()), ObjKind::URLSearchParams(SearchParams::new(list))))
    };
    if let Some(pairs) = read_pairs(&rt, &init, "new URLSearchParams(...)", site).await? {
        return Ok(wrap(pairs.into_iter().map(|(k, v)| (k.to_rust(), v.to_rust())).collect()));
    }
    if is_runtime_reference(&init) {
        return Err(type_error(
            "new URLSearchParams(...) expects a query string, data object, or synchronous iterable pairs.",
        ));
    }
    let Value::Object(o) = &init else {
        return Err(type_error(
            "new URLSearchParams(...) expects a query string, data object, iterable pairs, or URLSearchParams.",
        ));
    };
    let list = entries(o)?.into_iter().map(|(k, v)| (k.to_rust(), coerce_to_string(&v).to_rust())).collect();
    Ok(wrap(list))
}

fn params_receiver(this: &Value, name: &str) -> JsResult<Rc<SearchParams>> {
    let o = receiver(this, &format!("URLSearchParams.prototype.{name}"), |o| {
        matches!(o.kind(), ObjKind::URLSearchParams(_))
    })?;
    match o.kind() {
        ObjKind::URLSearchParams(p) => Ok(p.clone()),
        _ => unreachable!("receiver checked"),
    }
}

fn require_args(name: &str, args: &[Value], count: usize) -> JsResult<()> {
    if args.len() < count {
        return Err(type_error(format!(
            "URLSearchParams.{name} requires {count} argument{}.",
            if count == 1 { "" } else { "s" }
        )));
    }
    Ok(())
}

fn text_arg(args: &[Value], index: usize) -> String {
    coerce_to_string(&arg(args, index)).to_rust()
}

/// A live iterator over a parameter list: reads by index each step, finishing for good once exhausted.
struct ParamsCursor {
    params: RefCell<Option<Rc<SearchParams>>>,
    index: Cell<usize>,
    mode: Mode,
    array_proto: Obj,
}

impl Cursor for ParamsCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let step = (|| {
            let params = self.params.borrow().clone()?;
            let i = self.index.get();
            let entry = params.list.borrow().get(i).cloned();
            let Some((name, value)) = entry else {
                *self.params.borrow_mut() = None;
                return None;
            };
            self.index.set(i + 1);
            Some(match self.mode {
                Mode::Keys => Value::from(name),
                Mode::Values => Value::from(value),
                Mode::Entries => {
                    Value::Object(Obj::array(&self.array_proto, vec![Value::from(name), Value::from(value)]))
                }
            })
        })();
        let step = match step {
            Some(v) => Step::value(v),
            None => Step::done(),
        };
        Box::pin(std::future::ready(Ok(step)))
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.params.borrow_mut().take())
    }
}

pub fn params_cursor(rt: &Rt, params: Rc<SearchParams>, mode: Mode) -> Rc<dyn Cursor> {
    Rc::new(ParamsCursor {
        params: RefCell::new(Some(params)),
        index: Cell::new(0),
        mode,
        array_proto: rt.builtins.array.clone(),
    })
}

pub fn url_search_params_global(rt: &Rt) -> Value {
    let proto = rt.builtins.url_search_params.clone();
    let fp = rt.builtins.function.clone();
    let construct_proto = proto.clone();
    let ctor = constructor(
        &fp,
        &proto,
        "URLSearchParams",
        0,
        NativeFn {
            call: requires_new("URLSearchParams"),
            construct: Some(Rc::new(move |rt, site, args, new_target| {
                let into = prototype_from(&new_target, &construct_proto);
                let init = arg(&args, 0);
                let wrap = |list| {
                    NResult::ok(Value::Object(Obj::new(
                        Some(into.clone()),
                        ObjKind::URLSearchParams(SearchParams::new(list)),
                    )))
                };
                match &init {
                    Value::Undefined => wrap(Vec::new()),
                    Value::Object(o) if matches!(o.kind(), ObjKind::URLSearchParams(_)) => {
                        let ObjKind::URLSearchParams(p) = o.kind() else { unreachable!() };
                        wrap(p.pairs())
                    }
                    Value::String(s) => wrap(parse_init(&s.to_rust())),
                    Value::Null | Value::Number(_) | Value::Bool(_) => {
                        wrap(parse_init(&coerce_to_string(&init).to_rust()))
                    }
                    _ => NResult::future(construct_search_params(rt.clone(), init, into, site)),
                }
            })),
            callback: true,
        },
    );

    let size: Getter = Rc::new(|this| Ok(Value::Number(params_receiver(this, "size")?.list.borrow().len() as f64)));
    define_accessor(&proto, "size", Some(size), None);
    methods(
        &fp,
        &proto,
        vec![
            method("append", 2, |_, _, this, args| {
                NResult::from((|| {
                    require_args("append", &args, 2)?;
                    let p = params_receiver(&this, "append")?;
                    p.list.borrow_mut().push((text_arg(&args, 0), text_arg(&args, 1)));
                    p.update();
                    Ok(Value::Undefined)
                })())
            }),
            method("delete", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("delete", &args, 1)?;
                    let p = params_receiver(&this, "delete")?;
                    let name = text_arg(&args, 0);
                    let value = if arg(&args, 1).is_undefined() { None } else { Some(text_arg(&args, 1)) };
                    p.list.borrow_mut().retain(|(k, v)| !(*k == name && value.as_ref().is_none_or(|x| x == v)));
                    p.update();
                    Ok(Value::Undefined)
                })())
            }),
            method("get", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("get", &args, 1)?;
                    let p = params_receiver(&this, "get")?;
                    let name = text_arg(&args, 0);
                    let found = p.list.borrow().iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone());
                    Ok(found.map(Value::from).unwrap_or(Value::Null))
                })())
            }),
            method("getAll", 1, move |rt, _, this, args| {
                NResult::from((|| {
                    require_args("getAll", &args, 1)?;
                    let p = params_receiver(&this, "getAll")?;
                    let name = text_arg(&args, 0);
                    let values: Vec<Value> = p
                        .list
                        .borrow()
                        .iter()
                        .filter(|(k, _)| *k == name)
                        .map(|(_, v)| Value::from(v.clone()))
                        .collect();
                    Ok(Value::Object(Obj::array(&rt.builtins.array, values)))
                })())
            }),
            method("has", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("has", &args, 1)?;
                    let p = params_receiver(&this, "has")?;
                    let name = text_arg(&args, 0);
                    let value = if arg(&args, 1).is_undefined() { None } else { Some(text_arg(&args, 1)) };
                    let found =
                        p.list.borrow().iter().any(|(k, v)| *k == name && value.as_ref().is_none_or(|x| x == v));
                    Ok(Value::Bool(found))
                })())
            }),
            method("set", 2, |_, _, this, args| {
                NResult::from((|| {
                    require_args("set", &args, 2)?;
                    let p = params_receiver(&this, "set")?;
                    let (name, value) = (text_arg(&args, 0), text_arg(&args, 1));
                    {
                        let mut list = p.list.borrow_mut();
                        match list.iter().position(|(k, _)| *k == name) {
                            Some(first) => {
                                list[first].1 = value;
                                let mut index = 0;
                                list.retain(|(k, _)| {
                                    let keep = index <= first || *k != name;
                                    index += 1;
                                    keep
                                });
                            }
                            None => list.push((name, value)),
                        }
                    }
                    p.update();
                    Ok(Value::Undefined)
                })())
            }),
            method("sort", 0, |_, _, this, _| {
                NResult::from((|| {
                    let p = params_receiver(&this, "sort")?;
                    p.list.borrow_mut().sort_by(|a, b| compare_utf16(&a.0, &b.0));
                    p.update();
                    Ok(Value::Undefined)
                })())
            }),
            method("keys", 0, |rt, _, this, _| {
                NResult::from(
                    params_receiver(&this, "keys").map(|p| iterator_object(rt, params_cursor(rt, p, Mode::Keys))),
                )
            }),
            method("values", 0, |rt, _, this, _| {
                NResult::from(
                    params_receiver(&this, "values").map(|p| iterator_object(rt, params_cursor(rt, p, Mode::Values))),
                )
            }),
            method("entries", 0, |rt, _, this, _| {
                NResult::from(
                    params_receiver(&this, "entries").map(|p| iterator_object(rt, params_cursor(rt, p, Mode::Entries))),
                )
            }),
            method("toString", 0, |_, _, this, _| {
                NResult::from(params_receiver(&this, "toString").map(|p| Value::from(p.serialize())))
            }),
            method("forEach", 1, move |rt, site, this, args| {
                let prepared = (|| {
                    require_args("forEach", &args, 1)?;
                    let p = params_receiver(&this, "forEach")?;
                    let apply = apply_collection_callback(&arg(&args, 0), "URLSearchParams.forEach")?;
                    Ok((p, apply))
                })();
                let (p, apply) = match prepared {
                    Ok(v) => v,
                    Err(e) => return NResult::err(e),
                };
                let rt = rt.clone();
                let this_arg = arg(&args, 1);
                NResult::future(async move {
                    let snapshot = p.pairs();
                    for (name, value) in snapshot {
                        apply(&rt, vec![Value::from(value), Value::from(name), this.clone()], this_arg.clone(), site)
                            .await?;
                    }
                    Ok(Value::Undefined)
                })
            }),
        ],
    );
    let entries_fn = get(&proto, &PropKey::str("entries")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), entries_fn, HIDDEN);
    Value::Object(ctor)
}

// ---------------------------------------------------------------------------------------------------------------
// URI helpers (ECMAScript Encode/Decode over UTF-16 code units)

const URI_RESERVED: &[u8] = b";/?:@&=+$,";
const URI_MARK: &[u8] = b"-_.!~*'()";

fn is_unreserved(c: u16) -> bool {
    c < 128 && {
        let b = c as u8;
        b.is_ascii_alphanumeric() || URI_MARK.contains(&b)
    }
}

fn encode(units: &[u16], keep: impl Fn(u16) -> bool) -> Result<JsStr, &'static str> {
    const MESSAGE: &str = "String contained an illegal UTF-16 sequence.";
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut k = 0;
    while k < units.len() {
        let c = units[k];
        if keep(c) {
            out.push(c);
            k += 1;
            continue;
        }
        let code_point = if (0xDC00..=0xDFFF).contains(&c) {
            return Err(MESSAGE);
        } else if (0xD800..=0xDBFF).contains(&c) {
            match units.get(k + 1) {
                Some(&low) if (0xDC00..=0xDFFF).contains(&low) => {
                    k += 1;
                    0x10000 + (((c as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00)
                }
                _ => return Err(MESSAGE),
            }
        } else {
            c as u32
        };
        let ch = char::from_u32(code_point).ok_or(MESSAGE)?;
        let mut buffer = [0u8; 4];
        for byte in ch.encode_utf8(&mut buffer).bytes() {
            for unit in format!("%{byte:02X}").encode_utf16() {
                out.push(unit);
            }
        }
        k += 1;
    }
    Ok(JsStr::from_units(out))
}

fn hex_value(c: u16) -> Option<u8> {
    match c {
        0x30..=0x39 => Some((c - 0x30) as u8),
        0x41..=0x46 => Some((c - 0x41 + 10) as u8),
        0x61..=0x66 => Some((c - 0x61 + 10) as u8),
        _ => None,
    }
}

fn decode(units: &[u16], reserved: &[u8]) -> Result<JsStr, &'static str> {
    const MESSAGE: &str = "URI error";
    let len = units.len();
    let byte_at = |k: usize| -> Result<u8, &'static str> {
        // `k` indexes a '%'; the two hex digits follow it.
        if k + 2 >= len {
            return Err(MESSAGE);
        }
        match (hex_value(units[k + 1]), hex_value(units[k + 2])) {
            (Some(h), Some(l)) => Ok(h * 16 + l),
            _ => Err(MESSAGE),
        }
    };
    let mut out: Vec<u16> = Vec::with_capacity(len);
    let mut k = 0;
    while k < len {
        let c = units[k];
        if c != b'%' as u16 {
            out.push(c);
            k += 1;
            continue;
        }
        let start = k;
        let b = byte_at(k)?;
        k += 2;
        if b < 0x80 {
            if reserved.contains(&b) {
                out.extend_from_slice(&units[start..=k]);
            } else {
                out.push(b as u16);
            }
        } else {
            let n = b.leading_ones() as usize;
            if n == 1 || n > 4 {
                return Err(MESSAGE);
            }
            let mut octets = vec![b];
            if k + 3 * (n - 1) >= len {
                return Err(MESSAGE);
            }
            for _ in 1..n {
                k += 1;
                if units[k] != b'%' as u16 {
                    return Err(MESSAGE);
                }
                let next = byte_at(k)?;
                if next & 0xC0 != 0x80 {
                    return Err(MESSAGE);
                }
                k += 2;
                octets.push(next);
            }
            let text = std::str::from_utf8(&octets).map_err(|_| MESSAGE)?;
            out.extend(text.encode_utf16());
        }
        k += 1;
    }
    Ok(JsStr::from_units(out))
}

fn run_uri(name: &str, units: &[u16]) -> Result<JsStr, &'static str> {
    match name {
        "encodeURI" => {
            encode(units, |c| is_unreserved(c) || (c < 128 && (URI_RESERVED.contains(&(c as u8)) || c == b'#' as u16)))
        }
        "encodeURIComponent" => encode(units, is_unreserved),
        "decodeURI" => decode(units, b";/?:@&=+$,#"),
        _ => decode(units, b""),
    }
}

pub fn uri_global(rt: &Rt, name: &str) -> Value {
    let label = name.to_string();
    Value::Object(fn_(&rt.builtins.function, name, 1, move |_, _, _, args| {
        let value = coerce_to_string(&arg(&args, 0));
        match run_uri(&label, value.units()) {
            Ok(s) => NResult::ok(Value::String(s)),
            Err(message) => NResult::err(uri_error(format!("{label} received malformed URI data: {message}"))),
        }
    }))
}
