//! Replays recorded extension host functions (shared by the extension and hook replay tests).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::Engine;
use oc_codemode::host::{HostCall, HostFunction, HostValue, SimpleExtension};
use serde_json::Value as Json;

pub type Events = Arc<Mutex<VecDeque<Json>>>;

pub struct Replay {
    pub label: String,
    pub name: String,
    pub length: usize,
    pub events: Events,
}

impl HostFunction for Replay {
    fn name(&self) -> &str {
        &self.name
    }
    fn length(&self) -> usize {
        self.length
    }
    fn call(&self, _args: Vec<HostValue>) -> HostCall {
        let event = {
            let mut events = self.events.lock().unwrap();
            let i = events.iter().position(|e| e["label"] == self.label.as_str());
            i.and_then(|i| events.remove(i))
        };
        let Some(e) = event else {
            return HostCall::Ready(Err(HostValue::error("Error", format!("replay: no recorded call for {}", self.label))));
        };
        let outcome: Result<HostValue, HostValue> = match e["outcome"].as_str() {
            Some("success") => Ok(to_host(&e["value"], &self.label, &self.events)),
            Some("failure") => Err(to_host(&e["errorValue"], &self.label, &self.events)),
            _ => {
                return HostCall::Pending(Box::pin(futures::future::pending()));
            }
        };
        match e["returned"].as_str() {
            Some("promise") => HostCall::Pending(Box::pin(async move {
                tokio::task::yield_now().await;
                outcome
            })),
            _ => HostCall::Ready(outcome),
        }
    }
}

pub fn to_host(value: &Json, label: &str, events: &Events) -> HostValue {
    let next = |v: &Json, l: String| to_host(v, &l, events);
    match value {
        Json::Null => HostValue::Null,
        Json::Bool(b) => HostValue::Bool(*b),
        Json::Number(n) => HostValue::Number(n.as_f64().unwrap()),
        Json::String(s) => HostValue::String(s.clone()),
        Json::Array(items) => {
            HostValue::Array(items.iter().enumerate().map(|(i, v)| next(v, format!("{label}[{i}]"))).collect())
        }
        Json::Object(m) if m.len() == 1 || m.contains_key("$instance") => {
            let (k, v) = m.iter().next().unwrap();
            match k.as_str() {
                "$undefined" => HostValue::Undefined,
                "$number" => HostValue::Number(match v.as_str() {
                    Some("-0") => -0.0,
                    Some("Infinity") => f64::INFINITY,
                    Some("-Infinity") => f64::NEG_INFINITY,
                    _ => f64::NAN,
                }),
                "$function" => {
                    let name = v.as_str().unwrap_or("").to_string();
                    HostValue::Function(Arc::new(Replay { label: label.to_string(), name, length: 0, events: events.clone() }))
                }
                "$date" => HostValue::Date(match v.as_str() {
                    Some(iso) => chrono::DateTime::parse_from_rfc3339(iso).map(|d| d.timestamp_millis() as f64).unwrap_or(f64::NAN),
                    None => f64::NAN,
                }),
                "$regexp" => HostValue::RegExp {
                    source: v["source"].as_str().unwrap_or("").into(),
                    flags: v["flags"].as_str().unwrap_or("").into(),
                },
                "$bytes" => HostValue::Bytes(base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap_or("")).unwrap_or_default()),
                "$map" => HostValue::Map(
                    v.as_array().unwrap().iter().map(|p| (next(&p[0], label.to_string()), next(&p[1], label.to_string()))).collect(),
                ),
                "$set" => HostValue::Set(v.as_array().unwrap().iter().map(|p| next(p, label.to_string())).collect()),
                "$url" => HostValue::Url(v.as_str().unwrap_or("").into()),
                "$urlsearchparams" => HostValue::SearchParams(
                    url::form_urlencoded::parse(v.as_str().unwrap_or("").as_bytes()).into_owned().collect(),
                ),
                "$headers" => HostValue::Headers(
                    v.as_array()
                        .unwrap()
                        .iter()
                        .map(|p| (p[0].as_str().unwrap_or("").into(), p[1].as_str().unwrap_or("").into()))
                        .collect(),
                ),
                "$error" => {
                    let o = v.as_object().unwrap();
                    let name = o.get("name").and_then(|n| n.as_str()).unwrap_or("Error");
                    let standard = oc_codemode::interp::errors::ErrorType::from_name(name).is_some();
                    HostValue::Error {
                        name: o.get("name").and_then(|n| n.as_str()).unwrap_or("Error").into(),
                        message: o.get("message").and_then(|n| n.as_str()).unwrap_or("").into(),
                        cause: o.get("cause").map(|c| Box::new(next(c, format!("{label}.cause")))),
                        fields: o
                            .iter()
                            .filter(|(k, _)| !["message", "cause"].contains(&k.as_str()) && (k.as_str() != "name" || !standard))
                            .map(|(k, v)| (k.clone(), next(v, format!("{label}.{k}"))))
                            .collect(),
                    }
                }
                "$symbol" => HostValue::Unrepresentable("a symbol".into()),
                "$bigint" => HostValue::Unrepresentable("a bigint".into()),
                "$instance" => HostValue::Unrepresentable(format!("a {}", v.as_str().unwrap_or("object"))),
                "$promise" => HostValue::Unrepresentable("a Promise".into()),
                _ => HostValue::Object(vec![(k.clone(), next(v, format!("{label}.{k}")))]),
            }
        }
        Json::Object(m) => HostValue::Object(m.iter().map(|(k, v)| (k.clone(), next(v, format!("{label}.{k}")))).collect()),
    }
}


/// Extensions whose globals replay the record's `extension` events.
pub fn replay_extensions(record: &Json) -> Vec<Arc<dyn oc_codemode::host::Extension>> {
    let events: Events = Arc::new(Mutex::new(
        record["events"].as_array().unwrap().iter().filter(|e| e["type"] == "extension").cloned().collect(),
    ));
    let mut exts: Vec<Arc<dyn oc_codemode::host::Extension>> = Vec::new();
    for ext in record["extensions"].as_array().cloned().unwrap_or_default() {
        let globals = ext["globals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                let name = g.as_str().unwrap().to_string();
                let f: Arc<dyn HostFunction> =
                    Arc::new(Replay { label: name.clone(), name: name.clone(), length: 1, events: events.clone() });
                (name, f)
            })
            .collect();
        exts.push(Arc::new(SimpleExtension { name: ext["name"].as_str().unwrap().into(), globals }));
    }
    exts
}
