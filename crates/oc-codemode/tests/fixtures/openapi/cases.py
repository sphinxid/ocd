import json, sys
B = "http://localhost:4096"
def single(op, method="get", extra=None, openapi="3.1.0", path="/test"):
    operation = {"operationId": "test", "responses": {"200": {"description": "Success"}}}
    operation.update(op)
    spec = {"openapi": openapi, "paths": {path: {method: operation}}}
    if extra: spec.update(extra)
    return spec
def schemes(**s): return {"components": {"securitySchemes": s}}
bearer = {"type": "http", "scheme": "bearer"}
ok = {"status": 200, "headers": {"content-type": "application/json"}, "body": "{\"ok\":true}"}
cases = []
def case(name, spec, tool="test", input=None, auth=None, headers=None, response=ok, baseUrl=B):
    c = {"name": name, "spec": spec, "tool": tool, "input": input if input is not None else {}, "response": response}
    if baseUrl is not None: c["baseUrl"] = baseUrl
    if auth is not None: c["auth"] = auth
    if headers is not None: c["headers"] = headers
    cases.append(c)

case("basic auth", single({}, extra={"security": [{"basic": []}], **schemes(basic={"type": "http", "scheme": "Basic"})}), auth="basic")
case("header credential with scopes", single({"security": [{"oauth": ["read", "write"]}]}, extra=schemes(oauth={"type": "oauth2", "flows": {}})), auth="header")
case("api key header carrier", single({}, extra={"security": [{"key": []}], **schemes(key={"type": "apiKey", "in": "header", "name": "X-API-Key"})}), auth="apikey")
case("api key credential for http scheme", single({}, extra={"security": [{"bearer": []}], **schemes(bearer=bearer)}), auth="apikey")
case("no resolver configured", single({}, extra={"security": [{"bearer": []}], **schemes(bearer=bearer)}))
case("all alternatives unavailable", single({}, extra={"security": [{"a": []}, {"b": [], "a": []}], **schemes(a=bearer, b=bearer)}), auth={})
case("and requirement falls through to next alternative", single({}, extra={"security": [{"a": [], "b": []}, {"c": []}], **schemes(a=bearer, b=bearer, c={"type": "apiKey", "in": "query", "name": "token"})}), auth={"a": "bearer", "c": "apikey"})
case("resolver failure aborts", single({}, extra={"security": [{"a": []}], **schemes(a=bearer)}), auth="fail")
case("anonymous alternative", single({}, extra={"security": [{"a": []}, {}], **schemes(a=bearer)}), auth={})
case("static header precedence", single({"parameters": [{"name": "X-Trace", "in": "header", "schema": {"type": "string"}}], "security": [{"a": []}]}, extra=schemes(a=bearer)), input={"X-Trace": "param"}, auth="bearer", headers={"X-Static": "s", "X-Trace": "static", "Authorization": "static-auth"})
case("static header order", single({}), headers={"b": "x", "2": "two", "1": "one", "B": "y"})
case("http error json", single({}), response={"status": 404, "headers": {"content-type": "application/json"}, "body": "{\"error\":\"nope\",\"n\":1.50}"})
case("http error long text", single({}), response={"status": 500, "headers": {"content-type": "text/plain"}, "body": "x" * 1500})
case("http error empty", single({}), response={"status": 503})
case("http error malformed json", single({}), response={"status": 400, "headers": {"content-type": "application/json"}, "body": "{"})
case("http error problem json", single({}), response={"status": 422, "headers": {"content-type": "application/problem+json; charset=utf-8"}, "body": "{\"title\":\"bad\"}"})
case("openapi 3.0 component collision", single({}, openapi="3.0.3", extra={"components": {"schemas": {"X": {"type": "integer", "const": 1}}}}))
case("openapi 3.0 parameter collision", single({"parameters": [{"name": "q", "in": "query", "schema": {"type": "string", "examples": ["a"]}}]}, openapi="3.0.3"))
case("openapi 3.0 conversions", single({"parameters": [{"name": "q", "in": "query", "required": True, "schema": {"type": "integer", "nullable": True, "minimum": 1, "exclusiveMinimum": True, "maximum": 9, "exclusiveMaximum": False, "example": 3}}, {"name": "r", "in": "query", "schema": {"$ref": "#/components/schemas/R"}}]}, openapi="3.0.3", extra={"components": {"schemas": {"R": {"type": "object", "properties": {"s": {"type": "string", "nullable": True}, "t": {"$ref": "#/components/schemas/R"}}}}}}), input={"q": 2})
case("invalid path parameter", single({"parameters": [{"name": "id", "in": "path", "required": True, "schema": {"type": "string"}}]}, path="/items/{id}"), input={"id": "."})
case("path parameter encoding", single({"parameters": [{"name": "id", "in": "path", "required": True, "schema": {"type": "string"}}]}, path="/items/{id}/x"), input={"id": "a/b c~é(1)"})
case("unresolved path parameter", single({}, path="/items/{id}"))
case("deep object must be an object", single({"parameters": [{"name": "f", "in": "query", "style": "deepObject", "explode": True, "schema": {"type": "object"}}]}), input={"f": 1})
case("missing query parameter", single({"parameters": [{"name": "q", "in": "query", "required": True, "schema": {"type": "string"}}]}))
case("missing body field", single({"requestBody": {"required": True, "content": {"application/json": {"schema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": False}}}}}, method="post"))
case("header simple shapes", single({"parameters": [{"name": "a", "in": "header", "schema": {"type": "array"}}, {"name": "o", "in": "header", "schema": {"type": "object"}}, {"name": "n", "in": "header", "schema": {"type": "number"}}]}), input={"a": ["x", 1, True, None], "o": {"k": "v", "m": 2}, "n": 1e21})
case("query numbers", single({"parameters": [{"name": "a", "in": "query", "schema": {}}, {"name": "b", "in": "query", "schema": {}}, {"name": "c", "in": "query", "explode": False, "schema": {}}]}), input={"a": 0.1, "b": [1e-7, 1e21, -0.0, 0.000001, 123.456], "c": {"x": 1, "y": "z w"}})
case("query array nested without explode", single({"parameters": [{"name": "a", "in": "query", "explode": False, "schema": {}}]}), input={"a": [[1]]})
case("json with bom and charset", single({}), response={"status": 200, "headers": {"content-type": "Application/JSON ; charset=utf-8"}, "body": "﻿{\"b\":1,\"2\":2,\"1\":[3]}"})
case("text without content type", single({}), response={"status": 200, "body": "hello"})
case("empty json success", single({}), response={"status": 200, "headers": {"content-type": "application/json"}})
case("optional value body omitted", single({"requestBody": {"content": {"application/json; charset=utf-8": {"schema": {"type": "string"}}}}}, method="post"))
case("optional value body sent", single({"requestBody": {"content": {"application/json; charset=utf-8": {"schema": {"type": "string"}}}}}, method="post"), input={"body": "x y\"z"})
case("optional object body omitted", single({"requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": False}}}}}, method="put"))
case("required object body empty", single({"requestBody": {"required": True, "content": {"application/json": {"schema": {"type": "object", "properties": {"2": {"type": "string"}, "a": {"type": "string"}, "1": {"type": "number"}}, "additionalProperties": False}}}}}, method="patch"), input={"a": "x", "1": 1, "2": "two"})
case("accept header parameter ignored", single({"parameters": [{"name": "Accept", "in": "header", "schema": {"type": "string"}}, {"name": "X-Id", "in": "header", "required": True, "schema": {"type": "string"}, "description": "Trace id"}]}), input={"X-Id": "1", "Accept": "nope"})
case("unsupported encodings", {"openapi": "3.1.0", "paths": {
    "/a/{id}": {"get": {"operationId": "label", "parameters": [{"name": "id", "in": "path", "style": "label", "schema": {}}], "responses": {}}},
    "/b": {"get": {"operationId": "deep", "parameters": [{"name": "f", "in": "query", "style": "deepObject", "explode": False, "schema": {}}], "responses": {}}},
    "/c": {"get": {"operationId": "content", "parameters": [{"name": "f", "in": "query", "content": {"application/json": {}}}], "responses": {}}},
    "/d": {"get": {"operationId": "location", "parameters": [{"name": "f", "in": "body", "schema": {}}], "responses": {}}},
    "/e": {"get": {"operationId": "noschema", "parameters": [{"name": "f", "in": "query"}], "responses": {}}},
    "/f": {"get": {"operationId": "explode", "parameters": [{"name": "f", "in": "query", "explode": "yes", "schema": {}}], "responses": {}}},
    "/g": {"get": {"operationId": "pipe", "parameters": [{"name": "f", "in": "query", "style": "pipeDelimited", "schema": {}}], "responses": {}}},
    "/h": {"get": {"operationId": "unresolved", "parameters": [{"$ref": "#/components/parameters/Missing"}], "responses": {}}},
    "/i": {"get": {"operationId": "noname", "parameters": [{"in": "query", "schema": {}}], "responses": {}}},
    "/j": {"get": {"operationId": "servers", "servers": [{"url": "https://{tenant}.example.com"}], "responses": {}}},
    "/k": {"get": {"operationId": "relative", "servers": [{"url": "/v1"}], "responses": {}}},
    "/l": {"get": {"operationId": "noservers", "servers": [], "responses": {}}},
    "/m": {"get": {"operationId": "security", "security": [{"oauth": []}], "responses": {}}},
    "/n": {"get": {"operationId": "scopes", "security": [{"oauth": [1]}], "responses": {}}},
    "/o": {"get": {"operationId": "reqobj", "security": [1], "responses": {}}},
    "/p": {"get": {"operationId": "resp", "responses": {"200": {"$ref": "#/components/responses/Missing"}}}},
    "/q": {"post": {"operationId": "nobody", "requestBody": {"content": {}}, "responses": {}}},
    "/r": {"get": {"operationId": "reserved", "parameters": [{"name": "f", "in": "query", "allowReserved": "no", "schema": {}}], "responses": {}}},
    "/s": {"get": {"operationId": "style", "parameters": [{"name": "f", "in": "query", "style": "", "schema": {}}], "responses": {}}},
  }, "components": {"securitySchemes": {"oauth": {"type": "mutualTLS"}}}}, tool=None)
case("outputs and descriptions", {"openapi": "3.1.0", "servers": [{"url": "https://api.example.com/v2/"}], "paths": {
    "/a": {"get": {"summary": "Summary only", "responses": {"201": {"content": {"application/json": {"schema": {"type": "number"}}}}, "200": {"content": {"text/plain": {}, "application/json": {"schema": {"type": "string", "writeOnly": True}}}}, "2XX": {"description": "x"}, "default": {"content": {"application/octet-stream": {}}}}}},
    "/b": {"get": {"description": "Desc", "summary": "S", "responses": {"200": {"content": "text"}}}},
    "/c": {"get": {"responses": {"200": {"content": {"application/json": {}}}}}},
    "/d/{x}": {"servers": [{"url": "https://path.example.com"}], "get": {"responses": {}}, "post": {"servers": [{"url": "http://op.example.com"}], "responses": {}}},
  }}, tool=None, baseUrl=None)
case("example and dialect conversion", single({"parameters": [{"name": "q", "in": "query", "required": True, "schema": {"$schema": "https://spec.openapis.org/oas/3.1/dialect/base", "type": "string", "example": "e", "examples": ["f"], "description": "own"}, "description": "param"}]}), input={"q": "v"})
case("server url from spec with trailing slashes", single({}, extra={"servers": [{"url": "https://API.example.com/base//"}]}), baseUrl=None)
case("existing query in path", single({"parameters": [{"name": "b", "in": "query", "schema": {}}]}, path="/x?a=1%202&c"), input={"b": "q r"})
case("auth query replaces model query", single({"parameters": [{"name": "token", "in": "query", "schema": {}}, {"name": "z", "in": "query", "schema": {}}], "security": [{"k": []}]}, extra=schemes(k={"type": "apiKey", "in": "query", "name": "token"})), input={"token": "model", "z": "1"}, auth="apikey")
case("duplicate header credential", single({"security": [{"a": [], "b": []}]}, extra=schemes(a={"type": "oauth2"}, b={"type": "apiKey", "in": "header", "name": "x-custom"})), auth={"a": "header", "b": "apikey"})
case("server declarations", {"openapi": "3.1.0", "paths": {
    "/j": {"get": {"operationId": "servers", "servers": [{"url": "https://{tenant}.example.com"}], "responses": {}}},
    "/k": {"get": {"operationId": "relative", "servers": [{"url": "/v1"}], "responses": {}}},
    "/l": {"get": {"operationId": "noservers", "servers": [], "responses": {}}},
    "/m": {"servers": None, "get": {"operationId": "nullservers", "responses": {}}},
    "/n": {"get": {"operationId": "ftp", "servers": [1, {"url": "ftp://x.example.com"}], "responses": {}}},
    "/o": {"get": {"operationId": "fragment", "servers": [{"url": "https://x.example.com/#frag"}], "responses": {}}},
    "/p": {"get": {"operationId": "emptyquery", "servers": [{"url": "HTTPS://X.example.com/a/?"}], "responses": {}}},
    "/q": {"get": {"operationId": "badport", "servers": [{"url": "http://x.example.com:99999"}], "responses": {}}},
  }}, tool=None, baseUrl=None)
case("collisions and namespaces", {"openapi": "3.1.0", "paths": {
    "/1": {"get": {"operationId": "a.b", "responses": {}}},
    "/2": {"get": {"operationId": "a", "responses": {}}},
    "/3": {"get": {"operationId": "a.b.c", "responses": {}}},
    "/4": {"get": {"operationId": "a.b", "responses": {}}},
    "/5": {"get": {"operationId": "9 lives.__proto__", "responses": {}}},
    "/6": {"get": {"operationId": "constructor", "responses": {}}},
    "/7": {"get": {"operationId": "a_b_2", "responses": {}}},
    "/8": {"get": {"operationId": "a.b", "responses": {}}},
    "/9/{}/{x-y}": {"get": {"responses": {}}, "trace": {"responses": {}}, "x-other": {"responses": {}}, "parameters": []},
  }}, tool=None)
def param_op(op_id, schema, extra_params=None):
    return {"get": {"operationId": op_id, "parameters": [{"name": "p", "in": "query", "schema": schema}] + (extra_params or []), "responses": {"200": {"content": {"application/json": {"schema": schema}}}}}}
case("openapi 3.0 references", {"openapi": "3.0.1", "paths": {
    "/a": param_op("self", {"type": "object", "properties": {"a": {"type": "string"}, "b": {"$ref": "#/properties/a"}, "c": {"$ref": "#/properties/%61"}, "d": {"$ref": "#"}, "e": {"$ref": ""}, "f": {"$ref": "other.json#/x"}}}),
    "/b": param_op("names", {"allOf": [{"$ref": "#/components/schemas/A%20B", "description": "dropped sibling"}, {"$ref": "#/components/schemas/1"}], "items": [{"type": "string"}], "not": True, "additionalProperties": {"$ref": "#/components/schemas/2"}}),
    "/c": param_op("bounds", {"type": "number", "minimum": 1, "exclusiveMinimum": False, "maximum": 5, "exclusiveMaximum": 3, "nullable": False, "$ref": False}),
  }, "components": {"schemas": {"A B": {"type": "string", "nullable": True, "example": "x"}, "2": {"type": "integer"}, "1": {"type": "boolean"}, "b": {"oneOf": [{"type": "string"}, True]}}}}, tool=None)
case("openapi 3.1 references", {"openapi": "3.1.0", "paths": {
    "/a": param_op("rootid", {"$id": "https://example.com/s", "properties": {"x": {"$ref": "#/components/schemas/X"}}}),
    "/b": param_op("embedded", {"properties": {"x": {"$id": "inner", "$ref": "#/components/schemas/X", "properties": {"y": {"$ref": "#/components/schemas/X"}}}, "z": {"$dynamicRef": "#/components/schemas/X", "$ref": "#/components/schemas/X"}, "w": {"$id": "#frag", "$ref": "#/components/schemas/X"}}}),
    "/c": param_op("examples", {"type": "object", "properties": {"a": {"type": "string", "example": 1, "examples": "no"}, "b": {"type": "string", "example": 1, "examples": [2]}, "c": {"$schema": "https://spec.openapis.org/oas/3.1/dialect/base#", "type": "string"}}, "$defs": "notobject"}),
    "/d": param_op("emptydefs", {"type": "string", "$defs": {}}),
  }, "components": {"schemas": {"X": {"type": "string"}, "10": {"type": "number"}, "9": {"type": "null"}}}}, tool=None)
case("directional keywords everywhere", {"openapi": "3.1.0", "paths": {
    "/a": {"post": {"operationId": "write", "requestBody": {"required": True, "content": {"application/vnd.api+json": {"schema": {"$ref": "#/components/schemas/Doc"}}}}, "responses": {"200": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Doc"}}}}}}},
  }, "components": {"schemas": {
    "Doc": {"type": "object", "required": ["id", "secret", "items", 5], "properties": {"id": {"type": "string", "readOnly": True}, "secret": {"type": "string", "writeOnly": True}, "items": {"type": "array", "items": {"$ref": "#/components/schemas/Item"}}, "map": {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/Item"}, "patternProperties": {"^x": {"$ref": "#/components/schemas/Item"}}}, "tuple": {"prefixItems": [{"$ref": "#/components/schemas/Item"}]}, "local": {"$ref": "#/$defs/L"}, "legacy": {"$ref": "#/definitions/D"}}, "$defs": {"L": {"type": "object", "properties": {"r": {"readOnly": True}, "s": {}}, "required": ["r", "s"]}}, "definitions": {"D": {"writeOnly": True}}, "dependentSchemas": {"id": {"properties": {"q": {"readOnly": True}}}}, "then": {"properties": {"id": {}}, "required": ["id"]}, "unevaluatedProperties": {"properties": {"k": {"readOnly": True}, "j": {}}}},
    "Item": {"type": "object", "properties": {"rid": {"type": "string", "readOnly": True}, "wid": {"type": "string", "writeOnly": True}, "n": {"type": "number"}}, "required": ["rid", "wid", "n"]},
  }}}, tool=None)
case("parameter descriptions and conflicts", {"openapi": "3.1.0", "paths": {"/x/{a}": {
    "parameters": [{"name": "a", "in": "path", "required": True, "schema": {"type": "string", "description": "own"}, "description": "param"}, {"name": "b", "in": "query", "schema": {"type": "string"}, "description": "query b"}],
    "post": {"operationId": "conflicts", "parameters": [{"name": "a", "in": "query", "schema": {"type": "integer"}}, {"name": "query_a", "in": "header", "schema": {"type": "string"}}, {"name": "b", "in": "query", "schema": {"type": "boolean"}, "description": ""}, {"name": "2", "in": "query", "schema": {}}],
      "requestBody": {"required": True, "content": {"application/json": {"schema": {"type": "object", "additionalProperties": False, "required": ["a", "body_a"], "properties": {"a": {"type": "string"}, "body_a": {"type": "string"}, "1": {"type": "string"}}}}}}, "responses": {"204": {}}}}}}, tool=None)
json.dump(cases, open(sys.argv[1], "w"), indent=1)
print(len(cases))
