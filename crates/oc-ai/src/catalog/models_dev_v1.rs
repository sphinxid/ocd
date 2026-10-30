//! models.dev v1 catalogs (`/api.json`), still accepted from `OPENCODE_MODELS_PATH` and custom
//! `OPENCODE_MODELS_URL` mirrors (`packages/core/src/models-dev/v1.ts`). They are converted to the v2
//! shape, then decoded like any v2 catalog. v1 can't say everything v2 does (one API per model, broad
//! modality kinds), so a converted catalog behaves as v1 did.

use crate::json::Object;
use serde_json::{json, Value};

/// `isV1`: v1 providers always name an AI SDK package (`npm`); v2 providers never do.
pub fn is_v1(catalog: &Object) -> bool {
    catalog.values().any(|provider| matches!(provider.get("npm"), Some(Value::String(_))))
}

// A field decoded by an optional schema: absent is fine, anything present must match.
fn opt<'a>(object: &'a Object, key: &str) -> Result<Option<&'a Value>, ()> {
    Ok(object.get(key))
}

fn opt_str(object: &Object, key: &str) -> Result<Option<String>, ()> {
    match opt(object, key)? {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn opt_num(object: &Object, key: &str) -> Result<Option<f64>, ()> {
    match opt(object, key)? {
        None => Ok(None),
        Some(Value::Number(n)) => n.as_f64().map(Some).ok_or(()),
        Some(_) => Err(()),
    }
}

fn opt_bool(object: &Object, key: &str) -> Result<Option<bool>, ()> {
    match opt(object, key)? {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(()),
    }
}

fn req_str(object: &Object, key: &str) -> Result<String, ()> {
    opt_str(object, key)?.ok_or(())
}

fn strings(value: &Value) -> Result<Vec<String>, ()> {
    value.as_array().ok_or(())?.iter().map(|v| v.as_str().map(String::from).ok_or(())).collect()
}

fn record(value: &Value) -> Result<&Object, ()> {
    value.as_object().ok_or(())
}

struct Tier {
    size: f64,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

struct Cost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tiers: Option<Vec<Tier>>,
}

fn decode_cost(value: &Value) -> Result<Cost, ()> {
    let o = record(value)?;
    let tiers = match opt(o, "tiers")? {
        None => None,
        Some(v) => Some(
            v.as_array()
                .ok_or(())?
                .iter()
                .map(|item| {
                    let t = record(item)?;
                    let tier = record(t.get("tier").ok_or(())?)?;
                    Ok(Tier {
                        size: tier.get("size").and_then(Value::as_f64).ok_or(())?,
                        input: opt_num(t, "input")?,
                        output: opt_num(t, "output")?,
                        cache_read: opt_num(t, "cache_read")?,
                        cache_write: opt_num(t, "cache_write")?,
                    })
                })
                .collect::<Result<Vec<_>, ()>>()?,
        ),
    };
    Ok(Cost {
        input: opt_num(o, "input")?,
        output: opt_num(o, "output")?,
        cache_read: opt_num(o, "cache_read")?,
        cache_write: opt_num(o, "cache_write")?,
        tiers,
    })
}

struct ReasoningOption {
    kind: String,
    values: Option<Vec<Option<String>>>,
    min: Option<f64>,
    max: Option<f64>,
}

struct Mode {
    cost: Option<Cost>,
    body: Option<Object>,
    headers: Option<Object>,
}

struct Model {
    id: String,
    name: String,
    kind: Option<String>,
    family: Option<String>,
    release_date: String,
    status: Option<String>,
    reasoning: Option<bool>,
    reasoning_options: Option<Vec<ReasoningOption>>,
    temperature: Option<bool>,
    tool_call: Option<bool>,
    /// The reasoning field named by `interleaved` (a string or `{field}`).
    interleaved_field: Option<String>,
    cost: Option<Cost>,
    limit: Option<Object>,
    modalities: Option<(Vec<String>, Vec<String>)>,
    modes: Option<Vec<(String, Mode)>>,
    npm: Option<String>,
    api: Option<String>,
    shape: Option<String>,
}

fn decode_model(value: &Value) -> Result<Model, ()> {
    let o = record(value)?;
    let reasoning_options = match opt(o, "reasoning_options")? {
        None => None,
        Some(v) => Some(
            v.as_array()
                .ok_or(())?
                .iter()
                .map(|item| {
                    let r = record(item)?;
                    let values = match opt(r, "values")? {
                        None => None,
                        Some(v) => Some(
                            v.as_array()
                                .ok_or(())?
                                .iter()
                                .map(|x| match x {
                                    Value::Null => Ok(None),
                                    Value::String(s) => Ok(Some(s.clone())),
                                    _ => Err(()),
                                })
                                .collect::<Result<Vec<_>, ()>>()?,
                        ),
                    };
                    Ok(ReasoningOption { kind: req_str(r, "type")?, values, min: opt_num(r, "min")?, max: opt_num(r, "max")? })
                })
                .collect::<Result<Vec<_>, ()>>()?,
        ),
    };
    let interleaved_field = match opt(o, "interleaved")? {
        None | Some(Value::Bool(_)) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(f)) => Some(req_str(f, "field")?),
        Some(_) => return Err(()),
    };
    let limit = match opt(o, "limit")? {
        None => None,
        Some(v) => {
            let l = record(v)?;
            if l.values().any(|x| !x.is_number()) {
                return Err(());
            }
            Some(l.clone())
        }
    };
    let modalities = match opt(o, "modalities")? {
        None => None,
        Some(v) => {
            let m = record(v)?;
            Some((strings(m.get("input").ok_or(())?)?, strings(m.get("output").ok_or(())?)?))
        }
    };
    let modes = match opt(o, "experimental")? {
        None => None,
        Some(v) => match opt(record(v)?, "modes")? {
            None => None,
            Some(modes) => Some(
                record(modes)?
                    .iter()
                    .map(|(name, mode)| {
                        let m = record(mode)?;
                        let (body, headers) = match opt(m, "provider")? {
                            None => (None, None),
                            Some(p) => {
                                let p = record(p)?;
                                let body = match opt(p, "body")? {
                                    None => None,
                                    Some(b) => Some(record(b)?.clone()),
                                };
                                let headers = match opt(p, "headers")? {
                                    None => None,
                                    Some(h) => {
                                        let h = record(h)?;
                                        if h.values().any(|x| !x.is_string()) {
                                            return Err(());
                                        }
                                        Some(h.clone())
                                    }
                                };
                                (body, headers)
                            }
                        };
                        let cost = match opt(m, "cost")? {
                            None => None,
                            Some(c) => Some(decode_cost(c)?),
                        };
                        Ok((name.clone(), Mode { cost, body, headers }))
                    })
                    .collect::<Result<Vec<_>, ()>>()?,
            ),
        },
    };
    let (npm, api, shape) = match opt(o, "provider")? {
        None => (None, None, None),
        Some(p) => {
            let p = record(p)?;
            (opt_str(p, "npm")?, opt_str(p, "api")?, opt_str(p, "shape")?)
        }
    };
    Ok(Model {
        id: req_str(o, "id")?,
        name: req_str(o, "name")?,
        kind: opt_str(o, "type")?,
        family: opt_str(o, "family")?,
        release_date: req_str(o, "release_date")?,
        status: opt_str(o, "status")?,
        reasoning: opt_bool(o, "reasoning")?,
        reasoning_options,
        temperature: opt_bool(o, "temperature")?,
        tool_call: opt_bool(o, "tool_call")?,
        interleaved_field,
        cost: match opt(o, "cost")? {
            None => None,
            Some(c) => Some(decode_cost(c)?),
        },
        limit,
        modalities,
        modes,
        npm,
        api,
        shape,
    })
}

struct Provider<'a> {
    id: String,
    name: String,
    env: Vec<String>,
    npm: String,
    api: Option<String>,
    models: &'a Object,
}

fn decode_provider(value: &Value) -> Result<Provider<'_>, ()> {
    let o = record(value)?;
    Ok(Provider {
        id: req_str(o, "id")?,
        name: req_str(o, "name")?,
        env: strings(o.get("env").ok_or(())?)?,
        npm: req_str(o, "npm")?,
        api: opt_str(o, "api")?,
        models: record(o.get("models").ok_or(())?)?,
    })
}

/// `toV2(catalog)`.
pub fn to_v2(catalog: &Object) -> Object {
    let mut out = Object::new();
    for (key, value) in catalog {
        let Ok(provider) = decode_provider(value) else { continue };
        let mut models = Object::new();
        for (id, value) in provider.models {
            if let Ok(model) = decode_model(value) {
                models.insert(id.clone(), convert_model(&model, &provider));
            }
        }
        out.insert(
            key.clone(),
            json!({"id": provider.id, "name": provider.name, "env": provider.env, "models": models}),
        );
    }
    out
}

// The AI SDK packages that call the Responses API unless told otherwise.
const RESPONSES_BY_DEFAULT: &[&str] = &["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/xai"];

fn insert(object: &mut Object, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.insert(key.into(), value);
    }
}

fn convert_model(model: &Model, provider: &Provider<'_>) -> Value {
    let npm = model.npm.clone().unwrap_or_else(|| provider.npm.clone());
    let protocol = match model.shape.as_deref() {
        Some("responses") => "responses",
        None if RESPONSES_BY_DEFAULT.contains(&npm.as_str()) => "responses",
        _ => "chat-completions",
    };
    let mut out = Object::new();
    out.insert("id".into(), json!(model.id));
    out.insert("type".into(), json!(model.kind.clone().unwrap_or_else(|| "chat".into())));
    out.insert("name".into(), json!(model.name));
    insert(&mut out, "family", model.family.clone().map(Value::String));
    out.insert("release_date".into(), json!(model.release_date));
    insert(&mut out, "status", model.status.clone().map(Value::String));
    out.insert(
        "modalities".into(),
        match &model.modalities {
            Some((input, output)) => json!({"input": input, "output": output}),
            None => json!({"input": ["text"], "output": ["text"]}),
        },
    );
    let mut capabilities = Object::new();
    capabilities.insert("tools".into(), json!({"supported": model.tool_call.unwrap_or(false)}));
    capabilities.insert("reasoning".into(), reasoning(model));
    insert(&mut capabilities, "temperature", model.temperature.map(Value::Bool));
    out.insert("capabilities".into(), Value::Object(capabilities));
    insert(&mut out, "limit", model.limit.clone().map(Value::Object));
    insert(&mut out, "cost", model.cost.as_ref().map(cost));
    let mut api = Object::new();
    api.insert("base_url".into(), json!(model.api.clone().or_else(|| provider.api.clone()).unwrap_or_default()));
    api.insert("sdk".into(), json!({"aisdk": npm}));
    if let Some(field) = &model.interleaved_field {
        api.insert("compatibility".into(), json!({"reasoning_field": field}));
    }
    out.insert("api".into(), json!({ protocol: api }));
    if let Some(modes) = &model.modes {
        let mut converted = Object::new();
        for (name, mode) in modes {
            let mut m = Object::new();
            insert(&mut m, "cost", mode.cost.as_ref().map(cost));
            insert(&mut m, "body", mode.body.clone().map(Value::Object));
            insert(&mut m, "headers", mode.headers.clone().map(Value::Object));
            converted.insert(name.clone(), Value::Object(m));
        }
        out.insert("experimental".into(), json!({"modes": converted}));
    }
    Value::Object(out)
}

fn reasoning(model: &Model) -> Value {
    if model.reasoning != Some(true) {
        return json!({"supported": false});
    }
    let options = model.reasoning_options.as_deref().unwrap_or(&[]);
    let mut out = Object::new();
    out.insert("supported".into(), json!(true));
    if options.iter().any(|o| o.kind == "toggle") {
        out.insert("toggle".into(), json!(true));
    }
    if let Some(effort) = options.iter().find(|o| o.kind == "effort") {
        let values: Vec<String> = effort
            .values
            .clone()
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter(|v| v != "null")
            .collect();
        out.insert("effort".into(), json!(values));
    }
    if let Some(budget) = options.iter().find(|o| o.kind == "budget_tokens") {
        let mut b = Object::new();
        insert(&mut b, "min", budget.min.map(crate::json::num));
        insert(&mut b, "max", budget.max.map(crate::json::num));
        out.insert("budget".into(), Value::Object(b));
    }
    Value::Object(out)
}

// v1 tiers are context bands; v2 says the same with `context` overrides.
fn cost(input: &Cost) -> Value {
    let tiers = input.tiers.as_deref().unwrap_or(&[]);
    let cache = |read: Option<f64>, write: Option<f64>| {
        let mut c = Object::new();
        insert(&mut c, "read", read.map(crate::json::num));
        insert(&mut c, "write", write.map(crate::json::num));
        Value::Object(c)
    };
    let input_overrides: Vec<Value> = tiers
        .iter()
        .map(|tier| {
            let mut o = Object::new();
            o.insert("when".into(), json!({"context": {"gt": crate::json::num(tier.size)}}));
            insert(&mut o, "price", tier.input.map(crate::json::num));
            o.insert("cache".into(), cache(tier.cache_read, tier.cache_write));
            Value::Object(o)
        })
        .collect();
    let output_overrides: Vec<Value> = tiers
        .iter()
        .map(|tier| {
            let mut o = Object::new();
            o.insert("when".into(), json!({"context": {"gt": crate::json::num(tier.size)}}));
            insert(&mut o, "price", tier.output.map(crate::json::num));
            Value::Object(o)
        })
        .collect();
    json!({
        "text": {
            "input": {
                "price": crate::json::num(input.input.unwrap_or(0.0)),
                "cache": cache(input.cache_read, input.cache_write),
                "overrides": input_overrides,
            },
            "output": {
                "price": crate::json::num(input.output.unwrap_or(0.0)),
                "overrides": output_overrides,
            },
        }
    })
}
