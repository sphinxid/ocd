#!/usr/bin/env python3
"""Generate `src/generated.rs` from `assets/openapi.json`.

Reproducible: `python3 crates/oc-server/codegen/gen.py` (from the repo root, or pass the
openapi path and output path as arguments). The output is deterministic (no timestamps).

What is generated:
  * one Rust type per component schema (282), plus named types for inline objects/enums/unions;
  * a literal marker type (`LitXxx`) for every single-value enum (discriminators, `_tag`s);
  * `ErrorEncoded` impls (tag + HTTP status) for every `*ErrorEncoded` component;
  * per-operation request structs (path + query + body), inline body/response types;
  * the `Backend` trait (one method per operationId, snake_case), with default bodies that
    answer "not implemented";
  * the operation table (`OPS`) and the generic dispatcher used by the router.

Mapping rules (see README "Generated types"):
  * required property              -> `T`
  * required + nullable            -> `Option<T>` always serialized (null)
  * optional (not required)        -> `Option<T>` omitted when `None`
  * `anyOf: [X, null]`             -> `Option<X>`
  * `anyOf` unions                 -> `#[serde(untagged)]` enums; variants carrying a literal
                                      discriminator field are told apart by `LitXxx` markers
  * `number`                       -> `f64` serialized as an integer when integral (JS parity)
  * `number | "Infinity"|...`      -> `JsNumber`
  * `{type: object}`               -> `JsonObject` (`serde_json::Map`)
  * `additionalProperties: X`      -> `IndexMap<String, X>`
  * `{}` / `not`                   -> `serde_json::Value`
"""

import json
import os
import re
import sys
from collections import OrderedDict

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
SPEC_PATH = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "assets", "openapi.json")
OUT_PATH = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, "..", "src", "generated.rs")

spec = json.load(open(SPEC_PATH), object_pairs_hook=OrderedDict)
SCHEMAS = spec["components"]["schemas"]

NONFINITE = ["Infinity", "-Infinity", "NaN"]


def _nest_nonfinite(node):
    """Since v2.2 the document flattens `Schema.Number`'s `anyOf[number, "Infinity"|...]` into the
    enclosing union (`anyOf[string, number, enum Infinity…, boolean]`). Nest the pair again so it
    maps to `JsNumber` exactly like the older shape."""
    if isinstance(node, list):
        for x in node:
            _nest_nonfinite(x)
        return
    if not isinstance(node, dict):
        return
    for v in node.values():
        _nest_nonfinite(v)
    a = node.get("anyOf")
    if not isinstance(a, list) or len(a) <= 2:
        return
    out = []
    i = 0
    while i < len(a):
        m = a[i]
        nxt = a[i + 1] if i + 1 < len(a) else None
        if (isinstance(m, dict) and m == {"type": "number"} and isinstance(nxt, dict)
                and nxt.get("type") == "string" and nxt.get("enum") == NONFINITE and len(nxt) == 2):
            out.append(OrderedDict([("anyOf", [m, nxt])]))
            i += 2
            continue
        out.append(m)
        i += 1
    if len(out) != len(a):
        node["anyOf"] = out


_nest_nonfinite(spec)

RUST_KEYWORDS = {
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
}

# HTTP status per error `_tag` (spec 06 §6.1).
ERROR_STATUS = {
    "InvalidRequestError": 400, "InvalidCursorError": 400, "RpcError": 400,
    "FormInvalidAnswerError": 400, "WorktreeError": 400, "UnauthorizedError": 401,
    "ForbiddenError": 403, "LocationNotFoundError": 404, "SessionNotFoundError": 404,
    "MessageNotFoundError": 404, "AgentNotFoundError": 404, "ProviderNotFoundError": 404,
    "IntegrationNotFoundError": 404, "IntegrationAttemptNotFoundError": 404,
    "IntegrationMethodNotFoundError": 404, "ProjectNotFoundError": 404,
    "FileNotFoundError": 404, "SkillNotFoundError": 404, "McpServerNotFoundError": 404,
    "CommandNotFoundError": 404, "PermissionNotFoundError": 404, "PermissionSavedNotFoundError": 404,
    "FormNotFoundError": 404,
    "PtyNotFoundError": 404, "ShellNotFoundError": 404, "ConflictError": 409,
    "SessionBusyError": 409, "FormAlreadySettledError": 409,
    "InstructionEntryValueTooLargeError": 413, "UnknownError": 500,
    "CommandExecutionError": 500, "RpcInternalError": 500, "VcsInitNotSupportedError": 501,
    "ServiceUnavailableError": 503, "PluginNotFoundError": 404, "ModelNotFoundError": 404,
    "UnsupportedModelError": 422, "ProviderRequestError": 502, "ScheduleNotFoundError": 404,
}

# Scope per operation (spec 06 §1.1 / §7). Operations with a `location` query parameter are L.
SCOPE_S = {
    "session.update", "session.fork", "session.switchAgent", "session.switchModel",
    "session.prompt", "session.command", "experimental.session.skill", "session.synthetic",
    "session.shell", "session.compact", "experimental.session.wait", "session.revert.stage",
    "session.revert.clear", "session.revert.commit",
    "experimental.session.instructions.entry.list", "experimental.session.instructions.entry.put",
    "experimental.session.instructions.entry.remove", "session.generate", "session.interrupt",
    "session.background", "session.permission.create", "session.permission.get",
    "session.permission.reply",
}
SCOPE_F = {"session.form.list", "session.form.create", "session.form.get",
           "session.form.cancel", "session.form.reply"}
SCOPE_H = {"session.permission.list", "pty.connect"}

# Operations owned by the server itself (not part of the Backend trait).
SERVER_OWNED = {"server.pair", "server.connect", "pty.connect.token",
                "experimental.persistentPty.connectToken"}

# Operations whose handlers are hand-written (signature overridden below).
SPECIAL = {
    "session.list": ("crate::backend::SessionListRequest", "Vec<SessionInfo>"),
    "session.message.list": ("crate::backend::SessionMessageListRequest", "Vec<SessionMessageInfo>"),
    "experimental.session.log": ("SessionLogRequest", "crate::backend::SessionLogStream"),
    "event.subscribe": ("crate::backend::EventSubscribeRequest", "crate::backend::EventSubscription"),
    "fs.read": ("crate::backend::FsReadRequest", "crate::backend::FsFile"),
    "pty.connect": ("crate::backend::PtyAttachRequest", "crate::backend::PtyAttachment"),
    "experimental.media.get": ("MediaGetRequest", "crate::backend::MediaBlob"),
    "experimental.persistentPty.connect": ("crate::backend::PersistentPtyAttachRequest",
                              "crate::backend::PersistentPtyAttachment"),
    "rpc.call": None,      # generated normally, but defects map to RpcInternalError
    "server.info": None,   # generated normally, default impl builds from ctx.server
}

# Numeric / boolean query parameters (OpenAPI shows them as strings; upstream decodes
# NumberFromString + checks). (op, name) -> (rust kind, min, max)
# kind: "int" = Int with isGreaterThanOrEqualTo(min)/isLessThanOrEqualTo(max), "pos" = PositiveInt
# (isGreaterThan(0)) with optional max, "num" = any number. The kind picks Effect's check message.
QUERY_NUMERIC = {
    ("session.list", "limit"): ("int", 1, None),
    ("session.message.list", "limit"): ("int", 1, 200),
    ("experimental.session.stats", "from"): ("num", None, None),
    ("experimental.session.stats", "to"): ("num", None, None),
    ("session.diff", "context"): ("int", 0, None),
    ("experimental.session.log", "after"): ("int", 0, None),
    ("fs.find", "limit"): ("pos", None, None),
    ("experimental.persistentPty.read", "lines"): ("pos", None, 65535),
    ("shell.output", "cursor"): ("int", 0, None),
    ("shell.output", "limit"): ("int", 0, None),
    ("vcs.branch.list", "limit"): ("pos", None, None),
    ("vcs.diff", "context"): ("int", 0, None),
}


def pascal(s):
    parts = re.split(r"[^A-Za-z0-9]+", s)
    out = ""
    for p in parts:
        if not p:
            continue
        out += p[0].upper() + p[1:]
    if not out or out[0].isdigit():
        out = "V" + out
    return out


def snake(s):
    s = s.lstrip("$")
    s = re.sub(r"[^A-Za-z0-9]+", "_", s)
    s = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", s)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    s = s.lower().strip("_")
    if not s:
        s = "field"
    if s[0].isdigit():
        s = "f_" + s
    return s


def field_ident(name):
    s = snake(name)
    if s in ("self", "super", "crate", "Self"):
        return s + "_"
    if s in RUST_KEYWORDS:
        return "r#" + s
    return s


def op_pascal(op_id):
    return pascal(op_id)


def op_snake(op_id):
    return "_".join(snake(p) for p in op_id.split("."))


def ref_name(ref):
    assert ref.startswith("#/components/schemas/")
    return ref[len("#/components/schemas/"):]


def rust_str(s):
    return json.dumps(s, ensure_ascii=False)


def is_null(s):
    return isinstance(s, dict) and s.get("type") == "null" and len(s) == 1


def is_nonfinite_number(s):
    a = s.get("anyOf")
    if not a or len(a) != 2:
        return False
    return (a[0].get("type") == "number" and len(a[0]) == 1 and a[1].get("type") == "string"
            and a[1].get("enum") == ["Infinity", "-Infinity", "NaN"])


# --------------------------------------------------------------------------------------------
# Type model
# --------------------------------------------------------------------------------------------

class Gen:
    def __init__(self):
        self.items = OrderedDict()   # rust name -> rendered code
        self.names = set()
        self.lits = OrderedDict()    # (kind, value) -> rust name
        self.lit_names = set()
        self.component_types = {}    # component name -> rust type expr
        self.defaults = set()        # type names deriving Default
        self.struct_fields = {}      # struct name -> list of field type exprs (for Default calc)
        self.deps = {}               # type name -> set of named types used by value (non-indirect)
        self.f64_aliases = set()
        self.pending = []

    def unique(self, name):
        base = name
        i = 2
        while name in self.names:
            name = f"{base}{i}"
            i += 1
        self.names.add(name)
        return name

    def lit(self, value):
        if isinstance(value, bool):
            key = ("bool", value)
            name = "LitTrue" if value else "LitFalse"
        elif isinstance(value, int):
            key = ("int", value)
            name = f"Lit{value}" if value >= 0 else f"LitNeg{-value}"
        else:
            key = ("str", value)
            name = "Lit" + pascal(value)
        if key in self.lits:
            return self.lits[key]
        base = name
        i = 2
        while name in self.lit_names:
            name = f"{base}{i}"
            i += 1
        self.lit_names.add(name)
        self.lits[key] = name
        return name

    # ---- component resolution --------------------------------------------------------------

    def component_kind(self, name):
        s = SCHEMAS[name]
        return s

    def resolve_alias_prim(self, schema):
        """Follow $ref chains; return the primitive type keyword if the target is a plain alias."""
        seen = 0
        while "$ref" in schema and seen < 20:
            schema = SCHEMAS[ref_name(schema["$ref"])]
            seen += 1
        if "enum" in schema or "anyOf" in schema or "properties" in schema:
            return None
        return schema.get("type")

    # ---- type expression -------------------------------------------------------------------

    def rtype(self, schema, ctx):
        """Return (rust_type_expr, flags) where flags is a set of {'f64','opt'}."""
        if schema is True or schema == {}:
            return "serde_json::Value", set()
        if "$ref" in schema:
            name = ref_name(schema["$ref"])
            rt = self.component_types[name]
            prim = self.resolve_alias_prim(schema)
            flags = {"f64"} if prim == "number" else set()
            return rt, flags
        if "not" in schema:
            return "serde_json::Value", set()
        if "anyOf" in schema:
            a = schema["anyOf"]
            if is_nonfinite_number(schema):
                return "JsNumber", set()
            non_null = [x for x in a if not is_null(x)]
            if len(non_null) < len(a):
                if len(non_null) == 1:
                    inner, fl = self.rtype(non_null[0], ctx)
                    if inner == "serde_json::Value":
                        return "Option<serde_json::Value>", {"opt"}
                    return f"Option<{inner}>", fl | {"opt"}
                inner = self.union(ctx, non_null)
                return f"Option<{inner}>", {"opt"}
            if len(a) == 1:
                return self.rtype(a[0], ctx)
            return self.union(ctx, a), set()
        if "allOf" in schema and "properties" not in schema:
            return "serde_json::Value", set()
        if "enum" in schema:
            vals = schema["enum"]
            if len(vals) == 1:
                return self.lit(vals[0]), set()
            assert schema.get("type") == "string", (ctx, schema)
            return self.string_enum(ctx, vals), set()
        t = schema.get("type")
        if t == "string":
            return "String", set()
        if t == "integer":
            return "i64", set()
        if t == "number":
            return "f64", {"f64"}
        if t == "boolean":
            return "bool", set()
        if t == "array":
            items = schema.get("items", {})
            inner, fl = self.rtype(items, ctx + "Item")
            if "f64" in fl:
                return f"Vec<f64>", {"vecf64"}
            return f"Vec<{inner}>", set()
        if t == "object":
            props = schema.get("properties")
            ap = schema.get("additionalProperties")
            if props:
                return self.struct(ctx, schema), set()
            if isinstance(ap, dict):
                inner, fl = self.rtype(ap, ctx + "Value")
                assert "f64" not in fl
                return f"indexmap::IndexMap<String, {inner}>", set()
            return "JsonObject", set()
        if t is None and "properties" in schema:
            return self.struct(ctx, schema), set()
        if t is None and not ({"$ref", "anyOf", "oneOf", "allOf", "enum", "items"} & set(schema)):
            return "serde_json::Value", set()
        raise Exception(f"unhandled schema at {ctx}: {json.dumps(schema)[:200]}")

    def string_enum(self, ctx, vals, name=None):
        name = name or self.unique(ctx)
        lines = []
        lines.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]")
        lines.append(f"pub enum {name} {{")
        used = set()
        variants = []
        for v in vals:
            vn = pascal(v)
            if v == "-Infinity":
                vn = "NegInfinity"
            base = vn
            i = 2
            while vn in used:
                vn = f"{base}{i}"
                i += 1
            used.add(vn)
            variants.append((vn, v))
            lines.append(f"    #[serde(rename = {rust_str(v)})]")
            lines.append(f"    {vn},")
        lines.append("}")
        lines.append("")
        lines.append(f"impl {name} {{")
        lines.append("    /// Wire value of this variant.")
        lines.append("    pub fn as_str(&self) -> &'static str {")
        lines.append("        match self {")
        for vn, v in variants:
            lines.append(f"            Self::{vn} => {rust_str(v)},")
        lines.append("        }")
        lines.append("    }")
        lines.append("}")
        self.items[name] = "\n".join(lines)
        self.deps[name] = set()
        return name

    def discriminant_values(self, schema):
        """Map of property -> literal value for single-enum required properties of an object."""
        if "$ref" in schema:
            schema = SCHEMAS[ref_name(schema["$ref"])]
        out = {}
        if "anyOf" in schema:
            return out
        for k, v in (schema.get("properties") or {}).items():
            if isinstance(v, dict) and "enum" in v and len(v["enum"]) == 1 and k in schema.get("required", []):
                out[k] = v["enum"][0]
        return out

    def union(self, ctx, members, name=None):
        name = name or self.unique(ctx)
        self.items[name] = None  # reserve
        variants = []
        used = set()
        prefix_parts = ctx
        for i, m in enumerate(members):
            if "$ref" in m:
                rn = ref_name(m["$ref"])
                vn = pascal(rn)
                # strip the common prefix with the union name
                cp = pascal(rn)
                for cand in [name, re.sub(r"(Info|Encoded)$", "", name)]:
                    if cp.startswith(cand) and len(cp) > len(cand):
                        vn = cp[len(cand):]
                        break
                else:
                    # strip shared dotted prefix
                    un_parts = ctx.split(".")
                    rn_parts = rn.split(".")
                    j = 0
                    while j < min(len(un_parts), len(rn_parts)) - 1 and un_parts[j] == rn_parts[j]:
                        j += 1
                    vn = pascal(".".join(rn_parts[j:]))
            else:
                disc = self.discriminant_values(m)
                t = m.get("type")
                if disc:
                    vn = pascal(str(list(disc.values())[0]))
                elif "enum" in m:
                    vn = "Literal" if len(m["enum"]) == 1 else "Enum"
                elif t == "string":
                    vn = "String"
                elif t in ("number", "integer"):
                    vn = "Number"
                elif t == "boolean":
                    vn = "Bool"
                elif t == "array":
                    vn = "Array"
                elif t == "object":
                    vn = "Object"
                elif is_nonfinite_number(m):
                    vn = "Number"
                else:
                    vn = f"V{i}"
            base = vn
            k = 2
            while vn in used:
                vn = f"{base}{k}"
                k += 1
            used.add(vn)
            ty, fl = self.rtype(m, name + vn)
            if "f64" in fl:
                ty = "JsNumber"
            variants.append((vn, ty))
        lines = []
        lines.append("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]")
        lines.append("#[serde(untagged)]")
        lines.append(f"pub enum {name} {{")
        for vn, ty in variants:
            lines.append(f"    {vn}({ty}),")
        lines.append("}")
        for vn, ty in variants:
            if ty.startswith("Vec<") or ty.startswith("Option<") or ty in ("String", "bool", "i64", "JsNumber", "serde_json::Value", "JsonObject") or ty.startswith("indexmap"):
                continue
            if sum(1 for _, t in variants if t == ty) > 1:
                continue
            lines.append("")
            lines.append(f"impl From<{ty}> for {name} {{")
            lines.append(f"    fn from(value: {ty}) -> Self {{")
            lines.append(f"        Self::{vn}(value)")
            lines.append("    }")
            lines.append("}")
        self.items[name] = "\n".join(lines)
        self.deps[name] = {ty for _, ty in variants}
        return name

    def struct(self, ctx, schema, name=None, extra_doc=None):
        name = name or self.unique(ctx)
        self.items[name] = None  # reserve slot to keep order parent-first
        props = schema.get("properties") or {}
        required = set(schema.get("required", []))
        ap = schema.get("additionalProperties")
        flatten_extra = False
        if isinstance(ap, dict):
            flatten_extra = True
        for sub in schema.get("allOf", []):
            if isinstance(sub.get("additionalProperties"), dict):
                flatten_extra = True
        lines = []
        if extra_doc:
            lines.append(f"/// {extra_doc}")
        if schema.get("description"):
            for dl in schema["description"].splitlines():
                lines.append(f"/// {dl}".rstrip())
        fields = []
        used = set()
        for pname, pschema in props.items():
            fid = field_ident(pname)
            base = fid
            k = 2
            while fid in used:
                fid = f"{base}_{k}"
                k += 1
            used.add(fid)
            ty, fl = self.rtype(pschema, name + pascal(pname))
            attrs = []
            raw = fid[2:] if fid.startswith("r#") else fid
            if raw != pname:
                attrs.append(f"rename = {rust_str(pname)}")
            is_req = pname in required
            if not is_req:
                if not ty.startswith("Option<"):
                    ty = f"Option<{ty}>"
                    if "f64" in fl:
                        fl = fl | {"opt"}
                attrs.append("default")
                attrs.append('skip_serializing_if = "Option::is_none"')
            elif ty.startswith("Option<"):
                attrs.append("default")
            if "f64" in fl:
                if not ty.startswith("Option<"):
                    attrs.append('serialize_with = "crate::support::ser_f64"')
                elif ty.count("<") == 1:
                    attrs.append('serialize_with = "crate::support::ser_opt_f64"')
                else:
                    raise Exception(f"f64 shape {ty} at {name}.{pname}")
            if "vecf64" in fl:
                if ty == "Vec<f64>":
                    attrs.append('serialize_with = "crate::support::ser_vec_f64"')
                elif ty == "Option<Vec<f64>>":
                    attrs.append('serialize_with = "crate::support::ser_opt_vec_f64"')
            doc = pschema.get("description") if isinstance(pschema, dict) else None
            fields.append((fid, ty, attrs, doc))
        if flatten_extra:
            fields.append(("extra", "indexmap::IndexMap<String, serde_json::Value>", ["flatten"], "Additional properties."))
        self.struct_fields[name] = [f[1] for f in fields]
        self.deps[name] = {f[1] for f in fields}
        lines.append("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]")
        lines.append(f"pub struct {name} {{")
        for fid, ty, attrs, doc in fields:
            if doc:
                for dl in doc.splitlines():
                    lines.append(f"    /// {dl}".rstrip())
            if attrs:
                lines.append(f"    #[serde({', '.join(attrs)})]")
            lines.append(f"    pub {fid}: {ty},")
        lines.append("}")
        self.items[name] = "\n".join(lines)
        return name

    def define_component(self, cname):
        s = SCHEMAS[cname]
        name = self.component_types[cname]
        doc = f"Component schema `{cname}`."
        if "$ref" in s:
            target = self.component_types[ref_name(s["$ref"])]
            self.items[name] = f"/// {doc}\npub type {name} = {target};"
            self.deps[name] = {target}
            return
        if "anyOf" in s and not is_nonfinite_number(s):
            non_null = [x for x in s["anyOf"] if not is_null(x)]
            if len(non_null) == len(s["anyOf"]) and len(non_null) > 1:
                self.names.add(name)
                self.union(cname, s["anyOf"], name=name)
                self.items[name] = f"/// {doc}\n" + self.items[name]
                return
        if "enum" in s and s.get("type") == "string" and len(s["enum"]) > 1:
            self.names.add(name)
            self.string_enum(cname, s["enum"], name=name)
            self.items[name] = f"/// {doc}\n" + self.items[name]
            return
        if s.get("type") == "object" and s.get("properties"):
            self.names.add(name)
            self.struct(cname, s, name=name, extra_doc=doc)
            return
        # alias
        self.names.add(name)
        self.items[name] = None
        ty, fl = self.rtype(s, name + "Inner")
        if "f64" in fl:
            self.f64_aliases.add(name)
        desc = s.get("description")
        extra = ""
        if s.get("pattern"):
            extra = f" Pattern: `{s['pattern']}`."
        line = f"/// {doc}{extra}"
        if desc:
            line += "\n///\n" + "\n".join(f"/// {d}" for d in desc.splitlines())
        self.items[name] = f"{line}\npub type {name} = {ty};"
        self.deps[name] = {ty}

    # ---- Default derivation ----------------------------------------------------------------

    def compute_defaults(self):
        prim_default = {"String", "bool", "i64", "f64", "serde_json::Value", "JsonObject", "JsNumber"}
        changed = True
        default_types = set(self.lits.values()) | prim_default
        while changed:
            changed = False
            for name, ftypes in self.struct_fields.items():
                if name in self.defaults:
                    continue
                if all(self.type_default(t, default_types) for t in ftypes):
                    self.defaults.add(name)
                    default_types.add(name)
                    changed = True
        self.default_types = default_types

    def type_default(self, t, default_types):
        if t.startswith("Option<") or t.startswith("Vec<") or t.startswith("indexmap::IndexMap<"):
            return True
        return t in default_types or self.alias_target_default(t, default_types)

    def alias_target_default(self, t, default_types):
        code = self.items.get(t)
        if code and f"pub type {t} = " in code:
            target = code.split(f"pub type {t} = ")[1].rstrip(";").strip()
            return self.type_default(target, default_types)
        return False


g = Gen()

# Pre-assign component names so references resolve before definition.
for cname in SCHEMAS:
    g.component_types[cname] = pascal(cname)
for cname in SCHEMAS:
    g.define_component(cname)

# --------------------------------------------------------------------------------------------
# Operations
# --------------------------------------------------------------------------------------------

ops = []
for path, item in spec["paths"].items():
    for method, op in item.items():
        if method not in ("get", "post", "put", "patch", "delete"):
            continue
        ops.append((path, method, op))


def classify_response(op):
    resp = op["responses"]
    if "204" in resp:
        return ("none", None)
    r = resp["200"]
    content = r.get("content", {})
    if "application/json" not in content:
        return ("raw", None)
    s = content["application/json"]["schema"]
    if "$ref" not in s and s.get("type") == "object":
        keys = list((s.get("properties") or {}).keys())
        if keys == ["location", "data"] and s["properties"]["location"].get("$ref", "").endswith("Location.PublicRef"):
            return ("location", s["properties"]["data"])
        if keys == ["data"]:
            return ("data", s["properties"]["data"])
    return ("bare", s)


op_infos = []
for path, method, op in ops:
    oid = op["operationId"]
    params = op.get("parameters", [])
    has_location = any(p["name"] == "location" and p["in"] == "query" for p in params)
    if oid in SCOPE_S:
        scope = "S"
    elif oid in SCOPE_F:
        scope = "F"
    elif oid in SCOPE_H:
        scope = "H"
    elif has_location:
        scope = "L"
    else:
        scope = "G"
    kind, data_schema = classify_response(op)
    ws = op["responses"].get("200", {}).get("content", {}).get("application/json", {}).get("schema", {}).get("x-websocket") or op.get("x-websocket")
    info = dict(id=oid, path=path, method=method, op=op, scope=scope, kind=kind,
                data_schema=data_schema, params=params, ws=bool(ws))
    op_infos.append(info)

# Generate request/response types per operation.
for info in op_infos:
    oid = info["id"]
    P = op_pascal(oid)
    info["pascal"] = P
    info["snake"] = op_snake(oid)
    if oid in SERVER_OWNED:
        continue
    # response type
    kind = info["kind"]
    if kind == "none":
        info["resp_ty"] = "()"
    elif kind == "raw":
        info["resp_ty"] = None
    else:
        ds = info["data_schema"]
        ty, fl = g.rtype(ds, P + ("Data" if kind != "bare" else "Response"))
        if "f64" in fl:
            ty = "JsNumber"
        info["resp_ty"] = ty
    # request struct
    fields = []
    decode = []
    for prm in info["params"]:
        name = prm["name"]
        loc = prm["in"]
        if name == "location" and loc == "query":
            continue
        if loc == "header":
            continue
        if oid in ("pty.connect", "experimental.persistentPty.connect"):
            continue
        fid = field_ident(name)
        schema = prm["schema"]
        required = prm.get("required", False)
        doc = schema.get("description") or prm.get("description")
        arrays = [x for x in schema.get("anyOf", []) if isinstance(x, dict) and x.get("type") == "array"]
        if loc == "query" and arrays:
            # `Schema.ArrayEnsure(X)`: repeated `?k=a&k=b`, or one value promoted to an array.
            item = arrays[0]["items"]
            ty, _ = g.rtype(item, P + pascal(name) + "Item")
            expected = ref_name(item["$ref"]) if "$ref" in item else "string"
            call_expr = f"call.query_array::<{ty}>({rust_str(name)}, {rust_str(expected)})?"
            if required:
                decode.append(f"{fid}: crate::router::required({call_expr}, {rust_str(name)})?")
                fields.append((fid, f"Vec<{ty}>", doc, name, "query"))
            else:
                decode.append(f"{fid}: {call_expr}")
                fields.append((fid, f"Option<Vec<{ty}>>", doc, name, "query"))
            continue
        inner = schema
        nullable = False
        if "anyOf" in schema:
            nn = [x for x in schema["anyOf"] if not is_null(x)]
            if len(nn) < len(schema["anyOf"]):
                nullable = True
                inner = nn[0]
        # resolve pattern / enums
        resolved = inner
        if "$ref" in inner:
            resolved = SCHEMAS[ref_name(inner["$ref"])]
        pattern = resolved.get("pattern") if isinstance(resolved, dict) else None
        num = QUERY_NUMERIC.get((oid, name))
        if loc == "path":
            ty, _ = g.rtype(inner, P + pascal(name))
            pat = rust_str(pattern) if pattern else "None"
            pat = f"Some({pat})" if pattern else "None"
            decode.append(f"{fid}: call.path_str({rust_str(name)}, {pat})?")
            fields.append((fid, ty, doc, name, "path"))
            continue
        # query
        if num is not None:
            kindn, mn, mx = num
            if kindn == "int":
                ty = "i64"
                call_expr = f"call.query_int({rust_str(name)}, {('Some(%d)' % mn) if mn is not None else 'None'}, {('Some(%d)' % mx) if mx is not None else 'None'})?"
            elif kindn == "pos":
                ty = "i64"
                call_expr = f"call.query_pos_int({rust_str(name)}, {('Some(%d)' % mx) if mx is not None else 'None'})?"
            else:
                ty = "f64"
                call_expr = f"call.query_num({rust_str(name)})?"
        elif isinstance(resolved, dict) and resolved.get("enum") == ["true", "false"]:
            ty = "bool"
            call_expr = f"call.query_bool({rust_str(name)})?"
        elif isinstance(resolved, dict) and "enum" in resolved and "anyOf" not in resolved:
            ty, _ = g.rtype(inner, P + pascal(name))
            # Effect's message names the schema identifier when there is one, else the literals.
            if "$ref" in inner:
                expected = ref_name(inner["$ref"])
            else:
                expected = " | ".join(json.dumps(x) for x in resolved["enum"])
            call_expr = f"call.query_enum::<{ty}>({rust_str(name)}, {rust_str(expected)})?"
        else:
            ty, _ = g.rtype(inner, P + pascal(name))
            if ty not in ("String",) and not g.items.get(ty, "").startswith(f"/// Component schema") and ty != "String":
                pass
            pat = f"Some({rust_str(pattern)})" if pattern else "None"
            call_expr = f"call.query_str({rust_str(name)}, {pat})?"
        if required:
            decode.append(f"{fid}: crate::router::required({call_expr}, {rust_str(name)})?")
            fields.append((fid, ty, doc, name, "query"))
        else:
            decode.append(f"{fid}: {call_expr}")
            fields.append((fid, f"Option<{ty}>", doc, name, "query"))
    # body
    rb = info["op"].get("requestBody")
    body_ty = None
    body_kind = None
    if rb:
        content = rb["content"]
        if "application/json" in content:
            bs = content["application/json"]["schema"]
            body_ty, _ = g.rtype(bs, P + "Body")
            body_kind = "json"
        elif "application/octet-stream" in content:
            body_ty = "bytes::Bytes"
            body_kind = "bytes"
        else:
            raise Exception(f"unknown body content {oid}")
        fields.append(("body", body_ty, "Request body.", None, "body"))
        if body_kind == "json":
            decode.append(f"body: call.json_body_for({rust_str(oid)})?")
        else:
            decode.append("body: call.bytes_body()?")
    info["req_fields"] = fields
    info["decode"] = decode
    info["body_kind"] = body_kind

g.compute_defaults()

# Request structs
req_items = OrderedDict()
for info in op_infos:
    oid = info["id"]
    if oid in SERVER_OWNED or oid in ("pty.connect", "experimental.persistentPty.connect", "fs.read", "event.subscribe",
                                      "session.list", "session.message.list"):
        continue
    P = info["pascal"]
    name = P + "Request"
    g.names.add(name)
    fields = info["req_fields"]
    lines = [f"/// Request for `{oid}` (`{info['method'].upper()} {info['path']}`)."]
    derive_default = all(g.type_default(f[1], g.default_types) for f in fields)
    lines.append("#[derive(Debug, Clone, PartialEq" + (", Default" if derive_default else "") + ")]")
    lines.append(f"pub struct {name} {{")
    for fid, ty, doc, wire, loc in fields:
        if wire:
            lines.append(f"    /// {loc} parameter `{wire}`." + (f" {doc}" if doc else ""))
        else:
            lines.append(f"    /// {doc}")
        lines.append(f"    pub {fid}: {ty},")
    lines.append("}")
    req_items[name] = "\n".join(lines)
    info["req_ty"] = name

# --------------------------------------------------------------------------------------------
# Emit
# --------------------------------------------------------------------------------------------

out = []
w = out.append
w("// @generated by crates/oc-server/codegen/gen.py from assets/openapi.json. DO NOT EDIT.")
w("// Regenerate with: python3 crates/oc-server/codegen/gen.py")
w("#![allow(clippy::all, clippy::pedantic, non_camel_case_types, dead_code, unused_imports)]")
w("")
w("use serde::{Deserialize, Serialize};")
w("")
w("use crate::context::RequestContext;")
w("use crate::error::ApiError;")
w("pub use crate::support::{JsNumber, JsonObject};")
w("")
w("// ---- literal markers ----------------------------------------------------------------------")
w("")
for (k, v), name in g.lits.items():
    if k == "bool":
        w(f"crate::lit_bool!({name}, {'true' if v else 'false'});")
    elif k == "int":
        w(f"crate::lit_int!({name}, {v});")
    else:
        w(f"crate::lit_str!({name}, {rust_str(v)});")
w("")
w("// ---- component schemas --------------------------------------------------------------------")
w("")
for name, code in g.items.items():
    if code is None:
        raise Exception(f"undefined item {name}")
    if name in g.defaults:
        code = code.replace("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]\npub struct " + name,
                            "#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]\npub struct " + name)
    w(code)
    w("")

w("// ---- error encodings ----------------------------------------------------------------------")
w("")
w("/// A typed error body (`*ErrorEncoded` component) with its `_tag` and HTTP status.")
w("pub trait ErrorEncoded: Serialize {")
w("    /// The `_tag` discriminator.")
w("    const TAG: &'static str;")
w("    /// HTTP status code the error is served with.")
w("    const STATUS: u16;")
w("}")
w("")
for cname in SCHEMAS:
    if cname.endswith("ErrorEncoded"):
        tag = SCHEMAS[cname]["properties"]["_tag"]["enum"][0]
        st = ERROR_STATUS[tag]
        w(f"impl ErrorEncoded for {g.component_types[cname]} {{")
        w(f"    const TAG: &'static str = {rust_str(tag)};")
        w(f"    const STATUS: u16 = {st};")
        w("}")
        w("")
w("/// `(tag, status)` for every declared error.")
w("pub const ERROR_STATUS: &[(&str, u16)] = &[")
for tag, st in ERROR_STATUS.items():
    w(f"    ({rust_str(tag)}, {st}),")
w("];")
w("")

w("// ---- operation requests -------------------------------------------------------------------")
w("")
for name, code in req_items.items():
    w(code)
    w("")

w("// ---- operation table ----------------------------------------------------------------------")
w("")
w("/// How an operation picks its location (spec 06 §1.1).")
w("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
w("pub enum Scope {")
w("    /// Global services only.")
w("    G,")
w("    /// Request location (`location[directory]` / `x-opencode-directory` / cwd); must exist.")
w("    L,")
w("    /// Session location (path `sessionID`, validated `^ses`).")
w("    S,")
w("    /// Like S, but `sessionID == \"global\"` selects the request location.")
w("    F,")
w("    /// Handler resolves the location itself.")
w("    H,")
w("}")
w("")
w("/// Success envelope of an operation (spec 06 §2.2).")
w("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
w("pub enum Envelope {")
w("    /// `{location:{directory}, data}`")
w("    Location,")
w("    /// `{data}`")
w("    Data,")
w("    /// The body as-is.")
w("    Bare,")
w("    /// `204` with an empty body.")
w("    NoContent,")
w("    /// Raw / streaming / WebSocket response.")
w("    Raw,")
w("}")
w("")
w("/// Operation identifiers (one per OpenAPI operation).")
w("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]")
w("pub enum Op {")
for info in op_infos:
    w(f"    /// `{info['id']}`")
    w(f"    {info['pascal']},")
w("}")
w("")
w("/// Static metadata for one operation.")
w("#[derive(Debug, Clone, Copy)]")
w("pub struct OpMeta {")
w("    pub op: Op,")
w("    /// OpenAPI operationId.")
w("    pub id: &'static str,")
w("    /// Upper-case HTTP method.")
w("    pub method: &'static str,")
w("    /// OpenAPI path template.")
w("    pub path: &'static str,")
w("    /// axum 0.8 route path.")
w("    pub route: &'static str,")
w("    pub scope: Scope,")
w("    pub envelope: Envelope,")
w("    /// Handled by hand-written code (not the generic dispatcher).")
w("    pub special: bool,")
w("}")
w("")
w("/// All operations in OpenAPI order.")
w("pub const OPS: &[OpMeta] = &[")
env_map = {"location": "Location", "data": "Data", "bare": "Bare", "none": "NoContent", "raw": "Raw"}
for info in op_infos:
    route = info["path"].replace("/*", "/{*path}")
    special = info["id"] in SERVER_OWNED or (info["id"] in SPECIAL and SPECIAL[info["id"]] is not None)
    w(f"    OpMeta {{ op: Op::{info['pascal']}, id: {rust_str(info['id'])}, method: {rust_str(info['method'].upper())}, path: {rust_str(info['path'])}, route: {rust_str(route)}, scope: Scope::{info['scope']}, envelope: Envelope::{env_map[info['kind']] if not info['ws'] else 'Raw'}, special: {'true' if special else 'false'} }},")
w("];")
w("")

# Backend trait
w("// ---- backend trait --------------------------------------------------------------------------")
w("")
w("/// The engine behind the HTTP API. One method per OpenAPI operation (named after the")
w("/// operationId in snake_case); the server owns auth, pairing, PTY tickets, envelopes,")
w("/// location resolution and request decoding, and calls these with typed requests.")
w("///")
w("/// Every method has a default body answering \"not implemented\" (HTTP 500, empty body),")
w("/// so implementations can grow incrementally.")
w("///")
w("/// Return values are the *payload*: the server wraps them in the operation's envelope")
w("/// (`{location,data}`, `{data}` or bare) as listed on each method.")
w("#[async_trait::async_trait]")
w("pub trait Backend: Send + Sync + 'static {")
for info in op_infos:
    oid = info["id"]
    if oid in SERVER_OWNED:
        continue
    P = info["pascal"]
    sp = SPECIAL.get(oid)
    if sp:
        req_ty, resp_ty = sp
        if "::" not in req_ty:
            req_ty = info["req_ty"]
    else:
        req_ty = info["req_ty"]
        resp_ty = info["resp_ty"]
    summary = info["op"].get("summary") or ""
    desc = info["op"].get("description") or ""
    w(f"    /// `{info['method'].upper()} {info['path']}` — `{oid}`" + (f": {summary}." if summary else "."))
    if desc:
        w("    ///")
        for dl in desc.splitlines():
            w(f"    /// {dl}".rstrip())
    w("    ///")
    env = env_map[info["kind"]] if not info["ws"] else "Raw"
    w(f"    /// Scope `{info['scope']}`, envelope `{env}`.")
    if oid == "server.info":
        w(f"    async fn {info['snake']}(&self, ctx: &RequestContext, req: {req_ty}) -> Result<{resp_ty}, ApiError> {{")
        w("        let _ = req;")
        w("        Ok(ctx.server.info(Some(false)))")
        w("    }")
        continue
    w(f"    async fn {info['snake']}(&self, ctx: &RequestContext, req: {req_ty}) -> Result<{resp_ty}, ApiError> {{")
    w("        let _ = (ctx, req);")
    w(f"        Err(ApiError::not_implemented({rust_str(oid)}))")
    w("    }")
w("}")
w("")

# Dispatcher
w("// ---- generic dispatcher -------------------------------------------------------------------")
w("")
w("/// Decode the request for `op`, call the backend and encode the envelope.")
w("pub(crate) async fn dispatch(")
w("    backend: &dyn Backend,")
w("    op: Op,")
w("    call: &crate::router::Call,")
w(") -> Result<axum::response::Response, ApiError> {")
w("    use crate::router::respond;")
w("    match op {")
for info in op_infos:
    oid = info["id"]
    special = oid in SERVER_OWNED or (oid in SPECIAL and SPECIAL[oid] is not None)
    if special:
        continue
    P = info["pascal"]
    w(f"        Op::{P} => {{")
    if info["decode"]:
        w(f"            let req = {info['req_ty']} {{")
        for d in info["decode"]:
            w(f"                {d},")
        w("            };")
    else:
        w(f"            let req = {info['req_ty']} {{}};")
    if oid == "rpc.call":
        w(f"            let out = backend.{info['snake']}(&call.ctx, req).await.map_err(crate::error::rpc_defect)?;")
    else:
        w(f"            let out = backend.{info['snake']}(&call.ctx, req).await?;")
    k = info["kind"]
    if k == "location":
        w("            Ok(respond::location(&call.ctx, &out))")
    elif k == "data":
        w("            Ok(respond::data(&out))")
    elif k == "bare":
        w("            Ok(respond::bare(&out))")
    elif k == "none":
        w("            let () = out;")
        w("            Ok(respond::no_content())")
    else:
        raise Exception(oid)
    w("        }")
w("        _ => Err(ApiError::defect(format!(\"operation {op:?} is not handled by the generic dispatcher\"))),")
w("    }")
w("}")
w("")

text = "\n".join(out)
with open(OUT_PATH, "w") as f:
    f.write(text)
print(f"wrote {OUT_PATH}: {len(g.items)} types, {len(g.lits)} literals, {len(op_infos)} operations")
