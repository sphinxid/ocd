// Recording shim over tool-schema-impl.ts: direct calls from tests land in schema-calls.jsonl. The library itself
// imports the implementation, so only calls made through this module are recorded.
import * as Impl from "./tool-schema-impl.js"
import * as Recorder from "./record.js"

export type { InputProperty } from "./tool-schema-impl.js"
export const identifierSegment = Impl.identifierSegment
export const toTypeScript = Recorder.schemaCall("toTypeScript", Impl.toTypeScript)
export const jsonSchemaToTypeScript = Recorder.schemaCall("jsonSchemaToTypeScript", Impl.jsonSchemaToTypeScript)
export const inputProperties = Recorder.schemaCall("inputProperties", Impl.inputProperties)
export const inputTypeScript = Recorder.schemaCall("inputTypeScript", Impl.inputTypeScript)
export const isEmptyInput = Recorder.schemaCall("isEmptyInput", Impl.isEmptyInput)
export const outputTypeScript = Recorder.schemaCall("outputTypeScript", Impl.outputTypeScript)
export const decodeInput = Recorder.schemaCall("decodeInput", Impl.decodeInput)
export const decodeOutput = Recorder.schemaCall("decodeOutput", Impl.decodeOutput)
