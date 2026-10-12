//! Walks over value trees: runtime-reference detection and circular-insertion checks.
//! Port of `interpreter/references.ts`.

use std::collections::HashSet;

use super::errors::{invalid_data, JsResult};
use crate::object::{get_own, own_keys, Obj};
use crate::value::Value;

/// Interpreter machinery that is never data, unlike a Date or Map, which cross some boundaries as copies.
pub fn is_opaque(value: &Value) -> bool {
    match value {
        Value::Object(o) => o.is_opaque(),
        Value::Tool(_) => true,
        _ => false,
    }
}

/// Values that cannot cross the data boundary: opaque machinery and host-backed wrappers.
pub fn is_runtime_reference(value: &Value) -> bool {
    match value {
        Value::Object(o) => o.is_opaque() || o.is_wrapper(),
        Value::Tool(_) => true,
        _ => false,
    }
}

// Depth-first search over a value tree. `matches` stops the walk; `skip` prunes a subtree without matching it.
fn find(value: &Value, matches: &dyn Fn(&Value) -> bool, skip: &dyn Fn(&Obj) -> bool, seen: &mut HashSet<usize>) -> bool {
    let mut pending: Vec<std::vec::IntoIter<Value>> = vec![vec![value.clone()].into_iter()];
    while let Some(top) = pending.last_mut() {
        let Some(current) = top.next() else {
            pending.pop();
            continue;
        };
        if matches(&current) {
            return true;
        }
        let Value::Object(o) = &current else { continue };
        if skip(o) || !seen.insert(o.addr()) {
            continue;
        }
        let children: Vec<Value> = own_keys(o).iter().map(|k| get_own(o, k).unwrap_or_default()).collect();
        pending.push(children.into_iter());
    }
    false
}

pub fn contains_runtime_reference(value: &Value) -> bool {
    find(value, &is_runtime_reference, &|_| false, &mut HashSet::new())
}

pub fn contains_opaque_reference(value: &Value) -> bool {
    find(value, &is_opaque, &|o| o.is_wrapper(), &mut HashSet::new())
}

// Reject cycles before mutation so later boundary walks remain safe.
pub fn reject_circular_insertion(container: &Obj, value: &Value, label: &str) -> JsResult<()> {
    reject_circular_insertion_seen(container, value, label, &mut HashSet::new())
}

pub fn reject_circular_insertion_seen(
    container: &Obj,
    value: &Value,
    label: &str,
    seen: &mut HashSet<usize>,
) -> JsResult<()> {
    if !matches!(value, Value::Object(_)) {
        return Ok(());
    }
    let target = container.addr();
    let hit = find(
        value,
        &|v| matches!(v, Value::Object(o) if o.addr() == target),
        &|o| o.is_opaque() || o.is_wrapper(),
        seen,
    );
    if hit {
        return Err(invalid_data(format!("{label} contains a circular value.")));
    }
    Ok(())
}
