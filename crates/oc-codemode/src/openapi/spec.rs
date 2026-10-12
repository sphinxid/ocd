//! Spec analysis: references, directional (`readOnly`/`writeOnly`) projection, schema normalization, parameters,
//! bodies, responses, operation paths, servers, and security. Port of `openapi/spec.ts`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::{Map, Value as Json, json};

use super::json_schema::{from_schema_open_api_3_0, from_schema_open_api_3_1};
use super::types::{
    ApiKeyLocation, Body, BodyMode, InputField, InputLocation, OpenApiError, ParameterStyle, Parsed,
    SecurityRequirement, SecurityScheme,
};
use crate::data::js_ordered;

pub(crate) const METHODS: [&str; 8] = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];
const IGNORED_HEADER_PARAMETERS: [&str; 3] = ["accept", "content-type", "authorization"];

pub(crate) fn is_record(value: Option<&Json>) -> Option<&Map<String, Json>> {
    match value {
        Some(Json::Object(map)) => Some(map),
        _ => None,
    }
}

fn as_array(value: Option<&Json>) -> &[Json] {
    match value {
        Some(Json::Array(items)) => items,
        _ => &[],
    }
}

pub(crate) fn non_empty_string(value: Option<&Json>) -> Option<&str> {
    match value {
        Some(Json::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// JS `String.prototype.trim`.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) <= 0xFFFF && crate::number::is_js_whitespace(c as u16))
}

/// `mediaType.split(";")[0].trim().toLowerCase()`.
pub(crate) fn normalize_media_type(media_type: &str) -> String {
    js_trim(media_type.split(';').next().unwrap_or("")).to_lowercase()
}

pub(crate) fn is_json_media_type(media_type: &str) -> bool {
    let normalized = normalize_media_type(media_type);
    normalized == "application/json" || normalized.ends_with("+json")
}

fn resolve_pointer<'a>(root: &'a Json, reference: &str) -> Option<&'a Json> {
    let mut item = Some(root);
    for segment in reference.get(2..).unwrap_or("").split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        item = is_record(item).and_then(|map| map.get(&segment));
    }
    item
}

/// Follows local `#/` references (cycle-safe) to the referenced value.
pub(crate) fn resolve<'a>(document: &'a Json, value: Option<&'a Json>) -> Option<&'a Json> {
    let mut current = value;
    let mut seen: HashSet<&str> = HashSet::new();
    loop {
        let Some(map) = is_record(current) else { return current };
        let Some(reference) = non_empty_string(map.get("$ref")) else { return current };
        if !reference.starts_with("#/") || seen.contains(reference) {
            return current;
        }
        match resolve_pointer(document, reference) {
            None => return current,
            Some(target) => {
                seen.insert(reference);
                current = Some(target);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Request,
    Response,
}

impl Direction {
    fn hidden_keyword(self) -> &'static str {
        match self {
            Direction::Request => "readOnly",
            Direction::Response => "writeOnly",
        }
    }
}

#[derive(Clone, Copy)]
struct Resource<'a> {
    value: &'a Json,
    root: &'a Json,
}

fn address(value: &Json) -> usize {
    value as *const Json as usize
}

/// Memoized results per schema object; `pending` holds the discovery index of schemas whose strongly connected
/// component is unresolved.
struct Solver<T> {
    values: HashMap<usize, T>,
    pending: HashMap<usize, usize>,
    stack: Vec<usize>,
}

impl<T> Default for Solver<T> {
    fn default() -> Self {
        Solver { values: HashMap::new(), pending: HashMap::new(), stack: Vec::new() }
    }
}

#[derive(Default)]
struct DirectionCache {
    hidden: RefCell<Solver<bool>>,
    names: RefCell<Solver<Rc<HashSet<String>>>>,
}

/// Tarjan's strongly connected components: cycle members all reach the same declarations, so the component root's
/// value is final for every member. Only resolved components are cached.
fn solve_cycles<T: Clone>(
    solver: &RefCell<Solver<T>>,
    key: usize,
    provisional: T,
    scope: &mut usize,
    compute: impl FnOnce(&mut usize) -> T,
) -> T {
    {
        let s = solver.borrow();
        if let Some(cached) = s.values.get(&key) {
            return cached.clone();
        }
        if let Some(&pending) = s.pending.get(&key) {
            *scope = (*scope).min(pending);
            return provisional;
        }
    }
    let (index, base) = {
        let mut s = solver.borrow_mut();
        let index = s.pending.len();
        let base = s.stack.len();
        s.pending.insert(key, index);
        s.stack.push(key);
        (index, base)
    };
    let mut inner = usize::MAX;
    let value = compute(&mut inner);
    if inner < index {
        *scope = (*scope).min(inner);
        return value;
    }
    let mut s = solver.borrow_mut();
    let members: Vec<usize> = s.stack.drain(base..).collect();
    for member in members {
        s.pending.remove(&member);
        s.values.insert(member, value.clone());
    }
    value
}

const NESTED_SCHEMAS: [&str; 6] =
    ["items", "additionalProperties", "unevaluatedProperties", "propertyNames", "then", "else"];
const NESTED_SCHEMA_LISTS: [&str; 3] = ["anyOf", "oneOf", "prefixItems"];
const NESTED_SCHEMA_MAPS: [&str; 4] = ["patternProperties", "dependentSchemas", "$defs", "definitions"];

fn contains_directional(value: &Json) -> bool {
    match value {
        Json::Array(items) => items.iter().any(contains_directional),
        Json::Object(map) => {
            map.get("readOnly") == Some(&Json::Bool(true))
                || map.get("writeOnly") == Some(&Json::Bool(true))
                || map.values().any(contains_directional)
        }
        _ => false,
    }
}

/// One document being converted, with its projection caches.
pub(crate) struct Spec<'a> {
    pub document: &'a Json,
    open_api_3_0: bool,
    directional: bool,
    request: DirectionCache,
    response: DirectionCache,
}

/// Parameter, body, and resulting input fields before input names are assigned.
struct PlannedField {
    name: String,
    location: InputLocation,
    required: bool,
    schema: Json,
    style: Option<ParameterStyle>,
    explode: Option<bool>,
}

type PlannedBody = (Vec<PlannedField>, Option<Body>);

pub(crate) struct OperationInput {
    pub fields: Vec<InputField>,
    pub body: Option<Body>,
}

impl<'a> Spec<'a> {
    pub fn new(document: &'a Json) -> Spec<'a> {
        let open_api_3_0 = non_empty_string(is_record(Some(document)).and_then(|d| d.get("openapi")))
            .is_some_and(|v| v.starts_with("3.0"));
        Spec {
            document,
            open_api_3_0,
            directional: contains_directional(document),
            request: DirectionCache::default(),
            response: DirectionCache::default(),
        }
    }

    pub fn has_directional_schemas(&self) -> bool {
        self.directional
    }

    fn field(&self, key: &str) -> Option<&'a Json> {
        is_record(Some(self.document)).and_then(|d| d.get(key))
    }

    fn cache(&self, direction: Direction) -> &DirectionCache {
        match direction {
            Direction::Request => &self.request,
            Direction::Response => &self.response,
        }
    }

    /// Resolves one `$ref` hop. Local `$defs`/`definitions` pointers resolve against the schema being projected,
    /// other pointers rebase onto the target.
    fn resolve_resource(&self, resource: Resource<'a>) -> Resource<'a> {
        let Some(map) = is_record(Some(resource.value)) else { return resource };
        let Some(reference) = non_empty_string(map.get("$ref")) else { return resource };
        if !reference.starts_with("#/") {
            return resource;
        }
        let local = reference.starts_with("#/$defs/") || reference.starts_with("#/definitions/");
        match resolve_pointer(if local { resource.root } else { self.document }, reference) {
            None => resource,
            Some(target) => Resource { value: target, root: if local { resource.root } else { target } },
        }
    }

    fn is_hidden(&self, resource: Resource<'a>, direction: Direction, scope: &mut usize) -> bool {
        let Some(map) = is_record(Some(resource.value)) else { return false };
        if map.get(direction.hidden_keyword()) == Some(&Json::Bool(true)) {
            return true;
        }
        solve_cycles(&self.cache(direction).hidden, address(resource.value), false, scope, |inner| {
            let target = self.resolve_resource(resource);
            for item in as_array(map.get("allOf")) {
                if self.is_hidden(Resource { value: item, root: resource.root }, direction, inner) {
                    return true;
                }
            }
            !std::ptr::eq(target.value, resource.value) && self.is_hidden(target, direction, inner)
        })
    }

    /// Hidden property names declared by a schema itself or inherited through `$ref` and `allOf` composition.
    fn hidden_names(&self, resource: Resource<'a>, direction: Direction, scope: &mut usize) -> Rc<HashSet<String>> {
        let Some(map) = is_record(Some(resource.value)) else { return Rc::new(HashSet::new()) };
        solve_cycles(&self.cache(direction).names, address(resource.value), Rc::new(HashSet::new()), scope, |inner| {
            let mut names = HashSet::new();
            if let Some(properties) = is_record(map.get("properties")) {
                for (name, property) in properties {
                    let mut fresh = usize::MAX;
                    if self.is_hidden(Resource { value: property, root: resource.root }, direction, &mut fresh) {
                        names.insert(name.clone());
                    }
                }
            }
            for item in as_array(map.get("allOf")) {
                names.extend(
                    self.hidden_names(Resource { value: item, root: resource.root }, direction, inner).iter().cloned(),
                );
            }
            let target = self.resolve_resource(resource);
            if !std::ptr::eq(target.value, resource.value) {
                names.extend(self.hidden_names(target, direction, inner).iter().cloned());
            }
            Rc::new(names)
        })
    }

    fn directional_schema(&self, resource: Resource<'a>, direction: Direction, excluded: &HashSet<String>) -> Json {
        let Some(map) = is_record(Some(resource.value)) else { return resource.value.clone() };
        let mut hidden = excluded.clone();
        let mut fresh = usize::MAX;
        hidden.extend(self.hidden_names(resource, direction, &mut fresh).iter().cloned());
        let none = HashSet::new();
        let project = |item: &'a Json, inherited: &HashSet<String>| {
            self.directional_schema(Resource { value: item, root: resource.root }, direction, inherited)
        };
        let mut out = Map::new();
        for (key, item) in map {
            let k = key.as_str();
            let value = match item {
                Json::Object(properties) if k == "properties" => Json::Object(
                    properties
                        .iter()
                        .filter(|(name, _)| !hidden.contains(*name))
                        .map(|(name, property)| (name.clone(), project(property, &none)))
                        .collect(),
                ),
                Json::Array(names) if k == "required" => Json::Array(
                    names.iter().filter(|n| !matches!(n, Json::String(s) if hidden.contains(s))).cloned().collect(),
                ),
                Json::Array(entries) if k == "allOf" => {
                    Json::Array(entries.iter().map(|entry| project(entry, &hidden)).collect())
                }
                _ if NESTED_SCHEMAS.contains(&k) => project(item, &none),
                Json::Array(entries) if NESTED_SCHEMA_LISTS.contains(&k) => {
                    Json::Array(entries.iter().map(|entry| project(entry, &none)).collect())
                }
                Json::Object(entries) if NESTED_SCHEMA_MAPS.contains(&k) => {
                    Json::Object(entries.iter().map(|(name, entry)| (name.clone(), project(entry, &none))).collect())
                }
                _ => item.clone(),
            };
            out.insert(key.clone(), value);
        }
        Json::Object(out)
    }

    fn directional_root(&self, value: &'a Json, direction: Direction) -> Json {
        self.directional_schema(Resource { value, root: value }, direction, &HashSet::new())
    }

    pub fn normalize_schema(&self, value: Option<&Json>) -> Result<Json, OpenApiError> {
        let Some(map) = is_record(value) else { return Ok(Json::Object(Map::new())) };
        let (mut schema, definitions) = if self.open_api_3_0 {
            from_schema_open_api_3_0(map).map_err(|message| OpenApiError { name: "Error".into(), message })?
        } else {
            from_schema_open_api_3_1(map)
        };
        if !definitions.is_empty() {
            schema.insert("$defs".into(), Json::Object(definitions));
        }
        Ok(Json::Object(schema))
    }

    fn project_schema(&self, value: Option<&'a Json>, direction: Direction) -> Result<Json, OpenApiError> {
        match value {
            Some(value) if self.directional => self.normalize_schema(Some(&self.directional_root(value, direction))),
            other => self.normalize_schema(other),
        }
    }

    pub fn component_definitions(&self, direction: Direction) -> Result<Map<String, Json>, OpenApiError> {
        let components = is_record(self.field("components"));
        let schemas = components.and_then(|c| is_record(c.get("schemas")));
        let mut out = Vec::new();
        if let Some(schemas) = schemas {
            for (name, value) in schemas {
                out.push((name.clone(), self.project_schema(Some(value), direction)?));
            }
        }
        Ok(js_ordered(out))
    }

    fn is_binary_media_type(&self, media_type: &str, value: &'a Json) -> bool {
        let normalized = normalize_media_type(media_type);
        if !is_json_media_type(&normalized) && !normalized.starts_with("text/") {
            return true;
        }
        let Some(map) = is_record(Some(value)) else { return false };
        let schema = resolve(self.document, map.get("schema"));
        is_record(schema).is_some_and(|s| s.get("format") == Some(&Json::String("binary".into())))
    }

    fn operation_parameters(
        &self,
        path_item: &'a Map<String, Json>,
        operation: &'a Map<String, Json>,
    ) -> Result<Parsed<Vec<PlannedField>>, OpenApiError> {
        // Operation parameters override path parameters with the same location and name (keeping the position).
        let mut declared: Vec<(String, String, String, &'a Map<String, Json>)> = Vec::new();
        for raw in as_array(path_item.get("parameters")).iter().chain(as_array(operation.get("parameters"))) {
            let Some(resolved) = is_record(resolve(self.document, Some(raw))) else {
                return Ok(Err("parameter declaration is invalid or unresolved".into()));
            };
            let (Some(name), Some(location)) =
                (non_empty_string(resolved.get("name")), non_empty_string(resolved.get("in")))
            else {
                return Ok(Err("parameter declaration is missing name or location".into()));
            };
            let key = format!("{location}:{name}");
            let entry = (key.clone(), name.to_string(), location.to_string(), resolved);
            match declared.iter().position(|(k, ..)| *k == key) {
                Some(i) => declared[i] = entry,
                None => declared.push(entry),
            }
        }
        let mut unordered: Vec<PlannedField> = Vec::new();
        for (_, name, location, resolved) in declared {
            if location == "cookie" {
                return Ok(Err(format!("cookie parameter '{name}' is not supported")));
            }
            let location = match location.as_str() {
                "path" => InputLocation::Path,
                "query" => InputLocation::Query,
                "header" => InputLocation::Header,
                other => return Ok(Err(format!("parameter '{name}' uses unsupported location '{other}'"))),
            };
            if location == InputLocation::Header && IGNORED_HEADER_PARAMETERS.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            if resolved.get("schema").is_none() && resolved.get("content").is_none() {
                return Ok(Err(format!("parameter '{name}' declares neither schema nor content")));
            }
            if resolved.get("content").is_some() {
                return Ok(Err(format!("parameter '{name}' uses unsupported content encoding")));
            }
            let style_value = resolved.get("style");
            if style_value.is_some() && non_empty_string(style_value).is_none() {
                return Ok(Err(format!("parameter '{name}' has an invalid style")));
            }
            let explode_value = resolved.get("explode");
            if explode_value.is_some() && !matches!(explode_value, Some(Json::Bool(_))) {
                return Ok(Err(format!("parameter '{name}' has an invalid explode value")));
            }
            let reserved = resolved.get("allowReserved");
            if reserved.is_some() && !matches!(reserved, Some(Json::Bool(_))) {
                return Ok(Err(format!("parameter '{name}' has an invalid allowReserved value")));
            }
            if reserved == Some(&Json::Bool(true)) {
                return Ok(Err(format!("parameter '{name}' uses unsupported allowReserved encoding")));
            }
            let declared_style = non_empty_string(style_value)
                .unwrap_or(if location == InputLocation::Query { "form" } else { "simple" })
                .to_string();
            if location == InputLocation::Query && declared_style != "form" && declared_style != "deepObject" {
                return Ok(Err(format!("query parameter '{name}' uses unsupported style '{declared_style}'")));
            }
            if location != InputLocation::Query && declared_style != "simple" {
                return Ok(Err(format!(
                    "{} parameter '{name}' uses unsupported style '{declared_style}'",
                    location.as_str()
                )));
            }
            let style = match declared_style.as_str() {
                "deepObject" => ParameterStyle::DeepObject,
                "form" => ParameterStyle::Form,
                _ => ParameterStyle::Simple,
            };
            let explode = match explode_value {
                Some(Json::Bool(b)) => *b,
                _ => style == ParameterStyle::Form,
            };
            if style == ParameterStyle::DeepObject && !explode {
                return Ok(Err(format!("query parameter '{name}' uses deepObject with explode=false")));
            }
            let mut schema = self.project_schema(resolved.get("schema"), Direction::Request)?;
            if let (Json::Object(base), Some(description)) =
                (&mut schema, non_empty_string(resolved.get("description")))
                && !base.contains_key("description")
            {
                base.insert("description".into(), Json::String(description.into()));
            }
            unordered.push(PlannedField {
                name,
                location,
                required: resolved.get("required") == Some(&Json::Bool(true)) || location == InputLocation::Path,
                schema,
                style: Some(style),
                explode: Some(explode),
            });
        }
        let mut ordered = Vec::with_capacity(unordered.len());
        for location in [InputLocation::Path, InputLocation::Query, InputLocation::Header] {
            let (matching, rest): (Vec<_>, Vec<_>) = unordered.into_iter().partition(|f| f.location == location);
            ordered.extend(matching);
            unordered = rest;
        }
        Ok(Ok(ordered))
    }

    fn operation_body(&self, operation: &'a Map<String, Json>) -> Result<Parsed<PlannedBody>, OpenApiError> {
        let Some(resolved) = is_record(resolve(self.document, operation.get("requestBody"))) else {
            return Ok(Ok((Vec::new(), None)));
        };
        let empty = Map::new();
        let content = is_record(resolved.get("content")).unwrap_or(&empty);
        let selected = content.iter().find(|(media_type, _)| is_json_media_type(media_type));
        let selected = match selected {
            Some((media_type, Json::Object(entry))) => (media_type.clone(), entry.get("schema")),
            _ => {
                let declared = content.keys().cloned().collect::<Vec<_>>().join(", ");
                let declared = if declared.is_empty() { "none".to_string() } else { declared };
                return Ok(Err(format!("request body has no JSON content (declared: {declared})")));
            }
        };
        let (media_type, selected_schema) = selected;
        let resolved_schema = resolve(self.document, selected_schema);
        let schema: Option<Json> = match resolved_schema {
            Some(value) if self.directional => Some(self.directional_root(value, Direction::Request)),
            other => other.cloned(),
        };
        let required = resolved.get("required") == Some(&Json::Bool(true));
        let flattenable = schema.as_ref().and_then(|s| s.as_object()).filter(|s| {
            required
                && s.get("type") == Some(&Json::String("object".into()))
                && is_record(s.get("properties")).is_some()
                && s.get("additionalProperties") == Some(&Json::Bool(false))
                && s.get("nullable") != Some(&Json::Bool(true))
                && s.get("allOf").is_none()
                && s.get("anyOf").is_none()
                && s.get("oneOf").is_none()
        });
        let Some(schema) = flattenable else {
            return Ok(Ok((
                vec![PlannedField {
                    name: "body".into(),
                    location: InputLocation::Body,
                    required,
                    schema: self.project_schema(selected_schema, Direction::Request)?,
                    style: None,
                    explode: None,
                }],
                Some(Body { required, mode: BodyMode::Value, media_type }),
            )));
        };
        let required_properties: HashSet<&str> =
            as_array(schema.get("required")).iter().filter_map(|item| item.as_str()).collect();
        let mut fields = Vec::new();
        // Field schemas were already projected with the body as resolution root.
        for (name, value) in is_record(schema.get("properties")).unwrap_or(&empty) {
            fields.push(PlannedField {
                name: name.clone(),
                location: InputLocation::Body,
                required: required && required_properties.contains(name.as_str()),
                schema: self.normalize_schema(Some(value))?,
                style: None,
                explode: None,
            });
        }
        Ok(Ok((fields, Some(Body { required, mode: BodyMode::Object, media_type }))))
    }

    pub fn operation_input(
        &self,
        path_item: &'a Map<String, Json>,
        operation: &'a Map<String, Json>,
    ) -> Result<Parsed<OperationInput>, OpenApiError> {
        let parameters = match self.operation_parameters(path_item, operation)? {
            Ok(parameters) => parameters,
            Err(reason) => return Ok(Err(reason)),
        };
        let (body_fields, body) = match self.operation_body(operation)? {
            Ok(body) => body,
            Err(reason) => return Ok(Err(reason)),
        };
        let fields: Vec<PlannedField> = parameters.into_iter().chain(body_fields).collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for field in &fields {
            *counts.entry(field.name.as_str()).or_default() += 1;
        }
        let conflicts: HashSet<String> =
            counts.into_iter().filter(|(_, n)| *n > 1).map(|(name, _)| name.to_string()).collect();
        let mut used: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(fields.len());
        for field in fields {
            let base = if conflicts.contains(&field.name) {
                format!("{}_{}", field.location.as_str(), field.name)
            } else {
                field.name.clone()
            };
            let mut index = 1;
            let input_name = loop {
                let candidate = if index == 1 { base.clone() } else { format!("{base}_{index}") };
                if !used.contains(&candidate) {
                    break candidate;
                }
                index += 1;
            };
            used.insert(input_name.clone());
            out.push(InputField {
                input_name,
                name: field.name,
                location: field.location,
                required: field.required,
                schema: field.schema,
                style: field.style,
                explode: field.explode,
            });
        }
        Ok(Ok(OperationInput { fields: out, body }))
    }

    fn successful_responses(&self, operation: &'a Map<String, Json>) -> Parsed<Vec<&'a Map<String, Json>>> {
        let Some(responses) = is_record(operation.get("responses")) else { return Ok(Vec::new()) };
        let is_status = |s: &str| s.len() == 3 && s.starts_with('2') && s.bytes().skip(1).all(|b| b.is_ascii_digit());
        let mut numbered: Vec<(&String, &Json)> = responses.iter().filter(|(s, _)| is_status(s)).collect();
        numbered.sort_by(|a, b| a.0.cmp(b.0));
        let ranged = responses.iter().filter(|(s, _)| s.to_uppercase() == "2XX");
        let mut out = Vec::new();
        for (_, value) in numbered.into_iter().chain(ranged) {
            match is_record(resolve(self.document, Some(value))) {
                Some(resolved) if non_empty_string(resolved.get("$ref")).is_none() => out.push(resolved),
                _ => return Err("successful response declaration is invalid or unresolved".into()),
            }
        }
        Ok(out)
    }

    pub fn operation_output(
        &self,
        operation: &'a Map<String, Json>,
        definitions: &Map<String, Json>,
    ) -> Result<Parsed<Option<Json>>, OpenApiError> {
        if operation.get("x-websocket") == Some(&Json::Bool(true)) {
            return Ok(Err("WebSocket operations are not supported".into()));
        }
        let responses = match self.successful_responses(operation) {
            Ok(responses) => responses,
            Err(reason) => return Ok(Err(reason)),
        };
        let streams = responses.iter().any(|response| {
            is_record(response.get("content"))
                .is_some_and(|content| content.keys().any(|mt| normalize_media_type(mt) == "text/event-stream"))
        });
        if streams {
            return Ok(Err("SSE operations are not supported".into()));
        }
        let binary = responses.iter().any(|response| {
            is_record(response.get("content"))
                .is_some_and(|content| content.iter().any(|(mt, value)| self.is_binary_media_type(mt, value)))
        });
        if binary {
            return Ok(Err("binary responses are not supported".into()));
        }
        let mut outcomes: Vec<Json> = Vec::new();
        for response in responses {
            let content = response.get("content");
            if content.is_some() && is_record(content).is_none() {
                return Ok(Ok(None));
            }
            let empty = Map::new();
            let content = is_record(content).unwrap_or(&empty);
            if content.is_empty() {
                outcomes.push(json!({"type": "null"}));
                continue;
            }
            for (media_type, value) in content {
                if !is_json_media_type(media_type) {
                    outcomes.push(json!({"type": "string"}));
                    continue;
                }
                let Some(schema) = is_record(Some(value)).and_then(|v| v.get("schema")) else {
                    return Ok(Ok(None));
                };
                outcomes.push(self.project_schema(Some(schema), Direction::Response)?);
            }
        }
        if outcomes.is_empty() {
            return Ok(Ok(None));
        }
        let schema = if outcomes.len() == 1 { outcomes.pop().unwrap() } else { json!({ "anyOf": outcomes }) };
        Ok(Ok(Some(with_definitions(schema, definitions))))
    }
}

pub(crate) fn with_definitions(schema: Json, definitions: &Map<String, Json>) -> Json {
    if definitions.is_empty() {
        return schema;
    }
    let Json::Object(mut schema) = schema else { return schema };
    let mut entries: Vec<(String, Json)> = definitions.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(Json::Object(local)) = schema.get("$defs") {
        entries.extend(local.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    schema.insert("$defs".into(), Json::Object(js_ordered(entries)));
    Json::Object(schema)
}

pub(crate) fn input_schema(fields: &[InputField], definitions: &Map<String, Json>) -> Json {
    let required: Vec<Json> =
        fields.iter().filter(|f| f.required).map(|f| Json::String(f.input_name.clone())).collect();
    let mut schema = Map::new();
    schema.insert("type".into(), Json::String("object".into()));
    schema.insert(
        "properties".into(),
        Json::Object(js_ordered(fields.iter().map(|f| (f.input_name.clone(), f.schema.clone())).collect())),
    );
    if !required.is_empty() {
        schema.insert("required".into(), Json::Array(required));
    }
    with_definitions(Json::Object(schema), definitions)
}

fn sanitize_operation_segment(raw: &str) -> String {
    let mut replaced = String::new();
    let mut in_run = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            replaced.push(c);
            in_run = false;
        } else if !in_run {
            replaced.push('_');
            in_run = true;
        }
    }
    let trimmed = replaced.trim_matches('_');
    let out =
        if trimmed.starts_with(|c: char| c.is_ascii_digit()) { format!("_{trimmed}") } else { trimmed.to_string() };
    if out.is_empty() { "operation".into() } else { out }
}

fn fallback_operation_id(method: &str, path: &str) -> String {
    let mut words: Vec<String> = vec![method.to_string()];
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let parts: Vec<&str> = if part.starts_with('{') && part.ends_with('}') && part.len() >= 2 {
            vec!["by", &part[1..part.len() - 1]]
        } else {
            vec![part]
        };
        for piece in parts {
            words.extend(piece.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(String::from));
        }
    }
    words
        .iter()
        .enumerate()
        .map(|(index, word)| {
            let lower = word.to_lowercase();
            if index == 0 {
                lower
            } else {
                let mut chars = lower.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            }
        })
        .collect()
}

fn capitalize(segment: &str) -> String {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn is_operation_path_available(segments: &[String], used: &HashSet<String>, namespaces: &HashSet<String>) -> bool {
    let key = segments.join(".");
    if used.contains(&key) || namespaces.contains(&key) {
        return false;
    }
    (0..segments.len().saturating_sub(1)).all(|index| !used.contains(&segments[..=index].join(".")))
}

pub(crate) fn operation_path(
    method: &str,
    path: &str,
    operation: &Map<String, Json>,
    used: &HashSet<String>,
    namespaces: &HashSet<String>,
) -> Vec<String> {
    let raw: Vec<String> = match non_empty_string(operation.get("operationId")) {
        None => vec![fallback_operation_id(method, path)],
        Some(id) => id.split('.').map(String::from).collect(),
    };
    let segments: Vec<String> = raw.iter().map(|s| sanitize_operation_segment(s)).collect();
    if is_operation_path_available(&segments, used, namespaces) {
        return segments;
    }
    let conflict = (0..segments.len().saturating_sub(1)).find(|index| used.contains(&segments[..=*index].join(".")));
    if let Some(conflict) = conflict
        && conflict + 1 < segments.len()
    {
        let mut collapsed = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            if index == conflict {
                collapsed.push(format!("{segment}{}", capitalize(&segments[index + 1])));
            } else if index != conflict + 1 {
                collapsed.push(segment.clone());
            }
        }
        if is_operation_path_available(&collapsed, used, namespaces) {
            return collapsed;
        }
    }
    let fallback = segments.join("_");
    let mut index = 2;
    loop {
        let candidate = vec![format!("{fallback}_{index}")];
        if is_operation_path_available(&candidate, used, namespaces) {
            return candidate;
        }
        index += 1;
    }
}

fn has_template(url: &str) -> bool {
    let mut open = false;
    let mut content = false;
    for c in url.chars() {
        match c {
            '{' => {
                open = true;
                content = false;
            }
            '}' => {
                if open && content {
                    return true;
                }
                open = false;
            }
            _ => {
                if open {
                    content = true;
                }
            }
        }
    }
    false
}

pub(crate) fn spec_server_url(source: &Map<String, Json>) -> Parsed<String> {
    let server = as_array(source.get("servers")).iter().find_map(|s| s.as_object());
    let Some(url) = server.and_then(|s| non_empty_string(s.get("url"))) else {
        return Err("spec declares no servers; pass baseUrl".into());
    };
    if has_template(url) {
        return Err(format!("server URL '{url}' is not an absolute URL; pass baseUrl"));
    }
    validate_base_url(url)
}

pub(crate) fn validate_base_url(value: &str) -> Parsed<String> {
    let invalid = || format!("server URL '{value}' is not an absolute HTTP(S) URL");
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(invalid());
    }
    let Ok(url) = url::Url::parse(value) else { return Err(invalid()) };
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(invalid());
    }
    if url.query().is_some_and(|q| !q.is_empty()) || url.fragment().is_some_and(|f| !f.is_empty()) {
        return Err(format!("server URL '{value}' contains an unsupported query string or fragment"));
    }
    Ok(value.to_string())
}

pub(crate) fn security_requirements(value: Option<&Json>) -> Parsed<Vec<SecurityRequirement>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let Json::Array(items) = value else { return Err("security declaration is not an array".into()) };
    let mut requirements = Vec::new();
    for item in items {
        let Json::Object(item) = item else { return Err("security requirement is not an object".into()) };
        let mut requirement: SecurityRequirement = Vec::new();
        for (name, scopes) in item {
            let Json::Array(scopes) = scopes else {
                return Err("security requirement scopes are not string arrays".into());
            };
            let parsed: Vec<String> = scopes.iter().filter_map(|s| s.as_str().map(String::from)).collect();
            if parsed.len() != scopes.len() {
                return Err("security requirement scopes are not string arrays".into());
            }
            requirement.push((name.clone(), parsed));
        }
        requirements.push(requirement);
    }
    Ok(requirements)
}

fn scheme<'s>(schemes: &'s [(String, SecurityScheme)], name: &str) -> Option<&'s SecurityScheme> {
    schemes.iter().find(|(n, _)| n == name).map(|(_, s)| s)
}

fn is_cookie_scheme(scheme: Option<&SecurityScheme>) -> bool {
    matches!(scheme, Some(SecurityScheme::ApiKey { location: ApiKeyLocation::Cookie, .. }))
}

pub(crate) fn operation_security_requirements(
    value: Option<&Json>,
    defaults: &Parsed<Vec<SecurityRequirement>>,
    schemes: &[(String, SecurityScheme)],
) -> Parsed<Vec<SecurityRequirement>> {
    let parsed = match value {
        None => defaults.clone()?,
        Some(_) => security_requirements(value)?,
    };
    let supported: Vec<SecurityRequirement> = parsed
        .iter()
        .filter(|requirement| {
            requirement.iter().all(|(name, _)| {
                let found = scheme(schemes, name);
                found.is_some() && !is_cookie_scheme(found)
            })
        })
        .cloned()
        .collect();
    if parsed.is_empty() || !supported.is_empty() {
        return Ok(supported);
    }
    let mut names: Vec<&str> = Vec::new();
    for requirement in &parsed {
        for (name, _) in requirement {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }
    match names.iter().find(|name| is_cookie_scheme(scheme(schemes, name))) {
        None => Err(format!("security requirement references missing or malformed scheme: {}", names.join(", "))),
        Some(cookie) => Err(format!("cookie authentication '{cookie}' is not supported")),
    }
}

pub(crate) fn security_schemes(spec: &Spec) -> Vec<(String, SecurityScheme)> {
    let components = is_record(spec.field("components"));
    let Some(declared) = components.and_then(|c| is_record(c.get("securitySchemes"))) else { return Vec::new() };
    // `Object.fromEntries`: JS property order.
    let mut out: Vec<(String, Json)> = Vec::new();
    let mut parsed: HashMap<String, SecurityScheme> = HashMap::new();
    for (name, value) in declared {
        let Some(resolved) = is_record(resolve(spec.document, Some(value))) else { continue };
        let scheme = match non_empty_string(resolved.get("type")) {
            Some("apiKey") => {
                let carrier = non_empty_string(resolved.get("in"));
                let Some(parameter) = non_empty_string(resolved.get("name")) else { continue };
                let location = match carrier {
                    Some("header") => ApiKeyLocation::Header,
                    Some("query") => ApiKeyLocation::Query,
                    Some("cookie") => ApiKeyLocation::Cookie,
                    _ => continue,
                };
                SecurityScheme::ApiKey { name: parameter.to_string(), location }
            }
            Some("http") => match non_empty_string(resolved.get("scheme")) {
                Some(scheme) => SecurityScheme::Http { scheme: scheme.to_lowercase() },
                None => continue,
            },
            Some("oauth2") => SecurityScheme::OAuth2,
            Some("openIdConnect") => SecurityScheme::OpenIdConnect,
            _ => continue,
        };
        out.push((name.clone(), Json::Null));
        parsed.insert(name.clone(), scheme);
    }
    js_ordered(out)
        .into_iter()
        .map(|(name, _)| {
            let scheme = parsed.remove(&name).unwrap();
            (name, scheme)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids() {
        assert_eq!(
            fallback_operation_id("get", "/organizations/{organizationId}/users/{id}"),
            "getOrganizationsByOrganizationidUsersById"
        );
        assert_eq!(sanitize_operation_segment("--9lives!!x--"), "_9lives_x");
        assert_eq!(sanitize_operation_segment("..."), "operation");
    }

    #[test]
    fn templates() {
        assert!(has_template("https://{host}/x"));
        assert!(!has_template("https://h/{}"));
        assert!(has_template("https://h/{{a}}"));
    }
}
