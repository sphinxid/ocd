// Newline-delimited JSON-RPC peer over the sidecar's stdin/stdout.
//
// Both sides may send requests. A message with `method` and `id` is a request, one with
// `method` and no `id` a notification, and one with `id` and no `method` a response to a
// request this side sent. Ids are scoped per direction.

export interface WireError {
  readonly message: string
  readonly name?: string
  readonly type?: string
  readonly data?: unknown
  readonly metadata?: unknown
}

export class RemoteError extends Error {
  readonly type?: string
  readonly data?: unknown
  readonly metadata?: unknown
  constructor(error: WireError) {
    super(error.message)
    if (error.name) this.name = error.name
    this.type = error.type
    this.data = error.data
    this.metadata = error.metadata
  }
}

export interface RequestContext {
  readonly id: number | string
  readonly signal: AbortSignal
}

type Handler = (params: any, context: RequestContext) => unknown
type Listener = (params: any) => void

export class Peer {
  private next = 1
  private readonly pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void }>()
  private readonly handlers = new Map<string, Handler>()
  private readonly listeners = new Map<string, Listener>()
  private readonly inflight = new Map<number | string, AbortController>()
  private closed = false

  constructor(private readonly write: (line: string) => void) {}

  handle(method: string, handler: Handler) {
    this.handlers.set(method, handler)
  }

  on(method: string, listener: Listener) {
    this.listeners.set(method, listener)
  }

  request<A = any>(method: string, params?: unknown, signal?: AbortSignal): Promise<A> {
    if (this.closed) return Promise.reject(new Error("Plugin host connection closed"))
    const id = this.next++
    return new Promise<A>((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      if (signal) {
        const abort = () => {
          if (!this.pending.delete(id)) return
          this.notify("$/cancel", { id })
          reject(signal.reason instanceof Error ? signal.reason : new Error("Aborted"))
        }
        if (signal.aborted) return abort()
        signal.addEventListener("abort", abort, { once: true })
      }
      this.send({ jsonrpc: "2.0", id, method, params })
    })
  }

  notify(method: string, params?: unknown) {
    if (this.closed) return
    this.send({ jsonrpc: "2.0", method, params })
  }

  close() {
    this.closed = true
    for (const [, pending] of this.pending) pending.reject(new Error("Plugin host connection closed"))
    this.pending.clear()
  }

  receive(line: string) {
    if (!line.trim()) return
    let message: any
    try {
      message = JSON.parse(line)
    } catch {
      console.error(JSON.stringify({ $log: "warn", message: "invalid protocol line", line: line.slice(0, 200) }))
      return
    }
    if (typeof message.method === "string") {
      if (message.id === undefined || message.id === null) {
        if (message.method === "$/cancel") {
          this.inflight.get(message.params?.id)?.abort(new Error("Cancelled"))
          return
        }
        const listener = this.listeners.get(message.method)
        if (listener) {
          try {
            listener(message.params)
          } catch (error) {
            logError(`notification ${message.method} failed`, error)
          }
        }
        return
      }
      void this.dispatch(message.id, message.method, message.params)
      return
    }
    const pending = this.pending.get(message.id)
    if (!pending) return
    this.pending.delete(message.id)
    if (message.error) pending.reject(new RemoteError(message.error))
    else pending.resolve(message.result)
  }

  private async dispatch(id: number | string, method: string, params: unknown) {
    const handler = this.handlers.get(method)
    if (!handler) {
      this.send({ jsonrpc: "2.0", id, error: { message: `Unknown method: ${method}` } })
      return
    }
    const controller = new AbortController()
    this.inflight.set(id, controller)
    try {
      const result = await handler(params, { id, signal: controller.signal })
      this.send({ jsonrpc: "2.0", id, result: result === undefined ? null : result })
    } catch (error) {
      this.send({ jsonrpc: "2.0", id, error: wireError(error) })
    } finally {
      this.inflight.delete(id)
    }
  }

  private send(message: unknown) {
    let line: string
    try {
      line = JSON.stringify(message)
    } catch (error) {
      const id = (message as { id?: unknown }).id
      line = JSON.stringify({ jsonrpc: "2.0", id, error: wireError(error) })
    }
    this.write(line + "\n")
  }
}

export function wireError(error: unknown): WireError {
  if (error instanceof RemoteError)
    return clean({ message: error.message, name: error.name, type: error.type, data: error.data, metadata: error.metadata })
  if (error instanceof Error) {
    const tagged = error as Error & { _tag?: string; type?: string; data?: unknown; metadata?: unknown }
    return clean({
      message: error.message,
      name: tagged._tag ?? error.name,
      type: typeof tagged.type === "string" ? tagged.type : undefined,
      data: safe(tagged.data),
      metadata: safe(tagged.metadata),
    })
  }
  if (typeof error === "string") return { message: error }
  if (error && typeof error === "object") {
    const record = error as Record<string, unknown>
    return clean({
      message: typeof record.message === "string" ? record.message : String(error),
      name: typeof record._tag === "string" ? record._tag : undefined,
      type: typeof record.type === "string" ? record.type : undefined,
      data: safe(record.data),
      metadata: safe(record.metadata),
    })
  }
  return { message: String(error) }
}

function clean<T extends Record<string, unknown>>(value: T): T {
  for (const key of Object.keys(value)) if (value[key] === undefined) delete value[key]
  return value
}

function safe(value: unknown) {
  if (value === undefined) return undefined
  try {
    return JSON.parse(JSON.stringify(value))
  } catch {
    return undefined
  }
}

export function logError(message: string, error: unknown) {
  log("error", message, { error: error instanceof Error ? (error.stack ?? error.message) : String(error) })
}

export function log(level: "trace" | "debug" | "info" | "warn" | "error", message: string, fields?: Record<string, unknown>) {
  try {
    process.stderr.write(JSON.stringify({ $log: level, message, ...fields }) + "\n")
  } catch {}
}
