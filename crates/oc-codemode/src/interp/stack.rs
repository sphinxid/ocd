//! The program's call stack as V8 prints it in an error's `stack` (psychoscript 0.2.1, upstream #64939).
//!
//! Each running program function (and the program body) has an entry holding the site it is at: the call, `new` or
//! `await` it last reached. An error made by `new Error()` takes the `    at` lines of every entry, innermost first,
//! at most 10, like V8's `Error.stackTraceLimit`. A task (an async body, a generator) keeps its own entries while it
//! is suspended and puts them back on top of whatever runs it when it resumes, so an async function resumed by a job
//! shows only its own frames, as upstream's VM does.

use std::cell::{Cell, RefCell};

use crate::ast::Loc;
use crate::interp::errors::source_location;
use crate::object::{Obj, Slot, own};
use crate::value::{PropKey, Value};

pub struct Entry {
    id: u64,
    /// The running function; `None` for the program body (a bare `at <program>:l:c`).
    func: Option<Obj>,
    site: Cell<Option<Loc>>,
}

thread_local! {
    static STACK: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

/// Removes its entry when the frame ends (or its suspended future is dropped).
pub struct Guard(u64);

impl Drop for Guard {
    fn drop(&mut self) {
        let id = self.0;
        STACK.with(|s| {
            let mut s = s.borrow_mut();
            if let Some(at) = s.iter().rposition(|e| e.id == id) {
                s.remove(at);
            }
        });
    }
}

/// Pushes the entry of a frame that starts running.
pub fn enter(func: Option<Obj>) -> Guard {
    let id = NEXT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    STACK.with(|s| s.borrow_mut().push(Entry { id, func, site: Cell::new(None) }));
    Guard(id)
}

/// The innermost frame is now at `site`.
pub fn mark(site: Loc) {
    STACK.with(|s| {
        if let Some(top) = s.borrow().last() {
            top.site.set(Some(site));
        }
    });
}

/// Where the innermost frame is.
pub fn current_site() -> Option<Loc> {
    STACK.with(|s| s.borrow().last().and_then(|e| e.site.get()))
}

pub fn depth() -> usize {
    STACK.with(|s| s.borrow().len())
}

/// Takes the entries above `base` (a task suspending).
pub fn split_off(base: usize) -> Vec<Entry> {
    STACK.with(|s| {
        let mut s = s.borrow_mut();
        if base >= s.len() { Vec::new() } else { s.split_off(base) }
    })
}

/// Puts a resumed task's entries back on top.
pub fn restore(entries: Vec<Entry>) {
    if !entries.is_empty() {
        STACK.with(|s| s.borrow_mut().extend(entries));
    }
}

pub fn clear() {
    STACK.with(|s| s.borrow_mut().clear());
}

/// One V8-style `stack` line: `at name (<program>:line:col)`, or `at <program>:line:col` at top level.
pub fn frame_line(site: Loc, name: Option<&str>) -> String {
    let location = source_location(site);
    let at = format!("<program>:{}:{}", location.line, location.column);
    match name {
        None => format!("\n    at {at}"),
        Some(name) => format!("\n    at {name} ({at})"),
    }
}

/// A function's `name` as display reads it: a data property (own or inherited), program getters not run.
fn shown_name(func: &Obj) -> String {
    let key = PropKey::str("name");
    let mut current = Some(func.clone());
    while let Some(obj) = current {
        if let Some(slot) = own(&obj, &key) {
            return match slot {
                Slot::Data { value: Value::String(s), .. } if !s.is_empty() => s.to_rust(),
                _ => "<anonymous>".into(),
            };
        }
        current = obj.proto();
    }
    "<anonymous>".into()
}

/// The program frames as `stack` lines, innermost first, at most 10.
pub fn trace() -> String {
    STACK.with(|s| {
        let s = s.borrow();
        let mut out = String::new();
        let mut shown = 0;
        for entry in s.iter().rev() {
            if shown == 10 {
                break;
            }
            let Some(site) = entry.site.get() else { continue };
            let name = entry.func.as_ref().map(shown_name);
            out.push_str(&frame_line(site, name.as_deref()));
            shown += 1;
        }
        out
    })
}
