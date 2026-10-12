//! Generator objects and the `next`/`return`/`throw` prototype methods. Port of `interpreter/generators.ts`.

use std::cell::RefCell;
use std::rc::Rc;

use super::eval::{generator_request, Frame, GenState, ReqKind};
use super::function::{arg, fn_, method, methods, receiver, NResult};
use super::{Rt, Site};
use crate::object::{define, ObjKind, HIDDEN};
use crate::value::{PropKey, Sym, Value};

pub struct GeneratorHandle {
    pub asynchronous: bool,
    pub state: RefCell<Option<Rc<GenState>>>,
    pub frame: RefCell<Option<Rc<Frame>>>,
}

impl GeneratorHandle {
    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new((self.state.borrow_mut().take(), self.frame.borrow_mut().take()))
    }
}

fn request(rt: &Rt, site: Site, label: &'static str, kind: ReqKind, name: &'static str, this: Value, args: Vec<Value>) -> NResult {
    let generator = match receiver(&this, &format!("{label}.prototype.{name}"), |o| matches!(o.kind(), ObjKind::Generator(_))) {
        Ok(g) => g,
        Err(e) => return NResult::err(e),
    };
    let ObjKind::Generator(handle) = generator.kind() else { unreachable!() };
    let requested = generator_request(rt, handle, kind, arg(&args, 0));
    if handle.asynchronous {
        return NResult::Ready(rt.pending.create(rt, site, requested).map(Value::Object));
    }
    NResult::Future(requested)
}

/// `next`/`return`/`throw` on the generator prototypes; async generators answer with promises.
pub fn generator_globals(rt: &Rt) {
    let b = &rt.builtins;
    for asynchronous in [false, true] {
        let proto = if asynchronous { &b.async_generator } else { &b.generator };
        let label: &'static str = if asynchronous { "AsyncGenerator" } else { "Generator" };
        methods(
            &b.function,
            proto,
            vec![
                method("next", 1, move |rt, site, this, args| request(rt, site, label, ReqKind::Next, "next", this, args)),
                method("return", 1, move |rt, site, this, args| request(rt, site, label, ReqKind::Return, "return", this, args)),
                method("throw", 1, move |rt, site, this, args| request(rt, site, label, ReqKind::Throw, "throw", this, args)),
            ],
        );
        let (target, sym, name) = if asynchronous {
            (&b.async_iterator, Sym::AsyncIterator, "[Symbol.asyncIterator]")
        } else {
            (&b.iterator, Sym::Iterator, "[Symbol.iterator]")
        };
        let f = fn_(&b.function, name, 0, |_, _, this, _| NResult::ok(this));
        define(target, PropKey::Sym(sym), Value::Object(f), HIDDEN);
    }
}
