// A small AI SDK provider implementing the LanguageModelV3 spec (AI SDK v6, the version upstream's
// bridge supports), with no dependencies. Used by the ocd AI SDK bridge tests and the parity suite.
//
// Two modes:
// - `script`: `doStream` emits the given stream parts (`{"type": "throw", ...}` throws instead);
// - `baseURL`: `doStream` POSTs an OpenAI-chat-like body to `<baseURL>/chat/completions` through the
//   provider's `fetch` and turns the OpenAI SSE answer into V3 stream parts. The body carries the V3
//   prompt as `messages` and the tools in OpenAI shape, so a chat-completions mock can answer it.

const AI_ERROR = Symbol.for("vercel.ai.error")
const API_CALL_ERROR = Symbol.for("vercel.ai.error.AI_APICallError")

// Same markers as @ai-sdk/provider's APICallError, so `APICallError.isInstance` recognizes it.
export class APICallError extends Error {
  constructor({ message, url, requestBodyValues, statusCode, responseHeaders, responseBody, isRetryable, data }) {
    super(message)
    this.name = "AI_APICallError"
    this[AI_ERROR] = true
    this[API_CALL_ERROR] = true
    this.url = url
    this.requestBodyValues = requestBodyValues
    this.statusCode = statusCode
    this.responseHeaders = responseHeaders
    this.responseBody = responseBody
    this.isRetryable = isRetryable ?? false
    this.data = data
  }
}

export function createFixture(options = {}) {
  const provider = (modelId) => provider.languageModel(modelId)
  provider.specificationVersion = "v3"
  provider.languageModel = (modelId) => new FixtureLanguageModel(modelId, options)
  provider.options = options
  return provider
}

const FINISH = { stop: "stop", length: "length", tool_calls: "tool-calls", content_filter: "content-filter" }

function wireData(data) {
  if (data instanceof Uint8Array) return { bytes: Buffer.from(data).toString("base64") }
  if (data instanceof URL) return { url: data.href }
  return data
}

function wirePrompt(prompt) {
  return prompt.map((message) =>
    Array.isArray(message.content)
      ? { ...message, content: message.content.map((part) => (part.type === "file" ? { ...part, data: wireData(part.data) } : part)) }
      : message,
  )
}

class FixtureLanguageModel {
  specificationVersion = "v3"
  supportedUrls = {}

  constructor(modelId, options) {
    this.modelId = modelId
    this.options = options
    this.provider = `${options.name ?? "fixture"}.chat`
  }

  async doGenerate() {
    throw new Error("doGenerate is not supported by the fixture")
  }

  async doStream(call) {
    const options = this.options
    if (Array.isArray(options.script)) return { stream: scripted(options.script) }
    const url = `${String(options.baseURL).replace(/\/+$/, "")}/chat/completions`
    const body = {
      model: this.modelId,
      stream: true,
      messages: wirePrompt(call.prompt),
      tools: call.tools.length
        ? call.tools.map((tool) => ({
            type: "function",
            function: { name: tool.name, description: tool.description, parameters: tool.inputSchema },
          }))
        : undefined,
      tool_choice: call.toolChoice,
      max_tokens: call.maxOutputTokens,
      temperature: call.temperature,
      fixture: {
        providerOptions: call.providerOptions,
        callHeaders: call.headers,
        sdk: { name: options.name, marker: options.marker, fetch: typeof options.fetch },
      },
    }
    const send = options.fetch ?? fetch
    const response = await send(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(options.apiKey ? { authorization: `Bearer ${options.apiKey}` } : {}),
        ...options.headers,
        ...call.headers,
      },
      body: JSON.stringify(body),
      signal: call.abortSignal,
    })
    if (!response.ok) {
      const text = await response.text()
      let data
      try {
        data = JSON.parse(text)
      } catch {}
      throw new APICallError({
        message: data?.error?.message ?? text,
        url,
        requestBodyValues: body,
        statusCode: response.status,
        responseHeaders: Object.fromEntries(response.headers),
        responseBody: text,
        isRetryable: response.status === 408 || response.status === 409 || response.status === 429 || response.status >= 500,
        data,
      })
    }
    return { stream: response.body.pipeThrough(new TextDecoderStream()).pipeThrough(sseParts()), request: { body } }
  }
}

function scripted(parts) {
  return new ReadableStream({
    start(controller) {
      for (const part of parts) {
        if (part.type === "throw") {
          controller.error(new Error(part.message))
          return
        }
        controller.enqueue(part)
      }
      controller.close()
    },
  })
}

// OpenAI chat-completions SSE → LanguageModelV3StreamPart.
function sseParts() {
  let buffer = ""
  let started = false
  let reasoning = false
  let text = false
  const tools = new Map()
  let finish
  let usage
  const handle = (chunk, controller) => {
    if (!started) {
      started = true
      controller.enqueue({ type: "stream-start", warnings: [] })
      controller.enqueue({ type: "response-metadata", id: chunk.id, modelId: chunk.model, timestamp: new Date(0) })
    }
    if (chunk.usage) usage = chunk.usage
    for (const choice of chunk.choices ?? []) {
      const delta = choice.delta ?? {}
      if (delta.reasoning_content) {
        if (!reasoning) controller.enqueue({ type: "reasoning-start", id: "reasoning-0" })
        reasoning = true
        controller.enqueue({ type: "reasoning-delta", id: "reasoning-0", delta: delta.reasoning_content })
      }
      if (delta.content) {
        if (reasoning) {
          controller.enqueue({ type: "reasoning-end", id: "reasoning-0" })
          reasoning = false
        }
        if (!text) controller.enqueue({ type: "text-start", id: "txt-0" })
        text = true
        controller.enqueue({ type: "text-delta", id: "txt-0", delta: delta.content })
      }
      for (const call of delta.tool_calls ?? []) {
        let tool = tools.get(call.index)
        if (!tool) {
          tool = { id: call.id, name: call.function?.name, input: "" }
          tools.set(call.index, tool)
          controller.enqueue({ type: "tool-input-start", id: tool.id, toolName: tool.name })
        }
        if (call.function?.arguments) {
          tool.input += call.function.arguments
          controller.enqueue({ type: "tool-input-delta", id: tool.id, delta: call.function.arguments })
        }
      }
      if (choice.finish_reason) finish = choice.finish_reason
    }
  }
  return new TransformStream({
    transform(data, controller) {
      buffer += data
      const events = buffer.split("\n\n")
      buffer = events.pop() ?? ""
      for (const event of events) {
        const line = event.split("\n").find((item) => item.startsWith("data:"))
        if (!line) continue
        const payload = line.slice(5).trim()
        if (payload === "[DONE]") continue
        handle(JSON.parse(payload), controller)
      }
    },
    flush(controller) {
      if (reasoning) controller.enqueue({ type: "reasoning-end", id: "reasoning-0" })
      if (text) controller.enqueue({ type: "text-end", id: "txt-0" })
      for (const tool of tools.values()) {
        controller.enqueue({ type: "tool-input-end", id: tool.id })
        controller.enqueue({ type: "tool-call", toolCallId: tool.id, toolName: tool.name, input: tool.input })
      }
      if (finish === undefined) return
      const cached = usage?.prompt_tokens_details?.cached_tokens
      const reasoningTokens = usage?.completion_tokens_details?.reasoning_tokens
      controller.enqueue({
        type: "finish",
        finishReason: { unified: FINISH[finish] ?? "other", raw: finish },
        usage: {
          inputTokens: {
            total: usage?.prompt_tokens,
            noCache: usage?.prompt_tokens === undefined ? undefined : usage.prompt_tokens - (cached ?? 0),
            cacheRead: cached,
            cacheWrite: undefined,
          },
          outputTokens: {
            total: usage?.completion_tokens,
            text: usage?.completion_tokens === undefined ? undefined : usage.completion_tokens - (reasoningTokens ?? 0),
            reasoning: reasoningTokens,
          },
        },
        providerMetadata: { fixture: { finish } },
      })
    },
  })
}
