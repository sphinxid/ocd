# 07 — TUI specification (opencode v2 → Rust/ratatui)

Source: opencode v2 `packages/tui` (`@opencode/tui` 2.0.26, OpenTUI + SolidJS) and `packages/theme` (`@opencode/theme` 2.0.26), MIT. Read-only checkout at
`$OCD_SCRATCH/src/opencode/packages`.

Citation convention: `src/...:N` = `packages/tui/src/...` line N; `theme/...`, `client/...`, `cli/...` = paths under `packages/`. Colors are named by theme token (see §6); text in backticks or quotes is verbatim.

## 0. Orientation

- `@opencode/tui` exports `run(TuiInput)` (`src/index.tsx:1`, `src/app.tsx:204`), invoked by the CLI default command (`cli/src/commands/handlers/default.ts:3,96-156`) with `{app, server, args, config, updater, packages, environment, terminalHandoff, log}`.
- The full-screen TUI has two primary routes, **home** and **session**, plus plugin routes. Most chrome (home footer, sidebar sections, prompt footer, notifications, diff viewer, stats, storybook) is implemented as **built-in feature plugins** that fill named slots (`src/plugin/builtins.ts`, `src/feature-plugins/`).
- A second, independent frontend — **mini mode** (`opencode mini`, `src/mini/`) — writes the transcript to native scrollback with a live footer; summarised in §7.9 only.
- 518 files / ~64k lines (≈20k in `src/mini`). Largest: `src/routes/session/index.tsx` (3640), `src/component/prompt/index.tsx` (2011), `src/component/session-tabs.tsx` (1803), `src/app.tsx` (1402).
- Section map: §1 app shell & layout · §2 dialogs & modal prompts · §3 session view & message rendering · §4 prompt input · §5 keybindings & commands · §6 themes · §7 state/config/server · §8 consolidated open questions.
- Theme tokens are v2 nested names (e.g. `theme.text.action.primary.selected`, `theme.background.raised.base`); v1 flat names (`textMuted`, `backgroundPanel`) exist only as migration input (§6).
- `KM:<file>:<line>` cites in §5 refer to `@opentui/keymap@0.5.17` downloaded from npm to `scratchpad/dl-opentui-keymap` (not part of the opencode checkout).


---

## 1. App structure and shell

All paths are relative to `packages/tui`. Theme tokens use the v2 nested token names as written in source (e.g. `theme.background.base`, `theme.background.raised.base`, `theme.text.muted`, `theme.text.feedback.error.base`, `theme.hue.accent[200]`). The themes section defines how they resolve. `tint(a, b, t)` = per-channel linear mix `a + (b - a) * t`, rounded to 8-bit ints (`src/theme/color.ts:43-48`). Every timing below is in milliseconds unless marked otherwise.

### 1.1 Entry points and public API

| Item | Source |
|---|---|
| Package exports `run` and `TuiInput` from `./app`, plus `LogProvider`, `useLog`, and the log types | `src/index.tsx:1-2` |
| `src/runtime.tsx` only re-exports `abbreviateHome` | `src/runtime.tsx:1` |
| `registerOpencodeSpinner()` runs at module load. It registers the OpenTUI `<spinner>` intrinsic from `opentui-spinner` only if the catalogue does not already have one | `src/app.tsx:108`, `src/component/register-spinner.ts:4-6` |

#### `TuiInput` (`src/app.tsx:180-203`)

| Field | Type / meaning |
|---|---|
| `app` | `TuiApp = { name, version, channel }` (`src/context/runtime.tsx:3-7`) |
| `server.endpoint` | Server endpoint. `baseUrl = endpoint.url`, and headers come from `Service.headers(endpoint)` (`src/app.tsx:211`) |
| `server.service?` | `{ reconnect(signal) => Promise<Endpoint>, restart() }`. Present when the server is managed. It enables the `/restart` command and the "Restarting service…" overlay text (`src/app.tsx:220-230`, `999-1019`, `1396`) |
| `args` | `Args` (see below) |
| `config` | `Config.Interface` (async `get()`), resolved with `{ terminalSuspend: platform !== "win32" }` (`src/app.tsx:208-210`) |
| `updater?` | `UpdateSource` for the update notification (`src/app.tsx:393-395`) |
| `packages` | Plugin `PackageSource` (`src/app.tsx:397-399`) |
| `environment?` | Terminal env variables. These are forwarded to the server once per connected session through `session.environment` (`src/app.tsx:490-500`) |
| `terminalHandoff?` | Async callback that returns `{ renderer, mode, complete }`, so an existing renderer (e.g. from mini mode) can be reused (`src/app.tsx:194-201`, `248-253`, `438-441`) |
| `log?` | Log sink. Defaults to a no-op (`src/app.tsx:206`) |

#### CLI `Args` (`src/context/args.tsx:4-13`)

`model?` (`provider/model`), `agent?`, `prompt?`, `continue?`, `sessionID?`, `newSessionID?`, `fork?`, `auto?`.

- `takeNewSessionID()` returns the pending `newSessionID` once. `restoreNewSessionID(id)` puts it back only if none is pending (`src/context/args.tsx:18-29`).

### 1.2 Startup sequence (`Tui.run`, `src/app.tsx:205-452`)

1. Resolve the config (`src/app.tsx:208`) and build the API client (`212`).
2. Resolve the initial location with `api.file.list({ location: { directory: process.cwd() } }).location`, falling back to `api.location.get()` (`213-216`).
3. Discover local plugin directories (`218`) and await the optional terminal handoff (`219`).
4. Create the renderer inside an Effect scope:
   - With a handoff, reuse `handoff.renderer` and set `useMouse` on it (`248-253`).
   - With `OPENCODE_DRIVE` set, use the simulation driver (`254-257`).
   - Otherwise call `createCliRenderer(options)`.

   Every path destroys the renderer on scope release (`258-264`).
5. Call `renderer.setMaxListeners(15)` (`266`). Create the clipboard (`267-273`) and register finalizers. Lifecycle finalizers run in reverse registration order through `Promise.allSettled`, and rejections are logged (`274-282`).
6. On `SIGHUP`, call `destroyRenderer(renderer)` (`284-288`). The renderer's `destroy` event opens the shutdown latch (`289`).
7. Theme mode is `handoff.mode ?? await renderer.waitForThemeMode(1000) ?? "dark"` (`291`). If the renderer was already destroyed, return.
8. `render(() => <provider tree/>, renderer)` (`294-437`). With a handoff, `handoff.complete` runs on the first `FRAME` and a render is requested (`438-441`).
9. Await the shutdown latch, then return `{ epilogue, reason }` (`443-444`).
10. After the scope closes:
    - If `reason !== undefined`, write `cliErrorMessage(reason) ?? errorFormat(reason)` plus `"\n"` to **stderr**.
    - If `epilogue` is set, write `epilogue + "\n"` to **stdout** (`447-451`).

    This happens after the renderer is destroyed, so it lands in the normal terminal buffer.

#### Renderer options (quoted, `src/app.tsx:234-246`)

```ts
{
  externalOutputMode: "passthrough",
  targetFps: 60,
  gatherStats: false,
  exitOnCtrlC: false,
  useKittyKeyboard: {},
  autoFocus: false,
  openConsoleOnError: false,
  useMouse: config.mouse,            // config default true (src/config/index.tsx:311)
  consoleOptions: { keyBindings: [{ name: "y", ctrl: true, action: "copy-selection" }] },
}
```

- The screen mode is not set explicitly, so it is the OpenTUI default (alternate screen). Kitty keyboard protocol is on (`useKittyKeyboard: {}`), and Ctrl+C does not exit the process.
- A reactive effect re-applies `renderer.useMouse = config.data.mouse` whenever the config changes (`src/app.tsx:593-595`).

#### Renderer teardown

`destroyRenderer` clears the terminal title with `setTerminalTitle("")` and then calls `destroy()` if the renderer is not already destroyed (`src/util/renderer.ts:3-7`).

#### Palette probe

On the first `FRAME`, a 50 ms timer calls the theme's `afterPaint`. This keeps OSC palette queries behind visible output (`src/app.tsx:480-489`).

#### Exit and epilogue

- `ExitProvider.exit(reason?)`: if the renderer is not destroyed, it stores the reason and calls `destroyRenderer` (`src/app.tsx:297-303`). `useExit()` returns that function (`src/context/exit.tsx:5-8`).
- `EpilogueProvider.set(value?)` stores the text printed after exit (`src/app.tsx:304`, `src/context/epilogue.tsx:3-6`).
- Only the session route sets the epilogue. It does so whenever the title changes, using `sessionEpilogue({ title: Locale.truncate(title, 50), sessionID })`, and clears it on unmount (`src/routes/session/index.tsx:194-197`). Exiting from Home therefore prints nothing.
- `Locale.truncate(s, n)` returns `s.slice(0, n-1) + "…"` when `s.length > n` (`src/util/locale.ts:50-53`).

Epilogue format (`src/util/presentation.ts:3-35`). ANSI codes: reset `\x1b[0m`, bold `\x1b[1m`, dim `\x1b[90m`.

```
  <logo.left[i] drawn dim/235-shadow> <logo.right[i] drawn default/238-shadow>   ×4 lines
<blank>
  \x1b[90mSession   \x1b[0m\x1b[1m<title>\x1b[0m
  \x1b[90mContinue  \x1b[0m\x1b[1mopencode -s <sessionID>\x1b[0m
<blank>
```

- Labels are padded with `padEnd(10)` (`src/util/presentation.ts:27`).
- Glyph rules: `_` becomes `${bg} ${reset}`; `^` becomes `${fg}${bg}▀`; `~` becomes `${shadow}▀`; a space stays a space; any other character becomes `${fg}char`.
- The left half uses `fg=\x1b[90m`, `shadow=\x1b[38;5;235m`, `bg=\x1b[48;5;235m`. The right half uses `fg=reset`, `shadow=\x1b[38;5;238m`, `bg=\x1b[48;5;238m`. Left and right are joined with one space and prefixed by 2 spaces (`src/util/presentation.ts:7-24`).

### 1.3 Provider tree (outer → inner, `src/app.tsx:296-435`)

```
LogProvider(log)
└ ExitProvider(exit)
  └ EpilogueProvider(set)
    └ TuiAppProvider(input.app)
      └ ErrorBoundary  fallback → ClipboardProvider + ErrorComponent(error, reset, mode)
        └ TuiPathsProvider{cwd, home, state, worktree: global.data + "/worktree"}
          └ StorageProvider
            └ TuiLifecycleProvider{add(finalizer)}
              └ TuiTerminalEnvironmentProvider{platform, variables: input.environment}
                └ TuiStartupProvider{initialRoute, skipInitialLoading}
                  └ ClipboardProvider
                    └ ArgsProvider(...input.args)
                      └ ConfigProvider
                        └ Keymap.Provider
                          └ ToastProvider
                            └ RouteProvider(initialRoute = args.continue ? {type:"session",sessionID:"dummy"} : undefined)
                              └ ClientProvider(api, url, service)
                                └ PermissionProvider
                                  └ DataProvider(directory)
                                    └ LocationProvider
                                      └ SessionTabsProvider
                                        └ SessionTerminalsProvider
                                          └ ThemeProvider(mode, source)
                                            ├ ThemeErrorToast
                                            └ LocalProvider
                                              └ PromptStashProvider
                                                └ DialogProvider
                                                  └ FrecencyProvider
                                                    └ PromptHistoryProvider
                                                      └ PromptRefProvider
                                                        └ EditorContextProvider
                                                          └ AttentionProvider
                                                            └ UpdateNotificationProvider(updater)
                                                              └ PanelProvider
                                                                └ PluginProvider(packages, directories)
                                                                  └ App
```

- `createSimpleContext` providers render their children only when `init.ready` is `undefined` or `true` (`src/context/helper.tsx:11-19`). A provider with a `ready` flag therefore blanks the subtree until it is ready.
- `TuiStartup.initialRoute` resolves in this order (`src/app.tsx:336-353`):
  - `OPENCODE_STORY` set gives `{type:"plugin", id:"opencode.storybook", name:"storybook", data: story==="1" ? undefined : {story}}`.
  - Otherwise, `OPENCODE_ROUTE` is parsed as JSON.
  - `skipInitialLoading = Boolean(process.env.OPENCODE_FAST_BOOT)`.
- `InteractivityProvider(enabled)` ANDs its value with the parent's, defaulting to true. A disabled subtree ignores keymap and focus (`src/context/interactivity.tsx:6-15`). SessionFrame and PanelHost use it.

### 1.4 Routes (`src/context/route.tsx`)

| Route | Shape | Screen |
|---|---|---|
| `home` | `{ type:"home", prompt?: PromptInfo, location?: LocationRef }` (`:7-12`) | `Home` (§1.7) |
| `session` | `{ type:"session", sessionID, prompt? }` (`:14-18`) | `SessionFrame` (§1.9) |
| `plugin` | `{ type:"plugin", id, name, data? }` (`:20-25`) | `PluginRoute` or `PluginRouteMissing` (§1.12) |

- The initial route is `props.initialRoute ?? initialRoute(startup.initialRoute) ?? {type:"home"}` (`:33-35`). The startup value is validated, and unknown shapes are dropped (`:48-68`).
- `navigate(route)` calls `setStore(reconcile(route))` (`:41-43`).
- The session view is keyed on `sessionID`, so switching sessions remounts it (`src/app.tsx:1364`).

#### Post-mount navigation (`src/app.tsx:625-686`)

- **`--agent` / `--model`:** applied to the local state. An invalid model shows a warning toast `Invalid model format: ${args.model}` for 3000 ms (`:629-639`).
- **`--session` without `--fork`:** navigates to that session with `prompt = {text: args.prompt, files:[], agents:[], pasted:[]}` (`:640-646`).
- **`--continue`:**
  1. Initial route is `{type:"session", sessionID:"dummy"}`.
  2. Once, list sessions with `{limit:1, order:"desc", parentID:null, directory: default}`.
  3. Navigate to the newest one, or fork it first if `--fork` is set.

  If none exists, the route stays at "dummy" (see Open questions) (`:650-675`).
- **`--session` with `--fork`:** forks once, then navigates (`:678-686`).

#### Server-pushed events

- `tui.command.execute` → `keymap.dispatch` (`:1254-1257`).
- `tui.toast.show` → `toast.show` (`:1259-1267`).
- `tui.session.select` → navigate (`:1269-1275`).

All three are filtered by matching directory.

On `session.deleted` for the current session, the app navigates home and shows an info toast. The text is `Session "${title}" was deleted`, or `The current session was deleted` when the title is unknown (`:1283-1293`).

#### Terminal title (`src/app.tsx:597-623`), when `config.terminal.title` is on (default `true`, `:573`)

| Situation | Title |
|---|---|
| Home | `"OpenCode"` |
| Session with no title or a fallback title (`/^(New session\|Child session) - <ISO>$/`) | `"OpenCode"` |
| Session with a title | `` `OC | ${title.length > 40 ? title.slice(0, 37) + "…" : title}` `` |
| Plugin route | `` `OC | ${route.name}` `` |

### 1.5 Root layout (`App`, `src/app.tsx:1321-1401`)

```
┌ root box: width=termW height=termH column bg=theme.background.base ───────────────────┐
│┌ main row: flexGrow 1, row, relative ──────────────────────────────────────────────┐ │
││[VerticalSessionTabs width=W]│┌ content column flexGrow 1 ─────────────────────────┐ │ │
││  (only if verticalTabsVisible)││ (when plugins.ready())                            │ │ │
││                              ││ ┌ route column flexGrow 1 ───────────────────────┐ │ │ │
││                              ││ │ [HorizontalSessionTabs, 1 row] (tabs visible & │ │ │ │
││                              ││ │   !vertical)                                    │ │ │ │
││                              ││ │ Home | SessionFrame | PluginRoute               │ │ │ │
││                              ││ └────────────────────────────────────────────────┘ │ │ │
││                              ││ <Slot path="app"/>  (plugins, mostly invisible)    │ │ │
││                              │└────────────────────────────────────────────────────┘ │ │
││ [PaneResizeHandle at left=W-1, only if vertical tabs]                               │ │
│└─────────────────────────────────────────────────────────────────────────────────────┘ │
│ [DevToolsBar, 1 row]  (config.debug.devtools ?? channel==="local"; hidden on opencode.stats)│
│ overlays: StartupLoading(z5000) · Reconnecting(z10000) · MigrationOverlay(z10000) · Toast│
└────────────────────────────────────────────────────────────────────────────────────────┘
```

#### Visibility logic (`src/app.tsx:575-586`)

- `tabsVertical = config.tabs.layout==="vertical" && sessionTabsFitVertically(termW, tabsResize.size())`. The fit test is `termW >= W + 64` (`src/ui/layout.ts:8-10`).
- `tabsAvailable = sessionTabs.enabled() && tabs.length > 0 && route.type !== "plugin"`.
- `fullscreenPanel` is true on a session route whose panel targets this session with `presentation==="fullscreen"`.
- `tabsVisible = tabsAvailable && !fullscreenPanel`.
- `verticalTabsVisible = tabsVisible && tabsVertical`.
- `panels.setWidth(termW - (tabsAvailable && tabsVertical ? W : 0))`. This is measured even while fullscreen hides the tabs (`:586`).
- The panel closes when the route leaves its session (`:587-591`).

#### Tab config defaults (`src/config/index.tsx:285-330`)

- `tabs.mode` defaults to `auto`. Tabs are enabled when mode is `on`, or when mode is `auto` and `HERDR_ENV !== "1"`.
- `scope` defaults to `cwd`, `layout` to `horizontal`, `indicators` to `status`.

#### Vertical tabs width (`src/app.tsx:501-515`, `src/ui/layout.ts`)

- Persisted in storage `"layout".verticalTabsWidth`. The default is `SESSION_SIDEBAR_WIDTH = 42`.
- Clamp: `max(5, min(w, 72, termW - 44))`. The constants are `SESSION_TABS_COMPACT_WIDTH=5`, `SESSION_SIDEBAR_MAX_WIDTH=72`, and `SESSION_CONTENT_MIN_WIDTH=44` (`src/ui/layout.ts:1-17`).
- Dragging uses `fromMouse = event.x + 1`. The hit test is `x ∈ [W-1, W]`.

#### Mouse on the root

- **Right button down:** when copy mode is not `"select"`, copies the selection and swallows the event.
- **Mouse up:** a link under the cursor opens with `openUrl`. Otherwise, in `"select"` mode, copy-on-select runs on release (`src/app.tsx:1327-1339`).
- **Copy mode:** `config.terminal.copy ?? (win32 ? "manual" : "select")` (`:547-548`).
- **Selection keys:** intercepted at priority 101, before all bindings (`:551-557`).
- **Console copy:** shows the toast "Copied to clipboard" (info) (`:563-572`).

#### MCP alert toasts (`src/app.tsx:520-545`)

These fire once per status change:

| Status | Variant | Title | Message | Action |
|---|---|---|---|---|
| `needs_auth` | warning | `"MCP server needs authentication"` | `` `Connect "${name}" to use its tools.` `` | `{label:"Open MCP servers"}` → `mcp.list` |
| `failed` | error | `` `MCP server failed: ${name}` `` | `"Run /mcps to view details."` | same action |

### 1.6 Pane resize primitives

**`createPaneResize`** (`src/ui/pane-resize.ts:4-76`):

- State: `size = clamp(preferred)`.
- A left-button mousedown starts a resize.
- A second mousedown within **300 ms** of a click that did not move resets to `defaultValue()` and commits.
- Each drag sets `preferred = clamp(fromMouse(e))`.
- Release (drag-end or up) commits `fromMouse` if the pointer moved, else the current size. `onCommit` is called only when the value changed. Hover is then re-evaluated with `contains()`.
- Drag and release handlers are bound on the parent, so dragging continues outside the handle.

**`PaneResizeHandle`** (`src/ui/pane-resize-handle.tsx:4-33`):

- Absolute box at `left`, `top 0`, `zIndex 10`, `width 2`, `height 100%`.
- Its inner bar is `width 1`, with `marginLeft 1` when `highlight="right"`.
- The bar background is `theme.background.raised.high` while hovered or resizing, and transparent otherwise.

**Borders** (`src/ui/border.ts`):

- `EmptyBorder`: all corners and T-pieces are `""` and `horizontal: " "`.
- `SplitBorder = { border: ["left","right"], customBorderChars: {...EmptyBorder, vertical: "┃"} }`.

### 1.7 Home route (`src/routes/home.tsx`)

```
 (route column, padding L/R = termW<44 ? 1 : 2, alignItems center)
 ┌──────────────────────────────────────────────┐
 │            flexGrow spacer                   │
 │            3-row spacer (shrinkable)         │
 │            [Logo]  (measured → logoWidth)    │
 │            1-row spacer                      │
 │            [UpdateNotification width=logoW]  │
 │   [Prompt box width 100% maxWidth 75 pt 1]   │
 │            flexGrow spacer                   │
 └──────────────────────────────────────────────┘
 [home.footer slot, full width, flexShrink 0]
 [global FormPrompt overlay: absolute z2000 left0 right0 bottom1 paddingX2]
```

Source: `src/routes/home.tsx:81-122`.

- **Prompt:**
  - Placeholders: normal `"Fix a TODO in the codebase"`, `"What is the tech stack of this project?"`, `"Fix broken tests"`; shell `"ls -la"`, `"git status"`, `"pwd"` (`:20-23`).
  - Disabled while any global form is pending (`:102`).
- **`--prompt`:**
  - Pre-fills the prompt once per process (`once`) (`:53-59`).
  - Auto-submits once when the model and catalog are ready, an agent and model are set, and the composer text equals the arg (`:68-79`).
- **Location:** the home route's `location` or the default is applied with `location.set` (`:44-47`). The editor selection is cleared on mount (`:49-51`).

#### UpdateNotification (`src/routes/home.tsx:125-176`)

- **Visibility:** shown when `update.notification()` exists. For a remote-server notification it is shown only when `type==="available"`.
- **Box:** row, centered, `gap 1`, `width=logoWidth`, `maxWidth 100%`. Hover background is `theme.background.action.primary.hovered`.
- **Text:** rendered with `FadeInText` (fg `theme.text.muted`), backdrop = the hover background or `theme.background.base`.
- **Content:**
  - Local: span `"/exit"` (installed) or `"/update"` in `theme.text.action.primary.selected`, then `` ` restart to use v${version}` `` or `` ` to install v${version}` ``.
  - Remote: `"remote server update available"`.
- **Click:** remote → dismiss; installed → `exit()`; otherwise → `update.open("notification")`.

#### Logo (`src/component/logo.tsx`, data `src/logo.ts`)

Raw data, verbatim (`src/logo.ts:1-9`):

```ts
export const logo = {
  left: ["                   ", "█▀▀█ █▀▀█ █▀▀█ █▀▀▄", "█__█ █__█ █^^^ █__█", "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀"],
  right: ["             ▄     ", "█▀▀▀ █▀▀█ █▀▀█ █▀▀█", "█___ █__█ █__█ █^^^", "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀"],
}

export const go = {
  left: ["    ", "█▀▀▀", "█_^█", "▀▀▀▀"],
  right: ["    ", "█▀▀█", "█__█", "▀▀▀▀"],
}
```

Each line is 19 cells wide.

Per-character rendering (`src/component/logo.tsx:12-50`). `shadow = tint(theme.background.base, fg, 0.25)`. Every cell is `selectable={false}`.

| char | renders | fg | bg |
|---|---|---|---|
| `_` | `" "` | fg | shadow |
| `^` | `"▀"` | fg | shadow |
| `~` | `"▀"` | shadow | (none) |
| `,` | `"▄"` | shadow | (none) |
| other | itself | fg | (none) |

Breakpoints (`src/component/logo.tsx:52-77`):

| Terminal | Output |
|---|---|
| `height < 12` | nothing |
| `width < 22` | `go.right.slice(1)` (3 rows × 4 cols), fg `theme.text.base`, **bold** |
| `width < 44` | stacked: `logo.left.slice(1)` (3 rows, `theme.text.muted`, not bold), then `logo.right` (4 rows, `theme.text.base`, bold) → 7 rows × 19 |
| else | 4 rows; each row is `[left (muted)] gap 1 [right (text.base, bold)]` → 39 × 4 |

Rendered wide form, before shadow and colour:

```
                                 ▄
█▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█
█  █ █  █ █▀▀▀ █  █ █    █  █ █  █ █▀▀▀
▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀
```

In this form, the `_` cells are spaces on the shadow background, the `^` cells are `▀` on the shadow background, and the `~` cells are shadow-coloured `▀`.

#### Home footer (builtin plugin `opencode.home.footer`, `src/feature-plugins/home/footer.tsx`)

- **Slot:** claims `{append:"home.footer"}` (`:114`).
- **Visibility:** rendered only when `termH >= 14 && termW >= 44` (`:82`).
- **Layout:**
  - Row, `width 100%`, `paddingX 2`, `gap 2`.
  - `paddingY` is `0` when `termH < 16`, else `1` (`:83-92`).
  - Children in order: `Mcp`, `Plugins`, `<Slot "home.footer.status">`, a flexGrow spacer, then the version (`app.version`, `theme.text.muted`) when `termW >= 64` (`:93-101`).
- **Mcp:**
  - Shown only when there are servers.
  - Failed: `⊙ ` (error colour) + `"{n} MCP failed"`.
  - Otherwise: `⊙ ` (`feedback.success.base` if `connected>0`, else `text.muted`) + `"{n} MCP"`.
  - Then a muted `/mcps` when `termW >= 64`.
  - Click → `mcp.list` (`:15-50`).
- **Plugins:**
  - Shown only when the failed count is >0.
  - Text: `⊙ ` (error colour) + `` `${n} plugin${n===1?"":"s"} failed` ``, then a muted `/plugins` when `termW >= 80`.
  - Click → `plugins.list` (`:52-75`).
- **Thresholds:** `homeFooterVisibility` gives `{mcpCommand: w>=64, pluginCommand: w>=80, version: w>=64}` (`:7-13`).

### 1.8 Session tabs (`src/component/session-tabs.tsx`)

`SessionTabs` dispatches to vertical or horizontal. `numbers = (indicators ?? config.tabs.indicators) === "numbers"` (`:506-542`).

#### Shared tab status (`src/context/session-tabs.tsx:152-176`)

Status is computed per root session and its family:

- `unread`: `"error"` when the root's `outcome === "failed"`, `"activity"` otherwise, `undefined` when read.
- `promptPulse` counter.
- `attention`: `"permission"` if any family member has a pending permission, else `"question"` for a pending form, else `false`.
- `busy`: running, or a non-synthetic pending item.
- `renaming`: title generation pending.

`newTab()` is true on the Home route (`:146-151`). `complete = unread==="activity" && !busy` (`src/context/session-tabs-model.ts:29-31`).

#### Indicator glyphs (`src/component/session-tabs.tsx:59-73`, `TabIndicator` `:133-204`)

- **Spinners:**
  - `dots` (default): `["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]` @ 80 ms.
  - `arcs`: `["◜","◝","◞","◟"]` @ 120 ms.
  - `quadrants`: `["◴","◷","◶","◵"]` @ 120 ms.
  - `line`: `["|","/","-","\\"]` @ 120 ms.
- **Unread markers:** `small-dot "•"` (default), `dot "●"`, `square "▪"`, `large-square "■"`.
- **Label priority:**
  1. In numbers mode, always the 1-based number.
  2. Attention `permission` → `"!"`, `question` → `"?"`.
  3. Running (busy and no attention) → animated spinner, or `frames[0]` when animations are off.
  4. `"+"` for the new-session tab.
  5. The unread marker, also during its fade-out.
  6. `idleLabel ?? ""`.

  In status mode, idle tabs in the horizontal strip therefore show an **empty** indicator.
- **Indicator box:**
  - `width = numberWidth + 1`.
  - `justifyContent flex-end`, `paddingRight 1`. When centered (compact rail), `width = rail width`, `height 1`, centered.
- **Unread fade-out:** tween 0.18 s.
  - Colour = `tint(bg, tint(unreadColor, flashColor, flash*0.3), min(1, opacity/0.8))`, where `flash = max(0, 1-|opacity-0.8|/0.2)`.
  - In numbers mode or when selected, the colour does not fade.

#### Shared colours

| Name | Value |
|---|---|
| `unreadColor` | `theme.hue.accent[200]` |
| `activeNumber` (running) | `theme.hue.interactive[200]` |
| `idleNumber` | `tint(theme.text.formfield.base, <bg>, 0.55)` |
| `tabFeedbackColor` | attention → `theme.hue.accent[200]`; unread error → `theme.text.feedback.error.base` (`:127-131`) |

Sources: `:567-569`, `:1330-1332`.

#### Number ignition

When running toggles or `promptPulse` changes, the number jumps to a level of 0.85 toward `text.base` and decays to 0 over 0.7 s (`:215-230`). The attention glow dims to 0.7 when the selected tab has attention, with a 0.2 s tween (`:206-213`).

#### Title fade

When a title overflows and the width is greater than `FADE_WIDTH=4`, the last 4 cells fade toward the background. The opacity is `0.2 + 0.72*((p-1)/3)` (`:232-238`). During marquee scrolling the leading edge also fades in.

#### Marquee

`MARQUEE_DELAY=600`, `MARQUEE_INTERVAL=80` (`:79-80`).

- Hovering an overflowing title waits 600 ms, then scrolls one cell every 80 ms through `title + " · "` (the separator `·` is tinted 55% toward the background).
- After one full cycle it stops and resets. The leading fade is a 0.25 s tween (`:279-335`, `src/util/marquee.ts:3-50`).
- Mouse-out resets it through a `setTimeout(0)` debounce (`:337-370`).

#### Horizontal strip (`HorizontalSessionTabs`, `:1288-1803`)

```
‹3 │ 4│Title of tab four   ✕│ 5 •Other tab      │ 6⠙Running tab   │  2› + 
```

- **Strip:** 1 row, `flexShrink 0`, `zIndex 1`, no explicit background, so the route background shows (`:1529-1556`).
- **Overflow markers:**
  - `‹{before}` and `" "+{after}+"›"`, both muted.
  - Marker width = `digits(count) + 2` (`sessionTabOverflowWidth`, `src/context/session-tabs-model.ts:37`).
- **Add button:** `" + "`, width 3 (`ADD_TAB_WIDTH`). Muted normally; on hover, fg `text.base` and bg `background.action.primary.hovered`.
  - Shown when `tabs.add` exists and the strip is not in the new-tab state. In the new-tab state a `New session` pseudo-tab is appended as the active tab with label `"+"` (`:1340-1343`, `:1768-1797`).
- **Layout:** `adaptiveSessionTabLayout(items, activeID, termW - (showPlus ? 3 : 0), previousStart)` (`src/context/session-tabs-model.ts:178-261`), with `SESSION_TAB_WIDTH=22`, `SESSION_TAB_MAX_WIDTH=32`, `SESSION_TAB_MIN_WIDTH=8`.
  - **Count that fits:**
    - With an active tab: `1 + floor((avail - 22) / 8)`.
    - Without: `floor(avail / 8)`.
    - The count is clamped to `[1, n]`. It is re-solved up to 3 times after subtracting the overflow markers, keeping the active tab visible and preferring the previous start.
  - **Roomy** (`content >= 22*visible`): `total = min(content, 32*visible)`, split evenly; the remainder goes to the leftmost tabs.
  - **Tight:** `inactive = clamp(floor((total - min(22,total)) / (visible - 1)), 8, 22)` and `active = total - inactive*(visible - 1)`.
- **Width animation:** widths, selection (0..1) and completion activity animate with `spring({visualDuration: 0.1})`.
  - When a tab enters, retained tabs keep their animated values and new tabs grow from width 0.
  - On a terminal resize the values jump.
  - Rounding slack goes to the active tab (`:1386-1468`).
- **Tab cell** (`:1646-1759`):
  - `[TabIndicator width=numberWidth][title_shimmer width=titleW][✕ absolute right=1]`.
  - `numberWidth = max(2, digits(items.length))`.
  - `titleW = width - 1 - numberWidth`, minus 2 more while hovered, so the title stops before `✕`.
  - **Background** (`:1570-1575`): `tint(base, theme.decrease(theme.background.raised.base), dragged ? 1 : selection)`, where `base = theme.background.action.primary.hovered` if hovered or dragged and not selected, else `theme.background.base`.
  - **Foreground:** hovered → `text.base`; otherwise `tint(text.muted, text.base, selection)`. Bold when selected or dragged.
  - **Number colour:**
    - Running → `activeNumber`.
    - Otherwise feedback colour, or `tint(base, unreadColor, activity)`.
    - The result is then brightened toward `text.base` by `max(ignition, 0.15 * sweepLevel)` (`:1634-1643`).
  - **Close mark:** `"✕"`, shown only while hovered. Colour is `tint(text.muted, text.base, 0.6)`, and `text.base` on hover.
  - **Glow text:** title characters over the glow are tinted toward the glow colour by `0.12 * unreadGlowIntensity(1 + numberWidth + i, width)` (`:124-125`).
- **Interaction:**
  - Left-down starts a drag. Drop position is found with `slotAt(x)`, and the reorder is persisted once on release.
  - Middle-click closes the tab.
  - Right-click opens the context menu.
  - Clicking `✕` closes the tab and starts a **close hold**: the visible widths freeze for `MOUSE_CLOSE_HOLD_MS = 5000`, so the next tab's `✕` lands under the cursor. The hold is released when the pointer leaves the strip, on a terminal resize, or on timeout (`:1311-1327`, `:1470-1491`, `src/component/session-tabs.tsx:240-277`).

#### Vertical rail (`VerticalSessionTabs`, `:544-1286`)

- **Rail:** `width=W` (default 42), `height 100%`, `paddingTop 1`, bg `theme.background.raised.base`.
- **Compact:** `compact = W < SESSION_TABS_COMPACT_BREAKPOINT (12)` (`src/ui/layout.ts:3`).
- **Content:** a scrollbox with no scrollbars. Tabs sit in a column with `gap 1`; compact mode adds `paddingY 1`. Stride is 3 rows (normal) or 2 (compact). The active tab is scrolled into view after the next frame (`:657-675`).

Normal-width tab, 2 rows:

```
 1  Title text here…               ✕     ← row 1: indicator(numberWidth+1) + title (+ ✕ when hovered, right=1); paddingRight 1
    project ⎇ branch                     ← row 2: detail, paddingLeft numberWidth+1, paddingRight 2
```

- **Tab background:** selected → `theme.background.action.primary.selected`; hovered or dragged → `theme.background.raised.high`; else `raised.base` (`:776-781`).
- **Detail text:** `sessionTabDetail(project, branch, default, worktree)`.
  - Format: `"${project} ⎇ ${branch}"`.
  - The branch is shown only for worktrees not on the default branch (`src/context/session-tabs-model.ts:14-22`).
  - Colour: `tint(text.muted, bg, 0.35)`.
- **Separators:** half-block `TabPulse` rows at `top=-1` (edge `"above"`, `▄`). The last tab also gets one at `top=2` (`▀`). Pulse width is `indicatorWidth = 10` (`:850`, `:975-1027`).
- **Add slot** (`:1146-1237`):
  - Content: `+` (2 cols, `idleNumber` or `text.base`) followed by `"New session"` (muted, or `text.base`).
  - Indent: `paddingLeft 1`.
  - When it is the current new tab, bg is `action.primary.selected`, the text is bold, and a `×` close appears on hover.
- **Compact mode** (`W < 12`):
  - **Header:** `SessionTabsRailControls`, a 1-row `⌕` centered in `theme.text.action.secondary.base`. Click → `session.list` (`src/component/session-tabs-rail.tsx:20-81`).
  - **Tabs:** each is 1 row with a centered indicator. The idle label is the first grapheme of the title, or `"U"`.
  - **Highlighted tab:** gets `▄` / `▀` half-row caps above and below (`SessionTabHalfRow`, `:83-106`).
  - **Hover tooltip:**
    - Absolute at `left=W`, `width=min(54, termW-W)`, `height 4`, `paddingY 1`, `zIndex 2000`.
    - Body: 2 rows, bg `raised.base`, `paddingX 1`, with the title (truncated, `text.base`) and the detail (muted).
    - Caps: half-block caps against `background.base` (`:1240-1280`).
- **Resize handle:** on the rail's last column (`src/app.tsx:1385-1387`).

#### Tab context menu (`:372-504`)

- **Placement:** a portal at `zIndex 2500` with a full-screen click-catcher. The menu scrollbox sits at `(x, y+1)`, clamped on-screen.
- **Size:** `width = min(termW, max(20, ...min(50, strW(title)+2)))`. Rows are 1 high with `paddingX 1`.
- **Colours:** bg `raised.base`; hovered actionable row `raised.high`. Actionable items use `text.base`, headers `text.muted`.
- **Tab right-click items:** `"New session"` (if add is available), `"Rename"`, `"Copy session ID"` (toast `"Session ID copied to clipboard"`), `"Close"`.
- **"+" right-click items:** header `"Recently closed tabs"`, then up to 10 closed titles (`|| "Untitled session"`), or `"No recently closed tabs"`.
- **Keys:** a `menu` keymap mode where `escape,ctrl+c` closes.

#### Tab pulse animation (`src/component/tab-pulse.tsx`)

A custom renderable that paints one row of background cells. With `edge`, it paints `▄`/`▀` cells whose fg is the inner colour and bg the outer colour.

**Constants (`:34-54`):**

| Group | Values |
|---|---|
| Run sweep | `RUN_DURATION 2800`, `RUN_ATTACK 450`, `RUN_HEAD 4`, `RUN_TAIL 18`, `RUN_FADE_OUT 500` |
| Completion | `COMPLETION_DURATION 1200` (attack 0.12, opacity 0.18) |
| Edge flash | `EDGE_FLASH_DURATION 800` (attack 0.1, opacity 0.1); prompt flash scale 2 |
| Glow ignition | 600 (peak 1.5, attack 0.3) |
| Glow release | 900 (attack 0.12, peak 1.25; drains in the first 200) |
| Glow shape | `GLOW_TAIL 12`, `GLOW_OPACITY 0.16` |

**Curves:**

- `smootherstep(x) = x³(x(6x-15)+10)`.
- `coast(v)` is a ramped linear with `ramp=0.2`.
- `intensityAt(i, front, head, tail)`: ahead of the front, `smootherstep(1+d/head)`; behind it, `smootherstep(1-d/tail)` (`:56-65`).

**Behaviour:**

- **Running:** two sweep fronts. The second starts at half a cycle. Each travels from `-4` to `width-1+18` with coast easing. Strength is `0.14*running` (`:409-418`, `:687-704`).
- **Glow:** per-cell `smootherstep(1 - max(0,i-1)/tail) * 0.16 * level`, from the left edge (`:75-80`).
- **Blend order:** background → glow → running colour → flash → completion (`:88-124`).
- **Horizontal tab colours:** `color = tint(bg, text.base, 0.45)`, `flash = tint(bg, text.base, 0.65)`, `glowColor = tint(bg, feedback ?? unreadColor, glowLevel)`, `completionColor = unreadColor` (`src/component/session-tabs.tsx:1576-1582`, `:1684-1696`).
- **`onLevel`:** reports the sweep intensity at cell 1, quantized to 1/32. This drives the number brightening.

### 1.9 Session frame (`src/component/session-frame.tsx`)

The route column hosts `SessionFrame(sessionID, verticalTabsWidth)`:

```
row, flexGrow 1, relative
┌ #session-pane flexGrow 1 basis 0 ─────────────────┐┌ right pane (flexShrink 0) ─────┐
│ InteractivityProvider(activePane=session && !fs)   ││ Sidebar (42)  |  PanelHost     │
│   Session route (§1.10)                            ││   |  TerminalPane               │
│ [focus-guard overlay z1 when right pane active]    ││                                 │
└────────────────────────────────────────────────────┘└─────────────────────────────────┘
[PaneResizeHandle for terminal/panel, highlight="right"]
[narrow sidebar overlay: absolute full-frame, bg RGBA(0,0,0,70), alignItems flex-end → Sidebar]
```

#### Breakpoints and decision logic

- `wide = termW - verticalTabsWidth > 120` (`:106`).
- `sidebarVisible` (`:107-111`):
  - `false` for child sessions (`parentID`).
  - `true` if toggled open.
  - Otherwise `(config.session.sidebar ?? "auto") === "auto" && wide`. The config values are `auto` and `hide` (`src/config/index.tsx:151-152`).
- `rightPane` priority (`:112-117`):
  1. `"panel"` when there is an active panel.
  2. `"sidebar"` when explicitly opened and visible.
  3. `"terminal"` when a terminal is selected.
  4. `"sidebar"` when visible.
- **Right pane rendered** when it is terminal or panel, or when it is the sidebar and `wide` (`:325`).
  - Width: fullscreen → `availableWidth` (= `termW - verticalTabsWidth`); sidebar → `42`; otherwise → `paneResize.size()` (`:329-331`).
- **Sidebar when not wide:** a full-frame overlay dimmed with `RGBA.fromInts(0,0,0,70)`, the sidebar right-aligned (`:385-397`).
  - The Session route adds a priority-10 keymap layer `escape,ctrl+c` → "Close sidebar" while `sidebarVisible && termW - vTabs <= 120` (`src/routes/session/index.tsx:250-254`).
- **Toggling the sidebar** persists `config.session.sidebar = visible ? "hide" : "auto"` and closes any panel or terminal (`:118-130`).
- **Terminal/panel width:**
  - Default `max(1, floor(panels.width/2))`.
  - Persisted as `layout.paneWidth`, falling back to the legacy `layout.terminalWidth`.
  - Clamp `clampSessionPaneWidth`: `max(min(24, half), min(w, max(half, total - 44)))` (`src/ui/layout.ts:19-23`).
  - Drag uses `fromMouse = termW - x - 1`.
  - Handle at `left = availableWidth - size - 1`, shown when not fullscreen and `availableWidth >= 3` (`:382-384`).
- **Panel presentation:**
  - `canSplit = panels.width > 80`. Otherwise the panel is forced to `"fullscreen"` (`src/context/panel.tsx:14-15`).
  - Fullscreen hides the session pane (`visible=false`, absolute) and the tabs.
  - **PanelHost** (`src/component/panel-host.tsx:20-63`): `#session-panel`, `flexGrow 1`, focusable. Bg is `theme.background.raised.base` in panel mode and `theme.background.base` when fullscreen. It renders `<Slot "session.panel">` with `PanelInput{name, sessionID, width, presentation, focused, focus, close, toggleFullscreen}`.
- **Focus:**
  - `activePane` is `"session"` or `"right"`.
  - While right is active, a transparent overlay covers the session pane. A click there refocuses the session. Wheel events inside the transcript viewport are forwarded (`:300-323`).
  - Commands `pane.focus.left` ("Focus session pane") and `pane.focus.right` ("Focus right pane") are active when a terminal or panel is shown and no dialog is open (`:164-180`).
- **Palette commands** (`:183-259`):
  - `session.sidebar.toggle`: "Hide sidebar" / "Show sidebar".
  - When terminals are available: `terminal.toggle` ("Hide terminal pane" / "Show terminal pane"), `terminal.select` ("Select terminal"), `terminal.close` ("Close terminal pane"), `session.terminal` ("New terminal", slash `/terminal`).
  - Errors show the toast `"Unable to load terminal"` (`:35`).

#### TerminalPane (`src/component/terminal-pane.tsx:281-330`)

- Box: `flexGrow 1`, `overflow hidden`, bg `themes.currentTokens().background.raised.base`.
- The embedded terminal is absolute at `left=1`, `top=0`. The PTY size is `cols = width - 2`, `rows = height`.
- Failure texts, in `theme.text.feedback.error.base`:
  - `"Persistent terminal server is out of date; restart OpenCode"` (`:243`)
  - `"Terminal connection failed"` (`:267`)
  - `"Terminal disconnected"` (`:275`)

### 1.10 Session route outer frame (`src/routes/session/index.tsx:1394-1546`)

There is **no header bar**. The session title is shown in the sidebar, the tab strip and the terminal title.

```
row flexGrow 1
└ column flexGrow 1, paddingBottom 1, paddingLeft/Right = termW<44 ? 1 : 2
  ├ transcript (flexGrow 1, relative): scrollbox stickyScroll, stickyStart="bottom"
  │    scrollbar only if config.session.scrollbar (default false); then viewport paddingRight 1,
  │    scrollbar paddingLeft 1, track bg theme.decrease(background.raised.base), thumb theme.border.base
  ├ status line: height 1, row, justify flex-end
  │    "Loading session history…" (text.feedback.info.base)  OR
  │    "Jump to latest ↓" (text.action.secondary.base / .hovered), paddingLeft 1, click → bottom
  └ bottom dock (flexShrink 0):
       QueuedPromptDock (if queued & composer closed)
       <Slot "session.composer.top">
       Composer (subagents/terminals drawer)
       one of: PermissionPrompt | FormPrompt | SessionLocationMissing | Prompt
```

- `contentWidth = (width ?? termW - vTabs) - 4` (`:255`).
- `SessionLocationMissing` (`src/routes/session/location-missing.tsx:59-81`):
  - Renders a `SessionQuestion` titled `"Session location unavailable"`, group `"Session recovery"`, `choicesLabel "Recovery actions"`, option `move: "Choose directory"`.
  - Body (`paddingLeft 1`, `gap 1`): the muted path (`truncateMiddle(abbreviateHome(dir), 72)`), then `"Choose another directory to continue this session."`.
  - Choosing an option opens `DialogWorkspaces`.
  - Failure toasts: `"Creating workspace failed"` and `"Failed to move session"`.
- Transcript, composer and prompt internals are covered in the other sections.

### 1.11 Sidebar (`src/routes/session/sidebar.tsx`)

```
┌ width 42, height 100%, bg theme.background.raised.base, padding T/B 1, L/R 2 ┐
│ <Session title>  (title_shimmer, bold, text.base; box paddingRight 2, pb 1)  │
│ ┌ scrollbox flexGrow 1 (vertical bar absolute right 0, width 1) ───────────┐ │
│ │  content column gap 1, paddingRight 1:  <Slot "sidebar.content">           │ │
│ │    Context block   (opencode.sidebar.context)                              │ │
│ │    MCP block       (opencode.sidebar.mcp)                                  │ │
│ └────────────────────────────────────────────────────────────────────────────┘ │
│ footer column gap 1, paddingTop 1: <Slot "sidebar.footer">                    │
│    Getting started card (conditional)                                         │
│    ~/path/to/dir:branch  (FilePath maxWidth 38)                               │
└───────────────────────────────────────────────────────────────────────────────┘
```

- **Title:** `withTimestampedFallback(session)`, i.e. the title, or `"New session - <ISO>"` / `"Child session - <ISO>"`.
  - When pending and animations are off, the attribute is DIM instead of BOLD (`:33-48`).
  - Scrollbar track: bg `raised.base`, fg `theme.scrollbar.base`. The horizontal bar is hidden (`:50-68`).
- **Context block** (`src/feature-plugins/sidebar/context.tsx:20-41`), shown only when there is usage or `cost > 0`:
  - Line 1: bold `Context` (`text.base`).
  - Then, in muted: `` `${tokens.toLocaleString()} tokens` ``, `` `${percent}% used` `` (if known), and `` `${$X.XX} spent` `` (Intl USD).
- **MCP block** (`src/feature-plugins/sidebar/mcp.tsx:23-91`), shown only when there are servers:
  - **Header row:** `gap 1`.
    - `▼`/`▶` appears only when there are more than 2 servers. Clicking it toggles the persisted `view.open` state.
    - Then bold `MCP`.
    - When collapsed, a muted `` ` (${on} active${bad>0 ? `, ${bad} error${bad>1?"s":""}` : ""})` ``.
  - **Item rows:** `"•"`, then the bold name (truncated, no wrap), then the status.
    - The dot is coloured success / error / muted (disabled) / warning (needs_auth) / muted.
    - Status text: `Connected` | `Connecting` | `Error` (error colour) | `Disabled` | `Sign in` | raw status.
    - Click → `DialogMcp(initialServer)`.
- **Footer** (`src/feature-plugins/sidebar/footer.tsx`):
  - **Getting started card:** shown when `termH >= 22`, not dismissed, and no connected provider (`:18-23`).
    - Box: bg `theme.background.raised.high`, padding 1/2, row `gap 1`.
    - Left glyph: `⬖`.
    - Column (`gap 1`): bold `Getting started` with a muted `✕` dismiss (persisted); muted `"OpenCode includes free models so you can start immediately."`; muted `"Connect from 75+ providers to use other models, including Claude, GPT, Gemini etc"`; then a row `"Connect provider"` … muted `"/connect"`. Click → `provider.connect` (`:25-74`).
  - **Location:**
    - Text: `format.path(directory)` plus `:${branch}` when on a VCS branch.
    - Rendered with `FilePath maxWidth 38`, which middle-elides as `root/…/basename`.
    - Muted; `text.base` on hover.
    - Click opens a `DialogSelect` titled "Working directory" with `Copy path`, `Open folder` and `Workspaces` (`:87-111`, `src/ui/working-directory-actions.tsx:16-56`).

### 1.12 Overlays and full-screen states

| Component | Trigger / timing | Placement | Content and colours |
|---|---|---|---|
| **StartupLoading** (`src/component/startup-loading.tsx`) | Not rendered with `OPENCODE_FAST_BOOT` (`src/app.tsx:1392`). Appears **500 ms** after mount while `plugins.ready()` is false. Once shown, it stays until **3000 ms** after it appeared, even after ready (`:13-47`). If plugins load within 500 ms it never appears. | absolute `left 0 right 0 bottom 1`, `zIndex 5000`, centered | inner box bg `theme.background.raised.base`, `paddingX 1`; `Spinner(color text.muted)`: `"Loading plugins…"`, or `"Finishing startup…"` once ready |
| **Reconnecting** (`src/component/reconnecting.tsx`) | When the connection is not `connected`: after **1000 ms** if `reconnecting`, else after **5000 ms**. Hides immediately on connect (`src/app.tsx:1297-1319`). | absolute full-screen, `zIndex 10000`, bg `RGBA.fromInts(0,0,0,150)`, centered | card `width 48 maxWidth "90%"`, bg `raised.base`, padding 1/2, `gap 1`. Line 1: `Spinner(text.base)` `"Restarting service…"` (managed) or `"Connection lost…"`. Line 2 (muted): `"Your session will resume automatically."` or `"Reconnecting to the server automatically."` |
| **MigrationOverlay** (`src/component/migration-overlay.tsx`) | Waits 1 s, then polls `migration.v1.status` every 1 s while it is `running` (`:18-38`) | absolute `top 1 right 2`, `zIndex 10000` | row; bg `theme.background.raised.high`; left border `┃` (`SplitBorder`) in `theme.text.feedback.info.base`; padding 1/2; `Spinner(info)`: `label` + `" n"` or `" n/d"`. On error: toast error `"Data migration failed"`, duration 10000 |
| **ErrorComponent** (`src/component/error-component.tsx`) | Root `ErrorBoundary` fallback | full screen | See below |
| **PluginRouteMissing** (`src/component/plugin-route-missing.tsx`) | Plugin route with no registered page | `100%×100%`, column centered, `gap 1` | `"Unknown plugin route: {id}/{name}"` in `text.feedback.warning.base`; a button box (bg `background.action.primary.hovered`, `paddingX 1`) with `"go home"` in `text.action.primary.hovered`, click → Home |

#### ErrorComponent detail

The theme may be the thing that crashed, so the colours are hard-coded (`:19-42`):

| Key | Dark | Light |
|---|---|---|
| bg | `#0a0a0a` | `#ffffff` |
| element | `#1e1e1e` | `#f5f5f5` |
| borderSubtle | `#3c3c3c` | `#d4d4d4` |
| text | `#eeeeee` | `#1a1a1a` |
| muted | `#808080` | `#8a8a8a` |
| primary | `#fab283` | `#3b7dd8` |
| onPrimary | `#0a0a0a` | `#ffffff` |
| error | `#e06c75` | `#d1383d` |
| success | `#7fd88f` | `#3d9a57` |

The content column is `width = min(84, max(24, termW-4))`, `paddingY 1`, `gap 1` (`:104`, `:116`). Top to bottom:

1. **Headline:** bold `"OpenCode crashed"`. When `termH >= 18`, the muted line `"An unexpected error stopped the session."` follows.
2. **Error panel:** rounded border (`borderStyle="rounded"`) in the error colour, title `" Error "`, `paddingX 2`. It contains the message, or `"An unknown error occurred."`.
3. **Buttons:**
   - Layout: a wrapping row with `gap 2`, `rowGap 1`, centered.
   - Each button: `minWidth 15`, `paddingX 2`, bold, with its key letter below it.
   - `"Copy report"` / `"✓ Copied"` / `"Copy failed"` (`c`), `"Restart"` (`r`), `"Quit"` (`q`).
   - Selected: bg primary, fg onPrimary. Others: bg element. The copy result paints success or error.
4. **Stack box:**
   - Rounded border in borderSubtle, title `" Stack trace "`, bottom-right title `" ↑↓ scroll "`.
   - `flexGrow 1`, `minHeight 3`, `paddingX 1`, with a scrollbox inside.
5. **Footer**, when `termH >= 20`:
   - Line 1, by copy state:
     - Idle: `"Copy the report and open a GitHub issue to help us fix this."`
     - Copied: `"Report copied — paste it into a new GitHub issue."`
     - Failed: `"Clipboard write failed. Try again or report the crash manually."`
   - Line 2: `` `OpenCode ${version}` ``.

Keys (`:69-101`):

| Key | Action |
|---|---|
| ctrl+c, `q` | quit |
| enter | activate the selected button |
| ←/→, tab/shift+tab | move the selection |
| ↑/↓/PgUp/PgDn/Home/End | scroll the stack |
| `c` | copy |
| `r` | reset |

The issue URL is `https://github.com/anomalyco/opencode/issues/new?template=bug-report.yml`, with the stack trimmed so the URL stays at most 6000 chars (`:221-258`).

### 1.13 Toasts (`src/ui/toast.tsx`)

- **Options:** `{ title?, message, variant: "info"|"success"|"warning"|"error", duration = 5000, action?: {label, run} }` (`:8-18`, `:160`).
- **Placement:** absolute `top 1`, `right 2` within the root box. `maxWidth = min(60, termW - 6)`. `justifyContent center`, `alignItems flex-start` (`:47-56`).
- **Frame:** `border ["left","right"]` using `SplitBorder` chars (`┃`), coloured `theme.text.feedback[variant].base` (`:54-56`).
- **Inner box:** `width 100%`, padding L/R 2 and T/B 1, bg `theme.background.raised.high` (`:64-71`).
- **Content:**
  - With a title: row = bold title (`text.base`) + spacer + affordance, `marginBottom 1`; then the message (`text.base`, word wrap) (`:83-92`).
  - Without a title: row = message (flexGrow, word wrap) + affordance (`:75-80`).
  - **Affordance:** `marginLeft 2`, no wrap. It reads `` `› ${action.label}` `` when there is an action, otherwise `"x"`. Colour is `text.muted`, or `theme.text.action.primary.base` on hover. The action label is bold on hover (`:34-44`).
  - **Queue count:** `` `+${pending} more` `` in muted, `marginTop 1`, shown when toasts are queued (`:94-98`).

```
                                         ┃                                      ┃
                                         ┃  Title (bold)                    x   ┃
                                         ┃                                      ┃
                                         ┃  Message text wraps by word …        ┃
                                         ┃  +2 more                             ┃
                                         ┃                                      ┃
```

**Queue semantics** (`:121-197`):

- `show()` **replaces** the current toast immediately and restarts the timer, unless a toast is showing and the toast is paused or the queue is non-empty. In that case the new toast is appended to the queue.
- When the timer ends, the next queued toast is shown.
- Hovering pauses the timer and keeps the remaining time. Mouse-out resumes it.
- Mouse-up activates the toast: dismiss, then run the action. This is skipped if text is selected (`:59-62`).
- `toast.error(err)` is shorthand for an error-variant toast with `errorMessage(err)` (`:168-170`).

**Producers in this section:**

- `ThemeErrorToast` (`src/component/theme-error-toast.tsx:9-17`): error, title `` `Failed to load theme: ${name}` ``, message = the error message.
- Plugin crashes: error, title `"Plugin"`, message `` `${id} crashed in ${where}: ${msg}` `` (`src/plugin/render.tsx:26-46`).
- Plugin toasts that carry a `sessionID` for a session other than the current one get the action `{label:"Open"}` and fall back to the session title (`src/plugin/api.tsx:108-131`).
- App commands:
  - `"Restarting service…"` (30000 ms) → `"Service restarted"` (success).
  - `"Reloading configuration…"` (30000 ms) → `"Configuration reloaded"` (`src/app.tsx:1009-1031`).

### 1.14 Spinners and text animations

| Primitive | Frames / timing | Source |
|---|---|---|
| `Spinner` | `SPINNER_FRAMES = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]` @ **80 ms**. Layout: row, `gap 1`, glyph box `flexShrink 0`, then label text in the same colour. The default colour is `theme.text.muted`. With animations off it shows `"⋯ " + label` (or `"⋯"`). With the `shimmer` prop it renders `ShimmerText` of `"${frame} ${label}"` and advances its own 80 ms frame counter. | `src/component/spinner-frames.ts:1`, `src/component/spinner.tsx:14-51` |
| Prompt running spinner | `<spinner interval={40}>` with `spinnerDef()` frames and colour; `[⋯]` when animations are off (prompt section) | `src/component/prompt/index.tsx:1888-1893` |
| `ShimmerText` | 1200 ms loop. A highlight front goes from `-4 + coast(t)*(end+22)`; per column, `intensityAt(col, front, 4, 18)` blends fg toward the `shimmer` colour | `src/component/shimmer-text.tsx:11-43` |
| `FadeInText` | **200 ms** one-shot left-to-right reveal from the backdrop colour, with an 8-cell feather. Strength per column = `1 - smootherstep(clamp((front - col)/8))`, where `front = -8 + coast(p)*(width+16)`. Disabled when `config.animations === false` | `src/component/fade-in-text.tsx:15-104` |
| `title_shimmer` | While a rename is pending, the text dims up to 60% toward the backdrop, except a moving bright band (1200 ms loop, head 4 / tail 18). It fades in/out over 240 ms. When the title arrives, a 450 ms wipe with a 4-cell feather crossfades old → new | `src/component/title-shimmer.tsx:19-23`, `:112-252` |
| Masked-text compositing | Text is drawn to a scratch buffer, then `colorMatrix` is applied with a matrix whose translation column is the target colour (backdrop or shimmer). Per-cell strength mixes fg toward that colour. Wide-glyph continuation cells reuse the head cell's strength | `src/component/masked-text.ts:11-49` |
| `createAnimatable` | A shared 16 ms `setInterval` ticker. `tween({duration (s), ease = smoothstep})` or `spring({visualDuration, restDelta .002, restSpeed .002})`. Values jump when animations are disabled | `src/ui/animation.ts:32-235` |
| `OneCellSpinner` / `WORK_SPINNERS` | 1-cell octant spinners (e.g. `block-soft-slide` @100 ms with alpha levels, `small-toggle "▫▪"` @320, `quadrant-orbit "▘▝▗▖"` @160, `density-wave "░░░▒▓█▓▒"` @160, `seed`). Still glyph `"▪"`. **Used only by mini mode and the storybook** | `src/ui/one-cell-motion.ts:77-111`, `src/component/one-cell-spinner.tsx` |
| Knight-rider scanner (`createFrames` / `createColors`) | width 8, diamonds `⬥◆⬩⬪` / `·` or blocks `■` / `⬝`, holdStart 30, holdEnd 9 | `src/ui/spinner.ts:272-368`. No usage found in the main TUI |

Global switch: `config.animations` (default `true`). The `app.toggle.animations` command flips it (`src/app.tsx:1149-1161`).

### 1.15 DevTools bar (`src/component/devtools-bar.tsx`)

- **Visibility:** shown when `config.debug.devtools ?? (app.channel === "local")`, but not on the `opencode.stats` plugin route (`src/app.tsx:458`, `1389-1391`).
- **Bar:** 1 row at the bottom of the root column, bg `theme.decrease(theme.background.base)` (`:232`).
- **Items:**
  - `BarItem`s with `paddingX 1`. Hover bg is `background.action.primary.hovered`; active bg is `background.action.primary.focused` with text `text.action.primary.focused`.
  - Order: `{✓|↻|×} Server`, `{○|⦿|●} UI`, `Theme`, `Tools`, `Experiments`, then a flexGrow `TimeToFirstDraw` (`:245-440`).
- **Popovers:** `PanelBox`, absolute `bottom 1`, `left -1`, `width 42`, padding 1/2, bg `raised.base`, with a bold title and label/value `Row`s.
- **Sampling:** every 2000 ms, kept for 30000 ms.
- **UI status:** `high` when the event-loop p99 is ≥100 ms within 6 s, `medium` when ≥20 ms (`:21-24`, `569-591`).

### 1.16 Prompt-area footer (brief; details in the prompt section)

The prompt footer row is `width 100%`, `justify space-between`, `gap 2`. It holds `<Slot "prompt.footer">`, which wraps `<Slot "prompt.footer.status">` (running spinner + interrupt status / move progress / location label) and `<Slot "prompt.footer.file">` (editor file label) (`src/component/prompt/index.tsx:1875-1958`).

The builtin `opencode.prompt.footer` appends to `prompt.footer` (`src/feature-plugins/prompt/footer.tsx`):

- **Normal mode:**
  - Live work: `"{shortcut} N subagent(s) · M shell(s)"`. Click → `session.child.first`.
  - Then usage `" · "`-joined: context usage and `$cost`, in muted.
  - Otherwise: `"{agent.cycle key} agents"`.
  - Plus `"{palette key} commands"`.
  - The fit test is against `max(0, termW-8)`, reserving `min(28, w/2)` (`:51-59`, `:131-141`).
- **Shell mode:** `esc` + muted `"exit shell mode"`, or `"shell"` when `termW < 44` (`:107-114`).

`opencode.btw` appends a `Spinner(theme.hue.interactive[200])` labelled `"/btw"` to `prompt.footer.status` while a side question is pending (`src/feature-plugins/prompt/btw.tsx:28-40`).

### 1.17 Plugin slot system

- **Slot map** (`packages/plugin/src/tui/context.ts:191-203`, outside `packages/tui`): `app`, `home.footer`, `home.footer.status`, `prompt.footer`, `prompt.footer.status`, `prompt.footer.file`, `session.composer.top`, `session.panel`, `sidebar.content`, `sidebar.footer`.
- **Path containment:** paths are dot-separated, and a path contains its prefixes. A `<Slot>` nested under a non-prefix parent throws (`src/plugin/render.tsx:94-97`).
- **Placement:** each claim has exactly one of `prepend | append | before | after | replace` (`src/plugin/api.tsx:48`, `:99-112`).
- **Render order** (`src/plugin/render.tsx:122-143`):
  1. `before` siblings.
  2. Inside the boundary: either a single `replace`, or `prepend` → host children → `append`.
  3. `after` siblings.
- **Errors:** each contribution is wrapped in a `PluginBoundary`; a crash gives one error toast and renders nothing (`:26-46`).
- **Plugin routes:** rendered through `PluginRoute`, keyed, with the fallback `PluginRouteMissing` (`:48-71`).
- **Mount sites:**
  - `app` (`src/app.tsx:1382`)
  - `home.footer` (`src/routes/home.tsx:107`)
  - `home.footer.status` (`src/feature-plugins/home/footer.tsx:95`)
  - `sidebar.content` and `sidebar.footer` (`src/routes/session/sidebar.tsx:70,75`)
  - `session.composer.top` (`src/routes/session/index.tsx:1478`)
  - `session.panel` (`src/component/panel-host.tsx:36`)
  - `prompt.footer*` (`src/component/prompt/index.tsx:1876-1958`)

#### Builtins (`src/plugin/builtins.ts:15-31`), in enable order

| Plugin id | Contributes |
|---|---|
| `opencode.home.footer` | append `home.footer` (§1.7) |
| `opencode.prompt.footer` | append `prompt.footer` (§1.16) |
| `opencode.btw` | append `prompt.footer.status` (spinner); append `app` (command `session.aside`, `/btw`) |
| `opencode.sidebar.context` | append `sidebar.content` (Context block) |
| `opencode.sidebar.mcp` | append `sidebar.content` (MCP block) |
| `opencode.sidebar.footer` | append `sidebar.footer` (onboarding + location) |
| `opencode.notifications` | no UI slot (attention notifications/sounds) |
| plugins manager | append `app` (command `/plugins`) (`src/feature-plugins/system/plugins.tsx:356`) |
| `opencode.stats` | route `stats` + `app` command `stats.open` (`/stats`) (`src/feature-plugins/system/stats.tsx:183-215`) |
| Merman, Latex | markdown code-block renderers (external packages) |
| Storybook | only when `OPENCODE_STORY` is set |
| `opencode.diffs` | route `diff` + `app` commands (`src/feature-plugins/system/diff-viewer.tsx:41`, `:1233-1241`) |

The sidebar order is Context then MCP, which follows the enable order of `append` claims at one anchor (`packages/plugin/src/tui/context.ts` doc comment). No builtin uses `session.panel` or `session.composer.top`. There is **no** `home_logo` slot: the logo is hard-coded in `Home`.

### Open questions (app shell)

1. **Alternate screen and screen defaults.** `createCliRenderer` is called without screen-mode or alt-screen flags. The defaults depend on the OpenTUI version (alternate screen, bracketed paste, focus reporting). The Rust port should confirm the OpenTUI defaults: `useAlternateScreen`, `useConsole`, and the mouse tracking mode (SGR any-motion?).
2. **`--continue` with no prior session.** The route stays `{type:"session", sessionID:"dummy"}`. What renders then (an empty SessionFrame with no `session()`?) is not obvious in this section's files.
3. **RGBA alpha compositing.** OpenTUI alpha compositing for `RGBA.fromInts(0,0,0,150)` (Reconnecting) and `(0,0,0,70)` (narrow sidebar) blends over existing cells. The Rust port needs an explicit blend: probably `out = cell*(1-a) + overlay*a` on both fg and bg. Verify against OpenTUI.
4. **`colorMatrix` semantics.** The masked-text explanation (a translation column of the target colour, blended by strength) is inferred from `matrix[3]`, `[7]`, `[11]` and `[15]`. Confirm with the OpenTUI `colorMatrix` implementation.
5. **Background-less tab strip.** The horizontal strip has no background, so the base colour comes from the route (`theme.background.base`). A tab's own background is a `tint(...)` with selection 0, so it equals `background.base` or the hovered colour.
6. **`theme.decrease(color, n)` and `background.raised.high.a`.** `raised.high` alpha is used in `tint(raised.base, raised.high, raised.high.a)`. The exact values are defined in the themes section.
7. **Close mark glyphs.** Tabs use `"✕"`, the vertical new-session close uses `"×"`, and toasts use `"x"`. These are inconsistent in source and should be replicated verbatim.
8. **Dialog dimming and z-order versus toasts.** The Toast is the last child of the root (rendered above routes). Dialogs (DialogProvider) are covered by the dialogs section; whether dialogs or toasts sit higher needs confirming there.
9. **Status-mode idle tabs.** In status mode (the default), idle horizontal tabs show a blank 2-cell indicator column. This is intentional per `TabIndicator.label()` but surprising; double-check against the running app.

---

## 2. Dialogs and modal prompts

Scope: the modal dialog host, the generic fuzzy select list, every concrete dialog, and the two inline "question" surfaces that replace the prompt input (permission prompt, form prompt). All paths are relative to `packages/tui`. Keybinding *labels* (e.g. how `ctrl+d` or `<leader>l` is printed) come from `Keymap.useShortcuts()`; see the keybindings section for formatting. Theme internals (token resolution, `theme.decrease`) are covered in the themes section.

### 2.0 Token shorthand used below

Dialog content renders inside `ThemeContextProvider context="dialog"` (`src/ui/dialog.tsx:33`), and most dialogs read `useTheme().surface("dialog")`. For v1 (legacy JSON) themes the `@dialog` surface only overrides `background.base = background.raised.base` and `background.action.primary.hovered = background.raised.high` (`../theme/src/tui/v1-migrate.ts:218-223`). Approximate legacy equivalents, from `../theme/src/tui/v1-migrate.ts:107-160`:

| v2 token | legacy v1 meaning |
|---|---|
| `text.base` / `text.muted` | `text` / `textMuted` |
| `background.raised.base` / `.high` / `.max` | `backgroundPanel` / `backgroundMenu` / one step brighter |
| dialog `background.base` | = `background.raised.base` (`backgroundPanel`) |
| `background.action.primary.focused` | `primary` (selected row / focused button bg) |
| `background.action.primary.base` | transparent |
| `text.action.primary.focused` | contrast fg computed over `primary` (`selectedForeground`) |
| `text.action.primary.disabled` | `textMuted` |
| `text.formfield.base` / `.focused` / `.selected` / `.disabled` | `text` / `primary` / `primary` / `textMuted` |
| `background.action.destructive.base` | `error` (no `focused` defined in v1 migration, so it presumably falls back to base) |
| `text.action.destructive.base` | contrast fg over `error` |
| `text.feedback.{error,warning,success}.base` | `error` / `warning` / `success` |
| `hue.accent[200]` | accent hue step 200 (used for category headers) |
| `markdown.link` | `markdownLink` |

Where a dialog mixes `useTheme()` (non-surface, here called *overlayTheme*/*contentTheme*), its `background.raised.high` is the *base* theme's value, not the dialog surface's.

---

### 2.1 Dialog host (`src/ui/dialog.tsx`)

**Sizes** (`src/ui/dialog.tsx:12-18`):

| size | width (cols) |
|---|---|
| `medium` (default) | 60 |
| `large` | 88 |
| `xlarge` | 116 |

Width is clamped with `maxWidth = terminalWidth - 2` (`src/ui/dialog.tsx:65`). The panel has **no border**. Its background is the dialog-surface `background.base` (`:66`), with `paddingTop = 1` (`:67`) and no other padding. Each dialog's content supplies its own side and bottom padding. Height is content-driven, with no explicit max at the host level.

**Backdrop** (`src/ui/dialog.tsx:34-55`):
- Full-screen absolute box at (0,0), size = terminal dims, `zIndex 3000`, color `RGBA.fromInts(0,0,0,150)`, which is black at alpha 150/255 ≈ 0.588 composited over the UI.
- `alignItems="center"` centers the panel horizontally.
- Vertical placement:
  - Default (not centered): `paddingTop = height / 4`. The value is not floored in source (`:51`); see Open questions.
  - Centered: `justifyContent="center"` with `paddingTop 0` (`:48-51`).
- Mouse:
  - Mouse-down on the backdrop records whether a text selection exists.
  - Mouse-up on the backdrop closes the dialog (`value.clear()`) unless that mouse-down had a selection (`:35-44`, `:243`).
  - Mouse-up inside the panel calls `stopPropagation` unless text is selected, so the panel never closes itself (`:57-63`).

**Stack model** (`src/ui/dialog.tsx:76-209`):
- `replace(element, onClose?, {key?, size?})` calls `onClose` on every current entry, then sets the stack to exactly one entry. It also resets `size` to `options.size ?? "medium"` and `centered=false` (`:170-189`).
- On the first open it remembers the focused renderable and blurs it (`:171-174`).
- `clear()` calls every `onClose` and resets size to medium, centered to false, and the stack to empty. It then refocuses the remembered renderable after 1 ms, if that renderable is still mounted (`:97-113`, `:159-169`).
- `setSize()` / `setCentered()` are called by individual dialogs, usually in `onMount` or the component body.
- Only `stack.at(-1)` is rendered (`:244`). In practice the stack depth is always ≤ 1, because "sub-dialogs" are done via `replace`.
- While the stack is non-empty, keymap mode `"modal"` is pushed (`:90-94`). Every dialog layer registers in `mode: "modal"`.

**Host keys** (`src/ui/dialog.tsx:115-156`):

| key | behavior |
|---|---|
| `escape` | If a text selection exists, clear it. Otherwise call the top entry's `onClose`, pop it, and refocus. |
| `ctrl+c` | Clear the selection if any. Else, if the focused editor has text, empty it (input or textarea). Otherwise close like `escape`. |

**Copy-on-select wrapper** (`src/ui/dialog.tsx:221-241`): the host is wrapped in an absolute `zIndex 3000` box.
- When `terminal.copy` is `"select"` (default except on win32), mouse-up runs `copyOnSelectRelease`.
- Otherwise, a right-click copies the current selection.

#### Common header pattern

Almost every non-list dialog uses the same header:

- A `box paddingLeft=2 paddingRight=2 gap=1` (some use `paddingBottom=1`).
- Its first child is a row `justifyContent="space-between"` holding the title (BOLD, `text.base`) and `esc` (`text.muted`, clickable to close).
- Example: `src/ui/dialog-alert.tsx:31-39`.
- `DialogSelect` uses the same header at padding **4** (`src/ui/dialog-select.tsx:674-684`).

```
 x=0 .. W-1   (dialog bg)                         padding 2 variant
 row0 (host paddingTop)
 row1   Title (bold)                         esc
 row2   (gap)
 row3   message text (muted, wraps)
 ...
```

---

### 2.2 Generic select list (`src/ui/dialog-select.tsx`)

#### Props that change appearance

From `src/ui/dialog-select.tsx:16-47`, option fields `:68-87`:
- `title` / `titleView`.
- `placeholder`, default `"Search"` (`:708`).
- `footer` (JSX shown in the footer row, left side).
- `emptyView` / `noMatchView`.
- `flat`: when the filter is non-empty, categories collapse into one list.
- `filterThreshold`.
- `skipFilter` / `search` (caller-side filtering).
- `renderFilter=false` hides the search input.
- `locked`: disables all interaction and hides the highlight.
- `actions` (keybound footer buttons) and `footerHints` (static `title label` hints).
- `bindings` (extra keys), `current` (shows the `●` marker), `focusTarget` / `focusCurrent`, `preserveSelection`, `sectionNavigation`.

Option fields:
- `title`, `titleView`, `description`, `searchText`, `searchFooter`, `details[]`, `footer`, `footerColor`.
- `titleWidth` (default 61), `truncateTitle` (`true` by default, `"left"`, or `false`).
- `category`, `disabled` (hidden entirely), `bg` / `fg` (highlight override), `gutter(color)`, `margin`, `onSelect(dialog)`.

#### Layout (column positions relative to the dialog panel, width W)

Outer box is `gap=1, paddingBottom=1` (`:673`). It has three children: header, list, footer.

| part | geometry | source |
|---|---|---|
| Title row | `paddingLeft 4 / paddingRight 4`. Title is BOLD `text.base` at x=4. `esc` (`text.muted`) is right-aligned, ending at x=W-5. Clicking `esc` calls `onCancel ?? dialog.clear`. | `:674-684` |
| Search input | Below the title with `paddingTop 1`, at x=4..W-5. Placeholder in `text.muted`. Focused bg `background.formfield.focused`, text and cursor `text.formfield.focused`, cursor style `config.cursor`. Autofocused after 1 ms. traits `{status:"FILTER"}`. | `:685-712` |
| List | `scrollbox paddingLeft 1 paddingRight 1`, scrollbar hidden, `maxHeight = height()`. | `:736-743` |
| Category header | `paddingLeft 3` → x=4. `hue.accent[200]` BOLD. `paddingTop 1` for every group except the first. | `:747-753` |
| Option row | Spans x=1..W-2. `flexDirection row, gap 1, paddingRight 3`. `paddingLeft` is **1** if the row is `current` or has a `gutter`, else **3**. | `:781-793` |
| Title text | `flexGrow 1`, `overflow hidden`, `wrapMode none`, **`paddingLeft 3`**. A normal row's title starts at x=1+3+3=**7**. A current or gutter row has `●`/gutter at x=2, a 1-col gap, then padding 3, so its title also starts at x=7. | `:885-898` |
| Description | Inline span `" " + description` after the title, in the same text node. Colored `text.muted`, or the active text color when the row is highlighted. Suppressed when it equals the category name. | `:808`, `:899-903` |
| Row footer | Right side, `flexShrink 0`, last char at x=W-5. | `:905-919` |
| `margin` | Absolute at `left 1` (x=2), shown only when not current. | `:794-798` |
| `details[]` lines | Each `paddingLeft 3`, `text.muted`, `wrapMode none`, middle-truncated to `max(1, min(76, termWidth-12))`. | `:816-824` |
| Footer row | `paddingLeft 4, paddingRight 2`, `space-between`. Left group: `props.footer` then left-side actions/hints. Right group: right-side actions/hints with `gap 2`. | `:835-845` |
| No footer | An empty `box flexShrink 0` is still rendered, so the outer gap still adds one blank row. | `:835` |

`dialogSelectContentWidth(w) = w - 12` (`:89-92`): the width available to a title, used by callers to pre-truncate.

**Height** (`:232-241`):
- `rows = Σ(1 + details.length)` over options, plus category headers. Each header counts 1 if first, else 2 (padding row included).
- `listHeight = min(rows, floor(termHeight/2) - 6)`.
- Total panel height = `1 (host pad) + 3 (title, pad, input) + 1 (gap) + listHeight + 1 (gap) + footerRows + 1 (bottom pad)`, i.e. `listHeight + 7 + footerRows`. When `renderFilter=false`, subtract 2.

Mockup, `medium` (W=60), Commands palette:

```
          1         2         3         4         5
0123456789012345678901234567890123456789012345678901234567890
                                                              <- host paddingTop
    Commands                                         esc      <- bold text.base / text.muted
                                                              
    Search                                                    <- input, placeholder muted
                                                              <- gap
    Suggested                                                 <- hue.accent[200] bold
 ██████Switch model ...........................ctrl+x m███    <- selected: bg action.primary.focused
       Open session                              ctrl+o       
                                                              <- paddingTop of 2nd category
    Session                                                   
       Compact session                                        
                                                              <- gap (footer box empty: palette has no actions)
                                                              <- paddingBottom
```

With actions (e.g. Sessions, `large`), the footer row reads `    pin/unpin ctrl+f  delete ctrl+d  rename ctrl+r ...  all projects ctrl+a  ` (left group gap 2; right group flush right at paddingRight 2).

#### Row color states

`Option` text color, from `src/ui/dialog-select.tsx:866-871`:

| state | title fg | bold | row bg (`:786-792`) |
|---|---|---|---|
| active (selected), no footer action focused | `option.fg ?? text.action.primary.focused` | yes | `option.bg ?? background.action.primary.focused` |
| active while a footer action is focused | `text.muted` | no | `background.raised.high` |
| current (not active) | `text.formfield.selected` | no | transparent |
| current while action focused | `text.muted` | no | transparent |
| normal | `text.base` | no | transparent |

- **Current marker:** `●` in the same color, shown when `current` and no `gutter` (`:875-879`). A `gutter(color)` receives that same color (`:880-884`).
- **Description color:** the active title color when active, else `text.muted`.
- **Footer color:** the active color when active; `text.muted` when muted and active/current; else `footerColor ?? text.muted` (`:907-914`).
- **Title truncation:**
  - Default: `Locale.truncate(title, titleWidth ?? 61)`, which keeps `len-1` chars and appends `…`.
  - `"left"`: `Locale.truncateLeft`, which prepends `…`.
  - `false`: no truncation (`:893-898`, `src/util/locale.ts:50-85`).

#### Filtering

- Disabled options are always removed (`:184-191`).
- With `search` set: the result is `search(query)`.
- With `skipFilter`, or `renderFilter === false`: options pass through unfiltered.
- Otherwise: `fuzzysort.go(lowercased needle, options, {keys:["title","category","searchText"], scoreFn: r => r[0].score*2 + r[1].score + r[2].score, threshold: filterThreshold})`. The title is weighted 2×. Results come back in fuzzysort score order (`:194-204`).

Grouping:
- Groups follow `groupBy(category ?? "")` in first-seen order; there is no sorting (`:214-223`).
- Flatten (a single unlabeled group) applies when `search` is set, or when `flat && filter.length > 0` (`:212`).
- When flattened, a row's footer shows `searchFooter ?? category ?? footer` (`:802-804`).

Empty states (`:715-734`):
- No groups and a non-empty filter: `noMatchView ?? "No results found"`.
- Otherwise: `emptyView ?? "No items available"`.
- Both defaults are at `paddingLeft 4`, `text.muted`.

#### Selection and scroll

- Typing a filter resets the selection to index 0 and centers it (`:342-359`).
- With `current` / `focusTarget` set and `focusCurrent !== false`, the selection jumps to the current item and centers it after layout (`:134-150`, `:353-357`).
- When `props.options` changes, the selection is kept by value, or clamped with `reconcileSelection` (`:245-305`, `src/ui/select-controller.ts:1-3`).
- `move(delta)` wraps around (`src/ui/select-controller.ts:5-12`). The scroll is not centered; it scrolls by the minimal amount to keep the row visible (`:388-419`). Scrolling up to the first item resets the scroll to 0.
- Mouse hover moves the selection; mouse-down selects; mouse-up submits (`:762-779`).
- Every `moveTo` calls `onMove(option)` (`:376-386`).
- `select-controller.ts` also exports windowing helpers for non-scrollbox lists. `moveSelectionOffset` keeps a margin of `min(2, floor((limit-1)/2))` rows (`src/ui/select-controller.ts:14-45`).

#### Keys

Defaults are from `src/config/keybind.ts:257-263`; the layer is at `:445-565`.

| command | default key | behavior |
|---|---|---|
| `dialog.select.prev` / `next` | `up,ctrl+p` / `down,ctrl+n` | move ±1, wrapping |
| `dialog.select.page_up` / `page_down` | `pageup` / `pagedown` | move ±10, wrapping |
| `dialog.select.home` / `end` | `home` / `end` | first / last |
| `dialog.select.submit` | `return` | Triggers the focused footer action if one is focused. Otherwise calls `option.onSelect(dialog)`, then `props.onSelect(option)`. |
| each visible action's `command` | from keybind config | `trigger(action)`. Requires a selection unless `selection:"none"`; skipped if disabled. |
| `tab` / `shift+tab` | only when actions exist | Cycle focus through *enabled* footer actions. Moving past either end returns focus to the list (`:434-443`). |
| `escape` | only if `onCancel` | Clears the selection, else calls `onCancel()` ("Back"). |
| `alt+up` / `alt+down` | only if `sectionNavigation` | Jump to the first item of the previous or next non-empty group (`:367-374`). |
| `props.bindings` | caller-defined | |

Changing the filter results clears action focus (`:207-210`). `locked` blocks movement, submit, input, and mouse.

#### Footer actions and hints

- An action is shown only if `!hidden` and it has a non-empty shortcut label (`:154-173`).
- Rendering is `title` + `" " + label`:
  - title color: `text.base` normally, `text.action.primary.disabled` when disabled, `text.action.primary.focused` + BOLD when tab-focused.
  - label color: `text.muted`, or the same disabled/focused color.
  - Focused bg: `background.action.primary.focused` (`:639-668`).
  - Mouse-up triggers the action.
- `footerHints` render as **bold** `title` (`text.base`) + `" "` + `label` (`text.muted`) (`:626-635`).
- **Responsive:** when `terminalWidth < 60`, the left footer group becomes a column with `gap 0`; otherwise it is a row with `gap 2` (`:837`).

---

### 2.3 Simple dialogs (`src/ui/`)

| dialog | title / text (verbatim) | buttons / hints | keys |
|---|---|---|---|
| **DialogAlert** (`src/ui/dialog-alert.tsx`) | `props.title`; message in `text.muted` with `paddingBottom 1` (`:40-42`) | Right-aligned `ok`: `paddingLeft/Right 3`, bg `background.action.primary.focused`, fg `text.action.primary.focused` (`:43-55`) | `return` calls `onConfirm`, then `clear` (`:16-29`) |
| **DialogConfirm** (`src/ui/dialog-confirm.tsx`) | `props.title`; message in muted | Two buttons in order `Cancel`, `Confirm`: `Locale.titlecase(label ?? key)`, padding 1 each side. Active: bg `action.primary.focused`, fg `text.action.primary.focused`. Inactive: no bg, `text.muted`. Default active is **confirm** (`:23-25`, `:71-89`). | `left` / `right` both toggle; `return` runs the active callback, then `clear` (`:27-57`) |
| **DialogPrompt** (`src/ui/dialog-prompt.tsx`) | `props.title`; optional `description()` element above the textarea | Hint row `"{key} submit"` (key in `text.base`, "submit" in muted); `"processing…"` while busy (`:134-142`) | `dialog.prompt.submit` (`return`) confirms with `textarea.plainText` (priority 1, target textarea). `escape` calls `onCancel` if provided ("Back"). `esc` click is ignored while busy (`:36-70`, `:104-111`) |
| **DialogHelp** (`src/ui/dialog-help.tsx`) | `Help`; right label `esc/enter`; body `Press {command.palette.show key} to see all available actions and commands in any context.` | `ok` button as in Alert | `return` / `escape` close (`:11-17`) |
| **DialogExportOptions** (`src/ui/dialog-export-options.tsx`) | `Export session` | see below | `tab` cycles; `return` activates (`:56-78`) |
| **DialogExportResult** (`src/ui/dialog-export-result.tsx`) | `Session exported`; path in `text.base` | `Close` button, padding 3, focused colors | `return` closes (`:15-25`) |

DialogPrompt details (`src/ui/dialog-prompt.tsx:113-133`):
- 1-row `textarea` with `wrapMode none` and `initialValue = props.value`; the cursor goes to line end on mount (`:79`).
- Placeholder defaults to `"Enter text"` (muted).
- Text color is `text.formfield.base`, or `text.formfield.disabled` when busy.
- Cursor color is `text.base`, or `background.formfield.disabled` when busy.
- Busy state: shows `Spinner` with `busyText ?? "Working…"` in muted, blurs the textarea, and sets traits `{suspend:true,status:"BUSY"}` (`:82-96`, `:130-132`).
- Calls `dialog.setSize(props.size ?? "medium")` on mount (`:73`).

DialogExportOptions layout (`src/ui/dialog-export-options.tsx:85-271`):

```
  Export session                                       esc
  
  Export as: [◉ Markdown] [○ JSON]
  
  [x] Include thinking          <- markdown only
  [x] Include tools             <- markdown only
  [ ] Sanitize sensitive data   <- json only
  
                                     [    Copy    ] [   Export   ]
```

- Format chips use `paddingLeft/Right 1`. Each control's bg/fg:
  - focused: `background/text.formfield.focused`
  - checked/selected: `formfield.selected`
  - otherwise: `formfield.base`
- Labels: `"◉"`/`"○"` + `Markdown`/`JSON`, and checkboxes `"[x]"`/`"[ ]"` (`:121`, `:153`, `:191`, `:231`).
- Defaults: format markdown; thinking = `defaultThinking` (session passes `true`, `src/routes/session/index.tsx:1232`); tools true; sanitize false; active = markdown.
- Tab order:
  - markdown: `markdown, json, thinking, tools, copy, export`
  - json: `markdown, json, sanitize, copy, export` (`:64-67`)
- Buttons are `paddingLeft/Right 4`:
  - `Copy` uses the base theme `background.raised.high` / `text.base` and has no focused state.
  - `Export` uses `background.action.primary.base`, or `.focused` when active (`:246-269`).
- `return` on a format chip selects that format; on a checkbox it toggles; on copy/export it confirms.
- No custom `escape`; the host esc resolves `null` (`:274-293`).
- After export the flow shows DialogExportResult with the tmp file path. Copy shows the toast `"Copied to clipboard"` (`src/routes/session/index.tsx:1243-1257`).

#### UI helpers

- **Working directory menu** (`src/ui/working-directory-actions.tsx:16-58`): a `DialogSelect` titled `Working directory` with `renderFilter={false}`. Options (title / description):
  - `Copy path` / `{directory}`: toast `"Path copied to clipboard"`.
  - `Open folder` / `in system file manager`.
  - Optional `Workspaces` / `to another working directory`.
  - It opens on mouse-up of the hovered element, unless text is selected.
- **Link** (`src/ui/link.tsx:18-34`): `<text><a href>` (OSC-8 hyperlink); mouse-up calls `openUrl`. `clickedLink` resolves a plain left click (not drag, no modifiers) to the URL under the cell (`:42-47`).
- **FilePath** (`src/ui/file-path.tsx`): `wrapMode none`, truncated. The parent part uses `fg`; the basename uses `basenameFg ?? fg` (`:23-29`). `truncateFilePath(value, maxWidth)` (`:31-76`):
  1. If the path fits, keep it.
  2. Detect a drive / UNC / posix root.
  3. With fewer than 2 segments: `root + truncateBasename`.
  4. Otherwise the prefix is `root + "…" + sep`. If the basename alone overflows, `truncateBasename` keeps the extension: `stem… + .ext`, or `"…" + tail` of the extension (`:78-89`).
  5. Else prepend parent segments from the right while they fit. A partially-fitting segment is cut as `takeStart(segment, avail-1) + "…"` when more than 1 col is available.

---

### 2.4 Concrete select-based dialogs (summary)

| dialog | title | size | notable options / footer | actions / extra keys | empty text |
|---|---|---|---|---|---|
| Command palette (`src/component/command-palette.tsx`) | `Commands` | medium | Option per palette command: title, description, category = group, footer = shortcut. Empty query: a `Suggested` category (copies of suggested commands) first. Query: all commands plus settings rows with `searchFooter` `Settings · {category}`. `flat`, `filterThreshold 0.7` (`:17-65`) | Selecting a command: `clear`, then `run`. Selecting a setting: opens DialogConfig at that setting (`:43-45`) | default |
| Model (`src/component/dialog-model.tsx`) | `Select model`, or the provider name | medium | see 2.5 | `model.dialog.provider` (`ctrl+a`), `model.dialog.favorite` (`ctrl+f`) | default |
| Agent (`src/component/dialog-agent.tsx`) | `Select agent` | medium | title = agent id, description = agent description; `current` = current agent (`:10-29`) | — | default |
| Variant (`src/component/dialog-variant.tsx`) | `Select variant` | medium | `Default` first, then variants except `"default"`; `current` = current or `"default"`; `flat` (`:10-38`) | — | default |
| Themes (`src/component/dialog-theme-list.tsx`) | `Themes` | medium | theme names sorted case-insensitively (`:8-13`) | live preview, see 2.5 | default |
| Settings (`src/component/dialog-config.tsx`) | `Settings` | medium | see 2.5 | `left` / `right` change value; hint `←/→ change` | default |
| Session list (`src/component/dialog-session-list.tsx`) | `Sessions` (+ ` for {project}`) | large | see 2.5 | pin, delete, rename, `ctrl+a` | see 2.5 |
| MCP (`src/component/dialog-mcp.tsx`) | `MCP servers` | medium | see 2.5 | `dialog.mcp.toggle` (`space`) | default |
| Skills (`src/component/dialog-skill.tsx`) | `Skills` | large (`:19`) | Titles `padEnd` to the longest name. Description has whitespace collapsed (`:41-54`) | — | `No skills available` / `Loading skills…` / error block / no match `No skills found` (`:62-90`) |
| Stash (`src/component/dialog-stash.tsx`) | `Stash` | medium | see 2.5 | `stash.delete` (`ctrl+d`) | default |
| Open (`src/component/dialog-open.tsx`) | `Open`, or `{project} / Worktrees` | large | see 2.5 | `right`, `left`, `ctrl+n`, `alt+up/down` | `No recent sessions or projects` |
| Integrations (`src/component/dialog-integration.tsx`) | `Connect an integration` | medium | see 2.6 | — | `No integrations available` / `No integrations found` (`:147-156`) |
| Worktrees (`src/component/dialog-workspaces.tsx`) | `Worktrees` (+ spinner) | xlarge | see 2.5 | move, new, delete, refresh | see 2.5 |
| Experiments (`src/component/dialog-experiments.tsx`) | `Experiments` | medium | footer `on`/`off`; `experiments = []` today, so it is always empty with no filter input (`:16`, `:55`) | `left` / `right` toggle; hint `←/→ change` when non-empty | `No experiments available` (`:58-62`) |
| Message actions (`src/routes/session/dialog-message.tsx`) | `Message Actions` | medium | see 2.7 | — | — |
| Timeline (`src/routes/session/dialog-timeline.tsx`) | `Timeline` | large | see 2.7 | — | default |
| Fork (`src/routes/session/dialog-fork.tsx`) | `Fork session` | large | see 2.7 | — | — |

### 2.5 Concrete dialog details

#### Model picker (`src/component/dialog-model.tsx`)

`connected()` means at least one provider is connected (`useConnected`). `showExtra = connected && !props.providerID` (`:27`).

- **Sections.** These apply only when `showExtra` and the query is empty (`:31`):
  - `Favorites` (connected only), then `Recent` (recent models that are not favorites).
  - Their rows have description = provider name (`:35-64`).
  - Then all non-deprecated models, with **category = provider name** when connected (none when disconnected). Models already shown in Favorites/Recent are excluded (`:66-103`).
  - Provider-section rows have description `(Favorite)` if favorited (`:80`).
  - Footer `Free` when every cost entry has `input === 0` (`:49`, `:82`, `:214-216`).
- **Sort** (`sortModelOptions`, `:185-212`):
  1. `opencode-go` first, then `opencode`.
  2. Provider name A→Z.
  3. Free first.
  4. Newer release first.
  5. Title A→Z.
- **Search.** `DialogSelect` runs with `skipFilter` + `flat`. The dialog itself runs `fuzzysort.go(needle, modelOptions, {keys:["title","category"]})`, re-sorts with the ungrouped sort (free, release, title), then stable-sorts favorites first (`:105-113`, `:176-183`). The fuzzysort rank is discarded. While flattened, the row footer shows the category (provider name) instead of `Free` (dialog-select flatten rule).
- `current` = current model with `focusCurrent={false}`: the `●` marks the current model, but the cursor starts at row 0 (`:170-171`).
- **Actions** (`:144-165`):
  - `ctrl+a`: title `Connect an integration` (connected) or `View all integrations`. Opens DialogIntegration; on connect it reopens DialogModel filtered to that provider.
  - `ctrl+f`: `Favorite` (hidden when not connected) toggles favorite.
- **On select:** set the model with `recent:true`. If the model has variants and the current variant isn't valid, replace with DialogVariant; else close (`:126-139`).

#### Theme list (`src/component/dialog-theme-list.tsx`)

- Live preview: `onMove` → `themes.set(value)` (`:28-30`).
- Typing → preview the first filtered match; an empty query → revert to the initial theme (`:39-47`).
- Enter confirms and closes (`:31-35`).
- Any other close reverts to the initial theme in `onCleanup` (`:19-21`).
- `current` = initial theme (`●`).

#### Settings (`src/component/dialog-config.tsx`)

- Options are grouped by `category`. The footer shows the current value via `format`, or `labels[index]`, or `String(value)` (`:347-361`).
- `current` = index of the `props.current` setting, default 0. The `●` marker stays on that row (`:328-331`, `:389`).
- Changing a value:
  - `left` = previous, `right` = next, `return` = next (`:391-407`).
  - Lists cycle; numeric settings step and clamp (`:363-383`).
  - Changes are ignored while saving. Each change writes config immediately.
- Footer hint renders as **`←/→`** `change` (`:393`).

Settings rows (title, category, values/labels), from `:21-318`:

| category | title | values (labels) | default |
|---|---|---|---|
| Appearance | Theme | all theme names | `opencode` |
| Appearance | Color mode | system, dark, light | system |
| Appearance | Animations | off/on | off |
| Session | Sidebar | hide, auto | auto |
| Session | Scrollbar | off/on | off |
| Session | Thinking | hide, show | hide |
| Session | Markdown | source, rendered | rendered |
| Session | Tool grouping | none, auto | auto |
| Session | Verbosity | low, medium, high | medium |
| Session | Transcript images | off/on | off |
| Session | TPS | off/on | on |
| Session | New session location | `launch directory`, `active session` | launch |
| Session | Permissions | `prompt`, `auto accept` | prompt |
| Tabs | Mode | off, on, auto | auto |
| Tabs | Scope | `current directory`, `global` | cwd |
| Tabs | Layout | horizontal, vertical | horizontal |
| Tabs | Indicators | `status icons`, `always show numbers` | status |
| Diffs | Layout | auto, split, unified | auto |
| Diffs | Wrapping | none, word | word |
| Diffs | File tree | off/on | on |
| Diffs | Single patch | off/on | off |
| Input | Scroll speed | step 0.25 in 0.25..10, `toFixed(2)` | 3 |
| Input | Acceleration | off/on | off |
| Input | Mouse | off/on | on |
| Input | Editor context | off/on | on |
| Input | Large pastes | compact, full | compact |
| Input | Image previews | off/on | off |
| Input | Leader timeout | step 250 in 250..10000, `"{v} ms"` | 2000 |
| Alerts | Notifications | off/on | off |
| Alerts | Sounds | off/on | off |
| Alerts | Volume | step 0.1 in 0..1, `"{round(v*100)}%"` | 0.4 |
| Terminal | Window title | off/on | on |
| Terminal | Copy behavior | manual, select | select (win32: manual) |
| Debug | Developer tools | off/on | off |

#### Session list (`src/component/dialog-session-list.tsx`)

- **Size and title:** `large` (`:194`). The title view is `Sessions` (bold `text.base`) + ` for {projectName}` (`text.muted`) when not in all-projects mode (`:199-208`).
- **Scope:** "all projects" vs "current directory" is a persisted pref, defaulting to all unless `tabs.scope === "cwd"` (`:42-45`). `ctrl+a` toggles it; the binding title is `Show current directory sessions` / `Show all project sessions` (`:216-227`).
- **Search:**
  - The server search is debounced 150 ms, `limit 50`, `order desc`, root sessions only (`:40`, `:51-83`).
  - The local list is filtered by substring of the title while results load (`:86-109`).
  - `skipFilter`: there is no fuzzy matching in the select itself.
- **Status / empty text** (`:110-121`), shown in `text.muted`, or `text.feedback.error.base` on error (`:228-241`):
  - `Searching sessions…` / `Loading sessions…`
  - `Could not search sessions. Change the search to try again.` / `Could not load sessions.`
  - `No sessions found` / `No sessions available`
- **Categories:**
  - `Pinned` (only when session tabs are disabled).
  - Otherwise by `new Date(updated).toDateString()`, with `Today` for today's date (e.g. `Mon Oct 12 2026`) (`:141-192`).
  - Order is server order (updated desc).
- **Row:**
  - Title = `session.title ?? "New session - {ISO}"` (`../util/src/session-title-fallback.ts:14-36`).
  - Footer = basename of the session's root, truncated to 25, only when it lies outside the project's canonical dir (`:153-160`).
  - `current` = the open session (`●`).
- **Gutter** (`sessionStatusGutter`, `:302-320`):
  - Attention: `!` (permission) or `?` (question), BOLD, in `text.feedback.warning.base`, or the active color when highlighted.
  - Else a running `Spinner` in the row color.
  - Else the quick-slot number in `hue.accent[200]` (only when tabs are disabled).
- **Actions** (`:247-287`):
  - `pin/unpin` (`session.pin.toggle` = `ctrl+f`, hidden when tabs are enabled).
  - `delete` (`ctrl+d`): the first press turns the row title into `Press {key} again to confirm` with bg `background.action.destructive.focused` and fg `text.action.destructive.focused`. The second press deletes. Any cursor move cancels. On failure, toast `Failed to delete session: {msg}`.
  - `rename` (`ctrl+r`) opens DialogSessionRename.
- **Footer hints** (`:288-291`):
  - Left: optional **`switch`** `<leader>1-9`, via `quickSwitchRange` (`:296-300`). Shown only when tabs are disabled and slots exist.
  - Right: **`all projects`** `ctrl+a`, or **`current directory`** `ctrl+a`.

**DialogSessionRename** (`src/component/dialog-session-rename.tsx`):
- DialogPrompt with title `Rename session` and placeholder `Session title`, prefilled.
- An empty trimmed value is ignored.
- Error toast: `Failed to rename session: {msg}`.

#### Status (`src/component/dialog-status.tsx`)

- Title `Status`.
- With no servers: `No MCP servers` (`text.base`).
- Otherwise the header `{n} MCP server` / `servers`, then one row per server:
  - `•` colored by status: success / error / warning / muted.
  - Then **name** (bold) + muted status text: `Connected` | `{error}` | `Disabled in configuration` | `Needs authentication: {error}` | raw status (`:29-57`).

#### MCP servers (`src/component/dialog-mcp.tsx`)

- Servers are sorted by name.
- Row footer element (`:20-34`):

  | status | footer text |
  |---|---|
  | pending / loading | `Connecting …` (muted) |
  | connected | `Connected ✓` (bold) |
  | failed | `Failed !` |
  | needs_auth | `Sign in required →` |
  | other | `Disabled ○` |

  `footerColor` is muted while pending, else success / error / warning / muted (`:57-81`).
- **Action** `space` (`dialog.mcp.toggle`): its title follows the focused server's status — `disconnect` / `retry` / `sign in` / `connect` (`:85-91`). It calls connect/disconnect and tracks a loading set (`:113-132`).
- **Footer:** `enter to view error` (muted) when the focused server has an error (`:155-159`).
- **Enter:**
  - needs_auth with an integration: opens DialogIntegration with `autoConnect`.
  - Has an error: swaps in DialogErrorDetails titled `MCP server: {name}` with context `Status: …\nConfiguration: mcp.servers.{name}[\nIntegration: …]`. Its esc returns to the list and resets size to medium (`:100-109`, `:163-175`).

#### Stash (`src/component/dialog-stash.tsx`)

- Rows are newest first (`:55`).
- Title = first line of the stashed text, trimmed and truncated to 50 (`:24-27`).
- Description = relative time: `just now`, `{m}m ago`, `{h}h ago`, `{d}d ago`, else `Locale.datetime` (`:9-22`).
- Footer `~{n} lines` when multi-line (`:52`).
- `ctrl+d` uses the double-press confirm with `Press {key} again to confirm` and destructive colors (`:45-49`, `:74-87`).
- Selecting removes the entry from the stash and restores it into the prompt.

#### Open (`src/component/dialog-open.tsx`, `open.menu` = `ctrl+o`)

- Size `large` (`:332`). Title `Open`, or `{projectName ?? "Project"} / Worktrees`.
- Placeholder `Search sessions and projects…` / `Search worktrees…` (`:338-339`).
- `sectionNavigation` is on (alt+up/down).
- **Sessions category `Sessions`:**
  - Empty query: up to `max(8, attentionCount)` recent root sessions. Sessions already open as tabs are excluded unless they need attention and aren't current (`:27`, `:197-207`). Attention sessions sort first, then by updated desc (`:160-175`).
  - Typing searches all sessions. `searchText` includes the session id and directory.
  - A query matching `^ses_[0-9A-Za-z]{26}$` fetches that session (`:141-151`).
  - Footer: `{label · }{timeAgo}`. The label is `"{project} · {dirBasename}"`, or the project/basename alone, truncated to 30 (`:209-226`).
  - `timeAgo`: `now`, `{m}m`, `{h}h`, `{d}d`, `{mo}mo`, `{y}y` (`:497-508`).
  - Gutter: attention / running as in the session list; otherwise `▪` in `hue.accent[200]` for open tabs (`:228-233`).
- **Projects category `Projects`:**
  - Unique canonical dirs except `/`.
  - Title = project name or basename.
  - Footer = home-abbreviated path truncated (`truncateFilePath`) to `dialogSelectContentWidth(min(88, termW-2)) - titleWidth - (git?2:0)`, plus `" →"` for git projects (`:237-271`).
  - Gutter `●` (`text.formfield.selected`) for the current project.
- **Keys:**
  - `right` (projects view, on a git project): opens the worktrees view.
  - `left`, or `esc` (via `onCancel`): back, restoring the previous filter and selection.
  - `ctrl+n` (worktrees view): new worktree. Footer hint **`new worktree`** `ctrl+n` (`:384-419`).
- **Worktrees view:**
  - The project root comes first, then the current directory, then the rest.
  - Footer = truncated path padded to a fixed width, so the columns align (`:276-322`).
- **Footer status** (`:360-383`):
  - Spinners `Loading worktrees…` / `Refreshing sessions and projects…`.
  - Error `Could not refresh sessions and projects.` / `… sessions.` / `… projects.` in `text.feedback.error.base`.
- **No-match text:** `Loading worktrees…` | `No matching worktrees` | `Searching sessions and projects…` | `No matches · search all sessions with {session.list key}` | `No matches` (`:420-434`).
- **New worktree:** swaps to DialogPrompt with:
  - Size large, title `{project} / New worktree`.
  - Placeholder `Worktree name (optional)`, description `Leave blank for a random name.` (muted).
  - Busy text `Creating worktree…`. Error toast title `Creating worktree failed` (`:450-481`).

#### Worktrees / move session (`src/component/dialog-workspaces.tsx`)

- Size `xlarge` (`:55`). Outer `minHeight = 5` on error, else `max(8, min(16, termH - floor(termH/4) - 2))` (`:320-325`).
- Title view: `Worktrees` (bold) + `Spinner` while working or loading (`:328-337`). `flat`.
- **Rows:**
  - Title = home-abbreviated path, with `titleWidth = dialogSelectContentWidth(min(116, termW-2))` and `truncateTitle:"left"`.
  - Subdirectory rows show the root part normally and the `/{relative}` suffix in `text.muted` (`:165-199`).
  - Categories: `Current` (root containing the current dir) / `Other`. Roots sort current first, then non-worktree roots by length, then worktrees.
- **Delete** (`ctrl+d`):
  - Disabled for subdirectories and non-worktree roots.
  - The first press changes the title to `Press {key} again to confirm` in `text.action.destructive.base`, with bg `background.action.destructive.base`.
  - While removing, the row reads `Deleting {path}` in `text.feedback.error.base` (`:177-189`).
  - If the server demands force, it shows DialogWorkspaceFileChanges with title `Delete worktree?` and message `This worktree has file changes. Do you want to delete it anyway?` (`:256-292`). Error toast title `Failed to delete worktree`.
- **Actions:**
  - `move` (`ctrl+m`, only on the session route).
  - `new` (`ctrl+a`) → DialogWorktreeName.
  - `delete` (`ctrl+d`).
  - `refresh` (`ctrl+r`) (`:371-401`).
- **Empty states:**
  - Error: bold `Could not load worktrees` + message + `Close and reopen Worktrees to try again.`
  - `Loading worktrees…`, `No worktrees available`; no match `No worktrees found` (`:341-364`).

**DialogWorktreeName** (`src/component/dialog-worktree-name.tsx`):
- Title `Name worktree`.
- `input` with placeholder `Worktree name`, colored `text.formfield.base`.
- Hints: `enter` **submit**, `{tab} generate one` (`:49-80`).
- `tab` (`dialog.worktree.generate`) fills a random slug.
- Enter submits a slugified value (lowercase, `[^a-z0-9]+`→`-`, edges trimmed), or a random slug when empty (`:17-24`, `:92-99`).

**DialogWorkspaceFileChanges** (`src/component/dialog-workspace-file-changes.tsx`):
- Title `File Changes Found` (default); message `Do you want to move these changes with the session?` (muted, wraps) (`:76`, `:84`).
- File list:
  - Scrollbox height `min(files, 8)`, bg base-theme `background.raised.high` (`:40`, `:87-114`).
  - Each row has a 2-col status `A`/`D`/`M` (muted), then a FilePath of width `max(2, min(60, termW-2) - 6 - max(7, changeWidths))`.
  - Right side: `+N` in `diff.text.added` and ` -N` in `diff.text.removed` (`:93-111`).
- Buttons `no` / `yes`, `paddingLeft/Right 2`, default **yes** (`:39`, `:115-132`).
- Keys: `left` / `right` clamp (no wrap); `return` confirms (raw `useKeyboard`, `:50-70`).

### 2.6 Integration connect flow (`src/component/dialog-integration.tsx`)

**List** (`:115-158`):
- Sort order: MCP-sourced first; then priority `opencode-go, opencode, openai, github-copilot, anthropic, google`; then name, then id (`:40-66`).
- Categories: `MCP`, `Popular` (has a priority entry), `Services` (`:119-121`).
- Row description `Environment only` when there are no connect methods.
- Row footer:
  - `Sign in required →` in warning when the first connection has a status.
  - Otherwise `connectionSummary`: `"{n} connections"`, the credential label, or `$ENVNAME` (`:81-87`).
- Gutter `✓` in `text.feedback.success.base` when connected and not needing sign-in.
- Hidden (disabled) when there are no methods and no credentials.

Flow states:

| state | UI | text (verbatim) |
|---|---|---|
| Manage accounts (has credentials) | DialogSelect titled `{integration.name}`. Rows: `Add account` (if methods), then category `Connected accounts` (sorted by label). `current` = first credential (active). Footer `Sign in required →` (warning) for rows needing sign-in. | Actions `rename` (`ctrl+r`) → DialogPrompt `Rename account` / placeholder `Account name`. `delete` (`ctrl+d`) double-press `Press {key} again to confirm`; removing the last account shows toast `Disconnected {name}` and closes (`:161-276`). Selecting a non-active account activates it. |
| Choose method (>1) | DialogSelect `Connect {name}`. Methods are sorted with key methods last; title is the method label, with key methods defaulting to `API key` (`:278-296`). | |
| API key | DialogPrompt with title = method label or `Connect {name}` and placeholder `API key`. The description shows the error in `text.feedback.error.base` (`:537-572`). | |
| Command method | CommandView, `large`. Header title + `esc close`. An output box (base `background.raised.high`, padding 2/1) shows the trimmed output, then a muted status line. Polls every 500 ms (`:393-535`). | `Starting command…` → `Waiting for command to finish…`; failure toast is the message or `Authentication expired` |
| OAuth starting / external | OAuthView (padding 2) with title and muted message (`:796-841`). | `Starting authorization…` / `Connecting…` |
| OAuth auto (device/browser) | OAuthView with a Link (url, `markdown.link`), muted instructions, `Waiting for authorization…`, hints `o open` `c copy`. Polls every 500 ms (`:645-740`). | `o` opens the URL; on failure, toast `Could not open the browser. Copy the URL and continue manually.` `c` copies the first `XXXX-XXXX(X)` code found in the instructions, else the URL; toast `Copied to clipboard`. Expiry toast `Authorization expired`. |
| OAuth code | DialogPrompt with placeholder `Authorization code`; description is instructions + Link + error (`:742-794`). | |
| Method form (pre-connect questions) | Sequential dialogs per active field (`:843-1219`): boolean / options → DialogSelect (`Yes`/`No` order follows the default; `Skip` added if optional); text → DialogPrompt (validation error inline); multiselect → DialogSelect loop with `[x] label` / `[ ] label` rows, `Type your own answer`, and `Continue` (disabled with description = validation error); external → `Open link` / `Open link again` + `I finished` (disabled until opened), plus interstitial `Opening link…`. | |
| String choice with custom | StringChoiceField (`:920-1070`). Padding 4. Title = field label; optional muted description. Scrollbox height `min(count + describedRows, max(3, floor(termH/2)-6))`. Rows `"{n}. {label}"` (focused = `text.formfield.focused`, else `.base`) with indented muted descriptions. Then `"{n+1}."` + custom text or `Type your own answer`, then `"{n+2}. Skip"` if optional. Error line in red. | Hint `↑/↓ select · enter confirm · esc cancel`, or while editing `enter submit · esc back` |
| Connected | Toast `Connected {name}`. Then `onConnected(providerID)`, or close (`:1221-1243`). | |

Esc anywhere in the flow cancels via the host. Pending OAuth / command attempts are cancelled on unmount (`:493-501`, `:720-728`, `:757-764`).

### 2.7 Session-scoped and viewer dialogs

**DialogMessage** (`src/routes/session/dialog-message.tsx:24-88`): title `Message Actions`; options (title / description):

| option | description | effect |
|---|---|---|
| `Jump to` | `view message in session` | |
| `Revert` | `undo messages and file changes` | Stages a revert and restores the prompt from the user message. |
| `Copy` | `message text to clipboard` | |
| `Fork` | `create a new session` | User messages only → DialogFork. |

**DialogTimeline** (`src/routes/session/dialog-timeline.tsx`):
- Size large.
- One row per user message, newest first. Title = text with newlines → spaces; footer = `Locale.time(created)` (short time).
- `onMove` scrolls the transcript to the message. Select → DialogMessage (`:17-41`).

**DialogFork** (`src/routes/session/dialog-fork.tsx`):
- Size large.
- Rows: `Full session`, then user messages newest first, with a time footer.
- While forking, it shows only `Spinner` `Forking session…` (padding 2).
- When opened with a `messageID`, it forks immediately.
- Success toast `Forked session` (4 s). The new session opens with the prompt prefilled from that message (`:21-84`).

**DialogExecute** (`src/routes/session/dialog-execute.tsx`), size `xlarge`, centered (`:32-33`):
- **Header:** `execute` (bold, grows) · status · `esc`.
  - Status text: `Receiving code…` | `Running` | `Failed · {duration}` | `Completed · {duration}`.
  - Status color: `text.feedback.error.base` when failed, else muted (`:60-69`, `:96-104`).
- **Scrollbox:**
  - bg base-theme `background.raised.high`, inner padding 2/1.
  - Height = `min(max(3, floor(termH*0.7)-6), codeRows + outputRows + 5)`, with rows precomputed for char-wrap at width `min(116, termW-2) - 4 - digits - 1` (`:29`, `:51-59`, `:105-113`).
- **Sections:**
  - `Code` (muted bold), with code fallback `Waiting for code…`.
  - `Output` (muted bold), with output fallback `No output` / `Waiting for output…`.
  - Output that parses as JSON (optionally followed by `\n\nWarnings:` / `\n\nLogs:`) is syntax-highlighted.
  - Both code blocks share one left-aligned line-number gutter of width `digits`. Continuation rows are unnumbered (`:225-260`).
- **Hints:** `↑/↓ scroll`, `c copy code`, `o copy output` (each becomes `✓ copied` in success color), `esc back` (`:165-180`).
- **Keys:** `up` / `down` ±1, `pageup` / `pagedown` ± maxHeight, `home` (code), `end` (output), `c`, `o` (`:80-92`).

**DialogShellOutput** (`src/component/dialog-shell-output.tsx`), `xlarge`, centered (`:26-27`):
- Header: `Shell output` (bold, grows) · status · `esc`. Status is `Running` | `Timed out` | `Killed` | `Exited` | `Exited · code {n}` (`:83-88`).
- Then the command (muted, max 3 rows, word wrap). When truncated, `Earlier output omitted · showing recent output` (64 KiB tail, `:11`).
- Output scrollbox:
  - Height `max(3, floor(termH*0.6)-6)`, sticky bottom.
  - Text is ANSI-stripped. Fallback `Loading output…` or `No captured output. Output redirected to files is not shown here.` (`:119-133`).
- Errors (red): `Shell output is no longer available.` / `Unable to read shell output. Retrying…`.
- Hints `↑/↓ scroll`, `end follow`, `esc back`.
- Keys `up` / `down` / `pageup` / `pagedown` / `home` / `end` (`:90-100`). Polls every 1 s, or immediately while more pages remain.

**DialogImagePreview** (`src/component/dialog-image-preview.tsx`), `xlarge`, centered:
- Title `Image {i} of {n}`.
- Image `fit`, width 100%, height `max(3, termH-8)`.
- Bottom row: `← previous` · `{mention text ?? "Image {i}"}` (or `No preview` in error color) · `next →`. The arrows show only when there are ≥2 images.
- `left` / `right` wrap (`:13-71`).

**DialogErrorDetails** (`src/component/dialog-error-details.tsx`):
- Title (bold, truncated) + `esc`.
- Optional source as FilePath, at max width `min(dialogWidth, termW-2) - 4`.
- Error text in a scrollbox, max height 20, word wrap. Optional `Reference: {ref}`.
- Hints `i investigate`, `c copy details` (→ `✓ copied` in success color), `↑/↓ scroll` (`:73-133`).
- Keys:
  - `escape` = onBack.
  - `c` copies.
  - `i` opens home with an investigate prompt.
  - Arrows/page/home/end scroll; page = 20 (`:55-71`).

**DialogDebug** (`src/component/dialog-debug.tsx`), large:
- Title `Debug`.
- Rows `label.padEnd(10)` (muted) + value. Labels: `Version`, `Date`, `OS`, `Terminal`, `Session ID`, `Model`; values default to `n/a`.
- Footer: `Share this when reporting an issue.` (muted) and **`copy`** / **`✓ copied`** + muted `enter`.
- `return` copies; toast `Debug info copied to clipboard` (`:25-91`).

**DialogPair** (`src/component/dialog-pair.tsx`), large, centered:
- Title `Pair`.
- Text `Open a link to connect. Links work once and expire in {m} minutes.`, then `This device` + localhost link, then `Links` + all links.
- When the server is loopback-only: ``Run `opencode service set hostname 0.0.0.0` to access the service remotely.``
- QR code: `uqr.renderUnicodeCompact(url, {border:1})` in `text.base`.
- **Responsive:**
  - Width ≥ 96: a row layout with a 29-col text column and the QR right-aligned. Otherwise a column layout, centered.
  - Height < 36: content goes in a scrollbox of height `max(8, termH - floor(termH/4) - 6)` (`:39-116`).
- Loading text `Loading server information…`. Error: bold `Could not load server information` + message + `Close and reopen Pair to try again.`

**DialogUpdate** (`src/component/dialog-update.tsx`), centered:

| state | title | body |
|---|---|---|
| checking | `Update` | Spinner `Checking for updates…` |
| available | `Update available` | `An update is available. After installing, you'll be prompted to restart OpenCode.` |
| installing | `Updating OpenCode` | Spinner `Installing OpenCode {version}…` |
| installed | `Update` | `Update successful! A restart is required. Any active sessions will be resumed automatically.` |
| current | `Update` | `OpenCode is already up to date.` |
| unavailable | `Update` | message |
| failed | `Update available` | message in error color |
| check-failed | `Update` | message in error color |

- Source: titles `:91-97`, bodies `:103-139`.
- Buttons are padding 1, styled like Confirm:
  - `Skip`, plus `Update` (available) or `Restart` (installed).
  - No buttons while installing.
  - The default active button is the last one (`:44-65`).
- Keys: `return` runs the active button; `left` / `right` / `tab` / `shift+tab` **all advance +1 cyclically** (`:67-86`).

---

### 2.8 Permission prompt (`src/routes/session/permission.tsx`, `src/util/permission.ts`)

**Placement.** This is *not* an overlay. It replaces the prompt input at the bottom of the session view whenever the session has a pending permission. Permissions take precedence over forms; the first request is shown, keyed by id (`src/routes/session/index.tsx:1494-1512`). The keymap mode is `base`.

**Container** (`SessionQuestion`, `:407-622`):
- Bg `background.raised.base`.
- Left border only, `┃` (SplitBorder), in `background.action.primary.focused` (`:510-513`, `src/ui/border.ts:15-21`).
- Collapsed: `position relative`, **`maxHeight 15`**.
- Expanded: rendered in a `Portal` with `position absolute, top = -(termH)+1, bottom 1, left 2, right 2` (`:514-523`, `:617-621`).
- Body box: `gap 1, paddingLeft 1, paddingRight 3, paddingTop 1, paddingBottom 1`, flexGrow.
- Footer bar:
  - Bg `theme.decrease(background.raised.base)` (a slightly darker band), `paddingTop 1, paddingLeft 2, paddingRight 3, paddingBottom 1, gap 1`.
  - Wide: a row with `space-between`.
  - **Narrow (termW < 80)**: a column, left-aligned (`:428`, `:541-552`).

```
┃                                                                     
┃  △ Permission required                     <- △ warning, text.base   
┃    → Edit src/foo.ts                       <- icon muted, title base 
┃                                                                     
┃  <body: diff / lines / patterns / always-lines>                     
┃                                                                     
┃▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒ (decrease(raised.base))
┃▒ [Allow once] Always allow  Reject      ctrl+f fullscreen  ⇆ select  enter confirm ▒
┃▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
```

**Header** (`:200-215`):
- Row 1: `△` (`text.feedback.warning.base`) + `Permission required` (`text.base`).
- Row 2: `paddingLeft 2`, icon (muted) + title (base). It is omitted when the raw action is `"shell"`.

**Option buttons** (`:564-597`, labels from `src/util/permission.ts:162-168`):
- Labels: `Allow once`, `Always allow` (only when `request.save` is non-empty), `Reject` (`:232-240`).
- Buttons have padding 1. Selected: bg `background.action.primary.focused`, fg `text.action.primary.focused`. Otherwise: `background.action.primary.base` (transparent), fg `text.action.primary.base`.
- The first option is preselected. Mouse hover selects; click picks.

**Right-hand hints** (`:598-612`): `{ctrl+f} fullscreen` (or `minimize` when expanded), `⇆ select` (if more than 1 option), `enter confirm`.

**Keys** (`:440-496`):

| key | behavior |
|---|---|
| `left,h` / `right,l` | Cycle options, wrapping. |
| `return` | Pick the selected option. |
| `escape`, or `app.exit` (`ctrl+c,ctrl+d,<leader>q`) | If expanded, collapse. Otherwise choose the `escapeKey` = `reject`. |
| `permission.prompt.fullscreen` (`ctrl+f`) | Toggle expanded. |

**Reply** (`:243-257`):
- `once` → `"once"`; `always` → `"always"`.
- `reject` → `"reject"`, except in child (subagent) sessions, where it switches to the RejectPrompt stage first.

**Body per selected option:**
- When `Always allow` is highlighted, the body switches to `permissionAlwaysLines` (`:223-231`, `src/util/permission.ts:154-160`):
  - `This will always allow {action} for this project.`, when `save = ["*"]`.
  - Otherwise `This will always allow the following patterns for this project.` followed by `- {pattern}` lines.
  - The first line is muted; the rest are base.
- Otherwise the presentation body (`:168-198`), with icon/title/lines from `permissionPresentation` (`src/util/permission.ts:23-144`):

| canonical action | icon | title | body lines |
|---|---|---|---|
| edit | `→` | `Edit {path}` | Diff via `PatchDiff` in a 100%-height scrollbox. Split view when `diffs.view` is `split`, or `auto` with termW > 120, else unified. Line numbers, word wrap, diff theme colors (`:22-111`). Without a diff: raw patch as `code filetype=diff` in muted, or `No diff provided` (muted). |
| read / list | `→` | `Read {path}` / `List {path}` | `Path: {path}` (muted) |
| glob / grep | `✱` | `Glob "{pattern}"` / `Grep "{pattern}"` | `Pattern: {pattern}` (muted) |
| shell (`bash`) | `#` | `Shell command` (header hidden if the raw action is `shell`) | `$ {command}` in **text.base**, or `- {resource}` lines |
| subagent (`task`) | `#` | `{Titlecase agent ?? "general"} Subagent` | `◉ {description}` (base) |
| webfetch | `%` | `WebFetch {url}` | `URL: {url}` |
| websearch | `◈` | `Web Search via {provider} "{query}"`, or `Web Search` | `Query: {query}` |
| lsp | `→` | `LSP {op} {path}:{line}:{char}` | `Operation:`, `Path:`, `Position:` |
| external_directory | `←` | `Access external directory {dir}` (wildcard suffix stripped) | Muted `Patterns` label, then `- {resource}` lines in base (`:171-179`) |
| doom_loop | `⟳` | `Continue after repeated failures` | `This keeps the session running despite repeated failures.` |
| other | `⚙` | `Call tool {action}` | `Tool: {action}` |

Lines are `paddingLeft 1`. They are colored base for shell/subagent/task, otherwise muted (`:181-197`). Paths go through `usePathFormatter().format`.

**RejectPrompt** (`:272-405`):
- Same container, but the left border is in `text.feedback.error.base`.
- Header `△` (error) + `Reject permission`; sub-line `Tell OpenCode what to do differently` (muted).
- Footer band: a textarea (traits `REJECT`, focused when interactive), then hints `enter confirm`, `esc cancel`. It goes to a column layout when termW < 80.
- Keys:
  - `return` sends reject with the message (empty → no message).
  - `escape` returns to the options.
  - `app.exit` (ctrl+c) clears the text if non-empty, else returns.

### 2.9 Form / question prompt (`src/routes/session/form.tsx`, `src/util/form.ts`)

**Placement.** Inline, replacing the prompt like the permission prompt, and also on the home route (`src/routes/home.tsx:115`). It pushes keymap mode `"form"` (`:38`, `:536`). Drafts persist across remounts per form id (`:54-57`, `:211-228`).

**Container** (`:767-773`):
- Bg `background.raised.base`, left `┃` border in `background.action.primary.focused`.
- Body padding `1/3/1/1`, gap 1.
- Footer row `paddingLeft 2, paddingRight 3, paddingBottom 1`, `space-between`, with no band background (`:1125-1172`).

```
┃                                                         
┃  {form.title}                                 (muted)   
┃  {metadata.message}                           (base)    
┃  Language  Framework  Submit                  (tabs)    
┃                                                         
┃  Which language?                              (field description ?? label, base)
┃  1. TypeScript ✓                                         
┃  2. Rust                                                 
┃  3. Type your own answer                                 
┃                                                         
┃  ⇆ tab  ↑↓ select  enter confirm  esc dismiss      {error in red}
```

**Fields and tabs:**
- Visible fields are those whose `when` conditions hold against the answers given so far (`:97-110`).
- `single` = exactly one field, of type boolean or string-with-options. It has no tabs, and picking submits immediately (`:111-118`).
- Otherwise there is one tab per field plus a final `Submit` tab (`tabs = fields + 1`).
- **Tab strip vs counter.** The tab strip is used when `Σ(min(label,24)+3) + len("Submit")+3 ≤ termW - 8` (`:119-122`):
  - Tab strip: tabs at `paddingRight 2`, labels truncated to 24 with `…`. Current tab is `text.base` BOLD; hovered is `text.formfield.focused`; others are muted. `Submit` is styled the same (`:795-847`).
  - Otherwise a muted line `Field {i} of {n}` (or `Review`) · `{answered}/{n} completed` (`:783-794`).

**Field body** (`:880-1061`):
- Prompt line: `field.description ?? label` in base.
- **Textual fields** (string without options, number, integer):
  - A textarea indented 1, 1–6 rows, traits `ANSWER`.
  - Initial value: the existing answer, or `(none)`.
  - Placeholder:
    - field placeholder, or by format: `name@example.com`, `https://example.com`, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`;
    - numeric ranges: `{min}-{max}`, `at least {min}`, `at most {max}`;
    - else `Type your answer` (`:167-184`).
- **Choice rows:**
  - `"{n}."`: `text.formfield.focused` if active, else muted.
  - Multi only: a 4-col `[✓]` / `[ ]` (focused / selected / muted).
  - Label: focused or `formfield.base`.
  - Single only: ` ✓` in `formfield.selected` when picked.
  - Descriptions indented 3 (single) or 7 (multi), muted (`:913-974`).
  - Booleans render as `Yes`, `No` (`src/util/form.ts:49-53`).
- **Custom row** (when `custom`):
  - `"{n+1}."` + text or `Type your own answer`.
  - When editing, an inline textarea in `formfield.focused`. Typing any printable char while the custom row is selected starts editing (`:243-255`). Paste also targets it (`:345-379`).
- **External field:**
  - Title, description, the URL in `text.action.primary.base` (clickable), and a status line:
    - `✓ Acknowledged` (success)
    - `Complete the external action, then press enter to confirm.`
    - `Open or copy the URL, complete the external action, then confirm.` (`:849-878`)
  - An open failure sets the error `Could not open the browser. Copy the URL and continue manually.`; copy shows toast `Copied URL to clipboard`.
- **Review (Submit tab):**
  - A scrollbox with max height `max(3, termH-14)`.
  - One line per field: `{label≤40}:` (muted) + value.
  - Value colors: error color for invalid or missing; base when answered; muted otherwise.
  - Value text: `(required)`, `(not answered)`, `(none)` for an empty multiselect, or the validation message. External fields show `Acknowledged` / `(acknowledgement required)` (`:1063-1123`).

**Footer hints** (`:1134-1167`):
- `⇆ tab` (unless single).
- `↑↓ select` (choice fields, not editing).
- `↑↓ scroll` (review overflowing).
- `enter {actionLabel}`. The label (`:195-209`) is one of:
  - `submit` on review or single;
  - `continue` / `I finished` / `open link` on an external field;
  - `done` / `edit` / `toggle` on a multiselect;
  - else `confirm`.
- `c copy` (external).
- `esc close` while editing a custom answer, else `esc dismiss`.
- The error message is right-aligned in `text.feedback.error.base`.

**Keys:**

Choice mode (`:642-765`):

| key | behavior |
|---|---|
| `left` / `h`, `shift+tab` | previous tab (wrap) |
| `right` / `l`, `tab` | next tab (wrap) |
| `1`–`9` | select and activate row n (max = min(rows+custom, 9)) |
| `up` / `k`, `down` / `j` | move (wrap) |
| `return` | single: pick + submit; multi: toggle; custom row: start editing |
| `space` | toggle (multi only) |
| `escape`, `app.exit` | cancel (dismiss the form) |
| external field | `return` acknowledge / open; `c` copy link |
| review | `return` submit; `up/k`, `down/j` scroll; `escape` dismiss |

Edit / textual mode (priority 1, `:555-640`):

| key | behavior |
|---|---|
| `tab` / `shift+tab` | Commit the text, then move to the next / previous tab. On a validation error forward movement is blocked; backward still moves. |
| `return` | Textual: commit + next tab. Custom single: commit + next tab, or reply when single. Custom multi: commit only. |
| `up` | When on the first visual row, leave custom editing to the previous row. |
| `escape` | Textual: dismiss the form. Custom: close the editor. |
| `prompt.clear` (`ctrl+c`) | Clear the text if any. When already empty: textual dismisses the form; custom closes the editor. |
| `prompt.paste` (`ctrl+v`) | Paste from clipboard. |

**Validation messages** (`src/util/form.ts:72-110`, verbatim):
- `Answer required`
- `Select at least one option`
- `Expected text`
- `Must be at least {n} characters` / `Must be at most {n} characters`
- `Must match pattern: {p}` / `Invalid pattern: {p}`
- `Expected an email address`
- `Expected a URL`
- `Expected a date (YYYY-MM-DD)`
- `Expected a date and time`
- `Select an available option`
- `Expected a number`
- `Expected an integer`
- `Must be at least {n}` / `Must be at most {n}`
- `Expected yes or no`
- `Expected selections`
- `Select at least {n}` / `Select at most {n}`
- `Select only available options`

On submit with an unacknowledged external field: `External action must be acknowledged: {label}` (`:513-518`).

---

### Open questions (dialogs)

1. **Fractional top offset.** `paddingTop = dimensions().height / 4` is not floored (`src/ui/dialog.tsx:51`). The Rust port needs Yoga's rounding rule (likely `round`) to match the exact top row for odd heights.
2. **Text `paddingLeft` on option titles.** `paddingLeft={3}` on the `<text>` node (`src/ui/dialog-select.tsx:891`) puts titles at x=7 while category headers sit at x=4 and `details` at x=4. This is consistent with the `-12` comment in `dialogSelectContentWidth`, but it should be confirmed against a real screenshot.
3. **Escape precedence.** Both the host layer and DialogSelect / DialogPrompt bind `escape` in mode `modal`. DialogPrompt uses `priority:1`; DialogSelect's `onCancel` binding has no priority. Which layer wins on ties (likely last-registered) is defined in the keymap section and must be confirmed.
4. **Expanded permission geometry.** The expanded prompt is `Portal` + `position absolute top=-(termH)+1 bottom=1 left=2 right=2` (`src/routes/session/permission.tsx:514-516`). The effective on-screen rectangle depends on the Portal mount point and Yoga's handling of a negative `top`. The intended result appears to be "full screen minus 1 bottom row and 2 side cols", but this needs verification.
5. **Raw vs canonical action names.** `permission.tsx` checks the raw `request.action === "shell"` / `"edit"` / `"external_directory"`, while `permissionPresentation` canonicalizes `bash→shell`, `task→subagent`, `apply_patch→patch`. If servers send `bash`, the header title row is shown and lines are muted. `patch` has no presentation branch and falls into `Call tool apply_patch`. Confirm which names the v2 server emits.
6. **Update dialog key direction.** In DialogUpdate, `left` / `shift+tab` are titled "Previous" but advance +1, the same as `right` / `tab` (`src/component/dialog-update.tsx:76-84`). With ≤2 buttons this is invisible. Replicate as-is?
7. **Destructive focused token.** `background.action.destructive.focused` / `text.action.destructive.focused` are used for delete-confirm rows, but the v1 migration defines only `destructive.base`. The fallback resolution belongs to the themes section.
8. **Spinner glyphs/frames.** The `Spinner` component (used in prompt busy, open-dialog footers, gutters, the update dialog) and the `Spinner shimmer` variant are not covered here; see the shared components and animation section.
9. **QR rendering.** The `uqr` `renderUnicodeCompact` output (half-block characters) must be reproduced exactly for DialogPair, so a Rust QR crate with an equivalent compact half-block renderer and `border: 1` is needed.

---

## 3. Session view and message rendering

Scope: the session route body (transcript scrollbox, every row type, every tool renderer, grouping, scrolling/virtualization, the strip under the transcript, the composer tabs) plus the diff components. All paths are relative to `packages/tui`. Theme tokens use the v2 names from the source (`theme.text.muted`, `theme.text.feedback.error.base`, `theme.background.raised.base`, `theme.diff.*` …). `theme.decrease(c)` is a one-step-darker/quieter variant of `c` (`src/theme/component.ts:40`). The theme agent covers those definitions.

Cross-refs: app shell, header and sidebar → §1/§2. Prompt input → prompt section. Permission and form/question prompts that replace the prompt → dialogs section. Keybinding table → keybindings section.

---

### 3.1 Screen layout of the session route

```
┌ session route (row) ────────────────────────────────────────────────┐
│ pad L=2 (1 if term width<44)                     pad R=2 (1 if <44) │
│ ┌ scrollbox (flexGrow, stickyScroll bottom) ─────────────────────┐  │
│ │ <blank>               ← every row has marginTop=1              │  │
│ │ row … row …                                                    │  │
│ │ [BackgroundToolHint] [RevertMessage] [navigation slack]        │  │
│ └────────────────────────────────────────────────────────────────┘  │
│                                        Loading session history… /   │  ← 1-row strip, right-aligned
│                                                   Jump to latest ↓  │
│ [QueuedPromptDock]                                                  │
│ [plugin slot "session.composer.top"]                                │
│ [Composer (tabs) — when open]                                       │
│ Prompt | PermissionPrompt | FormPrompt | LocationMissing | nothing  │
│ pad bottom = 1                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

- Outer box: `paddingBottom={1}`, `paddingLeft/Right = width < 44 ? 1 : 2` (`src/routes/session/index.tsx:1395-1401`).
- Content width used by rows (`ctx.width`) is `(props.width ?? termWidth - verticalTabsWidth) - 4` (`src/routes/session/index.tsx:255`).
- Scrollbox (`src/routes/session/index.tsx:1404-1452`):
  - `stickyScroll={!navigationMessage() && !navigationSlack()}`, `stickyStart="bottom"`: follows new output while the view sits at the bottom.
  - Scrollbar: hidden by default (`config.session.scrollbar ?? false`, `:238`). When shown, it has `paddingLeft:1`, the viewport gets `paddingRight:1`, track bg is `theme.decrease(theme.background.raised.base)` and thumb fg is `theme.border.base` (`:1411-1421`). The command "Toggle session scrollbar" persists the setting (`:1055-1068`).
  - Scroll acceleration: `getScrollAcceleration(config)` (`src/util/scroll.ts:20-29`). Default is a fixed **3 lines per wheel tick** (`CustomSpeedScroll(3)`). `scroll.acceleration: true` uses macOS-style acceleration. `scroll.speed: N` gives a fixed N.
- Rows: `<For each={visibleRows()}>` → `SessionRowView`, a box with **`marginTop={1}`** and `flexShrink=0` (`src/routes/session/index.tsx:1564`). So there is exactly one blank line between rows.
- Strip under the transcript is a 1-row box with `justifyContent="flex-end"` (`:1454-1473`):
  - While a "first message" jump is loading history: `Loading session history…` in `theme.text.feedback.info.base` (`:1455-1457`).
  - Otherwise, when away from the bottom: `Jump to latest ↓` with `paddingLeft=1`, fg `theme.text.action.secondary.base` (`.hovered` on hover). Mouse-up scrolls to the bottom (`:1458-1472`).
  - In every other case the strip is a blank row. It is always 1 row tall.
- Bottom area, in order (`:1474-1543`):
  1. **QueuedPromptDock** when the composer is closed, nothing is prompting, and queued prompts exist.
  2. Plugin slot `session.composer.top`.
  3. **Composer** (§3.12). It is open when toggled, or always in a child session with no forms. `escape` in a child session navigates to the parent (`:1479-1493`).
  4. A `Switch` with these cases: composer open → nothing; pending permission → `PermissionPrompt`; form → `FormPrompt`; location missing → `SessionLocationMissing`; otherwise → `Prompt`. Submitting the prompt calls `toBottom()` (`:1494-1541`).

---

### 3.2 Row model (what becomes a row)

Rows are built by `reduceSessionRows` (`src/routes/session/rows.ts:343-411`) and then updated live by bus events (`rows.ts:261-319`). Row types (`src/routes/session/grouping/session.ts:14-34`):

| Row type | Source | Renderer |
|---|---|---|
| `message` | any non-assistant message (user, shell, agent/model/location switch, system, synthetic with non-empty description, skill, compaction, idle-with-error) | `SessionMessageView` (`index.tsx:1812-1845`) |
| `part` | one assistant content part (text/reasoning with non-blank text, or a tool). IDs are `text:<n>`, `reasoning:<n>`, or the tool id (`rows.ts:393-402`) | `SessionPartView` (`index.tsx:1856-1892`) |
| `assistant-footer` | after an assistant step whose `finish` ∉ {`tool-calls`,`unknown`}, or that has `error`, or `retry` (`rows.ts:403-406`). Live: added on `session.step.ended` (non tool-calls), `step.failed`, `retry.scheduled`; removed on `step.started` (`rows.ts:303-318`) | `AssistantFooter` (`index.tsx:1894-1944`) |
| `turn-usage` | debug only (`config.debug.turn_tokens`), one per turn (`rows.ts:358-372`) | `TurnTokenUsage` (`index.tsx:1618-1741`) |
| `compaction-queued` | pending inbox compaction | `CompactionQueued` (`index.tsx:2112-2124`) |
| `group` | adjacent entries sharing a grouping path (§3.8) | `SessionGroupView` (`group-view.tsx:33`) |

Filtering and ordering rules:
- Messages at or after `session.revert.messageID` are dropped (`rows.ts:64-66`). The RevertMessage banner appears at the bottom instead (§3.10).
- User prompts whose inbox delivery is `queue` are **removed from the transcript** and shown in the QueuedPromptDock (`rows.ts:60-63`).
- Pending inputs (user/synthetic still in the inbox, i.e. not yet delivered) and running compactions are moved to the **end** of the transcript (`rows.ts:349-376`). Live inserts put pending rows after everything else (`rows.ts:188-248`).
- Synthetic messages with a blank description are skipped (`rows.ts:383`).
- The `idle` marker only renders when it carries an error (`index.tsx:1840-1842`).

---

### 3.3 Common building blocks

#### 3.3.1 Left bar border
`SplitBorder.customBorderChars` draws every side empty except `vertical: "┃"` (U+2503, heavy vertical) (`src/ui/border.ts`). Used with `border={["left"]}`, it gives a 1-column `┃` bar on the left of a block. The bar color differs per block type (see each block).

#### 3.3.2 Spinner (`src/component/spinner.tsx`)
- Frames: `["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]` (`src/component/spinner-frames.ts:1`). Interval is **80 ms** (`spinner.tsx:36`).
- Layout: row, `gap=1`: `<glyph> <label>`. Default color is `theme.text.muted` (`spinner.tsx:17,34-41`).
- With `config.animations === false`, the spinner renders the static text `⋯ <label>` (or just `⋯`) (`spinner.tsx:27-29`).
- Shimmer variant: `<frame> <label>` with a shimmer gradient, frame advancing every 80 ms (`spinner.tsx:19-23,44-48`). It is not used in the transcript.

#### 3.3.3 InlineToolRow (`src/routes/session/message-parts.tsx:198-275`)
Single-line tool/notice row:
```
   ✓ label text that wraps under the label column …
   ^^ icon column, width INLINE_TOOL_ICON_WIDTH = 2  (message-parts.tsx:19)
^^^ paddingLeft = 3
```
- `spinner=true` → `<Spinner color>{children}</Spinner>` (the spinner glyph replaces the icon and the label follows). With a `status` element, the row is spinner + label + status badge, using `flexWrap` and `columnGap=1` (`:220-231,277-291`).
- Otherwise, if `complete || failed` → icon (width 2) + label (flexGrow). If not, the row shows the spinner with the **pending** text (e.g. `Reading file…`) (`:233`).
- `failed && !complete` → shows `failure ?? children` (`:250,260`).
- `denied` → icon and label get the STRIKETHROUGH attribute (`:238,248`).
- `failed && errorExpanded` → the error message appears below in `errorColor` with `paddingLeft=2` (aligned to the label) (`:268-272`).

#### 3.3.4 InlineTool wrapper (`src/routes/session/index.tsx:2645-2714`)
Color priority: explicit `color` → `theme.text.feedback.warning.base` when this tool has the **active permission request** (`useToolPermission`, `:2634-2643`) → `theme.text.feedback.error.base` when failed → `theme.text.base` on hover when clickable → `theme.text.muted` (`:2678-2684`).
- `error` is the tool error message when `status==="error"` and the tool is not `running` (`:2664-2666`).
- **Denied** (strikethrough, stays muted, not "failed") when the error contains any of `"QuestionRejectedError"`, `"rejected permission"`, `"specified a rule"`, `"user dismissed"` (`:2668-2674`).
- Click on a failed row toggles the inline error text (or calls `onErrorClick`). Otherwise it calls `onClick`. Clicks are ignored while text is selected (`:2701-2709`).

#### 3.3.5 BlockTool (`src/routes/session/index.tsx:2739-2821`)
Panel for tools with content:
```
┃                                   ← paddingTop 1
┃  # Wrote src/foo.ts               ← header (label + FilePath), gap 1
┃                                   ← gap=1 between children
┃  <content>
┃  <error text>                     ← theme.text.feedback.error.base (or errorColor)
┃                                   ← paddingBottom 1
```
- `border=["left"]` with the `┃` char. **borderColor = `theme.background.base`**, so the bar is a dark seam against the panel. Background is `theme.background.raised.base`, or `theme.decrease(raised)` on hover when clickable. `paddingTop=1`, `paddingBottom=1`, `paddingLeft=2`, `gap=1` (`:2750-2766`).
- Note: BlockTool has **no** `paddingLeft=3` indent. It starts at transcript column 0, like user messages.
- Header with `path`: row `gap=1`. The label is in `headerColor ?? theme.text.muted` (warning if a permission is pending). It is followed by `FilePath` truncated to `ctx.width - 4 - width(label) - (spinner?2:0)` (`:2790-2812`).
- If `spinner`, the label is wrapped in Spinner and a leading `"# "` is stripped (`.replace(/^# /,"")`) (`:2783,2804`). Otherwise the label is shown verbatim, **including the `# ` prefix**.
- `FilePath` truncation (`src/ui/file-path.tsx:31-76`) keeps the root and basename and inserts `…/`. If needed it truncates the basename middle while keeping the extension, e.g. `/…/dir/name….ts`. Parent and basename can have different colors (`:24-27`).

#### 3.3.6 StatusBadge (`index.tsx:2716-2725`)
`" Background "` (one space padding each side). bg `theme.decrease(raised|base)`, fg `theme.text.muted`.

---

### 3.4 User message (`src/routes/session/index.tsx:2227-2354`)

Renders only if text, files, or skills are non-empty (`:2253`).
```
┃                                  ← paddingTop 1
┃  Fix the bug in parser.ts        ← text, theme.text.base, wraps
┃                                  ← paddingTop 1 (only if chips)
┃   skill  review    file  src/a.ts    dir  src/  ← chips, gap 1, flexWrap
┃                                  ← paddingBottom 1
```
- Left bar `┃` in the **agent color** of the session's current agent (`local.agent.color(session.agent ?? "build")`, `:2242`). It is `theme.border.base` while the message is still a pending inbox delivery (`:2256`). Background is `theme.background.raised.base`. The inner box has `paddingTop=1`, `paddingBottom=1`, `paddingLeft=2`, and hover bg `theme.decrease(raised)` (`:2254-2300`).
- Chips are a label span plus a name span (`:2302-2349`):
  - label `" skill "` / `" file "` / `" dir "` (`dir` when mime is `application/x-directory`): bg `theme.hue.accent[300]` in light mode or `[200]` in dark mode, fg `theme.background.raised.base`, bold.
  - name `" <name> "`: bg `theme.decrease(raised)`, fg `theme.text.muted`. The name is `file.name ?? source.uri ?? "attachment"`.
- Images (if `config.session.image_preview`, which is unset/false by default, so hidden) render above the text box with `paddingLeft=2` (§3.11).
- Click, unless text is selected:
  - Pending steer → DialogSelect "Pending steer" with options "Move to queue" / "Delete" (`:2270-2283`).
  - Otherwise → `DialogMessage` ("Message Actions": Jump to / Revert / Copy / Fork; `src/routes/session/dialog-message.tsx:25-78`).
- No timestamp is shown. v2 has **no timestamps toggle and no "tool details" toggle**; a grep of `src/routes/session` finds neither.

#### 3.4.1 Shell message (user `!cmd`) (`index.tsx:2208-2225`)
Rendered as `ShellDisplay` (§3.6.1), with error text `Command cancelled` (killed), `Command timed out`, or `Command exited with code N` for a non-zero exit.

#### 3.4.2 Switch / notice / skill messages
All have `paddingLeft=3` and `theme.text.muted` unless noted.
- Location switch: `↳ Moved to ` (muted) followed by the directory in `theme.text.feedback.info.base` (`index.tsx:1949-1957`).
- Agent switch: `Switched agent from <Prev> to <Agent>`, or `Switched agent to <Agent>`, titlecased (`:1959-1964`).
- Model switch, via `switchLabel` (`src/util/model.ts:10-21`):
  - `Switched variant to <variant|default>` when the provider and model are the same.
  - `Switched model to <display name>[ (<variant>)]`.
  - `Switched model to provider/model[/variant]` when the model is unknown.
- System message: InlineToolRow icon `◈`, muted, text = description or `Instructions updated` (`:1989,2011-2013`).
- Synthetic completion notice (metadata.source `subagent` | `shell`) (`:1983-2031`):
  - One line, `wrapMode=none`, `marginLeft=3`.
  - Heading: `↳ <Actor> finished` (state completed) or `! <Actor> <failed|cancelled|state>`. Actor is `Shell` or the titlecased agent (default `Subagent`).
  - Suffix: ` · <description>` in muted, truncated to `ctx.width - 3 - width(heading)`. Shell descriptions have whitespace collapsed.
  - Heading color: error → `feedback.error`; cancelled → `feedback.warning`; hover on a subagent notice → `text.base`; otherwise `feedback.info`.
  - Clicking a subagent notice navigates to the child session.
- Other synthetic messages: InlineToolRow `◈` with the description.
- Skill message: InlineToolRow `→ Skill <name>` (`:2035-2042`).

#### 3.4.3 Error from idle marker
`Error: <message>` in `theme.text.feedback.error.base`, `paddingLeft=3` (`index.tsx:1847-1854`).

---

### 3.5 Assistant output

#### 3.5.1 Text part (`src/routes/session/message-parts.tsx:169-196`)
- Box with `paddingLeft=3`. It uses an OpenTUI `<markdown>` element:
  - `content = text.trim()`; `streaming` while `message.time.completed` is undefined.
  - `internalBlockMode="top-level"`; `tableOptions={{style:"grid", cellPaddingX:1}}`.
  - `conceal = (config.session.markdown ?? "rendered") === "rendered"`. Conceal hides markdown markers (`**`, backticks, `#`…); `"source"` shows them (`src/config/index.tsx:170`, `index.tsx:239`).
  - fg `theme.markdown.text`, bg `theme.background.base`, `syntaxStyle` = current theme syntax.
- `renderNode` comes from plugin code-block renderers (`src/plugin/markdown.ts:5-18`). These are per-language custom code-block renderers merged last-wins. With none registered it is `undefined` and OpenTUI's default code blocks are used.
- Headings, lists, link and code-block appearance are implemented inside OpenTUI's MarkdownRenderable (not in this repo). See Open questions.
- Tree-sitter languages: markdown, javascript, and typescript are OpenTUI built-ins. Extra WASM parsers come from `src/parsers-config.ts`: python, rust, go, cpp, csharp, bash, c, java, kotlin, ruby, php, scala, html, vue, hcl, json, yaml, haskell, css, julia, lua, ocaml, clojure, swift, toml, nix, diff, elixir, fsharp, r, make, vim, xml, agda (`parsers-config.ts:1-6,8-377`). They are registered via `addDefaultParsers` (`index.tsx:134`).
- Extension→filetype map: `src/util/filetype.ts:3-128`. `typescriptreact`, `javascriptreact`, and `javascript` all map to `"typescript"`. A missing path gives `"none"` (`filetype.ts:130-135`).

#### 3.5.2 Reasoning ("thinking")
Mode is `config.session.thinking ?? "hide"` (`index.tsx:237`). The command "Collapse thinking"/"Expand thinking" (`session.toggle.thinking`, no default key) flips show↔hide (`index.tsx:1037-1054`, `src/context/thinking.ts:17-20`). Reasoning text has `"[REDACTED]"` removed and is trimmed; empty reasoning renders nothing (`message-parts.tsx:115-118`).

Reasoning parts **always** go into a `reasoning` group (`grouping/session.ts:85-91`). Their rendering depends on the mode.

**thinking = "show"** (group not grouped → each part via `ReasoningPart`, `message-parts.tsx:42-113`):
```
   ┃ Thought: 2.4s                 ← header, warning color @ alpha 0.6
                                    ← marginTop 1
   ┃ body markdown in text.muted
```
- Outer box has `paddingLeft=3`. The header box has a left `┃` in `theme.decrease(theme.background.base)` and `paddingLeft=1`. The body box has the same border and `paddingLeft=1`.
- Header while streaming: Spinner with label `Thinking` (or `Thinking: <title>`).
- Header when done: `Thought` + (`: ` + duration). Duration is `Locale.duration(completed - created)` (`:60-64,86,120-166`).
- Header fg: `theme.text.feedback.warning.base` at **alpha 0.6** when open (`:128-136`).
- Body: OpenTUI `<code filetype="markdown" drawUnstyledText={false} streaming conceal=…>`, using a syntax style where every token's fg is forced to `theme.text.muted` (`src/routes/session/thinking-syntax.ts:3-7`), base fg muted (`:98-106`).

**thinking = "hide"** (grouped, `group-view.tsx:162-193`): one InlineToolRow per run of adjacent reasoning parts:
```
   + Thought: Inspecting PR workflow · 3 steps · 4.2s     (collapsed, done)
   - Thought · 3 steps · 4.2s                              (expanded)
   ⠹ Thinking: Inspecting PR workflow                      (still streaming)
```
- Icon `+` collapsed / `-` expanded.
- Color: `theme.text.base` while not completed; `warning.base` on hover or when expanded; otherwise warning at alpha 0.6 (`:166-177`).
- Title: the latest thought's bold summary title, from `reasoningSummary` (matches a leading `**Title**` followed by a blank line or end, `src/context/thinking.ts:9-14`). A finished title-less part resets it to null (`group-view.tsx:95-102`).
- ` · N steps` only when there are more than 1 thoughts. ` · <duration>` is the sum of part durations (`:186-188`).
- Expanded: children in a box with `paddingLeft=3`. Each thought has `marginTop=1`, a left `┃` in `theme.decrease(theme.background.raised.base)`, `paddingLeft=1`, and a muted markdown body (`group-view.tsx:190-192,358-400`).
- Click toggles. State persists per session tab and group (§3.8.4).

#### 3.5.3 Assistant footer (`src/routes/session/index.tsx:1894-1944`)
```
   Error: Provider returned 500             ← only if error ≠ "Step interrupted" and no retry; feedback.error
   ⚠ Retrying in 7s · attempt 2 · rate limited   ← AssistantRetry, feedback.warning
                                            ← marginTop 1 when retry or (error & not interrupted)
   Build · Claude Sonnet 4.5 · 1m 12s · 48.3 tok/s · interrupted
```
- All lines have `paddingLeft=3`.
- Agent: `Locale.titlecase(agent)` in the agent color, or `theme.text.muted` if the message has an error (`:1925-1927`).
- ` · <model name>` is shown only when **terminal width ≥ 28**. Name lookup is in the location's model list, falling back to `providerID/modelID` (`:1900-1906,1928-1930`).
- ` · <duration>` is shown when duration > 0 and (term width < 28 **or** ≥ 36), so it is hidden for widths 28–35 (`:1931-1933`).
  - Duration is the turn time from the turn's first input message `created` to this message's `completed` (`rows.ts:430-440,469-482`).
  - In legacy sessions without idle markers, the turn starts at the nearest preceding user/synthetic message. Otherwise it starts at the first input after the previous `idle`.
- ` · N.N tok/s` when `config.session.tps` (default **true**, `src/config/index.tsx:322`). The value is total (output+reasoning) tokens / Σ(streamed − created) across the turn's steps; it is omitted if any step lacks `time.streamed` (`rows.ts:442-463`). Format is `value.toFixed(1)` (`index.tsx:1934-1936`).
- ` · interrupted` when `error.message === "Step interrupted"` (`:1914,1937-1939`).
- All separators and secondary parts are `theme.text.muted`.
- Retry line (`:2389-2414`): `⚠ Retrying in <N>s · attempt <n> · <error msg>`, or `⚠ Retry due · …` once the countdown reaches 0. Seconds are `ceil((at-now)/1000)` updated every 1 s.
- No cost is displayed anywhere in the transcript footer.

#### 3.5.4 Debug turn token usage (`index.tsx:1618-1776`)
Only with `config.debug.turn_tokens` (true | `"verbose"`). `paddingLeft=3`, clickable.
```
   + Tokens: 3 steps · latest: 1,234 new · 45,678 cached · 46,912 total · ! 1 likely cache bust
     Step        New  Cached   Total        ← expanded header, italic muted, paddingLeft 2
     tool-call   1,234  45,678  46,912      ← New bold; numbers toLocaleString, right-aligned, 2-space gaps
       read      path/to/file  offset: 1    ← verbose only: tool name bold muted + dim summary
     ! Likely cache bust: 2,048 fewer cached tokens than the previous step
```
- Header text is `theme.text.muted`, or `text.base` on hover. `Tokens` is bold. The bust suffix is `feedback.warning`.
- `finish` `tool-calls` displays as `tool-call`.
- A cache bust is a drop in `cache.read` versus the previous step on the same model. OpenAI drops of 1,024–2,048 are ignored (`rows.ts:413-425`).

#### 3.5.5 Background-tool hint (`index.tsx:1778-1810`)
If the latest incomplete assistant message has a running `shell`/`bash` or `subagent`/`task` tool for **≥ 3000 ms** (`BACKGROUND_TOOL_HINT_DELAY`, `:138`), the transcript shows `Press <key> to move running work to the background` with `marginTop=1` and `paddingLeft=3`. Text is muted and the key is in `theme.text.base`. The default key is `ctrl+b` (`src/config/keybind.ts`: `session.background`).

---

### 3.6 Tool calls

Tool names are canonicalized: `bash→shell`, `task→subagent`, `apply_patch→patch` (`src/util/tool-display.ts:1-6`). Dedicated renderers exist **only** for `shell, glob, read, grep, webfetch, websearch, write, edit, subagent, execute, patch, question, skill` (`message-parts.tsx:21-40`). Everything else is **generic**, including list, todowrite/todoread, codesearch, lsp, invalid, and MCP tools.

Tool `output` text is the text content joined with `\n` (file content uses name/uri). It is undefined while streaming (`index.tsx:2428-2433`). Images in tool results are rendered after the tool (§3.11) (`index.tsx:2488-2494`).

States are `streaming` (input arriving) | `running` | `completed` | `error`.

| Kind | Form | Icon / label (verbatim) | Pending text (no input yet) | Spinner when | Click |
|---|---|---|---|---|---|
| read (`:3048-3084`) | inline | `→ Read <path>` + `:<offset\|1>-<offset+limit-1>` when offset/limit given (end empty if no limit) | `Reading file…` | status `running` | — |
| read, loaded extra files | extra lines | `↳ Loaded <path>` muted (box pad 3 + text pad 3 → col 6) | | | |
| glob (`:3035-3046`) | inline | `✱ Glob "<pattern>" in <path> (N match\|matches)`. `in …` only if path is set; count only if a non-zero finite number | `Finding files…` | never (icon shows once pattern known) | — |
| grep (`:3086-3097`) | inline | `✱ Grep "<pattern>" in <path> (N matches)` (metadata.matches) | `Searching content…` | never | — |
| webfetch (`:3099-3105`) | inline | `% WebFetch <url>` | `Fetching from the web…` | never | — |
| websearch (`:3107-3130`) | inline | `◈ Web Search "<query>"`, or `◈ Web Search via <Provider> "<query>"` | `Searching web…` | never | — |
| write, completed (`:3009-3025`) | block | header `# Wrote` + path; body: full file content with line numbers + syntax highlight; diagnostics | | | — |
| write, other (`:3026-3030`) | inline | `← Write <path>` | `Preparing write…` | — | — |
| edit with metadata.files (`:3308-3336`) | block | header `← Edit` + path; PatchDiff; diagnostics | | | — |
| edit, no diff yet (`:3337-3349`) | block (header only) | `← Edit <path>`, or title `# Preparing edit…` when path unknown | | `streaming` (label → `Preparing edit…`) | — |
| patch, per file (`:3382-3428`) | block per file, gap 1 | `# Created` / `# Deleted` / `← Patched` + path; body PatchDiff, or for delete `-N line(s)` in `theme.diff.text.removed` | | | — |
| patch, metadata.applied only (`:3429-3449`) | block per file | same labels; body = FilePath (removed color for delete, else muted) | | | — |
| patch, fallback (`:3450-3468`) | block | `Patching` (+ path if exactly 1 target parsed from `*** Add/Update/Delete File:`), or `# Patch failed`. Header in error color on error; error text muted | | streaming/running | — |
| shell (`:2825-2997`) | block | see §3.6.1 | `Writing command…` | running | expand/collapse |
| subagent (`:3132-3165`) | inline | `<Agent> Subagent — <description>[ · <model>]`, or `Continue subagent — …`; icon `↳` (continuation) / `✓` (completed) / `│` | `Delegating…` | running and not continuation | navigate to child session |
| execute (`:3242-3287`) | inline + nested | see §3.6.2 | `execute` | streaming/running | opens DialogExecute |
| question, answered (`:3486-3499`) | block | title `# Questions`; per question: question (muted) then answers joined `, ` (base) or `(no answer)`, gap 1 | | | — |
| question, unanswered (`:3500-3505`) | inline | `→ Asked N question(s)` | `Asking questions…` | — | — |
| skill (`:3509-3516`) | inline | `→ Skill "<name>"` | `Loading skill…` | — | — |
| generic (`:2577-2632`) | inline | `✓`/`✗` + `<tool> [k=v, k2=v2]` (primitive inputs only, whitespace collapsed) | `<tool name>` | streaming/running | expand inputs/output |

Details for the table:
- Glob and grep counts: `(1 match)` / `(N matches)` (`:3041-3043,3092-3094`).
- Websearch provider: `"opencode"` displays as `OpenCode`; other providers are capitalized (`src/util/tool-display.ts:40-44`). When the provider changes while running and on screen, the name cross-fades: 80 ms fade-out, 60 ms width morph, 80 ms fade-in (`src/component/retry-provider.tsx:19-21,124-146`).
- Subagent agent name: titlecased `input.agent ?? input.subagent_type ?? "General"`.
- Subagent model label: model display name or the raw reference, plus ` (<variant>)` from `provider/model#variant` (`:3167-3178`).
- Subagent status badge: ` Background ` StatusBadge when completed but `metadata.status==="running"` (`:3156-3160,3180-3185`).
- Generic tool: expanded content (`:2597-2624`) is in a box with `paddingLeft = 3 + 2`. Each input is a row `key: ` (muted, no shrink) + value (base, word wrap; non-strings as `JSON.stringify(v,null,2)`). Then `output: ` + trimmed output. The row is expandable only if there is input or output.
- Diagnostics (`:3518-3542,3626-3640`): only severity 1 (error) for the edited file, **max 3**. Format `Error [<line+1>:<col+1>] <message>` in `theme.text.feedback.error.base`.
- Write body (`:3014-3022`): `<line_number fg=muted minWidth=3 paddingRight=1>` wrapping `<code conceal={false} fg=text.base filetype=… syntaxStyle>`. **No truncation.**
- Generic error display: a failed inline tool turns error-colored, and clicking shows the error message under it. BlockTool always shows the error text at the bottom of the panel (`index.tsx:2816-2818`).

#### 3.6.1 Shell block (`ShellDisplay`, `index.tsx:2839-2997`)
```
┃
┃  $ cd packages/tui && bun test        ← theme.text.base; ≤2 lines collapsed (wrap char)
┃                                        ← gap 1
┃  (37 earlier lines)                    ← collapsed tail of output, theme.text.muted
┃  … last lines …
┃                                        ← gap 1
┃   Background                           ← StatusBadge when backgrounded
┃
```
- Command line: `$ ` + (`cd <workdir> && ` when workdir is set and ≠ `.`) + command.
- While running, the `$ ` is omitted and the line is `⠹ <command>` (spinner + text in `theme.text.base`, or warning when awaiting permission) (`:2937-2938,2962-2986`).
- Collapse limits: `maxLines = 10`; `maxChars = 10 × max(20, ctx.width − 6 − (running?2:0))` (`:2935-2936`). Via `collapseShellOutput` (`src/util/collapse-tool-output.ts:26-49`):
  - Command: ≤ 2 visual lines at `floor(maxChars/maxLines)` chars per line. Overflow is cut with a trailing `…`.
  - Output budget: `maxLines − commandLines − 1` lines and `maxChars − len(command) − 2` chars. When over budget, the **tail** is kept, prefixed by a line `(N earlier line|lines)` (`collapse-tool-output.ts:83-95`).
- Collapsed view uses `wrapMode="char"` and `maxHeight=2` for the command. Expanded uses `wrapMode="word"`, unlimited height, and full output (`:2965-2984`).
- Clickable (toggle) when the output overflowed or the call has a `shellID` (background shell) (`:2942-2947`).
- Output is ANSI-stripped and trimmed.
- Background shells:
  - Output is polled every **1000 ms** while running. If the row is a backgrounded completed tool, polling happens only while expanded.
  - Each fetch is capped at **1 MiB** (`SHELL_DISPLAY_LIMIT`). When capped, the text is prefixed with `[earlier output omitted]` (`:2823,2871-2934`).
  - A backgrounded and collapsed shell shows no output.
- No command yet: Spinner `Writing command…` (or muted static text if not running) (`:2952-2961`).
- Error string (user shell messages) is shown at the bottom in error color via BlockTool.

#### 3.6.2 Execute (code-mode) (`index.tsx:3189-3287`)
```
   ✓ execute                                  ← icon ✗ (runtime error, row error-colored) / ✓ / │, spinner while running
   › read [path=src/a.ts]                     ← one row per nested call, paddingLeft 3, icon col 2
   ✗ grep [pattern=foo]                       ← error call: icon ✗, feedback.error
      ↳ TypeError: …                          ← only on runtime error: ≤4 lines preview, error color
```
- Nested call row: muted, `theme.text.base` on hover, `theme.decrease(muted)` when expanded. `wrapMode="none"` truncate.
- When expanded, the summary becomes just the tool name and a box with a left border in `decrease(muted)` and `paddingLeft=1` lists `key: value` rows (`:3203-3238`).
- Error preview: `collapseToolOutput(output, 4, 4*max(20,width-6))`. First line prefixed `↳ `, the rest by 2 spaces (`:3250,3273-3284`).

#### 3.6.3 Diff inside edit/patch blocks (`index.tsx:3295-3331`, `src/component/patch-diff.tsx`)
- Wrapped in a box with `paddingLeft=1`.
- View: `config.diffs.view` `"unified"`/`"split"`. `"auto"` or unset → **split if `ctx.width > 120`, else unified** (`:3295-3301,3374-3378`).
- Wrap mode: `config.diffs.wrap ?? "word"` (`index.tsx:240`).
- Colors:
  - fg `theme.text.base`
  - `addedBg` `theme.diff.background.added`, `removedBg` `…removed`, `contextBg` `…context`
  - `addedSignColor` `theme.diff.highlight.added`, `removedSignColor` `…removed`
  - `lineNumberFg` `theme.diff.lineNumber.text`, `lineNumberBg` `theme.diff.background.context`
  - `addedLineNumberBg` `theme.diff.lineNumber.background.added`, removed likewise
  - `hunkFg` `theme.diff.text.hunkHeader`
- `showLineNumbers` is on, and the filetype comes from the path.
- Hunks: the patch is split at `@@` headers (`src/util/diff.ts:40-54`). Each hunk after the first is preceded by a 1-row bar with bg `lineNumberBg` containing ` <@@ header line>` in `hunkFg` (`patch-diff.tsx:97-115`). The first hunk's header is not shown.
- Each hunk's `minHeight` is its row count, where a +/- run counts `max(adds, dels)` rows (`diff.ts:56-84`).
- Line-number gutters are synchronized across hunks to the widest digit count (`patch-diff.tsx:60-86`).
- Virtualization only applies inside the full diff viewer (needs `scroll`): new-file patches over **3000** lines are chunked by **128** lines (`patch-diff.tsx:25-26`).

---

### 3.7 Compaction markers (`index.tsx:2044-2124`)
```
────────────────── ⠹ Compaction ──────────────────            running
────────── Compaction · 120.3K in · 2.1K out ─────────        completed (muted)
────────────── Provider compaction ───────────────            completed with providerContext
─────────── ✗ Compaction · <nothing> ───────────              failed (feedback.error), message below
──────────── Compaction · cancelled ──────────────            failed with error.type "aborted"
──────────── ◇ Compaction queued ─────────────────            queued (border theme.border.base, text muted)
```
- Layout: row `alignItems=center`. The left and right flexGrow boxes draw only their **top border** (OpenTUI default single line `─`) in the state color. The center has `gap=1` and `paddingLeft/Right=1`.
- Running spinner: braille frames at 80 ms, or `⋯` when animations are off.
- Usage: `<Locale.number(input+cache.read+cache.write)> in · <Locale.number(output+reasoning)> out`, shown only when > 0 (`:2056-2063`).
- `Locale.number`: ≥1e6 → `x.xM`; ≥1000 → `x.xK`; else integer (`src/util/locale.ts:21-28`).
- Body: the summary (completed) or error message (failed, non-cancelled) renders as markdown, `paddingTop=1`, `paddingLeft=3`, same options as a text part (`:2093-2107`).

---

### 3.8 Grouping and collapsing

#### 3.8.1 Paths (`src/routes/session/grouping/session.ts:78-107`)
- Reasoning → `["reasoning"]`.
- Tools named (lowercase) `read|glob|grep|webfetch|websearch` → `["exploration"]`.
- `question` → always standalone.
- Synthetic messages with `metadata.instruction.paths` → `["instructions"]`.
- **Verbosity `low`** prefixes every non-text part and instruction message with `"activity"` (one summary for a run).
- Adjacent entries with the same leading kind merge into one group (`grouping/tree.ts:17-53`).
- Verbosity: `config.session.verbosity ?? "medium"`. Command title `Verbosity: <Low|Medium|High>` cycles low→medium→high (`index.tsx:1083-1097`).
- Exploration grouping is controlled by `config.session.grouping` (`auto` default, `none` disables). Command title: `Show tool calls individually` / `Group related tool calls` (`index.tsx:241,1069-1082`).
- A group is "completed" once any row follows it, or (reasoning) once all its parts are completed (`grouping/session.ts:49-65,144-147`).

#### 3.8.2 Exploration group header (`src/routes/session/group-view.tsx:115-160`)
```
   → Explored: 3 reads, 2 searches, 1 fetch            (completed; click toggles)
   ⠹ Exploring: 1 read, 1 search                        (spinner while not complete)
```
- Counting: grep, glob, and websearch count as `search`; webfetch as `fetch`; read as `read`. Plural rules: `searches`, `fetches`, else `+s`. Names appear in first-seen order.
- Header color is `theme.text.muted`, or `text.base` on hover.
- Expanded: the individual tool rows are listed below. Images of the group's tools are always shown below the header, even when collapsed (`:158`).
- When grouping is off, the children render normally.

#### 3.8.3 Activity group (verbosity low) (`group-view.tsx:201-232`, `src/routes/session/activity-summary.ts`)
```
   + 3 commands, 1 edit, 2 thoughts, 4 reads, 1 tool, 2 instructions
   ⠹ Running command…            (nothing finished yet: busy label of first active item)
   − …                            (expanded; note U+2212 minus), children with gap 1, marginTop 1
```
- Counts are in fixed order `command, edit, thought, read, tool, instruction`, with `count name` and an `s` for plurals (`activity-summary.ts:61-95`).
- Categories: shell → command; edit/write/patch → edit; reasoning → thought (finished, non-empty only); read → read; others → tool.
- `execute` counts its finished nested calls (read→read, other→tool), or itself as a tool if it has none.
- Busy labels: `Thinking…`; `Preparing <noun>…` (streaming); `Running <noun>…`. The noun is `command` for shell, `code` for execute, else the tool name (`activity-summary.ts:47-52`).
- The spinner shows when collapsed and any item is active.

#### 3.8.4 Instructions group (`group-view.tsx:234-265`)
`◈ Instructions: N file|files` (distinct paths). Click expands to the individual system/synthetic notices.

#### 3.8.5 Expansion state
- Stored per session tab and group ID via `sessionTabs.setGroupExpanded`. The group ID is `JSON([firstMessageID, firstPartID, kind, level])` (`src/routes/session/anchors.ts:25-28`).
- Default: verbosity `high` → exploration and instructions groups start **expanded**. Everything else starts collapsed (`index.tsx:243-246`).
- Toggling keeps the clicked header on the same screen row (`holdAnchor`), except when the group is the last row and the view follows the bottom (`index.tsx:1356-1369,403-411`).
- Permission-blocked tools are pulled to the end of their exploration group and **always shown** below the collapsed header, in warning color (`grouping/session.ts:163-176`, `group-view.tsx:292-309`).

---

### 3.9 Scrolling, navigation, virtualization

#### 3.9.1 Commands (`index.tsx:782-906,1098-1145`; default keys from `src/config/keybind.ts:177-189`)
| Command | Default key | Effect |
|---|---|---|
| `session.page.up/down` | `pageup,ctrl+alt+b` / `pagedown,ctrl+alt+f` | scroll ∓ **viewport/2** |
| `session.half.page.up/down` | `ctrl+alt+u` / `ctrl+alt+d` | scroll ∓ **viewport/4** |
| `session.line.up/down` | `ctrl+alt+y` / `ctrl+alt+e` | ∓1 line |
| `session.first` | `ctrl+g,home,alt+home` | load **all** history (strip shows `Loading session history…`), mount the first 60-weight rows, scroll to top |
| `session.last` | `ctrl+alt+g,end` | `toBottom()`: re-enable sticky, after **50 ms** scroll to `scrollHeight` |
| `session.messages_last_user` | `alt+end` | jump to the last user message with text |
| `session.message.next/previous`, `…user.next/previous` | none | step between message boundaries |
| `session.timeline` | `<leader>g` | "Timeline" DialogSelect of user messages → jump (`dialog-timeline.tsx:27,41`) |

- First/last are bound only when no editor has focus (`index.tsx:1327-1330`). Page and line commands are global.
- Message boundaries (`src/routes/session/message-navigation.ts:18-48`) are user messages with non-blank text plus assistant messages that contain non-blank text. Assistant targets align **1 row above** the message (to include the blank `marginTop` gap).
- Jump alignment: the target's top is scrolled to the viewport top. If there is not enough content below, invisible **navigation slack** (a spacer box) is appended so the target can reach the top: slack = `ceil(top + viewportH − contentH)` (`message-navigation.ts:8-16`, `index.tsx:700-716,1449-1451`).
- Sticky-follow is disabled while navigating. Any wheel scroll, resize, or line/page command clears navigation and removes the slack (`index.tsx:301-319,767-780`).

#### 3.9.2 Away-from-bottom / sticky
- "Away" means `scrollTop < scrollHeight − viewportH`, or any navigation/reveal is in progress, or newer rows are unmounted (`index.tsx:508-520`).
- It is debounced through a `setTimeout` (0 ms). When back at the bottom, `stickyScroll` is set back to true and the "Jump to latest ↓" pill hides (`:521-536`).
- Scroll position is saved per tab as an anchor (the topmost visible part/group plus its screen offset) and restored when the tab is reopened (`:537-614`).

#### 3.9.3 Mount budget (user-visible: older rows appear as you scroll up)
- Initial mount is the last **40** weighted rows (`TRANSCRIPT_TAIL_ROWS`, `index.tsx:142`).
- Weight: 1 per row or collapsed group; an expanded group costs `1 + Σchildren`; an ungrouped group costs its size (`src/routes/session/mount-budget.ts:10-22`).
- Scrolling up within 1 viewport of the top mounts **60** more weighted rows (`TRANSCRIPT_BACKFILL_CHUNK`) and compensates the scroll offset so the content does not jump (`index.tsx:452-467`).
- At the top with everything mounted, older history pages are fetched from the server (`history.ts`). Fetching continues while the oldest row is a group, so groups aren't split across pages (`rows.ts:32-45`).
- Scrolling down near the bottom of a pinned window mounts 60 newer rows (`index.tsx:468-488`).
- Leaving the bottom pins the window. Returning to the bottom releases it to follow appends (`:413-430,523,531`).

---

### 3.10 Revert banner, queued prompts

**RevertMessage** (`index.tsx:2126-2206`) sits at the end of the transcript when `session.revert.messageID` is set:
```
┃
┃  2 messages reverted                         ← muted
┃
┃  M src/a.ts +12 -3                           ← statusLabel A/D/M muted, path text.base, +N diff.text.added, -N diff.text.removed
┃  A src/b.ts +40
┃
┃  <leader>r or /redo to restore               ← key in text.base, rest muted
┃
```
- Left `┃` in `theme.background.raised.base`. Background `raised.base` (`decrease` on hover). `marginTop=1`. Inner padding top/bottom 1, left 2.
- Click → `revert.clear` (redo).
- Path width: `ctx.width − 5 − widths of the +N/−N counts`.

**QueuedPromptDock** (`index.tsx:2356-2387`):
- Left `┃` in `theme.border.base`, `marginBottom=1`, padding 1/1/2/1, raised bg (`decrease` on hover).
- Single truncated line: `N queued` (text.base) + ` · <first prompt with newlines→spaces>` (muted).
- Click opens the "Queued prompts" dialog (`:646-694`).

---

### 3.11 Images in transcript (`index.tsx:2501-2568`)
- Shown only when `config.session.image_preview` is true. It is undefined, i.e. **off**, by default (`src/config/index.tsx:164`).
- Row of at most **3** thumbnails. Height is `clamp(floor(termH/4), 4, 8)` and width is `2×height`. Padding is top 1, left 3 (2 inside user messages), right 2, bottom 1, with gap 1.
- More than 3 images adds `+N more`. A failed load shows `No preview`. Clicking opens DialogImagePreview.
- Tool images come only from `data:image/` URIs (`:2562-2568`).

---

### 3.12 Composer (tab panel above the prompt) (`src/routes/session/composer/`)
```
┃  Subagents  Shell  Terminals                              esc   ← active tab bold text.base, others muted; gap 2
┃                                                                  ← gap 1
┃  ├─ Explore: find config loader                       Running   ← list scrollbox maxHeight 5, no scrollbar
┃  └─ General: write tests
┃
┃  interrupt ctrl+d  show inactive ctrl+a  tabs ←/→               ← footer hints: label bold text.base + shortcut muted, gap 2
```
- Container (`composer/index.tsx:84-157`): left `┃` in `theme.border.base`, bg `theme.background.raised.base`, padding L1 R2 T1 B1, inner `gap=1`, and header/footer `paddingLeft=1`.
  - With a single tab, its label shows bold instead of the tab strip.
  - `esc` (muted) on the right closes the composer on click.
  - The footer adds `tabs ←/→` when there is more than one tab.
- Keys in "composer" mode: `left`/`right` switch tabs (wrapping); `escape`/`ctrl+c` close (`composer/index.tsx:72-82`).
- Opened by `session.child.first` (default `down`) on the Subagents tab (`index.tsx:1274-1283`), or by the terminal picker on the Terminals tab (`:229`). It is always open in child sessions (`:1481`).
- Row styling (all tabs): `paddingLeft/Right=1`, `wrapMode=none`.
  - Selected row: bg `background.action.primary.focused`, fg `text.action.primary.focused`, bold.
  - Current item: bg/fg `…primary.selected`.
  - Other rows: `…primary.base`.
  - Mouse move selects; mouse up activates.
- **Subagents tab** (`subagents-tab.tsx`):
  - Entries are the whole session family tree under the root, with tree prefixes `├─ `, `└─ `, `│  `, `   ` (`src/util/session.ts:9-43`).
  - Row text: `<prefix><Agent>: <title>`. Agent is `session.agent`, or the `@name subagent` pattern from the title, or `Subagent`. A right-aligned `Running` appears for running sessions (muted, or focused fg).
  - Filter defaults to active (running) only. `ctrl+a` toggles to inactive; the hint reads `show inactive`/`show active`. Empty states: ` No active subagents` / ` No inactive subagents`.
  - Keys: `up`/`down` (up at index 0 closes the composer); `return` navigates; `ctrl+d` interrupts (hint `interrupt` only when the row is running).
  - On open, the current session is selected and centered.
- **Shell tab** (`shell-tab.tsx`):
  - Lists running shells for the session by the first line of the command. Empty state: ` No shell commands`.
  - Keys: `up` (closes at 0), `down` (wraps), `return` → DialogShellOutput, `ctrl+d` kills. Hints `output <key>` and `kill <key>`.
- **Terminals tab** (`terminals-tab.tsx`, only when the terminals feature is enabled):
  - Rows are `foregroundProcess ?? title`, plus a final `+ New terminal`, truncated.
  - Keys: `up,k` / `down,j` cycle including the "new" row; `return` selects (creates or focuses) and closes. A failure toasts `Unable to load terminal`.

---

### 3.13 Full diff viewer (plugin route, `src/feature-plugins/system/diff-viewer*.tsx`)
A full-screen overlay (absolute, zIndex 2500, `diff-viewer.tsx:193`), opened by the `diff.open` command.

- **Sources** (`:73-80`): `All` (Branch + local changes), `Committed`, `Uncommitted`, `Last turn` (only with a session). There is also a `Base` branch chooser ("Base branch" dialog, footer "Remembered until the TUI exits").
- Source detail text: `vs HEAD`, `vs <base>`, `Resolving diff…`, `Base not reported`, `Base or diff unavailable`, `Diff unavailable` (`:183-190`). The diff uses 12 context lines (`:46`).
- **Layout** (`:802-1095`):
  - File tree is shown when term width ≥ **90** and enabled. Width is `clamp(floor(W/4), 30, 40)` (`:42-44,315-321`).
  - Without the tree, a 1-row header (pad 2) shows the source label (bold, `text.action.secondary.base`), ` · detail` (muted), and a right-aligned `reviewed/total`, with a `?` help glyph top-right.
  - Patch pane has padding L/R 2, a 1-row top edge in `diff.background.context`, and a scrollbox with no scrollbars.
- **Split vs unified**: split is available when the patch pane width ≥ **100** (`MIN_SPLIT_WIDTH`). Default is split, persisted in `config.diffs.view`. Non-modified files (added/deleted) are always unified. Wrap is `char` (`:323-327,1046,1051`).
- **File card**:
  - Separator above every card but the first: a row of `▄` in the card color.
  - 2-row sticky header (stays pinned while the card scrolls): FilePath (dir muted, basename base), `✓` if reviewed, `+N` (diff.text.added) and `-N` (diff.text.removed), or `Image`.
  - Card bg is `theme.diff.background.context`, or `background.raised.high` when reviewed. A reviewed card collapses its body (with a `▀` bottom edge).
  - Fallback texts: `No patch available for this file.`, `Deleted image. The previous revision is not available for preview.`, `Committed image preview unavailable. The working-tree image is not shown.` (`:936-1066`).
  - States: `Loading diff…`, `No changes to show`, `Could not load diff. Reopen the diff viewer to try again.` / `Could not load diff. Choose a base branch from Diff source, or select Uncommitted.`, `Committed comparison unavailable without base metadata. Choose a base branch from Diff source.` (`:847-871`).
- **File tree** (`diff-viewer-file-tree.tsx`):
  - Raised bg, padding 2.
  - Rows: indent rails `│ ` per depth (very faint), marker `≡ ` for files or `▸ `/`▾ ` for dirs (faint), name (bold if selected; dirs muted), and a status column `M`/`A`/`D`/`?` or `✓` (added → diff.text.added, deleted → removed).
  - Hover bg is `background.raised.high`. Directory chains are truncated as `…/c/d`. Right-click opens a file menu ("Mark complete"/"Mark incomplete").
- **Keys** (`src/config/keybind.ts:80-89`): `]`/`[` next/previous hunk; `n,alt+down`/`p,alt+up` next/previous file; `b` toggle tree; `s` single-file mode; `d` switch source; `v` split/unified; `m` mark reviewed (then advances in single mode); `?` help dialog with groups Review/Scroll/View (`diff-viewer.tsx:1098-1190`). `ctrl+d` (app.exit) closes the viewer.

---

### 3.14 Formatting helpers (verbatim formats)
- `Locale.duration(ms)` (`src/util/locale.ts:30-50`):
  - `<1000` → `"<ms>ms"`
  - `<60s` → `"x.xs"` (toFixed 1)
  - `<1h` → `"<m>m <s>s"`
  - `<1d` → `"<h>h <m>m"`
  - else `"<d>d <h>h"`
- `Locale.number`: `1.2K`, `3.4M` (toFixed 1), else the raw integer (`locale.ts:21-28`).
- Turn tokens use JS `toLocaleString()` grouping (e.g. `46,912`).
- `Locale.titlecase`: uppercase the first letter of each `\b\w` word (`locale.ts:5-7`).
- `Locale.truncate(s,n)`: `s.slice(0,n-1)+"…"`. `truncateWidth` is display-width aware with `…` (`locale.ts:52-83`).
- Marquee (`src/util/marquee.ts`): scrolls an overflowing string with the gap `" · "`. It is not used in transcript rows.
- On exit, the session epilogue prints the wordmark plus `  Session   <title truncated 50>` and `  Continue  opencode -s <id>` (labels padded to 10, dim) (`src/util/presentation.ts:26-35`, `index.tsx:193-197`).
- Copy/export transcript markdown format (`index.tsx:1552-1582`):
  - `# <title>`, `**Session ID:**`, `**Created:**`, `**Updated:**`, `---`.
  - `## User`, `## Shell` (code block with `$ cmd`), `## Assistant`.
  - Reasoning as `_Thinking:_`; tools as `**Tool: name**` + `**Input:**` json block + output.

---

### Open questions (messages)
1. Markdown visuals (heading styles and prefixes, bullet glyphs, code-block frame/background, link rendering, what `conceal` hides exactly, `internalBlockMode="top-level"`) and the `<diff>` renderable's split/unified internals (sign column chars, line-number padding) live in `@opentui/core`, which is not vendored in this checkout. The Rust port needs those sources or screenshots.
2. Border corner/edge characters for `border={["top"]}` (compaction rule) use OpenTUI's default border style. Assumed `─` (single); please verify.
3. `theme.decrease()` semantics (darken vs. reduce contrast) and the exact `theme.hue.accent[200|300]` values are owned by the theme section.
4. `ReasoningPart` has a collapsible "hide" mode (`+ Thought: title · 2s`, `message-parts.tsx:51-89`), but in the transcript reasoning is always grouped, so with `thinking=hide` the group header (§3.5.2) is what renders. Is the per-part minimal mode reachable anywhere (plugins/other routes)? Treat it as dead in the transcript for now.
5. The user-message bar color uses the **session's current agent**, not the message's agent (`index.tsx:2242`). This could be intentional or a bug; replicate it as-is.
6. "Page up/down" scroll by **half** a viewport and "half page" by a quarter (`index.tsx:788-823`). Replicate verbatim.
7. Spinner inside InlineToolRow (`spinner=true`) replaces the icon column without fixed width 2. Confirm that the label's start column shifts by 0 (spinner glyph 1 col + gap 1 = 2), which it should.
8. Cost is not displayed anywhere in the v2 transcript (only tokens and tok/s). Confirm that no other section (sidebar/header) is expected to mirror per-message cost.

---

## 4. Prompt input

All paths are relative to `packages/tui` unless prefixed `../` (sibling packages). Main component: `src/component/prompt/index.tsx` (the `Prompt` component, `index.tsx:185`). Autocomplete popup: `src/component/prompt/autocomplete.tsx`. Metadata line: `src/component/prompt/metadata.tsx`. Footer hints: the `opencode.prompt.footer` plugin, `src/feature-plugins/prompt/footer.tsx`.

Cross-refs: app shell / routes (§ app shell), dialogs (DialogStash, DialogSkill, DialogImagePreview, DialogWorkspaces, DialogIntegration), keybinding table and leader handling (§ keybindings), theme tokens / `theme.decrease` / `tint` (§ themes), session data / queued inbox (§ state), and transcript rendering (§ messages). Note that `src/mini/footer.prompt.tsx` is a separate prompt used by the "mini" runtime and is not covered here.

### 4.1 Where the prompt is mounted

| Route | Mount | Props | Notes |
|---|---|---|---|
| Home | `src/routes/home.tsx:101-103` | `placeholders={placeholder}`, `disabled={forms().length > 0}` | Wrapper box: `width="100%" maxWidth={75} paddingTop={1}` (`home.tsx:101`). The home column's padding is 1 when width < 44, else 2 (`home.tsx:86-87`). |
| Session | `src/routes/session/index.tsx:1527-1539` | `visible`, `muted={props.promptMuted}`, `onSubmit` (scrolls to bottom), `onEmptySubmit` (steers the first queued prompt), `sessionID` | **No `placeholders` prop is passed**, so the session prompt shows no placeholder (`index.tsx:1598`). It is replaced by the permission prompt, the form prompt, or "location missing" when those are active (`session/index.tsx:1496-1525`). |

Home placeholder lists, verbatim (`home.tsx:20-23`):
- normal: `"Fix a TODO in the codebase"`, `"What is the tech stack of this project?"`, `"Fix broken tests"`
- shell: `"ls -la"`, `"git status"`, `"pwd"`

`--prompt` CLI arg: on first bind it sets the prompt text (`home.tsx:52-58`). It auto-submits once agent and model are ready (`home.tsx:68-79`).

### 4.2 Visual layout

#### Structure (top → bottom)

```
anchor box (width 100%)                                      index.tsx:1657
├─ box border=["left"], chars=SplitBorder (vertical "┃", bottomLeft "╹"), borderColor=borderHighlight   :1658-1666
│  └─ box bg=promptBg, paddingLeft/Right = (w<44 ? 1 : 2), paddingTop=1, flexGrow 1                    :1667-1675
│     ├─ [optional] image preview strip                                                                :1676-1743
│     ├─ textarea (minHeight 1, maxHeight max(6, floor(termH/3)))                                        :1744-1831, :1651
│     └─ row paddingTop=1, space-between → PromptMetadataRow                                            :1832-1846
├─ box height=1, border=["left"], vertical = "╹" (or " " if promptBg alpha==0), color=borderHighlight   :1849-1857
│  └─ box height=1 border=["bottom"], horizontal = "▀" (or " "), borderColor=promptBg                  :1858-1873
└─ footer row: width 100%, flexDirection row, justify space-between, gap 2                               :1875-1959
   Slot "prompt.footer"
     ├─ Slot "prompt.footer.status" → status box (flexGrow 1, shrink 1, minWidth 0)
     ├─ Slot "prompt.footer.file"   → editor-context file label
     └─ (appended by plugin) PromptFooter hints                                                         footer.tsx:119-128
<Autocomplete> (absolute overlay, rendered as a sibling after the anchor)                                :1961-2008
```

- `promptBg = theme.decrease(theme.background.raised.base)` (`index.tsx:1653`).
- The left bar `┃` covers every row of the inner box: the top pad row, the textarea rows, the pad row, and the metadata row. The row below it is a 1-row "cap": `╹` (heavy up half-line) in the bar color, then a run of `▀` (upper half block) drawn in `promptBg` as the foreground. This gives the background a half-cell bottom edge (`index.tsx:1849-1873`).
- The main box has no top, right, or bottom border characters.

#### ASCII mockup: home, width ≥ 44, normal mode

```
┃                                                         ← paddingTop (bg = promptBg)
┃  Ask anything… "What is the tech stack of this project?"← textarea / placeholder (text.muted)
┃                                                         ← metadata row paddingTop
┃  Build · Claude Sonnet 4.5 Anthropic · high             ← metadata row
╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀  ← cap row
~/git/ocd:main                      shift+tab agents  ctrl+p commands   ← footer
```
(The model and provider names are illustrative. The exact key text comes from keymap formatting; see § keybindings.)

#### ASCII mockup: session, running, shell mode not active

```
┃
┃  ▌(cursor)
┃
┃  Build · gpt-5 OpenAI
╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
 ■■⬝⬝⬝⬝⬝⬝ esc interrupt          12,345 (6%) · $0.12
```

#### Colors

| Element | Token / rule | Source |
|---|---|---|
| Left bar `┃`/`╹` (`borderHighlight`) | `tint(border.base, highlight, agentMetaAlpha)`. `highlight` = `border.base` if muted (leader key active or `muted` prop); `text.action.primary.selected` in shell mode; otherwise the agent color, falling back to `border.base` | `index.tsx:1565-1569,1585`, `index.tsx:193` |
| Agent color | `agent.color` (hex) if set, else `theme.categorical[i][200]` (deduped), where i = index among visible agents, modulo the number of colors. Unknown agent → first color | `src/context/local.tsx:75-81,125-131` |
| Inner background | `theme.decrease(background.raised.base)` | `index.tsx:1653` |
| Text | `text.base`, or `text.muted` when muted | `index.tsx:1748-1749` |
| Placeholder | `text.muted` | `index.tsx:1747` |
| Cursor | `text.base`; when disabled it uses `background.raised.base` (invisible). Style comes from `config.cursor` | `index.tsx:351-355,1829` |
| Focused bg | `"transparent"` | `index.tsx:1828` |

Fade-in: on the first reveal per `local` instance, the agent label, model, and variant fade in. Alpha tweens 0→1 over 0.16 s (`index.tsx:1571-1584`, `src/util/signal.ts:20-27`). The bar tints from `border.base` toward the agent color using the same alpha.

#### Textarea sizing / placeholder

- `minHeight={1}`, `maxHeight = max(6, floor(terminalHeight / 3))` (`index.tsx:1651,1750-1751`). Above that height the content scrolls inside the textarea.
- Placeholder text (`index.tsx:1592-1604`):
  - normal: `` `Ask anything… "${list[i % len]}"` `` (the ellipsis is U+2026)
  - shell: `` `Run a command… "${shell[i % len]}"` ``
  - no list → no placeholder.
  - Truncation width: `w < 44 ? w - 5 : min(75, w - 4) - 5`, clamped to ≥ 1. It uses `Locale.takeWidth` (a hard cut with no ellipsis) and then `trimEnd()` (`src/util/locale.ts:61-74`).
- Placeholder rotation: the index is `randomIndex(list.length)` at mount (`index.tsx:103-106,364`). It is re-randomized on every `sessionID` change (`index.tsx:407-415`) and when entering shell mode (re-randomized against the shell list, `index.tsx:977`). It does **not** rotate on a timer.

#### Image preview strip (opt-in `config.prompt.image_preview`)

`index.tsx:1676-1743`. It is shown when the config flag is set and image attachments (`data:image/…` URIs, deduped) exist.
- Tile height `max(4, min(8, floor(termH/4)))`, width = 2 × height. Row height = tile height + 1 (paddingBottom 1). Gap 1. At most 3 tiles (`index.tsx:397-399`).
- On image load error the tile shows `No preview` in `text.muted`, centered.
- If there are more than 3 images, an 8-wide box shows `+N more` (`text.muted`).
- Left-clicking a tile opens `DialogImagePreview` at that index. The `prompt.images.view` command (`<leader>i`) opens it at index 0 (`index.tsx:401-405,492-498`).

#### Metadata row (agent · model provider · variant)

`src/component/prompt/metadata.tsx`. Laid out as a row with gap 1:

```
<Agent>  [auto]  ·  <model (truncate)>  <provider>  ·  <VARIANT(bold)>
```

| Piece | Text | Color |
|---|---|---|
| Agent | `Locale.titlecase(agent.id)` (e.g. `build`→`Build`) (`index.tsx:1550`). In shell mode the literal is `Shell` (`index.tsx:1570`, `metadata.tsx:24`) | `highlight` faded by agentAlpha (`metadata.tsx:43`) |
| `auto` | literal `auto`, only when `local.permission.mode === "autoaccept"` (`index.tsx:1836`), normal mode | `text.muted` (`metadata.tsx:45-47`) |
| `·` separator | before the model, only if an agent is shown | `text.muted` (`metadata.tsx:50-52`) |
| Model | `info.name`, or `` `${modelID} (unavailable)` ``. With no selection: `No provider selected` (`local.tsx:395-409`) | `text.base` (`text.muted` when muted); truncates (`metadata.tsx:53-61`) |
| Provider | provider name, or `providerID`. With no selection: `Connect a provider` | `text.muted`, no shrink |
| `·` + variant | `local.model.variant.current()` (e.g. `high`) | `·` muted. Variant uses `text.feedback.warning.base` **bold** (`metadata.tsx:69-80`) |

Responsive fitting (`metadata.tsx:23-34,101-136`):
- Available width = `termW - (termW < 44 ? 9 : 13)`.
- `termW < 44` → the agent label and provider are hidden.
- Candidates are tried in order, and the first whose space-joined text fits wins:
  1. agent+auto+model+provider+variant
  2. drop `auto`
  3. provider shortened to its last `" / "` segment
  4. drop provider
- If none fit: model = `truncateWidth(model, max(9, width - width("Agent · ") - width(" · variant")))`, with `" …"`→`"…"`.
- If neither agent nor model is shown, a 1-row empty box keeps the height (`metadata.tsx:38-41`).
- The display memo keeps the last resolved value while the destination location's catalogs load (`index.tsx:1528-1564`).

#### Footer row (below the cap)

Left: the status box (`index.tsx:1878-1942`). The first matching state wins:

| State | Content |
|---|---|
| `status() === "running"` | `marginLeft 1` spinner, then `PromptInterruptStatus`. The spinner is the KnightRider `"blocks"` style (8 cells, `■` active / `⬝` inactive, `src/ui/spinner.ts:272-324`), tinted by the agent color (or `border.base`) with `inactiveFactor 0.6, minAlpha 0.3` and a 40 ms interval. With animations off it shows `[⋯]` in `text.muted` (`index.tsx:1632-1650,1891-1893`). |
| worktree move in progress | `paddingLeft 3`. `Spinner` in `hue.accent[500]` + progress text (`Creating worktree` / `Creating session` / `Submitting prompt`) + 1–3 dots in `text.muted`. The dots cycle every 1000 ms (`index.tsx:1905-1914`, `move.tsx:32,53,147,160-167`) |
| new worktree pending | `paddingLeft 3`, `(new worktree)` in `hue.accent[500]` (`index.tsx:1915-1921`) |
| default | Location label: `abbreviateHome(dir)` (`~`, `~/x`) plus `:branch` if VCS info is available. Uses `truncateFilePath` to the measured box width. Color is `text.muted`, or `text.base` on hover. Clicking opens working-directory actions (move → `move.open()`) (`index.tsx:1605-1630,1922-1940`). With no session it shows the next-session location. In a session it shows only when status is `idle` (`index.tsx:1605-1612`). While `/cd` is resolving it shows the pending directory (`index.tsx:1614-1615`). |

`PromptInterruptStatus` (`index.tsx:108-145`):
- Idle: `esc` in `text.base` + ` interrupt` in `text.muted`.
- Armed (`store.interrupt > 0`): `esc again to interrupt`, all in `text.feedback.warning.base`. If animations are on, the color flashes from `tint(warning, decrease(warning,2), 0.75)` back to warning over 0.22 s (`index.tsx:116-135,1895-1902`).

Plugin `/btw` appends to `prompt.footer.status`: while a side question is pending it shows a `Spinner` in `hue.interactive[200]` with the label `/btw` (`src/feature-plugins/prompt/btw.tsx:28-40`).

Middle: editor-context file label (`index.tsx:1944-1957`). It is shown when the editor `labelState !== "none"`.
- Text: basename. For `index.*` files it is `parent/index.ext`. A `#L` or `#L1-L2` range is appended for a range selection, plus ` +N` when there are extra ranges (`index.tsx:231-248`). Truncated middle to `max(12, min(48, floor(termW/3)))` (`index.tsx:249-253`).
- Color: `hue.accent[500]` while pending (not yet sent), `text.muted` once sent.

Right: `PromptFooter` (`footer.tsx:12-117`). `showDetails = store.interrupt === 0 || termW >= 80` (`index.tsx:1586-1590`).
- **Shell mode**: `esc` (`text.base`) + ` exit shell mode` (`text.muted`). When termW < 44 the second part is ` shell` (`footer.tsx:107-114`).
- **Normal mode**, when there are live children or usage status:
  - Live part: `[<session.child.first key> ]N subagent(s)[ · ]N shell(s)`, `text.muted` (key in `text.base`). It turns `text.base` on hover. Click dispatches `session.child.first`.
  - Usage part: then ` · ` and `"<tokens> (<pct>%) · $<cost>"` (`Locale.number`, `formatContextUsage`, USD `Intl` currency) in `text.muted` (`footer.tsx:20-48,65-93`, `src/util/session.ts:75-78`).
  - Usage is hidden unless `showDetails && layout.usage`.
- **Normal mode**, otherwise: `<agent.cycle key> agents` (key `text.base`, word `text.muted`) when `showDetails && layout.shortcuts` (`footer.tsx:95-99`).
- `<command.palette.show key> commands` is shown whenever `showDetails && layout.shortcuts` (`footer.tsx:101-105`).
- Layout: with no usage, `shortcuts = termW >= 44`. Otherwise `promptFooterLayout(width = termW - 8)`: available = `width - min(28, floor(width/2))`. Then try, in order: usage+shortcuts, usage only, shortcuts only, nothing (`footer.tsx:51-59,131-141`).
- Default keys: `agent.cycle` = `shift+tab`, `command.palette.show` = `ctrl+p`, `session.child.first` = `down` (`src/config/keybind.ts:172,57,136`).

### 4.3 Text model: prompt store, parts, and chips (extmarks)

`PromptInfo` = `{ text, files[], agents[], skills[], pasted[], mode? }` (`src/prompt/history.tsx:20-30`). Every non-text part carries a `mention {start,end,text}` range expressed in **textarea offsets**. Offsets are counted in display width, with a newline counting as 1 (`src/prompt/display.ts:5-12`).

Chips are OpenTUI **virtual extmarks** of type `"prompt-part"` (`index.tsx:1803-1805`). A virtual extmark covers inserted literal text such as `@src/foo.ts` or `[Image 1]`. The cursor treats it as one atomic unit (see open questions). The `extmarkToPart` map links each extmark to `{type, index}`. After every content change, `syncExtmarksWithPromptParts` (`index.tsx:804-874`) rebuilds the part arrays from the surviving extmarks, so deleting a chip drops its part. Mention-less file attachments are preserved (`src/prompt/attachment.ts:52-63`).

Chip styles (syntax rules, `../theme/src/tui/syntax.ts:13-20`):

| Part | Style id | Look |
|---|---|---|
| file `@path` | `extmark.file` | fg `text.feedback.warning.base`, bold |
| agent `@name` | `extmark.agent` | fg `categorical[0][200]`, bold |
| skill `@id` | `extmark.skill` | fg `categorical[1][200]` (or `[0]`), bold |
| pasted text, image/PDF (`[Image N]`, `[PDF N]`, `[SVG: …]`, `[Pasted ~N lines]`) | `extmark.paste` | fg `text.action.primary.focused` on bg `text.feedback.warning.base`, bold |

Image and PDF attachment chips also use the paste style (`index.tsx:1486-1492`).

Every chip insert appends a single trailing space after the chip text (`index.tsx:1402,1482`, `autocomplete.tsx:165-167`). Autocomplete omits that space if the next character is already a space.

### 4.4 Keyboard behavior

Default bindings come from `src/config/keybind.ts`. Leader = `ctrl+x` (`keybind.ts:41`).

#### Inside the textarea (OpenTUI textarea actions driven by `input.*` keys)

| Action | Default keys | Source |
|---|---|---|
| Submit | `return` (`input.submit`) → textarea `onSubmit` → double `setTimeout(0)` (IME flush) → `submit()` | `keybind.ts:206`, `index.tsx:1767-1772` |
| Newline | `shift+return`, `ctrl+return`, `alt+return`, `ctrl+j` | `keybind.ts:207` |
| Cursor left/right | `left`,`ctrl+b` / `right`,`ctrl+f` | `keybind.ts:208-209` |
| Up/down | `up` / `down` (see history below) | `keybind.ts:210-211` |
| Select | `shift+left/right/up/down` | `keybind.ts:212-215` |
| Line home/end | `ctrl+a` / `ctrl+e`. Select variants: `ctrl+shift+a` / `ctrl+shift+e` | `keybind.ts:216-219` |
| Visual line home/end | `alt+a` / `alt+e` (`alt+shift+…` selects) | `keybind.ts:220-223` |
| Buffer home/end | none. Selection: `shift+home` / `shift+end` | `keybind.ts:224-227` |
| Delete line / to end / to start | `ctrl+shift+d` / `ctrl+k` / `ctrl+u` | `keybind.ts:228-230` |
| Backspace / delete | `backspace`,`shift+backspace` / `ctrl+d`,`delete`,`shift+delete` | `keybind.ts:231-232` |
| Undo / redo | `ctrl+-`,`super+z` / `ctrl+.`,`super+shift+z`. `ctrl+z` is added to undo when terminal suspend is disabled | `keybind.ts:233-234`, `src/config/index.tsx:289-295` |
| Word fwd/back | `alt+f`,`alt+right`,`ctrl+right` / `alt+b`,`alt+left`,`ctrl+left`. Select: `alt+shift+f/right`, `alt+shift+b/left` | `keybind.ts:235-238` |
| Delete word fwd/back | `alt+d`,`alt+delete`,`ctrl+delete` / `ctrl+w`,`ctrl+backspace`,`alt+backspace` | `keybind.ts:239-240` |
| Select all | `super+a` | `keybind.ts:241` |

Textarea "traits" (`src/prompt/traits.ts:16-28`, applied at `index.tsx:748-757`):
- `capture` = `["tab"]` in normal mode, or `["escape","navigate","submit","tab"]` in normal mode with autocomplete visible. In shell mode it is undefined.
- `status` = `"SHELL"` in shell mode.

#### Prompt-level commands

| Command id | Title (palette) | Default key | Behavior | Source |
|---|---|---|---|---|
| `prompt.submit` | Submit prompt | none | `submit()`, then clears dialogs | `index.tsx:429-443`, `keybind.ts:195` |
| `prompt.queue` | Queue prompt | `<leader>return` | `submit("queue")`. With autocomplete open, it only proceeds if the selected item is a server command (`completeQueueableCommand` selects it first) | `index.tsx:444-457`, `autocomplete.tsx:775-779`, `keybind.ts:196` |
| `prompt.clear` | Clear prompt | `ctrl+c` | Only bound while text ≠ "" (`index.tsx:950-956`), so ctrl+c on an empty prompt falls through to `app.exit` (`keybind.ts:48`). It saves to history if trimmed text ≥ 20 chars **or** any pasted/file/agent parts exist (`index.tsx:100,1513-1526`) | `keybind.ts:204` |
| `prompt.paste` | Paste | `ctrl+v` (`preventDefault: false`) | Reads the clipboard. An image becomes an attachment chip; text goes through the paste pipeline | `index.tsx:468-491`, `keybind.ts:205` |
| `prompt.editor` | Open editor (slash `/editor`) | `<leader>e` | §4.10 | `index.tsx:548-578`, `keybind.ts:91` |
| `prompt.editor_context.clear` | Remove editor context | none | Dismisses the current editor selection (enabled only when one exists) | `index.tsx:458-467` |
| `prompt.images.view` | View image attachments | `<leader>i` | Opens the image preview dialog | `index.tsx:492-498`, `keybind.ts:198` |
| `prompt.skills` | Skills (slash `/skills`) | none | `DialogSkill`. On select it inserts an `@skillid ` skill chip at the cursor; duplicates are ignored | `index.tsx:579-616` |
| `prompt.stash` / `.pop` / `.list` | Stash prompt / Stash pop / Stash list | none | §4.9 | `index.tsx:876-934` |
| `session.interrupt` | Interrupt session | `escape` | §4.8 | `index.tsx:499-530`, `keybind.ts:126` |
| `session.background` | Background blocking tools | `ctrl+b` | Enabled while running and autocomplete is closed. Calls `client.api.session.background({sessionID})` | `index.tsx:531-547`, `keybind.ts:127` |
| `session.move` | Manage workspaces (slash `/worktrees`) | none | `DialogWorkspaces` via `move.open()` | `index.tsx:617-626` |
| `session.cd` | Change working directory (slash `/cd <dir>`, takes arguments) | none | §4.7 | `index.tsx:262-312` |
| `prompt.history.previous/next` | Previous/Next prompt history | `up` / `down` | §4.5 | `index.tsx:1016-1089`, `keybind.ts:242-243` |
| (unnamed) `!` | Shell mode | `!` | §4.6 | `index.tsx:958-983` |

All of these are registered with `palette: true` and `bind: false` and grouped under "Prompt" or "Session" (`index.tsx:627-636`). They are enabled only while the prompt is not disabled.

The prompt **auto-focuses** whenever it is visible, enabled, and no dialog is open. It blurs and becomes unfocusable otherwise (`index.tsx:734-746`). Key-downs, mouse-downs, and pastes are swallowed while disabled (`index.tsx:1761-1766,1773-1777,1814-1818`).

### 4.5 Prompt history

`src/prompt/history.tsx`.
- File: `<state>/prompt-history.jsonl`, one JSON `PromptInfo` per line (`history.tsx:93`).
- Max **50** entries (`history.tsx:60`). On load, the TUI keeps the last 50 valid lines and rewrites the file to self-heal it (`history.tsx:94-101`).
- Append (`history.tsx:117-140`) deep-clones the entry and skips it if it is identical (JSON) to the last entry. It resets the cursor `index` to 0, appends one line, or rewrites the file when it trims past 50.
- What gets appended: every non-empty submission, including its `mode` (`index.tsx:1181,1304`), and `prompt.clear` of substantial drafts.

`move(direction, input)` (`history.tsx:106-116`): `index` is 0 for "present" and runs from −1 down to −len for older entries.
- It refuses to move if the current input is non-empty and differs from the entry at the current index. This means user edits block navigation. Note that `store.history.at(0)` at index 0 is the *oldest* entry, so the check compares input with that entry (see open questions).
- It stops past the oldest entry or above present. Moving back to 0 returns an empty prompt.

Up/Down handling (`index.tsx:1016-1089`). These run only when autocomplete is hidden.
- **Up**:
  - If the cursor offset ≠ 0: when the cursor is on the first visual row (`scrollY + visualRow === 0`), jump to offset 0; otherwise move up one line. The key is consumed.
  - If the cursor offset is 0: `history.move(-1)` → `setText`, restore the parts, chips, and **mode**, and put the cursor at 0.
- **Down**: mirror image. It needs the cursor at the end offset; on the last visual row it first jumps to the end. Loading an entry puts the cursor at the end.
- If `move` returns nothing, the command returns `false`, so the key falls through to other bindings (e.g. `session.child.first` = `down`).

### 4.6 Shell mode (`!`)

- **Enter**: type `!` when in normal mode, the autocomplete is hidden, and the **visual cursor offset is 0** (start of the empty prompt or the start of the text). The key is consumed and not inserted (`index.tsx:958-983`). This re-randomizes the shell placeholder.
- **Exit**:
  - `escape` (priority layer, `index.tsx:985-1001`)
  - `ctrl+c` when the text is empty
  - `backspace` at visual offset 0 (`index.tsx:1003-1014`)
  - `escape` via `session.interrupt` while running (it exits shell mode instead of interrupting, `index.tsx:509-512`)
  - automatically after a shell submit (`index.tsx:1309`)
  - loading a history item with `mode` normal
- **Visual changes**:
  - left bar and agent label color = `text.action.primary.selected`
  - metadata shows only `Shell`; no model, provider, or variant (`metadata.tsx:24,45-48`)
  - placeholder `Run a command… "<cmd>"`
  - footer `esc exit shell mode`
  - the textarea trait status is `SHELL`
- **Submit**: `client.api.session.shell({ sessionID, command: inputText })`, where `inputText` has pasted-chip placeholders expanded (`index.tsx:1305-1309`). An empty shell submit is a no-op except for `onEmptySubmit` (`index.tsx:1120-1121`). Shell prompts cannot be queued (`index.tsx:1134-1137`). Shell mode does not require the model to be available when inside a session (`index.tsx:1164`).

### 4.7 Autocomplete (`@` mentions, `/` commands, `/cd` directories)

#### Triggers (`autocomplete.tsx:780-818`, `src/prompt/display.ts:40-61`)

`onInput` runs on every content change. The rules:

1. If the text equals the last dismissed value, do nothing.
2. **Directory argument mode**: when the text before the cursor starts with `/<cmd> `, `<cmd>` is a keymap slash command with `arguments`, and `argumentAutocomplete(cmd)` returns `"directory"` (only `session.cd`, `index.tsx:1963`), show `"directory"` with the index just after the space (`src/prompt/directory-completion.ts:29-52`).
3. If the popup is already visible, hide it when the cursor moves to or before the trigger index, or when the text between the trigger and the cursor contains whitespace.
4. Otherwise (and offset > 0):
   - **`/` trigger**: the last `/` in the text before the cursor that sits at the start or follows whitespace, with no whitespace after it and no other `/` in the query. This shows `"command"`. Command options are listed **only when the trigger index is 0**. A mid-text `/` shows an empty popup with `No matching commands` (`autocomplete.tsx:541-546`).
   - **`@` trigger**: the last `@` at the start or after whitespace, with no whitespace in `@query`. This shows `"reference"`.

Filter text = the textarea range from `index+1` to the cursor (from `index` in directory mode) (`autocomplete.tsx:133-151`).

#### Popup geometry and style (`autocomplete.tsx:822-952`)

- Absolute position, `zIndex 100`. Placed **above** the prompt anchor: `top = anchor.y - height`, `left = anchor.x`, `width = anchor.width`.
- Borders: `SplitBorder`, i.e. `┃` on the left and right only, in `border.base`.
- Body: scrollbox with bg `background.raised.high`, scrollbar hidden.
- Height = `min(10, max(1, optionCount), max(1, anchor.y))` rows.
- The anchor position is polled every 50 ms while visible (`autocomplete.tsx:102-115`).
- Row: paddingX 1, laid out as `[display] [ description] [flex spacer ≥2] [kind label]`.
  - display: `truncateMiddle(display, width-4-(label ? len+2 : 0))`
  - description: `" " + description` with whitespace collapsed; `text.muted`; shrinks
  - kind label: `skill` / `agent` / `reference` in `text.muted`
  - Selected row: bg `background.action.primary.focused`, all text `text.action.primary.focused`.
  - Destructive-confirm row: bg `background.action.destructive.focused`, fg `text.action.destructive.focused`. The row text is replaced by the confirm string.
- Mouse: hover or mousedown selects the row; mouseup activates it.
- Empty states (`autocomplete.tsx:832-843`). The text is `text.muted`, or `text.feedback.error.base` when a reference search failed:
  - command: `No matching commands`
  - directory: `Searching…` / `Could not search directories. Keep typing to try again.` / `No matching directories`
  - reference: `Searching…` / `Could not search files. Keep typing to try again.` / `No matching files, agents, or references`
- The selection resets to 0 whenever the filter changes (`autocomplete.tsx:578-582`). Up/down wraps around (`autocomplete.tsx:584-588`).

#### Autocomplete keys (mode `"autocomplete"` layer, `autocomplete.tsx:667-735`; defaults from `keybind.ts`)

| Key | Action |
|---|---|
| `up`, `ctrl+p` / `down`, `ctrl+n` | prev / next (wrap) |
| `escape` | hide; the typed text stays |
| `ctrl+c` (`prompt.clear` id in this mode) | dismiss **and** delete the trigger token in command mode |
| `return` | select |
| `tab` | complete. On a directory entry it expands into it: inserts `@path/` (reference mode) or `path/` (directory mode) and keeps the popup open. Otherwise it selects |
| `ctrl+d` | destructive action. First press arms the row (shows the confirm text); a second press runs it (only on rows that have `destructive`) |
| `<leader>return` | `prompt.queue`. Works only on a queueable (server) command |

Submit is blocked while the popup is visible (`index.tsx:1118`). History up/down is disabled while it is visible.

#### `@` reference sources and ordering (`autocomplete.tsx:276-576`)

| Source | Display | Kind label | Insert |
|---|---|---|---|
| Skills: `data.location.skill.list(location)` | `@<id>`, description = skill.description | `skill` | skill chip `@id`; ignored if that skill is already present |
| References (non-hidden): `data.location.reference.list()` | `@<name>`, description = git repository or path | `reference` | file chip `@name` with uri `file://<reference.path>` |
| Agents: non-hidden agents with `mode !== "primary"` | `@<id>` | `agent` | agent chip `@id` |
| Files: `client.api.file.find({query: base, limit: 20, location})` | relative path, truncate-middle to `anchor.width-4`; directories flagged | none | file chip `@path`; uri `file://abs` |

- If the query's head (before the first `/`) exactly names a reference, only that reference row is shown and the file search is skipped (`autocomplete.tsx:278-284,534-536`).
- Line ranges: `@path#12` or `@path#12-20` (`src/prompt/parse.ts:1-12`). The suffix is carried into the chip text and into the uri query `?start=12&end=20`. An end is kept only if it is greater than the start (`autocomplete.tsx:244-274`).
- Ordering:
  - With an empty query: `[skills, references, agents, files]`, unsliced.
  - With a query: non-file items are run through `fuzzysort` (keys: value/display, aliases; threshold 0.5; limit 10). The score doubles when display starts with `@query`, then is multiplied by `(1+frecency)`. Files keep the server's ranking and are appended. The total is capped at **10** (`autocomplete.tsx:538-575`).
- Frecency (`src/prompt/frecency.tsx`):
  - Stored in `<state>/frecency.jsonl`, max 1000 entries.
  - Score = `frequency / (1 + daysSinceLastOpen)`. It is updated when a file chip is inserted (`autocomplete.tsx:241`).
  - Fuzzy scoring reads `option.path`, and none of the non-file options has one, so frecency effectively never reorders anything in this version.
- Mention path normalization for IDE `at_mentioned` events: relative to the cwd if inside it, otherwise absolute (`autocomplete.tsx:286-310,762-769`).

#### `/` command list (`autocomplete.tsx:466-504`)

Sources:
1. Every **reachable keymap command that has a `slash`** (`Keymap.useCommands`, `src/context/keymap.tsx:321-344`). Each name **and each alias** becomes its own row `/<name>`, with description = `command.description ?? command.title`.
   - Built-ins include: `/sessions` (aliases `resume`, `continue`), `/new`, `/clear`, `/open` (`projects`, `project`), `/models`, `/agents`, `/mcps`, `/variants` (`thinking`, `effort`), `/connect`, `/settings`, `/status`, `/update`, `/pair` (`web`), `/restart`, `/reload`, `/debug`, `/themes`, `/help`, `/exit` (`quit`, `q`) (`src/app.tsx:705-1097`).
   - Session-only: `/share`, `/rename`, `/timeline`, `/fork`, `/undo`, `/redo`, `/compact`, `/copy`, `/export` (`src/routes/session/index.tsx:914-1224`).
   - Prompt: `/cd`, `/editor`, `/skills`, `/worktrees`.
   - Plugins: `/btw`, `/plugins`, `/diff`, `/stats`, `/terminal`.
2. **Server commands**: `data.location.command.list(location)`, each `{name, description?}` (`../schema/src/command.ts:9-12`). They are shown as `/<name>` with the server's description and marked `queueable`. MCP prompts and skills reach this list only if the server includes them; the TUI makes no distinction.

Display rules:
- Rows are sorted by `display.localeCompare`.
- Every display is `padEnd(maxLen + 2)`, so descriptions line up in a column.
- With a query: `fuzzysort` over display/value, **description** (command mode only), and aliases; threshold 0; limit 10. A `/query` prefix match doubles the score.

Selecting a command (`autocomplete.tsx:615-626,744-760`):
1. The `/query` token is deleted first (`hide(true)`).
2. Then one of:
   - a keymap command **without** `arguments` runs immediately (`command.run`)
   - a command with `arguments`, or any server command: `insertSlash` replaces everything from 0 to the cursor with `/name ` and leaves the cursor after it for typing arguments

#### `/cd` directory completion (`src/prompt/directory-completion.ts`, `index.tsx:1961-1986`)

- Query interpretation:
  - `~` or `~/…` → under home
  - `..`, `../..` → resolved relative
  - `/…` → from the filesystem root
  - otherwise relative to the current location directory
- Listing: `client.api.file.list({location, path: dir})`, keeping directories whose basename starts with the last path segment (case-insensitive). Dot-directories are hidden unless the query is non-empty.
- If the query names a directory exactly, an exact-match row (the query itself) comes first.
- Values end in `/` and keep the user's prefix style (`~/`, `/`, `../`).
- With an empty query, **recent directories** for the project come first, deduped by absolute path (`src/prompt/directory-recents.ts`):
  - storage key `directory-recents`, max 10 per project, newest first
  - display uses the `~/…` form, description `recent`
  - `ctrl+d` arms the row, showing `Press ctrl+d to confirm`; a second `ctrl+d` removes it from the recents
- Select inserts the value and dismisses the popup. The current text is remembered as dismissed so the popup won't immediately reopen (`autocomplete.tsx:623-625`). Tab descends into the directory.

`/cd <dir>` on submit (`index.tsx:270-309`):
- An empty argument shows toast `Directory is required` (error).
- With no session: it resolves via `client.api.location.get` and sets the current location. While that is pending, the footer shows the directory.
- In a session: `client.api.session.move({sessionID, directory: input})`.
- Failure shows toast title `Failed to change directory`. Success records the directory in recents.

### 4.8 Interrupt while busy (esc twice)

`session.interrupt` (`escape`), enabled only when `status() === "running"` (`index.tsx:499-530`). It is ignored if the autocomplete is visible or the input is unfocused. In shell mode it only exits shell mode.

1. First `esc`: `interrupt = 1`. The footer shows `esc again to interrupt` (warning color).
2. Second `esc` (interrupt ≥ 2): `client.api.session.interrupt({ sessionID, resume: true })`, then `interrupt = 0`.
3. Each press schedules a reset of the counter to 0 after **5000 ms**. Timers are not cancelled, so the reset happens 5 s after the first press.

While armed, `showDetails` is false on terminals narrower than 80 columns, which hides the footer usage and shortcuts (`index.tsx:1589`).

### 4.9 Submission flow

`submit(delivery = "steer" | "queue")` (`index.tsx:1091-1395`). A re-entrancy guard (`submitting`) prevents double-Enter.

1. Sync `plainText` into the store (IME fix). Abort if a worktree is being created or the autocomplete is visible.
2. Compute `trimmed`. For an **empty** prompt: if there is no session, or in shell mode, or with `queue`, call `onEmptySubmit` (steer only) and stop. In a session (steer), empty Enter is handled at step 9.
3. **Exit words**: `exit`, `quit`, `:q` → app exit (`index.tsx:1122,1138-1141`).
4. Expand pasted chips into the full text (`inputText`) (`src/prompt/part.ts:10-15`).
5. **Argument slash commands** (keymap commands with `arguments`, matched by name or alias): clear the prompt, then `command.run(args)` (`index.tsx:173-183,1142-1146`).
6. **Queue restrictions**: shell mode, exit words, and argument slash commands produce toast `This prompt cannot be queued` (warning). A pending editor selection produces `Editor context cannot be queued` (`index.tsx:1134-1137,1153-1156`).
7. Requires an agent. With no model selected: toast `Connect an integration to send prompts` (warning, 3000 ms), and if not connected, open `DialogIntegration` (`index.tsx:316-325`). If the model is unavailable at this location: toast title `Model unavailable`, message `` `${providerID}/${modelID} is not available in this session's location` `` (`index.tsx:1164-1172`).
8. **Snapshot and clear synchronously**: `entry = {...prompt, mode}`, `resetComposer()`, `props.onSubmit()`. On failure the snapshot is restored, but only if the composer is still empty (`index.tsx:1174-1206`).
9. **No session (home)**:
   - Optional worktree destination (`move.getDirectory()`, which may create a worktree).
   - Optimistic `data.session.create({id, location, agent, model{providerID,id,variant}})`. The ID is minted client-side.
   - A gate then calls `client.api.session.environment({variables})` if terminal env vars exist.
   - Failure toast: `Failed to set up session` / `Creating a session failed`. The draft is moved back to home, the tab is closed or the route returns home (`index.tsx:1214-1271`).
10. **Empty Enter in a session**: commit the selection (`switchAgent` if it differs, then `switchModel`), then `onEmptySubmit()`, which steers the first queued prompt (`session/index.tsx:1533-1538`). Failure toast: `Failed to prepare session`.
11. `history.append(entry)`.
12. Dispatch:
    - **Shell**: `client.api.session.shell({sessionID, command})`.
    - **Server command** (`/name` where `name` is in `data.location.command.list`): commit the selection, then `client.api.session.command({sessionID, name, text: arguments, files, agents, skills?, delivery})`. Error toast: `Failed to run command`.
    - **Normal prompt**:
      1. `prepareAgent` (`switchAgent`).
      2. If the session has a pending revert: `client.api.session.revert.commit`. Error toast: `Failed to commit revert`.
      3. If there is a pending editor selection: `client.api.session.synthetic({text: formatEditorContext(...), resume: false})` *before* the prompt. Error toast: `Failed to send editor context`.
      4. `data.session.prompt({sessionID, text, files, agents, skills?, delivery, gate, prepare: commitModel})`. This is optimistic; on rejection it rolls back and toasts `Failed to send prompt`.
      5. Mark the editor selection as sent.
    - The model-switch error message is `` `Failed to switch model: ${msg}` `` (`index.tsx:1288`).
13. From home: re-stash any text typed mid-flight under the new session's draft, then `route.navigate({type:"session", sessionID})` (`index.tsx:1376-1392`).

Editor context text, verbatim template (`index.tsx:160-171`):
- No range: `<system-reminder>Note: The user opened the file "<path>". This may or may not be relevant to the current task.</system-reminder>\n`
- Ranges: `<system-reminder>Note: The user selected [Selection N: ]#L[-L2] from "<path>". ```<text>```\n\n … This may or may not be relevant to the current task.</system-reminder>\n`

**Queued messages**:
- `<leader>return` sends with `delivery: "queue"`.
- Queued inbox items render above the prompt as a dock: left border `┃` in `border.base`; bg `background.raised.base`, or `decrease(...)` on hover; padding 1/1/2/1. Text: `N queued` (`text.base`) + ` · <first text, newlines→spaces>` (`text.muted`). Clicking it opens the queued-prompts dialog (`session/index.tsx:1475-1476,2356-2387`).
- Empty Enter steers the next queued prompt.

### 4.10 Paste handling

Bracketed paste → `onPaste` (`index.tsx:1773-1796`):
1. Decode the bytes and normalize `\r\n` and lone `\r` to `\n`.
2. **Zero bytes** (Windows Terminal < 1.25 image paste) → dispatch `prompt.paste`, which reads the clipboard.
3. Otherwise `preventDefault` and enqueue `pasteInputText` on a serial paste queue. A queued paste is dropped if the session, mode, or text changed in the meantime (`index.tsx:373-392`).

`pasteInputText` (`index.tsx:1437-1475`):
1. **File-path paste** (`src/component/prompt/local-attachment.ts:29-59`). The trimmed text is tested as a single path (quotes stripped, `file://` URL decoded, backslash escapes removed on non-Windows). Failing that, it is tested as a whitespace-separated, shell-quoted list of up to **32** paths, where every path must be valid.
   - `http(s)://` URLs are ignored.
   - Supported by extension: `.avif .gif .jpeg .jpg .png .webp` → image; `.pdf` → PDF; `.svg` → text (`local-attachment.ts:61-70`).
   - Size budget: **20 MiB** total (`local-attachment.ts:5`).
   - Result: SVG → pasted-text chip `[SVG: <filename>]` holding the SVG source. Image or PDF → attachment chip `[Image N]` / `[PDF N]` with a `data:<mime>;base64,…` uri.
2. **Large paste collapse**: if `lineCount >= 3` or `length > 150` (on the trimmed text, with lines = number of `\n` + 1), and `config.prompt.paste !== "full"`:
   - Insert the chip `[Pasted ~N lines]` and keep the full text in `prompt.pasted`.
   - Exception: if the cursor is right after an existing paste chip (end or end+1) with identical content, that chip is **expanded** in place instead. So pasting the same thing twice expands it.
3. Otherwise insert the raw normalized text.

Attachment numbering (`src/prompt/attachment.ts:76-96`): the label is `[Image N]` or `[PDF N]`, where N = 1 + the highest existing N for that kind. Pasting an identical image (same uri and metadata) reuses the existing label.

`ctrl+v` (`prompt.paste`):
- Clipboard read prefers `image/png`, then `text/plain` (`src/clipboard.ts:24-52`).
- PNG → attachment with filename `clipboard`. Text → the same `pasteInputText` pipeline.
- Errors (`Clipboard read timed out`, limit exceeded, …) surface as toasts through the paste queue's catch (`index.tsx:388-390`).

Expanding a chip:
- Left-clicking with the cursor inside a pasted-text chip replaces the chip with its full text (`index.tsx:1814-1827,1424-1435`).
- Copying from the textarea expands placeholders through `getClipboardText` (`index.tsx:1799-1801`, `src/prompt/part.ts:3-8`).

Clipboard write (`src/clipboard.ts:53-62`):
- `destination: "all-available"`: the host clipboard plus the terminal (OSC 52) via the OpenTUI `createClipboard({host, terminal})`.
- NUL bytes are stripped.
- Success means either the host write succeeded or the terminal write was attempted.
- The host mechanisms (pbcopy, wl-copy, xclip, PowerShell) live inside `@opentui/core` and are not in this checkout.

### 4.11 External editor

`prompt.editor` (`<leader>e`, `/editor`) → `openEditor` (`src/editor.ts:14-42`):
1. The command is `$VISUAL || $EDITOR`. If neither is set it **silently does nothing**.
2. A temp file `os.tmpdir()/<Date.now()>.md` is written with the prompt text, with pasted chips expanded (`index.tsx:556-565`).
3. The command string is split on spaces into argv, with the file appended. `cwd` = project directory (unless `/`), else the location directory, else the process cwd. `shell: true` on Windows only.
4. The renderer suspends and its buffer is cleared, then the editor runs with stdio inherited.
5. A non-zero exit throws `` `Editor exited with code N` `` or `` `signal S` ``.
6. The file is read back; empty content → no change. The temp file is always removed and the renderer resumes.
7. `normalizePromptContent` strips one trailing `\n` or `\r\n` only when the body is a single line (`src/prompt/content.ts:1-13`).
8. The text is replaced, paste parts are dropped, mentions are realigned by text search (`src/prompt/mention.ts:20-74`), chips are rebuilt, and the cursor goes to the end.

### 4.12 Editor (IDE) context integration

`src/context/editor.ts`, `src/editor.ts:44-89`, `src/editor-zed.ts`. It is enabled by `config.prompt.editor` (default true, `index.tsx:223`).

Connection, in order:
1. Env `CLAUDE_CODE_SSE_PORT` or `OPENCODE_EDITOR_SSE_PORT` → `ws://127.0.0.1:<port>` (`editor.ts:117-120,361-374`).
2. Lock files at `~/.claude/ide/<port>.lock` whose `workspaceFolders` contain the cwd and whose transport is `ws`. The best match is the deepest folder, then the newest mtime. The auth token goes in header `x-claude-code-ide-authorization` (`src/editor.ts:44-84`, `editor.ts:390-398`).
3. If `ZED_TERM=true` or `TERM_PROGRAM=zed`: poll Zed's SQLite DB every 1 s. DB path: `$OPENCODE_ZED_DB`, `~/Library/Application Support/Zed/db/0-stable/db.sqlite`, or `~/.local/share/zed/db/0-stable/db.sqlite` (`editor.ts:121,176-210`, `editor-zed.ts:187-195`).

Protocol and lifecycle:
- WebSocket protocol: MCP `initialize` (protocol `2025-11-25`) → `notifications/initialized`. It handles `selection_changed` (sets the selection) and `at_mentioned` (inserts a file mention chip with a line range) (`editor.ts:217-262`).
- Reconnect backoff: `min(1000·2^(n-1), 10000)` ms (`editor.ts:276-282`).
- `labelState`: `none` / `pending` / `sent` (`editor.ts:343-346`). This drives the footer file label color. The home route clears the selection on mount (`home.tsx:48-50`).

### 4.13 Stash and drafts

- **Stash** (`src/prompt/stash.tsx`):
  - Stored in `<state>/prompt-stash.jsonl` as `{prompt, timestamp}`, max 50.
  - `Stash prompt` pushes the current prompt and clears the composer.
  - `Stash pop` restores the last entry (text, parts, chips) with the cursor at the end.
  - `Stash list` opens `DialogStash`; on select it restores that entry.
  - The commands are enabled only when text exists (stash) or the stash is non-empty (pop and list) (`index.tsx:876-934`).
- **Per-tab drafts** (`src/component/prompt/draft-stash.ts`): an in-memory `Map<sessionID|undefined, {prompt, cursor}>`.
  - Saved on unmount if text is present (`index.tsx:725-732`).
  - Taken (consumed) on mount when the prompt is empty (`index.tsx:714-723`).
- **External append**: the `tui.prompt.append` event inserts text at the cursor when its directory matches, then moves to the buffer end (`index.tsx:338-349`).

### 4.14 PromptRef API (used by routes and other components)

`focused`, `current`, `mode`, `setMode`, `set(prompt)` (sets text, restores chips, cursor to end), `reset()`, `blur()`, `focus()`, `submit()` (`index.tsx:88-98,668-700`). Registered globally via `usePromptRef` (`src/context/prompt.tsx:4-17`).

### 4.15 `/btw` side question (plugin)

`src/feature-plugins/prompt/btw.tsx`. Command `session.aside`, title `Ask a side question`, slash `/btw <question>`.

1. Outside a session it shows toast `Open a session first`.
2. With no argument it opens a prompt dialog titled `/btw` with placeholder `Ask anything`.
3. It calls `client.session.generate({sessionID, prompt: instructions + "\n\n" + question})`. The instructions text is verbatim at `btw.tsx:17-21`.
4. While the request is pending, the footer shows a spinner labelled `/btw`.
5. The result opens a large, centered dialog:
   - Header: bold `/btw`, the question in `text.muted` (truncated), and `esc` at the right.
   - Body: a markdown scrollbox with max height `max(8, floor(h·0.6))` and bg `background.raised.high`.
   - Footer: `c copy` (after copying, it reads `✓ copied` in `text.feedback.success.base`) and `j/k ↑/↓ scroll` (`btw.tsx:88-171`).

### 4.16 Responsive thresholds summary

| Width/height | Effect | Source |
|---|---|---|
| termW < 44 | prompt padding 1 (else 2); placeholder width `w-5`; metadata hides the agent and provider and uses width `w-9`; footer `esc shell`; footer shortcuts hidden when there is no usage | `index.tsx:1602,1668-1669`, `metadata.tsx:26,110-111`, `footer.tsx:53,111` |
| termW ≥ 44 | placeholder width `min(75, w-4)-5`; metadata width `w-13` | `index.tsx:1602`, `metadata.tsx:26` |
| termW < 80 | while interrupt is armed, footer details are hidden | `index.tsx:1589` |
| termW / 3 | editor file label max = `max(12, min(48, floor(w/3)))` | `index.tsx:252` |
| termH / 3 | textarea maxHeight = `max(6, floor(h/3))` | `index.tsx:1651` |
| termH / 4 | image tile height = `max(4, min(8, floor(h/4)))` | `index.tsx:397` |
| Home | prompt column maxWidth 75 | `home.tsx:101` |
| Autocomplete | ≤ 10 rows, limited by the space above the anchor | `autocomplete.tsx:822-827` |

### Open questions (prompt)

1. **Extmark semantics**: the exact OpenTUI behavior of `virtual: true` extmarks lives in `@opentui/core`, which is not in this checkout. Specifically: does the cursor skip over the chip, does backspace delete the whole chip, and does a selection snap to it? A Rust port has to define "atomic chip" behavior itself.
2. **History index check**: `history.move` compares the input with `store.history.at(store.index)`. At index 0 that is the *oldest* entry, not "present" (`history.tsx:108-110`). The effect is that Up from a non-empty, never-navigated prompt is blocked unless the text equals the oldest entry. Is this intended, or should the port mirror it exactly?
3. `prompt.paste` is bound with `preventDefault: false` (`keybind.ts:205`). Does the terminal or textarea *also* perform a native paste on `ctrl+v`, which could double-insert? This needs runtime verification.
4. **Clipboard host fallbacks** (pbcopy, xclip, xsel, wl-copy, PowerShell; image read on Linux/Windows) and OSC 52 details are implemented inside `@opentui/core` `createHostClipboard`/`createRendererClipboardAdapter` and are not visible here.
5. Whether server `command.list` includes MCP prompts and skills is decided server-side. The TUI only sees `{name, description}`.
6. Frecency never affects ranking in practice, because file options are not fuzzied and the other options lack `path` (`autocomplete.tsx:540-571`). Should the port keep this no-op?
7. A `/` typed mid-text (after whitespace) opens an empty "No matching commands" popup (`autocomplete.tsx:541-546`, `display.ts:52-61`). This looks like a quirk; confirm before replicating.
8. The `openEditor` rejection (non-zero exit) is not caught in `prompt.editor`'s `run` (`index.tsx:553-577`). How the keymap surfaces it (toast or log) is unknown.
9. The interrupt reset timers are not cleared (`index.tsx:517-519`). An older timer can reset the counter between two presses that are less than 5 s apart. Replicate exactly or debounce?
10. The exact rendering of keybinding labels in the footer hints (e.g. `shift+tab` vs `⇧⇥`) depends on keymap formatting; see the § keybindings spec.

---

## 5. Keybindings and commands

Citation conventions: plain `path:line` is relative to `packages/tui`. `../cli/...` is relative to `packages/`. `KM:` means the published npm package `@opentui/keymap@0.5.17` (pinned in the root `package.json:57`), which the TUI uses for parsing, dispatch and formatting. Its files are `KM:src/addons/index.js` (parser and addons), `KM:src/extras/index.js` (lookup and formatting), `KM:src/index.js` (runtime), `KM:chunks/index-7g381cth.js` (key primitives) and `KM:src/solid/index.js`. The Rust port has to re-implement the KM behaviour described here.

### 5.1 Architecture overview

- **Config layer.** `src/config/keybind.ts` declares every configurable binding as `Definitions: { [commandId]: { default, description } }` (`src/config/keybind.ts:45-299`). User overrides from `keybinds` (`src/config/index.tsx:78`) are merged per id: a user value replaces the default and nothing is merged (`src/config/keybind.ts:337-346`). The result is turned into a lookup with `get(id) -> Binding[]` (`src/config/index.tsx:307-309`).
- **Runtime layer.** `src/context/keymap.tsx` wraps an OpenTUI keymap (`createDefaultOpenTuiKeymap`, `src/context/keymap.tsx:66`). Components register **layers** reactively with `Keymap.createLayer(() => KeymapLayer)` (`src/context/keymap.tsx:204-251`). A layer holds commands, inline key commands and a list of config-binding ids.
- **Commands** come in two kinds:
  - *Named* commands have an `id`. They appear in the palette, slash completion and dispatch. Their keys come from the config.
  - *Inline* commands have no id and must have a `bind` string (`src/context/keymap.tsx:253-275`).
- **Discovery UIs.**
  - The command palette (`src/component/command-palette.tsx`) shows reachable commands with `palette: true`.
  - Slash completion (`src/component/prompt/autocomplete.tsx:476-500`) shows reachable commands with `slash`.
  - Key hints use `Keymap.useShortcuts()` (`src/context/keymap.tsx:277-309`).

### 5.2 Config schema and parsing rules

- **Binding value grammar** (`src/config/keybind.ts:8-33`). A value is one of:
  - `false`
  - `"none"`
  - a string
  - a KeyStroke object `{name, ctrl?, shift?, meta?, super?, hyper?}`
  - a BindingObject `{key: string|KeyStroke, event?: "press"|"release", preventDefault?: boolean, fallthrough?: boolean, ...extra}`
  - an array of strings, KeyStrokes or BindingObjects
- **Disabling.** `false`, `"none"` and `[]` all mean no bindings (`KM:src/extras/index.js:60-77`). The test `test/config-v2.test.tsx:251-255` confirms that `"none"` yields `get(id) == []`.
- **Defaults and descriptions.** If a key is not overridden, `item.default` is decoded instead (`src/config/keybind.ts:343`). Each binding gets `desc = Definitions[id].description` unless it already has a `desc` (`src/config/keybind.ts:352-357`).
- **Unknown keys.**
  - `TuiKeybind.parse` throws `` `Unrecognized keybind${s}: ${list}` `` (`src/config/keybind.ts:337-339`).
  - The `Info` schema decode silently drops unknown keybind keys. For example, `{"app.heap_snapshot": ...}` decodes to `{keybinds: {}}` (`test/config-v2.test.tsx:266-268`).
  - v1 snake_case names are rejected by v2 (`test/config-v2.test.tsx:148`).
- **Comma lists are not split at lookup.** `get("session.tab.next")` returns `[{key:"ctrl+tab,alt+down"}]` (`test/config-v2.test.tsx:160`). The split happens later, when a binding is registered (see 5.3).
- **Platform rule** (`src/config/index.tsx:287-295`), applied when `terminalSuspend` is false (`process.platform === "win32"`, `src/app.tsx:209`):
  - `terminal.suspend` is forced to `"none"`, even if the user set it.
  - If `input.undo` is not overridden, it becomes `"ctrl+z,ctrl+-,super+z"`, i.e. `ctrl+z` prepended and deduplicated (`test/config-v2.test.tsx:270-281`).
- **Leader timeout.** Set by `leader.timeout` (Int > 0, `src/config/index.tsx:82-88`). It resolves to `2000` ms by default (`src/config/index.tsx:310`). The keymap falls back to `config.leader?.timeout ?? config.leader_timeout ?? 2000` (`src/context/keymap.tsx:143`). KM's own default of 1500 is never used (`KM:src/addons/index.js:1054`).
- **Kitty keyboard protocol** is requested (`useKittyKeyboard: {}`, `src/app.tsx:239`). Without it, `ctrl+tab`, `ctrl+1..0`, `shift+return`, `ctrl+shift+t` and `super+*` are indistinguishable. The port must enable crossterm keyboard-enhancement flags.

### 5.3 Key-string syntax (KM default parser)

1. **Comma alternatives come first.** `registerCommaBindings` (`src/context/keymap.tsx:79`) splits the string on `,` and trims each part. An empty entry throws `Invalid key sequence "...": comma-separated bindings cannot contain empty entries` (`KM:src/addons/index.js:562-580`). Because of this, a literal comma key must be written `comma`.
2. **Alias expansion.** A custom expander (`src/context/keymap.tsx:80-88`) rewrites whole words, case-insensitively, when they are bounded by start or `[+,\s>]` and followed by end or `[+,\s<]`:
   - `enter`→`return`
   - `esc`→`escape`
   - `pgdown`→`pagedown`
   - `pgup`→`pageup`

   `test/keymap.test.tsx:8-45` checks that `pgup`/`pgdown` work.
3. **Per-string parsing** (`KM:src/addons/index.js:327-415`). The parser scans left to right; each piece becomes one stroke of a **sequence**:
   - `<name>` is a token. Only `<leader>` is registered (`src/context/keymap.tsx:137-146`). An unknown token gives `unknownTokens`, meaning the binding is invalid.
   - `{name}` is a pattern (unused by the TUI).
   - `mod+mod+key` is a modified stroke. Modifiers are `ctrl|control`, `shift`, `meta|alt|option` (all three map to **meta**), `super` and `hyper` (`KM:src/addons/index.js:262-325`). The key after the last `+` is either the longest named key at that position or a single char.
   - Named single-stroke keys include:
     - navigation: `up down left right home end pageup pagedown insert delete`
     - editing: `escape return enter linefeed tab backspace space clear`
     - punctuation names: `lt gt plus minus equal comma period slash backslash semicolon quote backquote leftbracket rightbracket`
     - function keys `f1..f99`, and `kp*`, media and modifier names (`KM:src/addons/index.js:1-106, 179-189`)
   - Any other char is a single-char key. So `gg` is the two-stroke sequence g,g. `<leader>n` is leader,n. `<leader>return` is leader,return. `ctrl+xn` would be ctrl+x,n.
   - Special cases:
     - A string containing both `+` and whitespace at index 0 is parsed as ONE stroke split only on `+` (`KM:src/addons/index.js:329-334`). So `"ctrl+x n"` is NOT a sequence.
     - A bare space char parses as the `space` key, so `"g g"` = g,space,g.
     - A literal `"+"` is the `+` key.
     - Key names are normalized to lowercase (`KM:chunks/index-7g381cth.js:11-17`).
4. **Event matching** is on `{name, ctrl, shift, meta, super, hyper}` (`KM:chunks/index-7g381cth.js:112-114`).
   - `registerBaseLayoutFallback` (`src/context/keymap.tsx:89`) also tries the key's base-layout name (`event.baseCode`), so `ctrl+c` works on non-Latin layouts (`KM:src/addons/opentui/index.js:21-38`).
   - `alt+x` in config means `meta: true` on the event. Crossterm `ALT` maps to `meta`.

### 5.4 Leader key

- **Definition.** Defaults to `leader: "ctrl+x"` (`src/config/keybind.ts:41,46`). Only `get("leader")[0].key` is used (`src/context/keymap.tsx:137`). It is registered as token `leader` with a timer (`registerTimedLeader`, `src/context/keymap.tsx:138-146`).
- **Timeout.** Each change to the pending sequence whose first stroke is the leader restarts a timer of `timeoutMs`. When it expires, `clearPendingSequence()` runs and nothing executes (`KM:src/addons/index.js:1048-1103`).
- **Pending sequence** (any multi-stroke binding, e.g. leader or `gg`):
  - The keystroke that starts or advances a sequence is consumed (`KM:src/index.js:2794-2803`).
  - `escape` while pending clears the sequence and is consumed (`registerEscapeClearsPendingSequence`, `src/context/keymap.tsx:90`; `KM:src/addons/index.js:621-634`).
  - `backspace` while pending pops the last stroke and is consumed (`src/context/keymap.tsx:91`; `KM:src/addons/index.js:546-558`).
  - A key that continues no binding clears the sequence and is **not** consumed. It falls through to normal handling, e.g. it is typed into the prompt (`KM:src/index.js:2935-2952`, `handled: reason !== "sequence-miss"` at `KM:src/index.js:2733-2738`).
- **Visual indicator.** There is no which-key popup or "leader pending" text. `Keymap.useLeaderActive()` is true while `pending[0].tokenName === "leader"` (`src/context/keymap.tsx:316-319`). The prompt then becomes "muted" (`src/component/prompt/index.tsx:192-193`):
  - the text and focused text colour switch to `theme.text.muted` (`src/component/prompt/index.tsx:1748-1749`)
  - the accent/highlight colour becomes `theme.border.base` instead of the agent colour (`src/component/prompt/index.tsx:1565-1566`)
  - the metadata row gets `muted` (`src/component/prompt/index.tsx:1840`)
- **Plugin API.** Plugins can read `keymap.pending()` and `keymap.active()` (`src/plugin/api.tsx:194-197`).
- **which-key ids.** The `which-key.*` ids are configurable (`src/config/keybind.ts:288-298`) but nothing in TUI `src/` registers them (grep shows no consumer).
- **Embedded terminal.** The terminal pane forwards raw keys except the leader and keys pressed while the leader is pending (`src/component/terminal-pane.tsx:140-149`, intercept priority 100).

### 5.5 Complete v2 keybind table (`src/config/keybind.ts`)

Key strings are verbatim (comma = alternatives). "Consumer" is where the id is bound or registered:

- **G** = mode `global`
- **B** = mode `base`
- **M** = `modal`
- **C** = `composer`
- **A** = `autocomplete`
- **F** = `form`
- **∅** = no consumer in TUI `src/` (bindable but inert)

| id | default | description | consumer / scope |
|---|---|---|---|
| leader | `ctrl+x` | Leader key for keybind combinations | token, `src/context/keymap.tsx:137-146` |
| app.exit | `ctrl+c,ctrl+d,<leader>q` | Exit the application | B layer enabled when the prompt is unfocused or empty, `src/app.tsx:1245-1252`. Also overridden by permission/form/diff (5.8) |
| app.clear | `ctrl+l` | Clear the screen in mini | mini only, `src/mini/footer.view.tsx:553` |
| app.debug | `none` | Toggle debug panel | B `src/app.tsx:170` |
| app.console | `none` | Toggle console | B `src/app.tsx:171` |
| app.scrap | `none` | Open scrap screen | ∅ |
| app.toggle.animations | `none` | Toggle animations | B `src/app.tsx:174` |
| app.toggle.file_context | `none` | Toggle file context | B `src/app.tsx:175` |
| app.toggle.diffwrap | `none` | Toggle diff wrapping | B `src/app.tsx:176` |
| app.toggle.paste_summary | `none` | Toggle paste summary | B `src/app.tsx:177` |
| command.palette.show | `ctrl+p` | List available commands | B `src/app.tsx:144` |
| help.show | `none` | Open help dialog | B `src/app.tsx:167` |
| docs.open | `none` | Open documentation | B `src/app.tsx:168` |
| opencode.settings | `none` | Open settings | B `src/app.tsx:157` |
| server.pair | `none` | Pair device | B `src/app.tsx:160` |
| service.restart | `none` | Restart service | B `src/app.tsx:161` |
| location.reload | `none` | Reload configuration | B `src/app.tsx:162` |
| diff.open | `none` | Open diff viewer | B `src/app.tsx:169`. Command at `src/feature-plugins/system/diff-viewer.tsx:1195-1225` |
| diff.close | `escape,q` | Close diff viewer | diff viewer layer (B), `src/feature-plugins/system/diff-viewer.tsx:560-565,798-800` |
| diff.down | `j,down` | Move diff viewer down | same, `src/feature-plugins/system/diff-viewer.tsx:566-574` |
| diff.up | `k,up` | Move diff viewer up | `src/feature-plugins/system/diff-viewer.tsx:575-583` |
| diff.page.down | `pagedown,ctrl+f` | Page diff viewer down | `src/feature-plugins/system/diff-viewer.tsx:584-589` |
| diff.page.up | `pageup,ctrl+b` | Page diff viewer up | `src/feature-plugins/system/diff-viewer.tsx:590-595` |
| diff.half_page.down | `ctrl+d` | Scroll diff viewer down half a page | `src/feature-plugins/system/diff-viewer.tsx:596-601` |
| diff.half_page.up | `ctrl+u` | Scroll diff viewer up half a page | `src/feature-plugins/system/diff-viewer.tsx:602-607` |
| diff.first | `gg,home` | Go to the start of the diff | `src/feature-plugins/system/diff-viewer.tsx:608-616` |
| diff.last | `shift+g,end` | Go to the end of the diff | `src/feature-plugins/system/diff-viewer.tsx:617-625` |
| diff.toggle | `none` | Deprecated: file tree is mouse-controlled | ∅ (retained name, `src/config/keybind.ts:74`; `test/config-v2.test.tsx:258-264`) |
| diff.expand | `none` | Deprecated: file tree is mouse-controlled | ∅ |
| diff.expand_all | `none` | Deprecated: file tree is mouse-controlled | ∅ |
| diff.collapse | `none` | Deprecated: file tree is mouse-controlled | ∅ |
| diff.switch_focus | `none` | Deprecated: keyboard navigation always controls the diff | ∅ |
| diff.next_hunk | `]` | Jump to next diff hunk | `src/feature-plugins/system/diff-viewer.tsx:626-633` |
| diff.previous_hunk | `[` | Jump to previous diff hunk | `src/feature-plugins/system/diff-viewer.tsx:634-641` |
| diff.next_file | `n,alt+down` | Jump to next diff file | `src/feature-plugins/system/diff-viewer.tsx:642-649` |
| diff.previous_file | `p,alt+up` | Jump to previous diff file | `src/feature-plugins/system/diff-viewer.tsx:650-657` |
| diff.toggle_file_tree | `b` | Toggle diff viewer file tree | `src/feature-plugins/system/diff-viewer.tsx:666-675` |
| diff.single_patch | `s` | Toggle single patch view | `src/feature-plugins/system/diff-viewer.tsx:676-697` |
| diff.switch_source | `d` | Switch diff viewer source | `src/feature-plugins/system/diff-viewer.tsx:698-705` |
| diff.toggle_view | `v` | Toggle diff viewer split or unified view | `src/feature-plugins/system/diff-viewer.tsx:706-717` |
| diff.mark_reviewed | `m` | Toggle selected diff file reviewed | `src/feature-plugins/system/diff-viewer.tsx:658-665` |
| diff.help | `?,shift+?,shift+/` | Show more diff viewer shortcuts | `src/feature-plugins/system/diff-viewer.tsx:718-725` |
| prompt.editor | `<leader>e` | Open external editor | B prompt layer, `src/component/prompt/index.tsx:650-664` |
| theme.switch | `none` | List available themes | B `src/app.tsx:164` |
| theme.switch_mode | `none` | Switch between light and dark theme mode | B `src/app.tsx:165` |
| theme.mode.lock | `none` | Lock or unlock theme mode | B `src/app.tsx:166` |
| session.sidebar.toggle | `<leader>b` | Toggle sidebar | G `src/component/session-frame.tsx:182-194` |
| pane.focus.left | `<leader>left` | Focus session pane | G `src/component/session-frame.tsx:164-180` (enabled only when a terminal/panel is on the right and there is no dialog) |
| pane.focus.right | `<leader>right` | Focus right pane | same |
| terminal.select | `<leader>down` | Select terminal | G `src/component/session-frame.tsx:219-230` |
| terminal.toggle | `<leader>t` | Toggle terminal pane | G `src/component/session-frame.tsx:197-217` |
| terminal.close | `<leader>up` | Close terminal pane | G `src/component/session-frame.tsx:231-242` |
| session.toggle.scrollbar | `none` | Toggle session scrollbar | B session layer, `src/routes/session/index.tsx:1055-1067,1332-1334` |
| opencode.status | `<leader>s` | View status | B `src/app.tsx:158` |
| opencode.debug | `none` | View debug info | B `src/app.tsx:163` |
| session.export | `<leader>x` | Export session to editor | B `src/routes/session/index.tsx:1220-1262` |
| session.copy | `none` | Copy session transcript | B `src/routes/session/index.tsx:1199-1219` |
| session.copy.id | `none` | Copy session ID | B `src/routes/session/index.tsx:1187-1198` |
| session.move | `none` | Manage workspaces | B prompt, `src/component/prompt/index.tsx:617-626,662` |
| session.new | `<leader>n` | Create a new session | G `src/app.tsx:110,1228-1231` |
| session.list | `<leader>l` | List all sessions | G `src/app.tsx:110` |
| open.menu | `ctrl+o` | Open recent sessions and projects | G `src/app.tsx:110` |
| session.tab.next | `ctrl+tab,alt+down` | Switch to next open session tab | G, enabled only when tabs are enabled, `src/app.tsx:112-129,1233-1237` |
| session.tab.previous | `ctrl+shift+tab,alt+up` | Switch to previous open session tab | same |
| session.tab.history.back | `none` | Go back in session tab history | ∅ |
| session.tab.history.forward | `ctrl+i` | Go forward in session tab history | ∅ |
| session.tab.next_unread | `alt+shift+down` | Switch to next unread session tab | G tabs |
| session.tab.previous_unread | `alt+shift+up` | Switch to previous unread session tab | G tabs |
| session.tab.close | `<leader>w` | Close current session tab | G tabs |
| session.tab.reopen | `ctrl+shift+t` | Reopen last closed session tab | G tabs |
| session.timeline | `<leader>g` | Show session timeline | B `src/routes/session/index.tsx:932-945` |
| session.fork | `none` | Fork session from message | B `src/routes/session/index.tsx:947-962` |
| session.rename | `ctrl+r` | Rename session | B `src/routes/session/index.tsx:917-931`. Also a session-list dialog action, `src/component/dialog-session-list.tsx:281-285` |
| session.delete | `ctrl+d` | Delete session | M session-list action (press twice), `src/component/dialog-session-list.tsx:254-279` |
| session.share | `none` | Share current session | B, shows a "not implemented" toast, `src/routes/session/index.tsx:909-916,700-703` |
| session.unshare | `none` | Unshare current session | ∅ |
| session.interrupt | `escape` | Interrupt current session | B prompt, `src/component/prompt/index.tsx:499-530,659` |
| session.background | `ctrl+b` | Background blocking session tools | B prompt `src/component/prompt/index.tsx:531-547,660`, and session `src/routes/session/index.tsx:1264-1272` |
| session.compact | `<leader>c` | Compact the session | B `src/routes/session/index.tsx:964-987` |
| session.aside | `none` | Ask a side question | G plugin `src/feature-plugins/prompt/btw.tsx:48-80` |
| session.cd | `none` | Change working directory | G prompt `src/component/prompt/index.tsx:262-311` |
| session.queued_prompts | `<leader>q` | Manage queued prompts | B, enabled when queue > 0, `src/routes/session/index.tsx:1284-1290` |
| queued_prompt.undo | `ctrl+u` | Undo queued prompt | M queued-prompts dialog action, `src/routes/session/index.tsx:671-690` |
| queued_prompt.delete | `ctrl+d` | Delete queued prompt | M, `src/routes/session/index.tsx:661-670` |
| session.toggle.exploration_grouping | `none` | Toggle related tool call grouping | B `src/routes/session/index.tsx:1069-1081` |
| session.verbosity.cycle | `none` | Cycle transcript verbosity | B `src/routes/session/index.tsx:1083-1096` |
| session.child.first | `down` | Toggle subagent picker | B `src/routes/session/index.tsx:1274-1283` |
| session.child.next | `right` | Go to next child session | ∅ |
| session.child.previous | `left` | Go to previous child session | ∅ |
| session.parent | `up` | Go to parent session | B, enabled when `parentID` is set, `src/routes/session/index.tsx:1291-1309` |
| session.pin.toggle | `ctrl+f` | Pin or unpin session in the session list | M session-list action, hidden when tabs are enabled, `src/component/dialog-session-list.tsx:248-253` |
| session.quick_switch.1 … .9 | `<leader>1` … `<leader>9` | Switch to session in quick slot N | G, enabled when tabs are disabled, `src/app.tsx:131-141,1239-1243` |
| session.tab.select.1 … .9 | `<leader>N,ctrl+N` (N=1..9) | Switch to session tab N | G tabs, `src/app.tsx:112-129` |
| session.tab.select.10 | `<leader>0,ctrl+0` | Switch to session tab 10 | G tabs |
| stash.delete | `ctrl+d` | Delete stash entry | M stash dialog action (press twice), `src/component/dialog-stash.tsx:46,76` |
| model.dialog.provider | `ctrl+a` | Open provider list from model dialog | M `src/component/dialog-model.tsx:146` |
| model.dialog.favorite | `ctrl+f` | Toggle model favorite status | M `src/component/dialog-model.tsx:158` |
| model.list | `<leader>m` | List available models | B `src/app.tsx:145` |
| model.cycle_recent | `f2` | Next recently used model | B `src/app.tsx:146` |
| model.cycle_recent_reverse | `shift+f2` | Previous recently used model | B |
| model.cycle_favorite | `none` | Next favorite model | B |
| model.cycle_favorite_reverse | `none` | Previous favorite model | B |
| mcp.list | `none` | List MCP servers | B `src/app.tsx:151` |
| provider.connect | `none` | Connect integration | B `src/app.tsx:156` |
| agent.list | `<leader>a` | List agents | B `src/app.tsx:150` |
| agent.cycle | `shift+tab` | Next agent | B `src/app.tsx:152` (`test/keybind.test.ts:4-7`) |
| agent.cycle.reverse | `none` | Previous agent | B |
| variant.cycle | `ctrl+t` | Cycle model variants | B `src/app.tsx:154` |
| variant.list | `none` | List model variants | B `src/app.tsx:155` |
| session.page.up | `pageup,ctrl+alt+b` | Scroll messages up by one page | G `src/routes/session/index.tsx:782-789,1321-1325` |
| session.page.down | `pagedown,ctrl+alt+f` | Scroll messages down by one page | G |
| session.line.up | `ctrl+alt+y` | Scroll messages up by one line | G |
| session.line.down | `ctrl+alt+e` | Scroll messages down by one line | G |
| session.half.page.up | `ctrl+alt+u` | Scroll messages up by half page | G |
| session.half.page.down | `ctrl+alt+d` | Scroll messages down by half page | G |
| session.first | `ctrl+g,home,alt+home` | Navigate to first message | B, two layers, `src/routes/session/index.tsx:827-895,1327-1334` |
| session.last | `ctrl+alt+g,end` | Navigate to last message | B |
| session.message.next | `none` | Navigate to next message | B |
| session.message.previous | `none` | Navigate to previous message | B |
| session.message.user.next | `none` | Navigate to next user message | B |
| session.message.user.previous | `none` | Navigate to previous user message | B |
| session.messages_last_user | `alt+end` | Navigate to last user message | B |
| messages.copy | `<leader>y` | Copy message | B |
| session.undo | `<leader>u` | Undo message | B |
| session.redo | `<leader>r` | Redo message | B, enabled when a revert exists |
| session.toggle.thinking | `none` | Toggle thinking blocks visibility | B |
| prompt.submit | `none` | Submit prompt | B prompt, `src/component/prompt/index.tsx:652` |
| prompt.queue | `<leader>return` | Queue prompt | B priority 1 `src/component/prompt/index.tsx:645-649`. Also A `src/component/prompt/autocomplete.tsx:670` |
| prompt.editor_context.clear | `none` | Clear editor context | B prompt |
| prompt.images.view | `<leader>i` | View image attachments | B prompt |
| prompt.skills | `none` | Open skill selector | B prompt |
| prompt.stash | `none` | Stash prompt | B prompt |
| prompt.stash.pop | `none` | Pop stashed prompt | B prompt |
| prompt.stash.list | `none` | List stashed prompts | B prompt |
| prompt.clear | `ctrl+c` | Clear input field | B, input-targeted, only when text is non-empty, `src/component/prompt/index.tsx:950-956`. A: dismiss `src/component/prompt/autocomplete.tsx:697-704`. F: `src/routes/session/form.tsx:555-575` |
| prompt.paste | `{key:"ctrl+v",preventDefault:false}` | Paste from clipboard | B, input-targeted, `src/component/prompt/index.tsx:942-948`. F `src/routes/session/form.tsx:538-553` |
| input.submit | `return` | Submit input | managed textarea layer, `src/context/keymap.tsx:92-135` (active when the focused editor is a Textarea and not an Input) |
| input.newline | `shift+return,ctrl+return,alt+return,ctrl+j` | Insert newline in input | same |
| input.move.left | `left,ctrl+b` | Move cursor left in input | same |
| input.move.right | `right,ctrl+f` | Move cursor right in input | same |
| input.move.up | `up` | Move cursor up in input | same |
| input.move.down | `down` | Move cursor down in input | same |
| input.select.left | `shift+left` | Select left in input | same |
| input.select.right | `shift+right` | Select right in input | same |
| input.select.up | `shift+up` | Select up in input | same |
| input.select.down | `shift+down` | Select down in input | same |
| input.line.home | `ctrl+a` | Move to start of line in input | same |
| input.line.end | `ctrl+e` | Move to end of line in input | same |
| input.select.line.home | `ctrl+shift+a` | Select to start of line in input | same |
| input.select.line.end | `ctrl+shift+e` | Select to end of line in input | same |
| input.visual.line.home | `alt+a` | Move to start of visual line in input | same |
| input.visual.line.end | `alt+e` | Move to end of visual line in input | same |
| input.select.visual.line.home | `alt+shift+a` | Select to start of visual line in input | same |
| input.select.visual.line.end | `alt+shift+e` | Select to end of visual line in input | same |
| input.buffer.home | `none` | Move to start of buffer in input | same |
| input.buffer.end | `none` | Move to end of buffer in input | same |
| input.select.buffer.home | `shift+home` | Select to start of buffer in input | same |
| input.select.buffer.end | `shift+end` | Select to end of buffer in input | same |
| input.delete.line | `ctrl+shift+d` | Delete line in input | same |
| input.delete.to.line.end | `ctrl+k` | Delete to end of line in input | same |
| input.delete.to.line.start | `ctrl+u` | Delete to start of line in input | same |
| input.backspace | `backspace,shift+backspace` | Backspace in input | same |
| input.delete | `ctrl+d,delete,shift+delete` | Delete character in input | same |
| input.undo | `ctrl+-,super+z` (win32: `ctrl+z,ctrl+-,super+z`) | Undo in input | same |
| input.redo | `ctrl+.,super+shift+z` | Redo in input | same |
| input.word.forward | `alt+f,alt+right,ctrl+right` | Move word forward in input | same |
| input.word.backward | `alt+b,alt+left,ctrl+left` | Move word backward in input | same |
| input.select.word.forward | `alt+shift+f,alt+shift+right` | Select word forward in input | same |
| input.select.word.backward | `alt+shift+b,alt+shift+left` | Select word backward in input | same |
| input.delete.word.forward | `alt+d,alt+delete,ctrl+delete` | Delete word forward in input | same |
| input.delete.word.backward | `ctrl+w,ctrl+backspace,alt+backspace` | Delete word backward in input | same |
| input.select.all | `super+a` | Select all in input | same |
| prompt.history.previous | `up` | Previous history item | B priority 1, input-targeted, hidden when autocomplete is visible, `src/component/prompt/index.tsx:1003-1050` |
| prompt.history.next | `down` | Next history item | same, `src/component/prompt/index.tsx:1052-1090` |
| composer.subagent.up | `up` | Previous subagent | C priority 1, `src/routes/session/composer/subagents-tab.tsx:136-195` |
| composer.subagent.down | `down` | Next subagent | C |
| composer.subagent.select | `return` | Navigate to subagent | C |
| composer.subagent.interrupt | `ctrl+d` | Interrupt subagent | C |
| composer.shell.up | `up` | Previous shell | C `src/routes/session/composer/shell-tab.tsx:64-108` |
| composer.shell.down | `down` | Next shell | C |
| composer.shell.select | `return` | View shell output | C |
| composer.shell.kill | `ctrl+d` | Kill shell command | C |
| composer.terminal.up | `up,k` | Previous terminal | C `src/routes/session/composer/terminals-tab.tsx:42-64` |
| composer.terminal.down | `down,j` | Next terminal | C |
| composer.terminal.select | `return` | Select terminal | C |
| dialog.select.prev | `up,ctrl+p` | Move to previous dialog item | M `src/ui/dialog-select.tsx:445-466` |
| dialog.select.next | `down,ctrl+n` | Move to next dialog item | M |
| dialog.select.page_up | `pageup` | Move up one page in dialog | M (moves 10 items) |
| dialog.select.page_down | `pagedown` | Move down one page in dialog | M (moves 10 items) |
| dialog.select.home | `home` | Move to first dialog item | M |
| dialog.select.end | `end` | Move to last dialog item | M |
| dialog.select.submit | `return` | Submit selected dialog item | M |
| dialog.prompt.submit | `return` | Submit dialog prompt | M priority 1, `src/ui/dialog-prompt.tsx:36-50`, `src/component/dialog-integration.tsx:972-985` |
| dialog.integration.rename | `ctrl+r` | Rename integration account | M action, `src/component/dialog-integration.tsx:226` |
| dialog.integration.delete | `ctrl+d` | Delete integration account | M action (press twice), `src/component/dialog-integration.tsx:206,251` |
| dialog.worktree.generate | `tab` | Generate worktree name | M priority 1, `src/component/dialog-worktree-name.tsx:27-40` |
| dialog.move_session.new | `ctrl+a` | New worktree | M action, `src/component/dialog-workspaces.tsx:379` |
| dialog.move_session.move | `ctrl+m` | Move session to worktree | M action, `src/component/dialog-workspaces.tsx:376` |
| dialog.move_session.delete | `ctrl+d` | Delete worktree | M action (press twice), `src/component/dialog-workspaces.tsx:181,385` |
| dialog.move_session.refresh | `ctrl+r` | Refresh worktrees | M action, `src/component/dialog-workspaces.tsx:395` |
| prompt.autocomplete.prev | `up,ctrl+p` | Move to previous autocomplete item | A, input-targeted, `src/component/prompt/autocomplete.tsx:667-735` |
| prompt.autocomplete.next | `down,ctrl+n` | Move to next autocomplete item | A |
| prompt.autocomplete.hide | `escape` | Hide autocomplete | A |
| prompt.autocomplete.select | `return` | Select autocomplete item | A |
| prompt.autocomplete.complete | `tab` | Complete autocomplete item | A (expands directories) |
| permission.prompt.fullscreen | `ctrl+f` | Toggle permission prompt fullscreen | B permission layer, `src/routes/session/permission.tsx:455-463,495` |
| plugins.toggle | `return` | Toggle plugin | M action (right side), `src/feature-plugins/system/plugins.tsx:233-238` |
| dialog.mcp.toggle | `space` | Toggle MCP server | M action, `src/component/dialog-mcp.tsx:146-152` |
| dialog.plugins.error | `space` | View plugin error | M action, `src/feature-plugins/system/plugins.tsx:223-230` |
| dialog.plugins.update | `ctrl+u` | Update plugin from plugin dialog | M action (right side), `src/feature-plugins/system/plugins.tsx:239-244` |
| dialog.plugins.check | `ctrl+r` | Check for plugin updates from plugin dialog | M action, `src/feature-plugins/system/plugins.tsx:213-221` |
| terminal.suspend | `ctrl+z` | Suspend terminal | B `src/app.tsx:172` (forced to `none` on win32) |
| terminal.title.toggle | `none` | Toggle terminal title | B `src/app.tsx:173` |
| plugins.list | `none` | Open plugin manager dialog | G `src/feature-plugins/system/plugins.tsx:333-350` |
| which-key.toggle | `ctrl+alt+k` | Toggle which-key panel | ∅ |
| which-key.layout.toggle | `ctrl+alt+shift+k` | Switch which-key layout | ∅ |
| which-key.pending.toggle | `ctrl+alt+shift+p` | Toggle which-key pending preview | ∅ |
| which-key.group.previous | `ctrl+alt+left,ctrl+alt+[` | Previous which-key group | ∅ |
| which-key.group.next | `ctrl+alt+right,ctrl+alt+]` | Next which-key group | ∅ |
| which-key.scroll.up | `ctrl+alt+up,ctrl+alt+p` | Scroll which-key up | ∅ |
| which-key.scroll.down | `ctrl+alt+down,ctrl+alt+n` | Scroll which-key down | ∅ |
| which-key.page.up | `ctrl+alt+pageup` | Page which-key up | ∅ |
| which-key.page.down | `ctrl+alt+pagedown` | Page which-key down | ∅ |
| which-key.home | `ctrl+alt+home` | Jump to first which-key binding | ∅ |
| which-key.end | `ctrl+alt+end` | Jump to last which-key binding | ∅ |

Notes on the table:

- Some keys are bound to several commands by default. Which one runs is decided by modes, enablement and layer order (5.8):
  - `<leader>q` (exit, queued prompts)
  - `<leader>1-9` (quick slot, tab select)
  - `ctrl+d`, `ctrl+f`, `up/down`
- Some named commands have no keybind entry and therefore can never be bound: `session.clear`, `opencode.update`, `session.terminal`, `stats.open`, `app.storybook*`, `composer.subagent.toggle-activity`, `prompt.autocomplete.destructive`.

### 5.6 v1 config → v2 mapping

- **v1 schema.** `src/config/v1/keybind.ts` uses snake_case keys (dotted only for the `dialog.*`/`prompt.autocomplete.*`/`permission.*`/`plugins.toggle` group) with the same value grammar (`src/config/v1/keybind.ts:8-33`).
- **CommandMap.** `CommandMap` maps v1 name → v2 command id (`src/config/v1/keybind.ts:251-414`). It is applied at lookup time (`createBindingLookup(..., {commandMap})`, `src/config/v1/index.tsx:124-127`), so v1 bindings are keyed under v2 ids.
- **Leader timeout.** v1 has a top-level `leader_timeout` (default `2000`, `src/config/v1/index.tsx:21-24,68,128`). v2 uses `leader.timeout`.
- **Non-obvious renames:**
  - `command_list`→`command.palette.show`
  - `editor_open`→`prompt.editor`
  - `theme_list`→`theme.switch`
  - `sidebar_toggle`→`session.sidebar.toggle`
  - `scrollbar_toggle`→`session.toggle.scrollbar`
  - `status_view`→`opencode.status`
  - `debug_view`→`opencode.debug`
  - `model_provider_list`→`model.dialog.provider`
  - `model_favorite_toggle`→`model.dialog.favorite`
  - `messages_{page,line,half_page}_{up,down}`→`session.{page,line,half.page}.{up,down}`
  - `messages_first/last`→`session.first/last`
  - `messages_next/previous[_user]`→`session.message[.user].next/previous`
  - `messages_last_user`→`session.messages_last_user`
  - `messages_undo/redo`→`session.undo/redo`
  - `display_thinking`→`session.toggle.thinking`
  - `input_clear`→`prompt.clear`
  - `input_paste`→`prompt.paste`
  - `history_previous/next`→`prompt.history.previous/next`
  - every other `a_b_c` becomes `a.b.c` (`src/config/v1/keybind.ts:251-414`)
- **v1 defaults that differ in v2:**
  - `theme_list` `<leader>t` → `theme.switch` `none`. `<leader>t` is now `terminal.toggle`.
  - `diff_toggle` `enter,space`, `diff_expand` `right`, `diff_expand_all` `E`, `diff_collapse` `left`, `diff_switch_focus` `tab` → all `none` and deprecated.
  - `diff_next_file` `n` → `n,alt+down`. `diff_previous_file` `p` → `p,alt+up`.
  - `diff_help` `?` → `?,shift+?,shift+/`.
  - `dialog.move_session.new` `ctrl+m` ("New project copy") → `ctrl+a` ("New worktree"). `ctrl+m` is now `dialog.move_session.move`.
  - `dialog.project_copy.generate` (`tab`) has no v2 entry. It was replaced by `dialog.worktree.generate`, which is not mapped (`src/config/v1/keybind.ts:223-226` vs `src/config/keybind.ts:267-271`).
  - Description changes: "Move session"→"Manage workspaces", "...open tab"→"...open session tab", "View queued prompts"→"Manage queued prompts", "Connect an integration"→"Connect integration".
- **Orphan.** `app_heap_snapshot`→`app.heap_snapshot` has no v2 definition and is dropped.
- **Migration** (`../cli/src/config/migrate.ts`):
  - Legacy `tui.json` keybinds are renamed through `CommandMap`. Targets not in v2 `Definitions` are dropped (`../cli/src/config/migrate.ts:173-182`).
  - `leader_timeout` → `leader.timeout` (`../cli/src/config/migrate.ts:197`).
  - An existing v2 file with legacy names is rewritten in place. Earlier duplicate properties are removed, and the later one wins (`../cli/src/config/migrate.ts:61-90`).
  - Legacy `terminal.copy_on_select:boolean` → `terminal.copy:"select"|"manual"` (`../cli/src/config/migrate.ts:44-60`).
- **IDs only in v2:** `app.clear`, `app.scrap`, `opencode.settings`, `server.pair`, `service.restart`, `location.reload`, `diff.{down,up,page.*,half_page.*,first,last,mark_reviewed}`, `pane.focus.*`, `terminal.{select,toggle,close}`, `session.copy.id`, `session.tab.history.*`, `session.aside`, `session.cd`, `queued_prompt.undo`, `session.toggle.exploration_grouping`, `session.verbosity.cycle`, `session.child.next/previous`, `composer.*`, `dialog.integration.*`, `dialog.worktree.generate`, `dialog.move_session.move`, `dialog.plugins.*`, `plugins.list`, `which-key.*`. These are partly listed in `test/config-v2.test.tsx:189-221`.

### 5.7 Keymap runtime: layers, modes, priority, dispatch

- **KeymapLayer fields** (`../plugin/src/tui/context.ts:435-448`):
  - `mode?` (default `"base"`; `"global"` opts out of the mode check)
  - `enabled?` (bool or fn)
  - `target?` (focused renderable)
  - `priority?` (number, default 0)
  - `commands?`
  - `bindings?` (ids whose configured keys attach in this layer)
- **KeymapCommand fields** (`../plugin/src/tui/context.ts:407-433`):
  - `id?`, `title?`, `description?`, `group?`, `enabled?`
  - `bind?: false|string`
  - `palette?: true`
  - `slash?: {name, aliases?, arguments?: true}`
  - `suggested?`
  - `run(input?, event?)`, which may return `false` to continue to the next binding
- **Modes** form a stack (`src/context/keymap.tsx:420-453`). The current mode is the last pushed entry whose `enabled()` (interactivity) is true, else `"base"` (`src/context/keymap.tsx:424`). A layer with mode X is active only when the current mode is X (`src/context/keymap.tsx:214,427-431`). Pushers:
  - `modal`: while the dialog stack is non-empty (`src/ui/dialog.tsx:89-93`)
  - `autocomplete`: while the popup is visible (`src/component/prompt/autocomplete.tsx:96-100`)
  - `composer`: while open (`src/routes/session/composer/index.tsx:58-63`)
  - `form`: question form mounted (`src/routes/session/form.tsx:38,536`)
  - `menu`: tab context menu `src/component/session-tabs.tsx:380`, diff file menu `src/feature-plugins/system/diff-viewer-file-menu.tsx:23`

  `test/keymap.test.tsx:103-141` confirms that global commands stay reachable when the mode changes and base commands do not.
- **Interactivity.**
  - Every layer is forced disabled when its `InteractivityProvider` is disabled (`src/context/keymap.tsx:213`; `src/context/interactivity.tsx:6-15`).
  - The session pane subtree, i.e. the session route and the prompt, is disabled while the right pane has focus or is fullscreen (`src/component/session-frame.tsx:287`). Panels follow `focused` (`src/component/panel-host.tsx:60`).
  - `test/keymap-scope.test.tsx:10-77` covers this.
- **Layer order.** Active layers are sorted by `priority` descending, then by **registration order descending**, so newer layers win (`KM:src/index.js:3627-3630`; `KM:chunks/index-7g381cth.js:143-159`).
  - A layer with `target` is active only when the target is on the focus path (`focus-within` by default) (`KM:chunks/index-7g381cth.js:160-172`; `KM:src/solid/index.js:69`).
  - Layers re-register, getting a new order, whenever their reactive factory re-runs (`KM:src/solid/index.js:64-97`).
- **Named command → keys** (`src/context/keymap.tsx:240-246`):
  - `bind === false` gives no keys from the command itself.
  - Otherwise configured keys for `id` are used if there are any.
  - Otherwise a string `bind` is used as a fallback.
  - `bindings: [ids]` attaches configured keys for commands defined elsewhere (`src/context/keymap.tsx:247`). All app, session and prompt commands use `bind:false` plus separate `bindings` layers, which is how one command gets keys in a different mode or layer than where it is registered (`src/app.tsx:1205-1243`; `src/routes/session/index.tsx:1310-1334`; `src/component/prompt/index.tsx:627-664`).
- **Inline command.** `enabled` is checked at run time. When disabled it returns `false`, so dispatch continues; `title` becomes the binding `desc` (`src/context/keymap.tsx:228-239`).
- **Dispatch order for a key press:**
  1. **Intercepts**, by priority:
     - selection keys, priority 101 (`src/app.tsx:550-557`)
     - embedded terminal forwarding, priority 100 (`src/component/terminal-pane.tsx:140-150`)
     - pending escape/backspace, priority 0 (`src/context/keymap.tsx:90-91`)
     - the devtools-panel escape intercept (`src/component/devtools-bar.tsx:86-100`)
     - the form typing intercept (`src/routes/session/form.tsx:243-255`)
  2. Pending-sequence continuation (5.4).
  3. Otherwise active layers in order. Within a layer, the first binding whose command is reachable and handles the key stops dispatch. A `run` returning `false` is "rejected" and dispatch continues (`KM:src/index.js:1937-1941`). The same command id registered in several reachable layers forms a chain tried in order until one handles it (`KM:src/index.js:1949-1958`). This is how permission, form and diff viewer override `app.exit`.
- **Programmatic dispatch.** `keymap.dispatch(id, input?)` reaches only reachable commands and passes `input` to `run(input)` (`src/context/keymap.tsx:69-77,159,221`).
  - External `tui.command.execute` events dispatch by id (`src/app.tsx:1254-1257`).
  - Toast actions dispatch `mcp.list` (`src/app.tsx:529-545`).
- **Managed textarea layer.** Registered once in the provider, before any component layer, i.e. the oldest priority-0 layer, so it has the lowest precedence. Its input bindings are active only when the focused editor is a `TextareaRenderable` that is not an `InputRenderable` (`src/context/keymap.tsx:92-135`). App and session base-mode keys therefore beat textarea editing keys when both are active. `home`/`end` → `session.first/last`. `ctrl+b` → `session.background` while running; otherwise the command is disabled and the key falls to `input.move.left`.

### 5.8 Effective precedence for overloaded keys (derived from 5.7)

**`ctrl+c`:**
1. Selection intercept (5.15).
2. Dialog open (`modal`, `src/ui/dialog.tsx:134-153`): clear the selection, else clear the focused input's text, else pop the dialog.
3. Menus: close (`src/component/session-tabs.tsx:381-384`; `src/feature-plugins/system/diff-viewer-file-menu.tsx:27`).
4. Composer open: close (`src/routes/session/composer/index.tsx:72-81`).
5. Autocomplete visible: `prompt.clear` hides it and deletes the `/token` (`src/component/prompt/autocomplete.tsx:697-704,751-768`).
6. Narrow sidebar overlay: priority-10 "Close sidebar" (`src/routes/session/index.tsx:250-254`; enabled when sidebar visible ∧ `width - verticalTabsWidth <= 120`).
7. Shell mode with empty text: exit shell (`src/component/prompt/index.tsx:986-999`).
8. Prompt has text: `prompt.clear` (`src/component/prompt/index.tsx:950-956`).
9. Permission or form prompt: their `app.exit` override (`src/routes/session/permission.tsx:284-297,440-452`; `src/routes/session/form.tsx:648-656`).
10. Otherwise `app.exit` (prompt unfocused or empty, `src/app.tsx:1245-1252`).

**`escape`:**
1. Pending sequence: clear it.
2. Selection: clear it.
3. Dialog: clear the selection or pop the dialog (`src/ui/dialog.tsx:119-132`). DialogSelect with `onCancel` shows "Back" (`src/ui/dialog-select.tsx:531-547`).
4. Autocomplete: hide.
5. Composer: close.
6. Shell mode: exit shell (priority 1).
7. Narrow sidebar: close.
8. `session.interrupt`, enabled only while status is running and the prompt is focused:
   - The first press arms the interrupt and the hint reads `esc again to interrupt` (`src/component/prompt/index.tsx:136-142`).
   - A second press within 5000 ms calls `session.interrupt({resume:true})` (`src/component/prompt/index.tsx:499-530`).
   - In shell mode it exits shell instead (`src/component/prompt/index.tsx:507-510`).

**`up` / `down` in the prompt:**
1. Autocomplete prev/next.
2. Otherwise `prompt.history.*` (priority 1). It moves the cursor between visual lines, jumps to the start or end of the buffer, then walks history. It returns `false` when there is no history item (`src/component/prompt/index.tsx:1026-1047,1062-1086`).
3. Dispatch then continues to `session.parent` (`up`, when in a child session) or `session.child.first` (`down`, toggle the subagent picker).

**`<leader>q`:** `session.queued_prompts` when the queue is non-empty, since the session route layer is newer than the app layer. Otherwise `app.exit`.

**`<leader>1..9`:** `session.tab.select.N` when tabs are enabled, else `session.quick_switch.N`. The two are mutually exclusive through layer `enabled` (`src/app.tsx:1233-1243`).

**`ctrl+d`:**
- prompt empty: `app.exit`
- typing: `input.delete`
- dialogs: delete action (press twice to confirm; the row shows `Press ctrl+d again to confirm`, `src/component/dialog-session-list.tsx:165-167`)
- autocomplete: `prompt.autocomplete.destructive` (press twice, `src/component/prompt/autocomplete.tsx:638-648,726-732`)
- composer: interrupt subagent or kill shell
- diff viewer: half page down (a comment there says "Specific diff bindings take precedence over app.exit's Ctrl+D binding", `src/feature-plugins/system/diff-viewer.tsx:727-733`)

**`!`** at visual-cursor offset 0 in normal mode (autocomplete hidden): enter shell mode. **`backspace`** at offset 0 in shell mode: leave shell mode (`src/component/prompt/index.tsx:958-1014`).

**Typed exit words.** Typing `exit`, `quit` or `:q` and submitting exits the app. Queueing these is refused with the toast "This prompt cannot be queued" (`src/component/prompt/index.tsx:1122,1133-1141`).

### 5.9 Shortcut display formatting (hints)

`formatOptions` (`src/context/keymap.tsx:455-474`) is passed to KM `formatKeySequence`/`formatCommandBindings` (`KM:src/extras/index.js:255-340`):

- **Modifier order** is fixed: `ctrl`, `shift`, `alt` (meta aliased to `alt`), `super`, `hyper`, joined with `+`, then the key name. So `alt+shift+down` displays as `shift+alt+↓` and `super+shift+z` as `shift+super+z`.
- **Key names:**
  - `return` → `enter`
  - aliases: `up:"↑"`, `down:"↓"`, `left:"←"`, `right:"→"`, `pageup:"pgup"`, `pagedown:"pgdn"`, `delete:"del"`
  - everything else is the lowercase name as-is (`escape`, `tab`, `space`, `home`, `f2`, `?`, `[`)
- **Tokens.** `<leader>` displays as the configured leader key **string verbatim** (default `ctrl+x`). A KeyStroke object is stringified canonically.
- **Sequences** join strokes with one space: `<leader>n` → `ctrl+x n`, `gg` → `g g`, `<leader>return` → `ctrl+x enter`.
- **Multiple bindings:**
  - `shortcuts.get(id)` returns only the first binding, e.g. `ctrl+c` for `app.exit`.
  - `shortcuts.all(id)` joins with `", "` and deduplicates, e.g. `ctrl+tab, alt+↓`.
  - `shortcuts.list(id)` returns an array (`src/context/keymap.tsx:277-309`).
- **Tests:** `test/keymap.test.tsx:41` (`pgup`/`pgdn`), `:68-71` (`↑`/`↓`), `:97` (`["x","y"]`).
- **Unbound commands.** `get` returns `""` (`formatKeySequence` of undefined), so callers hide hints with `<Show when=...>`. Several UIs hard-code `esc`/`enter` labels instead of formatting, e.g. `src/ui/dialog-help.tsx:26`, `src/routes/session/index.tsx:692`, `src/routes/session/composer/index.tsx:123`. Layout of hints is covered by the dialog and prompt sections.

### 5.10 Command registry (palette + slash)

Fields:

- **Palette:** ✓ means listed. — means `palette: undefined`, which hides it from the palette but keeps it dispatchable. The defaults are `palette:true` and `bind:false` (`src/app.tsx:1205-1215`; `src/routes/session/index.tsx:1310-1319`; `src/component/prompt/index.tsx:627-636,924-933`).
- **Key** is the v2 default from 5.5.
- **Group** is the category shown in the palette.

| id | title (verbatim) | group | key | slash (aliases) | palette | enabled / suggested | action |
|---|---|---|---|---|---|---|---|
| command.palette.show | Show command palette | System | ctrl+p | — | — (also explicitly excluded, `src/component/command-palette.tsx:19`) | | open palette (`src/app.tsx:691-699`) |
| session.list | Switch session | Session | <leader>l | /sessions (/resume,/continue) | ✓ | suggested if any sessions | DialogSessionList (`src/app.tsx:700-709`) |
| session.new | New session | Session | <leader>n | /new | ✓ | suggested on session route | go home at new-session location, keep agent/model, clear dialogs (`src/app.tsx:710-736`) |
| session.clear | Clear session | Session | — | /clear | ✓ | | close current tab then same as new (`src/app.tsx:737-763`) |
| open.menu | Open session or project | Session | ctrl+o | /open (/projects,/project) | ✓ | | DialogOpen size large; no-op if already open (`src/app.tsx:764-775`) |
| session.quick_switch.N | Switch to session in quick slot N | Session | <leader>N | — | — | !tabs | `src/app.tsx:777-784` |
| session.tab.next / previous | Next tab / Previous tab | Session | see 5.5 | — | — | tabs | cycle ±1 (`src/app.tsx:785-800`) |
| session.tab.next_unread / previous_unread | Next unread tab / Previous unread tab | Session | | — | — | tabs | `src/app.tsx:801-816` |
| session.tab.close | Close tab | Session | <leader>w | — | ✓ | tabs | `src/app.tsx:817-823` |
| session.tab.reopen | Reopen closed tab | Session | ctrl+shift+t | — | ✓ | tabs | `src/app.tsx:824-830` |
| session.tab.select.N (1-10) | Switch to tab N | Session | | — | — | tabs | `src/app.tsx:831-838` |
| model.list | Switch model | Agent | <leader>m | /models | ✓ | suggested | DialogModel (`src/app.tsx:839-847`) |
| model.cycle_recent / _reverse | Model cycle / Model cycle reverse | Agent | f2 / shift+f2 | — | — | | `src/app.tsx:848-865` |
| model.cycle_favorite / _reverse | Favorite cycle / Favorite cycle reverse | Agent | — | — | — | | `src/app.tsx:866-883` |
| agent.list | Switch agent | Agent | <leader>a | /agents | ✓ | | DialogAgent (`src/app.tsx:884-892`) |
| mcp.list | MCP servers | Agent | — | /mcps | ✓ | | DialogMcp (`src/app.tsx:893-901`) |
| agent.cycle | Agent cycle | Agent | shift+tab | — | — | | `src/app.tsx:902-910` |
| variant.cycle | Variant cycle | Agent | ctrl+t | — | ✓ | | `src/app.tsx:911-918` |
| variant.list | Switch model variant | Agent | — | /variants (/thinking,/effort) | ✓ only if the model has variants | | DialogVariant, else info toast "No variants available" / "The current model does not support any variants." (`src/app.tsx:919-936`) |
| agent.cycle.reverse | Agent cycle reverse | Agent | — | — | — | | `src/app.tsx:937-945` |
| provider.connect | Connect an integration | Integration | — | /connect | ✓ | suggested if not connected | DialogIntegration → on connect DialogModel(providerID) (`src/app.tsx:946-958`) |
| opencode.settings | Open settings | System | — | /settings | ✓ | suggested | DialogConfig (`src/app.tsx:959-967`) |
| opencode.status | View status | System | <leader>s | /status | ✓ | | DialogStatus (`src/app.tsx:968-976`) |
| opencode.update | Update OpenCode | System | — | /update | ✓ | only registered if an updater exists | `updater.open("manual")` (`src/app.tsx:977-988`) |
| server.pair | Pair device | System | — | /pair (/web) | ✓ | | DialogPair (`src/app.tsx:989-997`) |
| service.restart | Restart service | System | — | /restart | ✓ | only if the client can restart | toast "Restarting service…" (30 s) → "Service restarted" (`src/app.tsx:998-1018`) |
| location.reload | Reload configuration | System | — | /reload | ✓ | | toast "Reloading configuration…" → "Configuration reloaded" (`src/app.tsx:1019-1035`) |
| opencode.debug | View debug info | System | — | /debug | ✓ | | DialogDebug (`src/app.tsx:1036-1044`) |
| theme.switch | Switch theme | System | — | /themes | ✓ | | DialogThemeList (`src/app.tsx:1045-1053`) |
| theme.switch_mode | "Switch to light mode" \| "Switch to dark mode" | System | — | — | — | if the other mode is supported | `src/app.tsx:1054-1064` |
| theme.mode.lock | "Unlock theme mode" \| "Lock theme mode" | System | — | — | — | | `src/app.tsx:1065-1075` |
| help.show | Help | System | — | /help | ✓ | | DialogHelp (`src/app.tsx:1076-1084`) |
| docs.open | Open docs | System | — | — | ✓ | | open `https://opencode.ai/docs` (`src/app.tsx:1085-1093`) |
| app.exit | Exit the app | System | ctrl+c,ctrl+d,<leader>q | /exit (/quit,/q) | ✓ | | `src/app.tsx:1094-1100` |
| app.debug | Toggle debug panel | System | — | — | — | | renderer debug overlay (`src/app.tsx:1101-1110`) |
| app.console | Toggle console | System | — | — | ✓ | | `src/app.tsx:1111-1119` |
| terminal.suspend | Suspend terminal | System | ctrl+z | — | — | non-win32 | SIGTSTP, resume on SIGCONT (`src/app.tsx:1120-1131`) |
| terminal.title.toggle | "Disable terminal title" \| "Enable terminal title" | System | — | — | — | | `src/app.tsx:1132-1146` |
| app.toggle.animations | "Disable animations" \| "Enable animations" | System | — | — | — | | `src/app.tsx:1148-1161` |
| app.toggle.file_context | "Disable file context" \| "Enable file context" | System | — | — | — | | `src/app.tsx:1162-1175` |
| app.toggle.diffwrap | "Disable diff wrapping" \| "Enable diff wrapping" | System | — | — | — | | `src/app.tsx:1176-1192` |
| app.toggle.paste_summary | "Disable paste summary" \| "Enable paste summary" | System | — | — | — | | `src/app.tsx:1193-1206` |
| session.page.up / down | Page up / Page down | Session | pageup,ctrl+alt+b / pagedown,ctrl+alt+f | — | — | | scroll ±**viewport/2** (`src/routes/session/index.tsx:782-796`) |
| session.line.up / down | Line up / Line down | Session | ctrl+alt+y / ctrl+alt+e | — | — | | ±1 (`src/routes/session/index.tsx:797-810`) |
| session.half.page.up / down | Half page up / Half page down | Session | ctrl+alt+u / ctrl+alt+d | — | — | | ±**viewport/4** (`src/routes/session/index.tsx:811-824`) |
| session.first | First message | Session | ctrl+g,home,alt+home | — | — | | load full history, then scroll to 0; status "Loading session history…" (`src/routes/session/index.tsx:827-895,1455-1457`) |
| session.last | Last message | Session | ctrl+alt+g,end | — | — | | to bottom (`src/routes/session/index.tsx:896-905`) |
| session.share | Share session | Session | — | /share | ✓ | suggested on session route | error toast "Sharing is not implemented for V2 sessions yet" (5 s) (`src/routes/session/index.tsx:909-916,700-703`) |
| session.rename | Rename session | Session | ctrl+r | /rename (takes args) | ✓ | | no arg: DialogSessionRename; arg: set title; blank arg: regenerate title (`src/routes/session/index.tsx:917-931`) |
| session.timeline | Jump to message | Session | <leader>g | /timeline | ✓ | | DialogTimeline (`src/routes/session/index.tsx:932-945`) |
| session.fork | Fork session | Session | — | /fork | ✓ | | DialogFork (`src/routes/session/index.tsx:946-962`) |
| session.compact | Compact session | Session | <leader>c | /compact | ✓ | | compact with current model/variant (`src/routes/session/index.tsx:963-987`) |
| session.undo | Undo previous message | Session | <leader>u | /undo | ✓ | | revert last user message and put it back into the prompt; toast "Nothing to undo" (`src/routes/session/index.tsx:988-1018`) |
| session.redo | Redo | Session | <leader>r | /redo | ✓ | only when a revert exists | `src/routes/session/index.tsx:1019-1035` |
| session.toggle.thinking | "Collapse thinking" \| "Expand thinking" | Session | — | — | — | | cycle `session.thinking` (`src/routes/session/index.tsx:1036-1053`) |
| session.toggle.scrollbar | Toggle session scrollbar | Session | — | — | — | | `src/routes/session/index.tsx:1055-1067` |
| session.toggle.exploration_grouping | "Show tool calls individually" \| "Group related tool calls" | Session | — | — | — | | `src/routes/session/index.tsx:1069-1081` |
| session.verbosity.cycle | `Verbosity: Low\|Medium\|High` | Session | — | — | ✓ | | low→medium→high (`src/routes/session/index.tsx:1083-1096`) |
| session.messages_last_user | Jump to last user message | Session | alt+end | — | — | | `src/routes/session/index.tsx:1098-1115` |
| session.message.next / previous / user.next / user.previous | Next message / Previous message / Next user message / Previous user message | Session | — | — | — | | `src/routes/session/index.tsx:1118-1145` |
| messages.copy | Copy last assistant message | Session | <leader>y | — | ✓ | | toasts "Message copied to clipboard!" / "No assistant messages found" / … (`src/routes/session/index.tsx:1146-1186`) |
| session.copy.id | Copy session ID | Session | — | — | ✓ | | toast "Session ID copied to clipboard!" (`src/routes/session/index.tsx:1187-1198`) |
| session.copy | Copy session transcript | Session | — | /copy | ✓ | | toast "Session transcript copied to clipboard!" (`src/routes/session/index.tsx:1199-1219`) |
| session.export | Export session transcript | Session | <leader>x | /export | ✓ | | DialogExportOptions → copy or temp file (`src/routes/session/index.tsx:1220-1262`) |
| session.background | Background blocking tools | Session | ctrl+b | — | — | prompt version only while running | `src/routes/session/index.tsx:1264-1272`; `src/component/prompt/index.tsx:531-547` |
| session.child.first | Toggle subagent picker | Session | down | — | ✓ | | open/close composer on the subagents tab (`src/routes/session/index.tsx:1274-1283`) |
| session.queued_prompts | View queued prompts | **Prompt** | <leader>q | — | ✓ | queue > 0 | "Queued prompts" DialogSelect (`src/routes/session/index.tsx:1284-1290,646-698`) |
| session.parent | Go to parent session | Session | up | — | — | has parent | `src/routes/session/index.tsx:1291-1309` |
| prompt.clear | Clear prompt | Prompt | ctrl+c | — | — | | `src/component/prompt/index.tsx:419-428` |
| prompt.submit | Submit prompt | Prompt | — | — | — | | `src/component/prompt/index.tsx:429-443` |
| prompt.queue | Queue prompt | Prompt | <leader>return | — | ✓ | | submit with delivery "queue" (`src/component/prompt/index.tsx:444-457`) |
| prompt.editor_context.clear | Remove editor context | Prompt | — | — | ✓ | editor context present | `src/component/prompt/index.tsx:458-467` |
| prompt.paste | Paste | Prompt | ctrl+v | — | — | | image → attachment; text → paste (`src/component/prompt/index.tsx:468-491`) |
| prompt.images.view | View image attachments | Prompt | <leader>i | — | ✓ | images > 0 | `src/component/prompt/index.tsx:492-498` |
| session.interrupt | Interrupt session | Session | escape | — | — | running | double-press (5.8) |
| prompt.editor | Open editor | **Session** | <leader>e | /editor | ✓ | | `$EDITOR` round-trip (`src/component/prompt/index.tsx:548-578`) |
| prompt.skills | Skills | Prompt | — | /skills | ✓ | | DialogSkill, inserts `@skill ` (`src/component/prompt/index.tsx:579-616`) |
| session.move | Manage workspaces | Session | — | /worktrees | ✓ | | `move.open()` (`src/component/prompt/index.tsx:617-626`) |
| prompt.stash | Stash prompt | Prompt | — | — | ✓ | text non-empty | `src/component/prompt/index.tsx:878-889` |
| prompt.stash.pop | Stash pop | Prompt | — | — | ✓ | stash non-empty | `src/component/prompt/index.tsx:890-905` |
| prompt.stash.list | Stash list | Prompt | — | — | ✓ | stash non-empty | DialogStash (`src/component/prompt/index.tsx:906-922`) |
| session.cd | Change working directory | (none) | — | /cd (takes args; directory autocomplete) | — (no `palette`) | | move session or home location; toast "Directory is required" (`src/component/prompt/index.tsx:262-311`) |
| pane.focus.left | Focus session pane | (none) | <leader>left | — | — | right pane shown ∧ no dialog ∧ !fullscreen | `src/component/session-frame.tsx:164-180` |
| pane.focus.right | Focus right pane | (none) | <leader>right | — | — | same | |
| session.sidebar.toggle | "Hide sidebar" \| "Show sidebar" | Session | <leader>b | — | ✓ | | `src/component/session-frame.tsx:186-194` |
| terminal.toggle | "Hide terminal pane" \| "Show terminal pane" | Session | <leader>t | — | ✓ | terminals available | `src/component/session-frame.tsx:197-217` |
| terminal.select | Select terminal | Session | <leader>down | — | ✓ | terminals available | opens the composer terminals tab (`src/component/session-frame.tsx:219-230`) |
| terminal.close | Close terminal pane | Session | <leader>up | — | ✓ | right pane is terminal | `src/component/session-frame.tsx:231-242` |
| session.terminal | New terminal | Session | — | /terminal | ✓ | terminals available | `src/component/session-frame.tsx:243-253` |
| diff.open | Open diff viewer | VCS | — | /diff | ✓ | | plugin route `diffs` (`src/feature-plugins/system/diff-viewer.tsx:1195-1225`) |
| session.aside | Ask a side question (description: "One-shot answer from the session's context without adding to the conversation") | Session | — | /btw (takes args) | ✓ | | prompt "/btw" if no arg; answer dialog (large, centered) (`src/feature-plugins/prompt/btw.tsx:48-80`) |
| stats.open | Usage statistics | System | — | /stats | ✓ | | plugin route `stats` (`src/feature-plugins/system/stats.tsx:193-212`) |
| plugins.list | Plugins | System | — | /plugins | ✓ | | PluginsDialog (`src/feature-plugins/system/plugins.tsx:333-350`) |
| app.storybook, app.storybook.<story> | Open storybook / `Storybook: <title>` | Debug | — | — | ✓ | only when `OPENCODE_STORY` is set (`src/plugin/builtins.ts:27-29`) | `src/feature-plugins/system/storybook/index.tsx:29-56` |

**Slash autocomplete** (`src/component/prompt/autocomplete.tsx:476-500`):

- Contents: every reachable command's `slash.name` plus each alias, and server-defined custom commands (`data.location.command.list`) as `/<name>` with `queueable: true`.
- Ordering and layout: sorted by `display.localeCompare`, then padded to max length + 2.
- Description: `command.description ?? command.title`.
- Selection: commands with `arguments: true` insert `"/name "` into the prompt (`insertSlash`, `src/component/prompt/autocomplete.tsx:467-473`). Others run immediately.
- Trigger: `/` with whitespace (or start) before it and no whitespace or `/` after it (`src/prompt/display.ts:52-61`). Only offered when the trigger index is 0 (`src/component/prompt/autocomplete.tsx:541-546`). Fuzzy matching uses display, description and aliases (limit 10, prefix match ×2) (`src/component/prompt/autocomplete.tsx:551-574`).
- **On submit**, `/name args` matching an `arguments` command runs `command.run(args)` and clears the prompt (`src/component/prompt/index.tsx:173-183,1133-1145`). `/name` matching a server command is sent as a command (`src/component/prompt/index.tsx:1147-1150,1310-1318`). Queueing an argument slash command is refused.

The resulting built-in slash list, alphabetical:

/agents /btw /cd /clear /compact /connect /continue /copy /debug /diff /editor /effort /exit /export /fork /help /mcps /models /new /open /pair /plugins /project /projects /q /quit /redo /reload /rename /restart /resume /sessions /settings /share /skills /stats /status /terminal /themes /thinking /timeline /undo /update /variants /web /worktrees

### 5.11 Command palette behaviour and ordering

- **Source.** `Keymap.useCommands()` returns reachable commands (`src/context/keymap.tsx:321-353`). The palette keeps those with `id`, `palette` set, and id ≠ `command.palette.show` (`src/component/command-palette.tsx:17-36`). Each option has:
  - `title = command.title ?? id`
  - `category = group`
  - `footer = shortcuts.all(id)`
  - `searchText = "id description"`
  - `searchFooter = "group · footer"`
- **Settings entries** are appended (`src/component/command-palette.tsx:37-46`). Each has `searchFooter: "Settings · <category>"`, and selecting one opens DialogConfig at that setting.
- **Without a filter** (`src/component/command-palette.tsx:49-61`): suggested commands come first, duplicated under category `"Suggested"` with value `suggested:<id>`, followed by all options. Settings entries appear **only** when the filter is non-empty. The DialogSelect is titled "Commands", uses `flat=true`, and `filterThreshold=0.7` (`src/component/command-palette.tsx:63-65`).
- **Grouping.** Options are grouped by category in first-appearance order. The alphabetical sort is commented out (`src/ui/dialog-select.tsx:214-222`). With a filter, the list is flat fuzzysort output over `title`, `category`, `searchText`, scored `title*2 + category + searchText` (`src/ui/dialog-select.tsx:194-212`).
- **Command order** = KM reachable order: active layers sorted (priority desc, newest first), commands in declaration order, and the first occurrence of an id wins (`KM:src/index.js:934-956`). This order is therefore dynamic. Typically the session route, prompt and session-frame commands appear before app commands, because they mount later; any layer whose memo recomputes moves to the front.
- **Selecting** an option clears the dialog and then runs the command (`src/component/command-palette.tsx:30-33`).

### 5.12 Context-local inline bindings (not configurable)

| context (mode) | keys → action (title verbatim) | source |
|---|---|---|
| any dialog (M) | `escape` "Close dialog"; `ctrl+c` "Close dialog" (see 5.8) | `src/ui/dialog.tsx:115-155` |
| DialogSelect (M) | `tab` / `shift+tab` "Next/Previous dialog action" (only when actions are visible); action keys from config; `escape` "Back" if `onCancel`; `alt+up` / `alt+down` "Previous/Next section" if `sectionNavigation`; extra `props.bindings` | `src/ui/dialog-select.tsx:445-562` |
| session list | `ctrl+a` "Show all project sessions" / "Show current directory sessions" | `src/component/dialog-session-list.tsx:216-226` |
| open dialog | `right` "Show project worktrees", `left` "Return to projects", `ctrl+n` "New worktree" | `src/component/dialog-open.tsx:384-419` |
| plugins dialog | `ctrl+a` "Toggle internal plugins" | `src/feature-plugins/system/plugins.tsx:189-199` |
| config / experiments dialog | `left` / `right` "Previous/Next value" | `src/component/dialog-config.tsx:394-407`, `src/component/dialog-experiments.tsx:64-79` |
| help (M) | `return` / `escape` "Close help" | `src/ui/dialog-help.tsx:11-17` |
| alert / confirm / update (M) | `return` confirm; confirm also `left`/`right` toggle | `src/ui/dialog-alert.tsx:16-24`, `src/ui/dialog-confirm.tsx:27-56`, `src/component/dialog-update.tsx:67-75` |
| prompt dialog (M, priority 1) | `dialog.prompt.submit`; `escape` "Back" | `src/ui/dialog-prompt.tsx:36-62` |
| export options (M) | `tab` "Next export option", `return` "Select export option" | `src/ui/dialog-export-options.tsx:56-77` |
| image preview (M) | `left` / `right` "Previous/Next image" | `src/component/dialog-image-preview.tsx:31-36` |
| shell output (M) | `up` / `down` / `pageup` / `pagedown` / `home` / `end` (`end` = "Follow shell output") | `src/component/dialog-shell-output.tsx:90-99` |
| execute (M) | `up` / `down` / `pageup` / `pagedown` / `home` / `end`, `c` "Copy code", `o` "Copy output" | `src/routes/session/dialog-execute.tsx:80-91` |
| error details (M) | `escape` "Back", `c` "Copy details", `i` "Investigate error" | `src/component/dialog-error-details.tsx:55-61` |
| debug (M) | `return` "Copy debug info" | `src/component/dialog-debug.tsx:50-52` |
| integration auth (M) | `o` "Open authorization URL", `c` "Copy authorization details"; answers use `dialog.select.*`, `escape` "Back to answers" | `src/component/dialog-integration.tsx:660-680,953-985` |
| btw answer (M) | `c` "Copy answer"; raw `up` / `k` / `down` / `j` scroll | `src/feature-plugins/prompt/btw.tsx:112-120` |
| composer (C, priority 1) | `left` / `right` "Previous/Next tab", `escape` / `ctrl+c` "Close composer"; subagents `ctrl+a` "Toggle active subagents" | `src/routes/session/composer/index.tsx:72-81`, `src/routes/session/composer/subagents-tab.tsx:172-181` |
| autocomplete (A) | `ctrl+d` "Confirm autocomplete action" | `src/component/prompt/autocomplete.tsx:726-732` |
| prompt | `!` "Shell mode"; in shell mode `escape` / `ctrl+c` (empty) / `backspace` (offset 0) "Exit shell mode" | `src/component/prompt/index.tsx:958-1014` |
| narrow sidebar (priority 10) | `escape,ctrl+c` "Close sidebar" | `src/routes/session/index.tsx:250-254` |
| permission (B) | `left,h` / `right,l` "Previous/Next option", `return` "Select option", `escape` "Reject permission"; reject-message editor: `escape` cancel, `return` confirm, `app.exit` keys clear text or cancel | `src/routes/session/permission.tsx:284-306,440-496` |
| question form (F) | edit mode: `escape`, `tab` / `shift+tab`, `up` (leave edit), `return`; select mode: `left` / `h`, `right` / `l`, `tab` / `shift+tab`, `1`-`9` "Select answer N", `up` / `k`, `down` / `j`, `return`, `space` (multi) "Toggle answer", `escape` "Dismiss form"; external link: `return`, `c` "Copy link"; review: `up` / `k` / `down` / `j` "Scroll review"; typing any printable char starts a custom answer | `src/routes/session/form.tsx:538-765,243-255` |
| tab context menu (menu) | `escape,ctrl+c` "Close tab menu" | `src/component/session-tabs.tsx:380-384` |
| diff file menu (menu) | `escape,ctrl+c` "Close file menu", `return` | `src/feature-plugins/system/diff-viewer-file-menu.tsx:23-30` |
| stats page | `escape` "back" | `src/feature-plugins/system/stats.tsx:151-153` |
| crash screen (raw) | `ctrl+c` exit, `return`, `left` / `right` | `src/component/error-component.tsx:69-90` |

### 5.13 Help dialog

`src/ui/dialog-help.tsx:6-46` does **not** list keybinds. It contains:

- the bold title "Help" with a muted "esc/enter" label at top right (clicking it closes the dialog)
- the muted sentence `Press {shortcuts.get("command.palette.show")} to see all available actions and commands in any context.`, i.e. "Press ctrl+p …" by default
- a right-aligned "ok" button, padded 3 left and right, on `background.action.primary.focused` (clicking it closes the dialog)

`return` and `escape` close it. The diff viewer has its own shortcut help dialog with rows built from `keymap.shortcuts(id)` joined with `" / "` (`src/feature-plugins/system/diff-viewer.tsx:1098-1145`).

### 5.14 Mouse

- **Capture.** `renderer.useMouse = config.mouse` (default `true`, `src/config/index.tsx:247,311`; `src/app.tsx:594`).
- **Wheel scrolling.** Scroll boxes use `scrollAcceleration`. With `scroll.acceleration` true it is `MacOSScrollAccel`. Otherwise it is a constant `scroll.speed` lines per tick, **default 3** (`src/util/scroll.ts:20-29`; `src/routes/session/index.tsx:1425`).
  - A transcript wheel-up past the loaded rows reveals older rows instead of scrolling (`src/routes/session/index.tsx:1426-1431`).
  - "Jump to latest ↓" appears when away from the bottom; clicking it scrolls to the bottom (`src/routes/session/index.tsx:1458-1470`).
- **Wheel and click while the right pane is focused.** An overlay over the session pane forwards wheel events inside the transcript viewport to the transcript without changing focus. Its first click only re-focuses the session (the release is consumed) (`src/component/session-frame.tsx:300-321`; `test/session-wheel-focus.test.tsx:106-140`).
- **Click to focus:**
  - Prompt: left mouse-down focuses it, ignored while disabled (`src/component/prompt/index.tsx:1814-1819`).
  - Embedded terminal: mouse-down calls `interact()` (`src/component/terminal-pane.tsx:293`).
  - Clickable hints: the prompt footer subagent hint dispatches `session.child.first` (`src/feature-plugins/prompt/footer.tsx:66-71`); help "ok" and "esc/enter"; composer "esc" (`src/routes/session/composer/index.tsx:123-125`).
- **Links.** On mouse-up with no ctrl/alt/shift, where the selection is a cell selection that has not moved (`isStart && behavior==="cell"`) and `event.isDragging` is true, `renderer.getLinkAt(x,y)` is opened (`src/ui/link.tsx:42-47`; `src/app.tsx:1336-1337`).
- **Tabs** (details belong to the tabs section): middle-click closes, right-click opens the context menu (vertical rail only), drag reorders (`src/component/session-tabs.tsx:893-915,716-725`). Pane and tab-rail resize use drag (`src/component/session-frame.tsx:268-270`; `src/app.tsx:1345-1348`).

### 5.15 Text selection and copy

- **Config.** `terminal.copy: "manual" | "select"`, default `"select"` (`"manual"` on win32) (`src/config/index.tsx:131-133`; `src/app.tsx:547-548`). Selection itself is drawn by the renderer's native drag selection.
- **Copy-on-select.** On mouse-up with `event.isDragging`, `copy()` runs (`src/util/selection.ts:34-42`). It is wired on the root box (`src/app.tsx:1335-1339`) and on the dialog overlay layer (zIndex 3000, `src/ui/dialog.tsx:224-237`).
- **Manual mode.** Right mouse-down copies the selection and consumes the event (`src/app.tsx:1327-1334`; `src/ui/dialog.tsx:226-233`).
- **`copy()`** (`src/util/selection.ts:44-63`):
  - Returns false (no copy) if there is no selection, if it is a fresh cell selection (`isStart && behavior==="cell"`), or if the text is empty.
  - If the focused renderable is part of the selection and has `getClipboardText`, that transforms the text.
  - On success it writes to the clipboard and shows toast `"Copied to clipboard"` (variant `info`), or `toast.error` on failure.
  - It never clears the selection.
- **Keys while a selection exists** (`handleSelectionKey`, intercept priority 101, `src/util/selection.ts:65-101`; `src/app.tsx:550-557`):
  - `ctrl+c` (or a Kitty `baseCode` of 99/67):
    - If copy-on-select is on and the selection is not inside the focused editor, the selection is cleared **without consuming the key**, so normal `ctrl+c` handling continues.
    - Otherwise `copy()` runs. On success the key is consumed. On failure the selection is cleared and the key is not consumed.
  - `escape`: the selection is cleared. The key is consumed only if the selection had text.
  - Any other key: if the selection belongs to the focused editor, nothing happens (normal editing). Otherwise the selection is cleared and the key is not consumed.
- **Dialogs.** `escape` / `ctrl+c` first clear the selection instead of closing (`src/ui/dialog.tsx:121-124,137-141`). DialogSelect "Back" does the same (`src/ui/dialog-select.tsx:537-540`).
- **Devtools panel.** In base mode, `escape` / `ctrl+c` close the panel. If text is selected and copy is "select", they clear the selection first (`src/component/devtools-bar.tsx:86-100`).
- **Console overlay.** The console has its own `ctrl+y` "copy-selection" binding (`src/app.tsx:243-245`). It copies, shows toast "Copied to clipboard", then clears the selection (`src/app.tsx:562-572`).

### Open questions (keybinds)

1. **Selection plus `ctrl+c`.** With copy-on-select (the default), `ctrl+c` while text is selected outside the editor clears the selection and is NOT consumed (`src/util/selection.ts:78-82`), so it continues to `prompt.clear` or `app.exit`. Confirm in the real app whether this can exit the app, or whether the KM intercept stops after `clearSelection`.
2. **Comma in the leader.** A leader configured with commas, e.g. `"ctrl+x,ctrl+a"`, reaches `registerTimedLeader` unsplit (`src/context/keymap.tsx:137`). The behaviour is undefined: possibly an invalid token or multi-trigger.
3. **Palette ordering is dynamic.** Layers move to the front when their memo recomputes (`KM:src/solid/index.js:64-97`). The port should pick a deterministic order (suggest reverse mount order: session-frame, then prompt, then session route, then plugins, then app). The exact order seen on screen needs capturing from the running app.
4. **Wait time for sequences like `gg`.** `diff.first`/`diff.last` use `gg`, but only the leader has a timeout. Does a bare `g` pending sequence wait forever until the next key? KM `registerNeovimDisambiguation` is not registered.
5. **Inert ids.** `session.child.next/previous` (`right`/`left`), `session.tab.history.forward` (`ctrl+i`), `session.unshare`, `app.scrap` and all `which-key.*` have defaults but no consumer in TUI `src/`. Should the port bind them, e.g. a which-key panel? Note that `ctrl+i` equals `tab` without Kitty.
6. **Redundant session layers.** `baseAndUnfocusedCommands` are bound in two layers: one enabled only when no editor is focused, and one always enabled, both in base mode (`src/routes/session/index.tsx:1327-1334`). This looks redundant. Verify that `home`/`end` while typing in the prompt really jump the transcript rather than move the cursor.
7. **Pinning the parser.** Exact `@opentui/keymap@0.5.17` parse results for `shift+?`, `ctrl+-`, `ctrl+.`, `ctrl+alt+[` and for uppercase names in user config such as v1 `E` are inferred from the parser source. They should be pinned by tests in the Rust port.
8. **Close-sidebar threshold.** It uses `dimensions().width - props.verticalTabsWidth <= 120` (`src/routes/session/index.tsx:252`). Confirm that the sidebar is drawn as an overlay at exactly this threshold; this is cross-referenced to the layout section.

---

## 6. Themes

All paths are relative to `PKG` (`…/scratchpad/src/opencode/packages`). Absolute asset root: `PKG/tui/src/theme/assets/`.

### 6.0 Architecture in one paragraph

There are two on-disk formats. **V1** (`{defs?, theme:{…52 flat color keys…}}`, the old opencode format) and **V2** (`{base, light?, dark?}` with hue scales + nested semantic tokens). The runtime only ever consumes V2: every V1 source (all 32 built-ins, V1 custom files, and the generated `system` theme) is converted with `migrateV1` (`theme/src/tui/v1-migrate.ts:52`) on first use and cached per source object (`tui/src/theme/index.ts:53-61`, WeakMap at `:13`). Format detection is purely structural: a source with a `theme` key is V1, otherwise it is parsed as a V2 document (`tui/src/theme/index.ts:57`); sources with neither a `theme` nor a `base` key are discarded (`tui/src/theme/index.ts:48-51,68-71`). The default theme `opencode` is the **V2** file `assets/v2/opencode.json` (`tui/src/theme/index.ts:4,19-24,30`); the V1 `assets/opencode.json` is **not registered** at runtime (absent from `DEFAULT_THEMES`, `tui/src/theme/v1.ts:37-70`) and is only used by tests (`theme/test/theme.test.ts:5-7`) and as the documented source of the crash-screen fallback palette (`tui/src/component/error-component.tsx:17-43`).

Pipeline (per active theme name + mode):

```
source JSON ──isThemeSource──► parseTheme (V1→migrateV1 | V2→parseThemeDocument)   tui/src/theme/index.ts:53
  ► ThemeDocument {base, light?, dark?}
  ► selectThemeMode(doc, mode): mergeTheme(base, doc[mode])                          theme/src/tui/select.ts:8-17
  ► expandTheme (fill implicit state/muted refs)                                    theme/src/tui/expand.ts:10
  ► resolveExpandedTheme: hue scales → RGBA, "$path" refs → RGBA, @dialog surface   theme/src/tui/resolve.ts:65-81
  ► ResolvedTheme  ──► createComponentTheme (reactive getters)                      tui/src/theme/component.ts:5
                   ──► generateSyntax → OpenTUI SyntaxStyle                         theme/src/tui/syntax.ts:4
                   ──► renderer.setBackgroundColor(background.base)                 tui/src/context/theme.tsx:309
```

### 6.1 V1 theme JSON format

Type: `theme/src/tui/v1.ts:3-76` (re-exported by `tui/src/theme/v1.ts:35`).

```jsonc
{ "$schema": "https://opencode.ai/theme.json",      // optional, ignored
  "defs":  { "<name>": "#hex" | "<refName>" },      // optional palette aliases
  "theme": { "<ThemeColor>": ColorValue, …, "thinkingOpacity"?: number } }
```

`ColorValue` (`theme/src/tui/v1.ts:67`) and resolution rules (`theme/src/tui/v1-migrate.ts:340-376`; identical logic in legacy `tui/src/theme/resolve.ts:5-53`):

| Value | Meaning | Cite |
|---|---|---|
| `"transparent"` or `"none"` | RGBA(0,0,0,0) | `v1-migrate.ts:346` |
| `"#…"` | `RGBA.fromHex` (OpenTUI; #rgb/#rrggbb/#rrggbbaa) | `v1-migrate.ts:347` |
| other string | reference: looked up in `defs` first, then in `theme` (any other theme key), recursively | `v1-migrate.ts:349-351` |
| number | ANSI 256-color index → fixed RGB (table below) | `v1-migrate.ts:353,470-502` |
| `{ "dark": X, "light": Y }` | per-mode variant; picks `[mode]`, then resolves X/Y recursively (reference chain carried) | `v1-migrate.ts:354-355` |
| RGBA instance | passes through (used by generated `system` theme) | `v1-migrate.ts:344` |

Errors (verbatim): `Circular color reference: a -> b -> a` (`v1-migrate.ts:348`), `Color reference "X" not found in defs or theme` (`:350`), `Invalid V1 theme color` (object lacking the mode key, `:354`).

ANSI number → RGB (`v1-migrate.ts:470-502`, same as `tui/src/theme/color.ts:3-41`): 0-15 = `#000000 #800000 #008000 #808000 #000080 #800080 #008080 #c0c0c0 #808080 #ff0000 #00ff00 #ffff00 #0000ff #ff00ff #00ffff #ffffff`; 16-231 = 6×6×6 cube, `i=n-16`, r=`i/36`, g=`(i/6)%6`, b=`i%6`, component `0→0, k→k*40+55`; 232-255 = gray `(n-232)*10+8`; ≥256 → black.

Special keys: `selectedListItemText` optional, defaults to resolved `background`, and a flag `_hasSelectedListItemText` records whether it was explicit (`v1-migrate.ts:363-366`); `backgroundMenu` optional, defaults to `backgroundElement` (`:367-369`); `thinkingOpacity` optional number default `0.6` (`:374`) — **dropped by migration, unused by V2 runtime** (thinking text instead uses `text.muted`, `tui/src/routes/session/message-parts.tsx:49`). No key is validated as present: a missing required key crashes inside `hex(undefined)` during migration (`v1-migrate.ts:87`) and surfaces as a theme load error.

#### 6.1.1 All 52 V1 color tokens and where they end up (semantic role)

V1 tokens have no direct consumers in the V2 TUI; their role is entirely defined by the migration mapping (`theme/src/tui/v1-migrate.ts:86-225`). "N.step" = position anchored in the generated neutral scale; "hue" = drives hue inference.

| V1 token | Required | V2 destination(s) | Cite |
|---|---|---|---|
| `primary` | y | hue inference → `hue.interactive` alias (scale anchored at step 200 = primary). Feeds every `$hue.interactive.200` (formfield hover/focus/press/select text, `text.action.primary.$selected`, `background.action.primary.$focused`). Its "on" color: `selectedForeground(primary)` → `text.action.primary.$focused` | `:88,98,114,124-137,154,281,307` |
| `secondary` | y | hue inference + categorical slot #1 only | `:13,284` |
| `accent` | y | hue inference → `hue.accent` alias | `:113,278,306` |
| `error` | y | `text.feedback.error.base`, `background.action.destructive.base`; `selectedForeground(error)` → `text.action.destructive.base` | `:89,129,140,156` |
| `warning` / `success` / `info` | y | `text.feedback.{warning,success,info}.base` | `:141-143` |
| `text` | y | neutral anchor **200** → `text.base`, `text.formfield.base` | `:96,119,132,430-432` |
| `textMuted` | y | neutral anchor **400** → `text.muted`, `*.$disabled` | `:97,120,429` |
| `selectedListItemText` | n | `selectedForeground()` result (see 6.3.4) | `:378-384` |
| `background` | y | neutral anchor **800** → `background.base` (and formfield/feedback backgrounds via `$background.base`) | `:99,147,159-166,426` |
| `backgroundPanel` | y | neutral anchor **700** → `background.raised.base`, `background.action.primary.$hovered`, dialog surface background | `:100,149,154,220` |
| `backgroundElement` | y | neutral anchor **600** → `background.raised.high`, dialog primary hover | `:101,150,221,428` |
| `backgroundMenu` | n | only a fallback for anchor 600 when `backgroundElement` is falsy (never, RGBA is an object) → effectively ignored | `:428` |
| `border` | y | `border.base` | `:168` |
| `borderActive` | y | `scrollbar.base` | `:169` |
| `borderSubtle` | y | **dropped** (unused) | — |
| `diffAdded`, `diffRemoved`, `diffContext`, `diffHunkHeader` | y | `diff.text.{added,removed,context,hunkHeader}` | `:171-176` |
| `diffAddedBg`, `diffRemovedBg`, `diffContextBg` | y | `diff.background.{added,removed,context}` | `:177-181` |
| `diffHighlightAdded`, `diffHighlightRemoved` | y | `diff.highlight.{added,removed}` | `:182` |
| `diffLineNumber`, `diffAddedLineNumberBg`, `diffRemovedLineNumberBg` | y | `diff.lineNumber.text`, `diff.lineNumber.background.{added,removed}` | `:183-189` |
| `markdownText`, `markdownHeading`, `markdownLink`, `markdownLinkText`, `markdownCode`, `markdownBlockQuote`, `markdownEmph`, `markdownStrong`, `markdownHorizontalRule`, `markdownListItem`, `markdownListEnumeration`, `markdownImage`, `markdownImageText`, `markdownCodeBlock` | y | `markdown.{text,heading,link,linkText,code,blockQuote,emphasis,strong,horizontalRule,listItem,listEnumeration,image,imageText,codeBlock}` (note `Emph`→`emphasis`) | `:202-217` |
| `syntaxComment`, `syntaxKeyword`, `syntaxFunction`, `syntaxVariable`, `syntaxString`, `syntaxNumber`, `syntaxType`, `syntaxOperator`, `syntaxPunctuation` | y | `syntax.{comment,keyword,function,variable,string,number,type,operator,punctuation}` | `:191-201` |
| `thinkingOpacity` (number) | n | dropped | `:374` |

Constant V2 tokens emitted by migration regardless of input (`v1-migrate.ts:118-167,218-223`): `text.action.primary = {base:"$text.base", $disabled:N400, $focused:selFg(primary), $selected:I200}`, `text.action.secondary = {base:"$text.muted", $hovered:"$text.base"}`, `text.action.destructive = {base:selFg(error), $disabled:N400}`, `text.formfield = {base:N200, $hovered/$focused/$pressed/$selected:I200, $disabled:N400}`, `background.raised.max = N500`, `background.action.primary = {base:"transparent", $hovered:N700, $focused:I200, $selected:"transparent"}`, `background.action.secondary = {base:"transparent"}`, `background.formfield.base = "$background.base"`, `background.feedback.*.base = "$background.base"`, `"@dialog": {background:{base:"$background.raised.base", action:{primary:{$hovered:"$background.raised.high"}}}}` (N=`$hue.neutral`, I=`$hue.interactive`).

### 6.2 V2 theme format

Schema: `theme/src/tui/schema.ts`. Document (`schema.ts:288-301`): `{ $schema?, base: BaseThemeDefinition, light?: ModeDefinition, dark?: ModeDefinition }`, at least one of `light`/`dark` required.

| Part | Shape | Cite |
|---|---|---|
| `base` | complete token set (all leaves required except state keys `$…`, `text.feedback.*.muted`) + `categorical` + optional `@dialog` partial overrides; **no `hue`** | `schema.ts:270-275,203-259` |
| `light`/`dark` | **`hue` required**, optional `categorical`, any partial token overrides, optional `@dialog` | `schema.ts:277-283` |
| `hue` | `Record<name, scale \| "$hue.<other>">`; must contain `accent`, `interactive`, `neutral` (error text `Missing required semantic hues: …`) | `schema.ts:39-60` |
| hue scale | object with exactly steps `100..900` (by 100), each hex; unknown step → `Unknown hue step at "hue.x.N"` | `schema.ts:3,44`, `resolve.ts:215-225` |
| `categorical` | non-empty array of hue names (any declared hue, incl. aliases) | `schema.ts:37`, `resolve.ts:67-71` |
| `ColorValue` | hex `#rgb/#rgba/#rrggbb/#rrggbbaa`, `"transparent"`, or `"$<dot.path>"` reference | `schema.ts:27-33` |
| syntax/markdown leaves | restricted: hex or `$hue.<name>.<step>` only (no `$text.base` etc.) | `schema.ts:39-42,139,159` |
| stateful color | `{ base, $hovered?, $focused?, $pressed?, $selected?, $disabled? }` | `schema.ts:62-69` |

Token tree (complete form, `schema.ts:221-259`):

```
text.base, text.muted
text.action.{primary,secondary,destructive}.{base,$disabled,$pressed,$focused,$selected,$hovered}
text.formfield.{base,$…states}
text.feedback.{error,warning,success,info}.{base, muted?}
background.base
background.raised.{base,high,max}
background.action.{primary,secondary,destructive}.{base,$…states}
background.formfield.{base,$…states}
background.feedback.{error,warning,success,info}.base
border.base
scrollbar.base
diff.text.{added,removed,context,hunkHeader}
diff.background.{added,removed,context}
diff.highlight.{added,removed}
diff.lineNumber.text
diff.lineNumber.background.{added,removed}
syntax.{comment,keyword,function,variable,string,number,type,operator,punctuation}
markdown.{text,heading,link,linkText,code,blockQuote,emphasis,strong,horizontalRule,listItem,listEnumeration,image,imageText,codeBlock}
@dialog: <any partial of the above>     // only SurfaceName is "dialog" (schema.ts:14)
```

#### 6.2.1 Mode selection & merge

- `themeModes(doc)` = `["light","dark"]` filtered to present keys (order light, dark) (`select.ts:19-21`).
- `selectThemeMode(doc, mode="light")`: if requested mode absent use `modes[0]` (`select.ts:8-17`). Then `mergeTheme(doc.base, doc[mode])`.
- `mergeTheme` = recursive deep-merge of plain objects; `undefined` skipped; **arrays and strings replace** (so a mode's `categorical` replaces base's) (`expand.ts:26-37`).

#### 6.2.2 Expansion (implicit defaults) — `theme/src/tui/expand.ts`

Applied to the merged definition and separately to its `@dialog` (`expand.ts:10-16`):

| Rule | Condition | Value | Cite |
|---|---|---|---|
| `text.muted` | missing and `text.base` set | `"$text.base"` | `expand.ts:43` |
| `text.feedback.<k>.muted` | missing and `.base` set | `"$text.feedback.<k>.base"` | `expand.ts:46-57` |
| `{text,background}.action.<variant>.$<state>` | variant has `base` and state missing | `"$<text|background>.action.<variant>.base"` | `expand.ts:81-99` |
| `{text,background}.formfield.$<state>` | has `base`, state missing | `"$<…>.formfield.base"` | `expand.ts:71-79` |

States list order: `disabled, pressed, focused, selected, hovered` (`schema.ts:10`).

#### 6.2.3 Resolution — `theme/src/tui/resolve.ts`

1. **Hues** (`resolve.ts:196-231`): each hue name → `{100..900: RGBA}`. An alias string `$hue.X` produces a **clone** of X's 9 RGBA objects (`resolve.ts:210`) — distinct object identity per alias. Errors: `Circular hue reference: a -> b -> a`, `Hue alias "…" must reference a hue scale`, `Hue alias "…" references a missing hue`, `Hue "x" was not found`, `Invalid hue color at "hue.x.N"`.
2. **Categorical** → `HueScale[]` in declared order; `Categorical hue "x" was not found` (`resolve.ts:67-71`).
3. **Token refs** (`resolve.ts:233-283`): a source object `{hue, ...tokens}` is walked; string leaf → `"transparent"` ⇒ RGBA(0,0,0,0); hex ⇒ new RGBA; `"$a.b.c"` ⇒ dot-path lookup in the *same view's* source (special-cased `$hue.<name>.<step>` reads the resolved hue RGBA instance, `resolve.ts:263-271`), cached per path. Keys `$hovered` etc. are renamed to `hovered` etc. in the output (`resolve.ts:273-277`). Errors: `Invalid color "x" at "path"`, `Circular theme reference: …`, `Theme reference "$x" was not found`, `Theme reference "$x" at "path" is not a color`.
4. **Identity matters**: a token that references `$hue.n.s` (directly or through other `$` refs) resolves to the *same RGBA object* as `hue.n[s]`; hex literals create fresh objects. This identity drives `source/increase/decrease` (6.3.2). A Rust port must carry hue provenance `(hue_name, step)` alongside the color.
5. **Stateful colors** get a `state(states)` helper: returns the first state in `[disabled, pressed, focused, selected, hovered]` that is truthy in `states`, else `base` (`resolve.ts:165-170`). (Priority: disabled > pressed > focused > selected > hovered.)
6. **Dialog surface** (`resolve.ts:79,95-131`): if `@dialog` present, the dialog view = `mergeTheme(base, @dialog)` with **action variants recomputed**: for each variant/state, value = `dialog.$state ?? dialog.base ?? base.$state ?? base.base` (so overriding a variant's `base` in `@dialog` resets all its states to that base unless the dialog also sets them). All `$` refs in the dialog view resolve against the dialog view's own tree — e.g. `background.formfield.base: "$background.base"` becomes the dialog background. `surface("dialog")` is **absolute**: both the base view and the dialog view return the same dialog view (`resolve.ts:74-80`, `types.ts:70-71`).
7. Decode/parse errors are re-wrapped (`resolve.ts:39-53`): `Invalid theme: <name> <value> is an invalid value`, where `<value>` is the first `got (…)` token of the Effect schema message or literally `value`. `parseThemeDocument` eagerly resolves every present mode to validate (`resolve.ts:48`); any resolution error is also wrapped this way (message becomes `Invalid theme: <name> value is an invalid value` unless it contains `got …`).

### 6.3 Resolved runtime theme object (what components consume)

Type `ResolvedTheme` (`theme/src/tui/types.ts:25-72`); components get it through `ComponentTheme` (= `ResolvedTheme`, reactive getters, `tui/src/theme/component.ts:5-51`).

| Field | Type | Notes |
|---|---|---|
| `hue` | `Record<"accent"\|"interactive"\|"neutral"\|<any declared>, Record<100..900, RGBA>>` | all declared hue names present (`resolve.ts:230`) |
| `categorical` | `HueScale[]` | used e.g. `categorical[0][200]` for agent extmark |
| `source(color)` | `{hue, step} \| undefined` | identity lookup |
| `increase(color, n=1)` / `decrease(color, n=1)` | `RGBA` | step shift, see 6.3.2 |
| `text.base`, `text.muted` | RGBA | |
| `text.action.{primary,secondary,destructive}` | `StatefulColor` = `{base,disabled,pressed,focused,selected,hovered, state(states)}` | |
| `text.formfield` | `StatefulColor` | |
| `text.feedback.{error,warning,success,info}` | `{base, muted}` | |
| `background.base` | RGBA | also painted as renderer clear color (`tui/src/context/theme.tsx:309`) |
| `background.raised.{base,high,max}` | RGBA | |
| `background.action.{primary,secondary,destructive}`, `background.formfield` | `StatefulColor` | |
| `background.feedback.{error,warning,success,info}.base` | RGBA | |
| `border.base`, `scrollbar.base` | RGBA | |
| `diff.text.{added,removed,context,hunkHeader}`, `diff.background.{added,removed,context}`, `diff.highlight.{added,removed}`, `diff.lineNumber.text`, `diff.lineNumber.background.{added,removed}` | RGBA | |
| `syntax.<9 tokens>`, `markdown.<14 tokens>` | RGBA | |
| `surface("dialog")` | `ResolvedTheme` | absolute; `ComponentTheme.surface` caches wrappers in a shared Map (`component.ts:41-47`) |

Typical consumers (sample, for semantic role): `text.muted` (362 uses, e.g. `tui/src/routes/session/permission.tsx:82`); `background.raised.base` for sidebar (`tui/src/routes/session/sidebar.tsx:23`); `scrollbar.base` as scrollbar thumb fg (`sidebar.tsx:65`); `border.base` rules (`tui/src/routes/session/index.tsx:2116`); list selection = `background.action.primary.focused` bg + `text.action.primary.focused` fg (`tui/src/ui/dialog-select.tsx:642,650`); formfield focus (`dialog-select.tsx:695-698`); destructive confirm (`tui/src/component/dialog-integration.tsx:212-213`); diff colors in diff renderable (`permission.tsx:68-73`); `hue.interactive[200]` active tab number (`tui/src/component/session-tabs.tsx:568`); `hue.accent[200]` (`dialog-select.tsx:749`); `background.raised.max` currently unused in TUI code (0 hits). Dialogs: `Dialog` wraps children in `<ThemeContextProvider context="dialog">` so `useTheme()` inside a dialog returns the dialog surface; backdrop is `RGBA(0,0,0,150)`; panel bg = dialog `background.base` (`tui/src/ui/dialog.tsx:28,33,54,66`; provider `tui/src/context/theme.tsx:367-379`).

#### 6.3.1 Resolved default theme (`opencode`, V2) — computed values

Computed from `assets/v2/opencode.json` with the rules above. Dialog column only where it differs.

| Token | dark | dark @dialog | light | light @dialog |
|---|---|---|---|---|
| hue.accent / hue.interactive alias | purple / orange | | orange / blue | |
| categorical | blue,purple,green,orange,red | | purple,orange,green,blue,red | |
| text.base | #eeeeee | | #1a1a1a | |
| text.muted | #808080 | | #8a8a8a | |
| text.action.primary base/disabled/focused/selected/pressed/hovered | #eeeeee/#808080/#0a0a0a/#fab283/#eeeeee/#eeeeee | | #1a1a1a/#8a8a8a/#ffffff/#3b7dd8/#1a1a1a/#1a1a1a | |
| text.action.secondary base / hovered / others | #808080 / #eeeeee / #808080 | | #8a8a8a / #1a1a1a / #8a8a8a | |
| text.action.destructive base / disabled / others | #0a0a0a / #808080 / #0a0a0a | | #ffffff / #8a8a8a / #ffffff | |
| text.formfield base / hovered,focused,pressed,selected / disabled | #eeeeee / #fab283 / #808080 | | #1a1a1a / #3b7dd8 / #8a8a8a | |
| text.feedback error/warning/success/info (base = muted) | #e06c75/#f5a742/#7fd88f/#56b6c2 | | #d1383d/#d68c27/#3d9a57/#318795 | |
| background.base | #0a0a0a | #141414 | #ffffff | #fafafa |
| background.raised base/high/max | #141414/#1e1e1e/#4c4c4c | | #fafafa/#f5f5f5/#bebebe | |
| background.action.primary base/hovered/focused/selected/disabled/pressed | transparent/#141414/#fab283/transparent/transparent/transparent | hovered #1e1e1e | transparent/#fafafa/#3b7dd8/transparent… | hovered #f5f5f5 |
| background.action.secondary (all) | transparent | | transparent | |
| background.action.destructive (all) | #e06c75 | | #d1383d | |
| background.formfield (all states), background.feedback.*.base | #0a0a0a | #141414 | #ffffff | #fafafa |
| border.base / scrollbar.base | #484848 / #606060 | | #b8b8b8 / #a0a0a0 | |
| diff.text added/removed/context/hunkHeader | #4fd6be/#c53b53/#828bb8/#828bb8 | | #1e725c/#c53b53/#7086b5/#7086b5 | |
| diff.background added/removed/context | #20303b/#37222c/#141414 | | #d5e5d5/#f7d8db/#fafafa | |
| diff.highlight added/removed | #b8db87/#e26a75 | | #4db380/#f52a65 | |
| diff.lineNumber text / bg added / bg removed | #8f8f8f/#1b2b34/#2d1f26 | | #595959/#c5d5c5/#e7c8cb | |
| syntax comment/keyword/function/variable/string/number/type/operator/punctuation | #808080/#9d7cd8/#fab283/#e06c75/#7fd88f/#f5a742/#e5c07b/#56b6c2/#eeeeee | | #8a8a8a/#d68c27/#3b7dd8/#d1383d/#3d9a57/#d68c27/#b0851f/#318795/#1a1a1a | |
| markdown text/heading/link/linkText/code/blockQuote/emphasis/strong/horizontalRule/listItem/listEnumeration/image/imageText/codeBlock | #eeeeee/#9d7cd8/#fab283/#56b6c2/#7fd88f/#e5c07b/#e5c07b/#f5a742/#808080/#fab283/#56b6c2/#fab283/#56b6c2/#eeeeee | | #1a1a1a/#d68c27/#3b7dd8/#318795/#3d9a57/#b0851f/#b0851f/#d68c27/#8a8a8a/#3b7dd8/#318795/#3b7dd8/#318795/#1a1a1a | |

#### 6.3.2 Step arithmetic (`source`/`increase`/`decrease`) — `resolve.ts:172-194`

Index every RGBA *object* of every hue scale (including aliases, which are clones) → `{hue, step, position 0..8}`. `increase(c, n)` = `hue[match.hue][STEPS[clamp(pos + trunc(n), 0, 8)]]`; `decrease` = `pos - n`. If `c` is not a hue-scale object (e.g. a hex literal token, or transparent), it is returned unchanged. Non-finite `n` → 0. Equal-valued but distinct steps are still distinguished (test `tui/test/theme/v2/resolve.test.ts:198-217`). Direction: higher step = toward `background` (darker in dark mode, lighter in light mode). Example: dark `decrease(background.raised.base)` = neutral.600 `#1e1e1e` (used at `tui/src/routes/session/permission.tsx:338`). `ComponentTheme.increase/decrease` default `amount=1` (`component.ts:39-40`).

#### 6.3.3 Syntax highlight scope → color (`theme/src/tui/syntax.ts:4-86`)

`SyntaxStyle.fromTheme([...rules])`; each rule `{scope[], style:{foreground, bold?, italic?, underline?, background?}}`. Rules in source order:

| # | Scopes | fg | Extra |
|---|---|---|---|
| 1 | default | text.base | |
| 2 | prompt | hue.accent[200] | |
| 3 | extmark.file | text.feedback.warning.base | bold |
| 4 | extmark.agent | categorical[0][200] | bold |
| 5 | extmark.skill | (categorical[1] ?? categorical[0])[200] | bold |
| 6 | extmark.paste | text.action.primary.focused | bg text.feedback.warning.base, bold |
| 7 | comment, comment.documentation | syntax.comment | italic |
| 8 | string, symbol, character.special, character | syntax.string | |
| 9 | number, boolean, constant, float | syntax.number | |
| 10 | keyword.return, keyword.conditional, keyword.repeat, keyword.coroutine | syntax.keyword | italic |
| 11 | keyword.type | syntax.type | bold, italic |
| 12 | keyword.function, function.method | syntax.function | |
| 13 | keyword | syntax.keyword | italic |
| 14 | keyword.import, string.escape, string.regexp, tag.attribute, keyword.export | syntax.keyword | |
| 15 | operator, keyword.operator, punctuation.delimiter, keyword.conditional.ternary | syntax.operator | |
| 16 | variable, variable.parameter, function.method.call, function.call, property, parameter, field | syntax.variable | |
| 17 | variable.member, function, constructor | syntax.function | |
| 18 | type, module, class, namespace | syntax.type | |
| 19 | type.definition | syntax.type | bold |
| 20 | punctuation, punctuation.bracket | syntax.punctuation | |
| 21 | variable.builtin, type.builtin, function.builtin, module.builtin, constant.builtin, variable.super | text.feedback.error.base | |
| 22 | keyword.directive, keyword.modifier, keyword.exception | syntax.keyword | italic |
| 23 | punctuation.special, tag.delimiter | syntax.operator | |
| 24 | markup.heading, markup.heading.2 … markup.heading.6 | markdown.heading | bold |
| 25 | markup.heading.1 | markdown.heading | bold, underline |
| 26 | markup.bold, markup.strong | markdown.strong | bold |
| 27 | markup.italic | markdown.emphasis | italic |
| 28 | markup.list | markdown.listItem | |
| 29 | markup.quote | markdown.blockQuote | italic |
| 30 | markup.raw, markup.raw.block | markdown.code | |
| 31 | markup.raw.inline | markdown.code | bg background.base |
| 32 | markup.link, markup.link.url, string.special, string.special.url | markdown.link | underline |
| 33 | markup.link.label | markdown.linkText | underline |
| 34 | label | markdown.linkText | |
| 35 | spell, nospell | text.base | |
| 36 | markup.underline | text.base | underline |
| 37 | comment.error | feedback.error.base | italic, bold |
| 38 | comment.warning | feedback.warning.base | italic, bold |
| 39 | comment.todo, comment.note | feedback.info.base | italic, bold |
| 40 | attribute, annotation | feedback.warning.base | |
| 41 | tag | feedback.error.base | |
| 42 | markup.strikethrough, markup.list.unchecked, debug | text.muted | |
| 43 | markup.list.checked | feedback.success.base | |
| 44 | diff.plus | diff.text.added | bg diff.background.added |
| 45 | diff.minus | diff.text.removed | bg diff.background.removed |
| 46 | diff.delta | diff.text.context | bg diff.background.context |
| 47 | error | feedback.error.base | bold |
| 48 | warning | feedback.warning.base | bold |
| 49 | info | feedback.info.base | |

(`feedback.*` = `text.feedback.*`.) Unused theme tokens in syntax: `markdown.text/horizontalRule/listEnumeration/image/imageText/codeBlock`. Thinking/reasoning blocks reuse every registered style with fg forced to `text.muted` (`tui/src/routes/session/thinking-syntax.ts:3-7`, `tui/src/routes/session/message-parts.tsx:49`). SyntaxStyle objects are recreated on theme change and old ones destroyed after `renderer.idle()` (`tui/src/context/theme.tsx:394-420`).

#### 6.3.4 Derived-color formulas

| Derivation | Formula | Cite |
|---|---|---|
| `tint(base, overlay, a)` (blend) | per channel `round((b + (o-b)*a)*255)`, alpha always 255 | `tui/src/theme/color.ts:43-48` |
| V1 selected foreground `selectedForeground(theme, bg)` | explicit `selectedListItemText` if given; else V1 `background` if its alpha≠0; else `0.299r+0.587g+0.114b > 0.5` (0..1 floats of `bg`) ? black : white | `v1-migrate.ts:378-384` |
| V1 mode detection | `lum(text) > lum(background) ? "dark" : "light"`, lum = `0.299r+0.587g+0.114b` | `v1-migrate.ts:78-84` |
| terminal mode | `lum(defaultBackground) > 0.5 ? "light" : "dark"`; undefined if no OSC 11 bg (does not fall back to palette[0]) | `tui/src/theme/system.ts:4-9`, test `tui/test/theme.test.ts:116-118` |
| terminal pane ANSI palette (OSC 4/10/11 sent *to embedded terminals*) | 0=neutral[dark?800:200], 1..6 = error, success, warning, nearest-hue(blue h of rgb(0,0,1)), nearest-hue(magenta), info, 7=neutral[dark?200:800], 8=text.muted, 9..14=`decrease()` of 1..6, 15=neutral[dark?100:900]; OSC10=text.base, OSC11=bg. nearest-hue: hue[*][200] with alpha>0, chroma≥0.03, min OKLCH hue distance; if >30° → accent[200] | `tui/src/component/terminal-pane.tsx:339-382` |
| other component blends (owned by component specs) | e.g. tab rail `tint(raised.base, raised.high, raised.high.a)`, file-tree faint `tint(text.muted, raised.base, .45)`/rail `.7`, idle tab number `tint(formfield.base, bg, .55)` | `tui/src/component/session-tabs-rail.tsx:17`, `tui/src/feature-plugins/system/diff-viewer-file-tree.tsx:40-42`, `tui/src/component/session-tabs.tsx:569-571` |

### 6.4 V1 → V2 migration algorithm (`theme/src/tui/v1-migrate.ts`)

**Top level** (`:52-71`): resolve the V1 theme twice (`light`, `dark`). If both backgrounds are opaque, equal, and `detectMode` agrees on both → emit a **single-mode** doc `{base: def-minus-hue, <thatMode>: {hue}}` built from that resolution. Otherwise emit `{base: lightDef-minus-hue, light:{hue:lightHue}, dark: darkDef (complete, incl. hue)}`.

**Hue inference** (`:276-338`):
- OKLCH via `rgbToOklch` (`theme/src/tui/color.ts:25-38`, standard Björn Ottosson matrices).
- A color is *ambiguous* (no hue) if alpha==0 or chroma < `0.03` (`:21,336-338`).
- Reference angles: if `L >= 0.6` use light swatches else dark swatches (`:22-50,322`). Computed OKLCH hue angles (deg): light red 19.57, orange 66.29, yellow 98.11, green 154.45, cyan 207.08, blue 251.81, purple 306.38; dark red 27.52, orange 38.40, yellow 66.44, green 150.07, cyan 223.13, blue 264.38, purple 301.92. Nearest by circular distance; ties → first in order red, orange, yellow, green, cyan, blue, purple.
- Pass 1 over tokens `accent, success, warning, primary, error, info, secondary` (`:277-285`): `byToken[token] = hue`; `byHue[hue]` keeps the color with the smallest distance (earlier wins ties).
- Pass 2: `accent` then `primary` unconditionally claim `byHue[their hue]` (`:304-316`) — so primary wins if both share a hue.
- Emitted hues: `gray` (neutral scale), each chromatic hue in `byHue` (`hueScale`), `accent: "$hue.<byToken.accent>"` or `"$hue.gray"`, `interactive: "$hue.<byToken.primary>"` or `"$hue.gray"`, `neutral: "$hue.gray"` (`:105-116`).
- `categorical` = hues of tokens in order `secondary, accent, success, warning, primary, error, info` (skipping ambiguous), deduped keeping first; `["neutral"]` if empty (`:12-20,91-95,117`).

**Chromatic scale `hueScale(color, mode)`** (`:386-403`): step 200 = exact source hex. For other steps `p = (step-200)/700` (step 100 → p = −1/7, extrapolation). `endpoint = light ? max(0.97, L) : min(0.18, L)`; `L' = L + (endpoint-L)*p`, `C' = C*(1 - 0.5p)`, `h' = h`; → `oklchToHex` with gamut fitting: clamp L∈[0,1], C≥0; if out of sRGB, try `C*0.9^k` for k=1..24, else C=0 (`color.ts:56-77`). Source alpha≠255 is appended as `aa` byte.

**Neutral scale `neutralScale(theme)`** (`:405-433`): anchors (after reversal `1000-step`): **200=text, 400=textMuted, 600=backgroundElement (or backgroundMenu), 700=backgroundPanel, 800=background** — same mapping for both modes. Missing steps interpolate in OKLCH between bracketing anchors (`interpolate`, `:435-451`: L, C linear; hue via shortest arc `((Δ%360)+540)%360−180`; alpha linear, rounded, clamped 0..255, appended if ≠255):
- 100: extrapolated from (200, 400) with t = −0.5
- 300: (200, 400) t = 0.5
- 500: (400, 600) t = 0.5  → `background.raised.max`
- 900: extrapolated from (700, 800) with t = 2

**Literal → reference rewriting `referenceHues`** (`:227-274`): build map lowercase-hex → `$hue.<name>.<step>` indexing chromatic hues (no overwrite), then `gray` (no overwrite), then `accent`, `interactive`, `neutral` (overwrite) — final precedence neutral > interactive > accent > gray > chromatic(first in red…purple order), later steps overwrite earlier within an overwriting scale. Every string token (except `hue`/`categorical`) whose lowercase value matches is replaced by the reference (test: `tui/test/theme/v2/v1-migrate.test.ts:60-73`). Effect: tokens equal to a scale color gain hue provenance (so `increase/decrease` work on them).

`hex()` output: `#rrggbb` lowercase, with `aa` only if alpha≠255 (`:458-468`).

Single-mode V1 built-ins (both backgrounds equal → dark-only document; "Switch to light mode" is disabled): **aura, ayu, catppuccin-frappe, catppuccin-macchiato, nightowl** (computed from assets). `lucent-orng` uses `"none"`/transparent background → always two modes.

### 6.5 Built-in themes

Registered names = 32 V1 assets (`tui/src/theme/v1.ts:37-70`) + `opencode` (V2) + `system` (`tui/src/theme/index.ts:26-37`) = 34. Default name `"opencode"` (`tui/src/context/theme.tsx:115,137,292`). Theme picker lists `Object.keys(all())` sorted with `localeCompare(…, {sensitivity:"base"})` (`tui/src/component/dialog-theme-list.tsx:8-13`).

| Name | File (absolute: prefix `PKG/tui/src/theme/assets/`) | Modes |
|---|---|---|
| aura | aura.json | dark only |
| ayu | ayu.json | dark only |
| carbonfox | carbonfox.json | light+dark |
| catppuccin | catppuccin.json | light+dark |
| catppuccin-frappe | catppuccin-frappe.json | dark only |
| catppuccin-macchiato | catppuccin-macchiato.json | dark only |
| cobalt2 | cobalt2.json | light+dark |
| cursor | cursor.json | light+dark |
| dracula | dracula.json | light+dark |
| everforest | everforest.json | light+dark |
| flexoki | flexoki.json | light+dark |
| github | github.json | light+dark |
| gruvbox | gruvbox.json | light+dark |
| kanagawa | kanagawa.json | light+dark |
| lucent-orng | lucent-orng.json (transparent bg, explicit `selectedListItemText`, `backgroundMenu`) | light+dark |
| material | material.json | light+dark |
| matrix | matrix.json | light+dark |
| mercury | mercury.json | light+dark |
| monokai | monokai.json | light+dark |
| nightowl | nightowl.json | dark only |
| nord | nord.json | light+dark |
| one-dark | one-dark.json | light+dark |
| orng | orng.json (explicit `selectedListItemText`) | light+dark |
| osaka-jade | osaka-jade.json | light+dark |
| palenight | palenight.json | light+dark |
| rosepine | rosepine.json | light+dark |
| solarized | solarized.json | light+dark |
| synthwave84 | synthwave84.json | light+dark |
| tokyonight | tokyonight.json | light+dark |
| vercel | vercel.json | light+dark |
| vesper | vesper.json | light+dark |
| zenburn | zenburn.json | light+dark |
| **opencode** (default) | **v2/opencode.json** (V2) | light+dark |
| system | generated (6.6); until a palette is known it is an alias of `opencode` (or a custom `system.json`) | light+dark |
| (unregistered) | opencode.json (V1, tests/fallback reference only) | — |

Copy command target: `PKG/tui/src/theme/assets/*.json` and `PKG/tui/src/theme/assets/v2/opencode.json`.

### 6.6 The `system` theme (terminal palette)

Generation `generateSystem(colors, mode)` (`tui/src/theme/system.ts:11-99`) returns a **V1-shaped** `{theme:{…RGBA}}` source, then goes through `migrateV1` like any V1 theme (`tui/src/theme/index.ts:57`).

Inputs: OpenTUI `TerminalColors` = `palette[0..15]` (OSC 4 replies) + `defaultForeground` (OSC 10) + `defaultBackground` (OSC 11), obtained by `renderer.getPalette({size:16})` (`tui/src/context/theme.tsx:201`). `bg = defaultBackground ?? palette[0]`, `fg = defaultForeground ?? palette[7]`; missing palette slot `i` → `ansiToRgba(i)` (`system.ts:12-21`). A palette is *usable* only if both bg and fg sources exist (`theme.tsx:381-385`).

Gray scale `grays[i]`, i=1..12, `f=i/12`, `lum = 0.299R+0.587G+0.114B` of bg in 0..255 (`system.ts:101-133`):
- dark & lum<10: `g=floor(f*0.4*255)` gray.
- light & lum>245: `g=floor(255 - f*0.4*255)` gray.
- else `next = dark ? lum + (255-lum)*f*0.4 : lum*(1-f*0.4)`, `ratio=next/lum`, each channel `floor(clamp(C*ratio,0,255))`.

Muted text (`system.ts:135-144`): dark: `lum<10 ? 180 : min(floor(160+lum*0.3),200)`; light: `lum>245 ? 75 : max(floor(100-(255-lum)*0.2),60)` (gray).

Token assignment (`system.ts:25-97`; `col(n)` = palette slot n): primary=accent=info=`col6` cyan; secondary=`col5`; error=`col1`; warning=`col3`; success=`col2`; text=fg; textMuted=muted; **selectedListItemText=bg (opaque)**; **background = bg with alpha 0** (transparent → terminal's own bg shows; renderer clear color transparent); backgroundPanel=grays[2]; backgroundElement=backgroundMenu=grays[3]; borderSubtle=grays[6]; border=grays[7]; borderActive=grays[8]; diffAdded=col2; diffRemoved=col1; diffContext=diffHunkHeader=grays[7]; diffHighlightAdded=col10; diffHighlightRemoved=col9; `a = dark?0.22:0.14`; diffAddedBg=`tint(bg,col2,a)`; diffRemovedBg=`tint(bg,col1,a)`; diffContextBg=grays[2]; diffLineNumber=muted; diffAdded/RemovedLineNumberBg=`tint(grays[2], col2/col1, a)`; markdownText/Heading/Strong/CodeBlock=fg; markdownLink=col4; LinkText=col6; Code=col2; BlockQuote=Emph=col3; HorizontalRule=grays[7]; ListItem=col4; ListEnumeration=col6; Image=col4; ImageText=col6; syntaxComment=muted; Keyword=col5; Function=col4; Variable=fg; String=col2; Number=col3; Type=col6; Operator=col6; Punctuation=fg.

Because background alpha is 0, migration always emits two modes (6.4); the neutral 800 anchor is transparent, so `background.base`/formfield/feedback backgrounds are transparent and neutral.900 extrapolates to alpha 0 too.

Lifecycle (`tui/src/context/theme.tsx`):
- Cache: last good palette persisted to storage key `system-theme` → file `<state>/<channel>/tui/system-theme.json` as `{colors}` (`theme.tsx:129,208-210`; `tui/src/context/storage.tsx:47-57`; state = `$XDG_STATE_HOME/opencode` or `~/.local/state/opencode`, `util/src/global-roots.ts:8,16`).
- On init, if a cached usable palette exists: set mode `lock ?? terminalMode(palette) ?? mode` and register the system theme immediately — **regardless of which theme is active** (`theme.tsx:175-184`); else `setSystemTheme(undefined)` (system = opencode alias).
- Probing (`refreshPalette`, `theme.tsx:190-218`) only runs when active theme is `system`, only after first paint (`afterPaint` → `canProbe`), with single-flight + queued re-run; it clears the renderer palette cache, queries 16 colors, updates mode (unless locked), regenerates, persists cache. `afterPaint` is invoked 50 ms after the first renderer FRAME event (`tui/src/app.tsx:481-485`).
- Re-probe triggers: selecting `system` (`theme.tsx:146,335`), mode change (`apply`, `:225-231`), terminal color-scheme notifications `ESC[?997;1n` / `ESC[?997;2n` (input handler, not consumed, `:261-266`), SIGUSR2 (`:269-278`).
- Regeneration uses the current `store.mode` (`applyPalette`, `:177-179`).

### 6.7 Light/dark mode detection

Config `theme.mode ∈ "system"|"dark"|"light"` (`tui/src/config/index.tsx:70-77`).

1. Startup: `mode = terminalHandoff?.mode ?? await renderer.waitForThemeMode(1000) ?? "dark"` (`tui/src/app.tsx:291`) → passed as `ThemeProvider mode` (`app.tsx:380-383`).
2. Provider init (`theme.tsx:131-140`): `lock = config.theme.mode if "dark"|"light"`; `store.mode = lock ?? renderer.themeMode ?? props.mode`; `store.active = config.theme.name ?? "opencode"`.
3. Cached system palette may override the unlocked mode (6.6).
4. Runtime: OpenTUI `CliRenderEvents.THEME_MODE` → if unlocked, `apply(mode)` (`theme.tsx:255-259`).
5. Effective mode per theme: `loadTheme` falls back to the document's first available mode if requested is unsupported, while `store.mode` keeps the request (`theme.tsx:387-392`). `themes.mode()` reports the *effective* mode; `modes()`/`supports()` the document's modes (`theme.tsx:302-303,321-323`).
6. Commands (`tui/src/app.tsx:1054-1074`): `theme.switch_mode` title "Switch to light mode"/"Switch to dark mode", enabled only if target mode supported → `setMode(other)` → pins + persists. `theme.mode.lock` title "Lock theme mode"/"Unlock theme mode" → `lock()` pins current effective mode (persist) / `unlock()` sets lock undefined, `apply(renderer.themeMode ?? store.mode)`, persists `mode:"system"` (`theme.tsx:233-253,324-331`). Keybinds default `none` (`tui/src/config/keybind.ts:92-94`); `theme.switch` = slash `/themes` (`app.tsx:1045-1053`; V1 keybind `theme_list` `<leader>t`, `tui/src/config/v1/keybind.ts:77`).
7. Config hot changes: effect pins (no persist) when config mode becomes dark/light; frees when it becomes `system` while locked (`theme.tsx:149-156`).

### 6.8 Custom themes: discovery, precedence, reload, errors

- Directories (`tui/src/util/config-directories.ts:4-6,38-45`): `[globalConfig, …ancestors(cwd) root→cwd each joined with ".opencode"]`, deduped. `globalConfig = $OPENCODE_CONFIG_DIR ?? $XDG_CONFIG_HOME/opencode ?? ~/.config/opencode` (`util/src/global.ts:79`, `util/src/global-roots.ts:7,15`; wired at `tui/src/app.tsx:382`, `tui/src/context/theme.tsx:67-70`).
- Per directory: `<dir>/themes/*.json` (regular files or symlinks, extension exactly `.json`), sorted by path; theme name = basename without `.json`; content `JSON.parse` (no JSONC) (`tui/src/theme/discovery.ts:4-21`). Missing dir ignored (ENOENT); other errors reject.
- Precedence: later directories override earlier (closest `.opencode` to cwd wins over global) (test `tui/test/theme.test.ts:120-131`). Overall registry: built-in V1 < `opencode` V2 < custom (custom may shadow `opencode` or any built-in) < generated `system` (`tui/src/theme/index.ts:26-37`).
- File may be V1 (`theme` key) or V2 (`base` key); others silently ignored (`index.ts:48-51,68-71`). Parsing is lazy at selection time (`theme.tsx:290-301`).
- Reload: initial discovery on mount (`theme.tsx:167-173`); **no file watcher on theme dirs**; `SIGUSR2` schedules refreshPalette at 250 ms and refreshPalette + re-discovery at 1000 ms (`theme.tsx:72-77,268-278`). Config file changes are watched (`tui/src/config/index.tsx:357-364`) and re-apply `theme.name`/`theme.mode`.
- Discovery failure (e.g. invalid JSON anywhere) → `active` silently reset to `opencode`, custom list unchanged, **no toast** (`theme.tsx:158-165`).
- Unknown active name → silently renders `opencode` (`theme.tsx:292`).
- Load/parse/resolve failure of a theme → error emitted (buffered until a handler subscribes, `theme.tsx:34-60`), `active` reset to `opencode` (not persisted), opencode rendered (`theme.tsx:293-300`). If `opencode` itself fails, it throws (crash screen).
- Error toast (`tui/src/component/theme-error-toast.tsx:10-16`), verbatim: `variant: "error"`, `title: \`Failed to load theme: ${name}\``, `message: error.message` (e.g. `Invalid theme: mytheme "opaque" is an invalid value`, `Circular color reference: one -> two -> one`).

### 6.9 Persistence of selection

- Stored in the CLI config file `<globalConfig>/cli.json` (`cli/src/config/config.ts:31`) under `"theme": { "name": string, "mode": "system"|"dark"|"light" }` (`tui/src/config/index.tsx:70-77`).
- `themes.set(name)`: ignored (returns false) if unknown; else sets active, probes if `system`, writes `theme.name` (`theme.tsx:332-342`). The theme picker calls `set` on every cursor move/filter (live preview, each a config write) and restores the initial theme on cancel (`dialog-theme-list.tsx:19-47`).
- Mode writes: `setMode`/`lock` → `theme.mode = <mode>`; `unlock` → `"system"` (`theme.tsx:233-253`).
- Legacy migration: old `tui.json` `theme` or `kv.json` `theme` → `theme.name`; `kv.json` `theme_mode_lock` → `theme.mode` (`cli/src/config/migrate.ts:100-102,167-191`).
- Palette cache: `system-theme.json` (6.6).

### 6.10 Default theme sources (verbatim)

#### `PKG/tui/src/theme/assets/opencode.json` (V1; reference only, not registered)

```json
{
  "$schema": "https://opencode.ai/theme.json",
  "defs": {
    "darkStep1": "#0a0a0a",
    "darkStep2": "#141414",
    "darkStep3": "#1e1e1e",
    "darkStep4": "#282828",
    "darkStep5": "#323232",
    "darkStep6": "#3c3c3c",
    "darkStep7": "#484848",
    "darkStep8": "#606060",
    "darkStep9": "#fab283",
    "darkStep10": "#ffc09f",
    "darkStep11": "#808080",
    "darkStep12": "#eeeeee",
    "darkSecondary": "#5c9cf5",
    "darkAccent": "#9d7cd8",
    "darkRed": "#e06c75",
    "darkOrange": "#f5a742",
    "darkGreen": "#7fd88f",
    "darkCyan": "#56b6c2",
    "darkYellow": "#e5c07b",
    "lightStep1": "#ffffff",
    "lightStep2": "#fafafa",
    "lightStep3": "#f5f5f5",
    "lightStep4": "#ebebeb",
    "lightStep5": "#e1e1e1",
    "lightStep6": "#d4d4d4",
    "lightStep7": "#b8b8b8",
    "lightStep8": "#a0a0a0",
    "lightStep9": "#3b7dd8",
    "lightStep10": "#2968c3",
    "lightStep11": "#8a8a8a",
    "lightStep12": "#1a1a1a",
    "lightSecondary": "#7b5bb6",
    "lightAccent": "#d68c27",
    "lightRed": "#d1383d",
    "lightOrange": "#d68c27",
    "lightGreen": "#3d9a57",
    "lightCyan": "#318795",
    "lightYellow": "#b0851f"
  },
  "theme": {
    "primary": {
      "dark": "darkStep9",
      "light": "lightStep9"
    },
    "secondary": {
      "dark": "darkSecondary",
      "light": "lightSecondary"
    },
    "accent": {
      "dark": "darkAccent",
      "light": "lightAccent"
    },
    "error": {
      "dark": "darkRed",
      "light": "lightRed"
    },
    "warning": {
      "dark": "darkOrange",
      "light": "lightOrange"
    },
    "success": {
      "dark": "darkGreen",
      "light": "lightGreen"
    },
    "info": {
      "dark": "darkCyan",
      "light": "lightCyan"
    },
    "text": {
      "dark": "darkStep12",
      "light": "lightStep12"
    },
    "textMuted": {
      "dark": "darkStep11",
      "light": "lightStep11"
    },
    "background": {
      "dark": "darkStep1",
      "light": "lightStep1"
    },
    "backgroundPanel": {
      "dark": "darkStep2",
      "light": "lightStep2"
    },
    "backgroundElement": {
      "dark": "darkStep3",
      "light": "lightStep3"
    },
    "border": {
      "dark": "darkStep7",
      "light": "lightStep7"
    },
    "borderActive": {
      "dark": "darkStep8",
      "light": "lightStep8"
    },
    "borderSubtle": {
      "dark": "darkStep6",
      "light": "lightStep6"
    },
    "diffAdded": {
      "dark": "#4fd6be",
      "light": "#1e725c"
    },
    "diffRemoved": {
      "dark": "#c53b53",
      "light": "#c53b53"
    },
    "diffContext": {
      "dark": "#828bb8",
      "light": "#7086b5"
    },
    "diffHunkHeader": {
      "dark": "#828bb8",
      "light": "#7086b5"
    },
    "diffHighlightAdded": {
      "dark": "#b8db87",
      "light": "#4db380"
    },
    "diffHighlightRemoved": {
      "dark": "#e26a75",
      "light": "#f52a65"
    },
    "diffAddedBg": {
      "dark": "#20303b",
      "light": "#d5e5d5"
    },
    "diffRemovedBg": {
      "dark": "#37222c",
      "light": "#f7d8db"
    },
    "diffContextBg": {
      "dark": "darkStep2",
      "light": "lightStep2"
    },
    "diffLineNumber": {
      "dark": "#8f8f8f",
      "light": "#595959"
    },
    "diffAddedLineNumberBg": {
      "dark": "#1b2b34",
      "light": "#c5d5c5"
    },
    "diffRemovedLineNumberBg": {
      "dark": "#2d1f26",
      "light": "#e7c8cb"
    },
    "markdownText": {
      "dark": "darkStep12",
      "light": "lightStep12"
    },
    "markdownHeading": {
      "dark": "darkAccent",
      "light": "lightAccent"
    },
    "markdownLink": {
      "dark": "darkStep9",
      "light": "lightStep9"
    },
    "markdownLinkText": {
      "dark": "darkCyan",
      "light": "lightCyan"
    },
    "markdownCode": {
      "dark": "darkGreen",
      "light": "lightGreen"
    },
    "markdownBlockQuote": {
      "dark": "darkYellow",
      "light": "lightYellow"
    },
    "markdownEmph": {
      "dark": "darkYellow",
      "light": "lightYellow"
    },
    "markdownStrong": {
      "dark": "darkOrange",
      "light": "lightOrange"
    },
    "markdownHorizontalRule": {
      "dark": "darkStep11",
      "light": "lightStep11"
    },
    "markdownListItem": {
      "dark": "darkStep9",
      "light": "lightStep9"
    },
    "markdownListEnumeration": {
      "dark": "darkCyan",
      "light": "lightCyan"
    },
    "markdownImage": {
      "dark": "darkStep9",
      "light": "lightStep9"
    },
    "markdownImageText": {
      "dark": "darkCyan",
      "light": "lightCyan"
    },
    "markdownCodeBlock": {
      "dark": "darkStep12",
      "light": "lightStep12"
    },
    "syntaxComment": {
      "dark": "darkStep11",
      "light": "lightStep11"
    },
    "syntaxKeyword": {
      "dark": "darkAccent",
      "light": "lightAccent"
    },
    "syntaxFunction": {
      "dark": "darkStep9",
      "light": "lightStep9"
    },
    "syntaxVariable": {
      "dark": "darkRed",
      "light": "lightRed"
    },
    "syntaxString": {
      "dark": "darkGreen",
      "light": "lightGreen"
    },
    "syntaxNumber": {
      "dark": "darkOrange",
      "light": "lightOrange"
    },
    "syntaxType": {
      "dark": "darkYellow",
      "light": "lightYellow"
    },
    "syntaxOperator": {
      "dark": "darkCyan",
      "light": "lightCyan"
    },
    "syntaxPunctuation": {
      "dark": "darkStep12",
      "light": "lightStep12"
    }
  }
}
```

#### `PKG/tui/src/theme/assets/v2/opencode.json` (V2; the actual runtime default, 387 lines — included because every default-render pixel derives from it)

```json
{
  "$schema": "https://opencode.ai/theme.json",
  "base": {
    "categorical": [
      "blue",
      "purple",
      "green",
      "orange",
      "red"
    ],
    "text": {
      "base": "$hue.neutral.200",
      "muted": "$hue.neutral.400",
      "action": {
        "primary": {
          "base": "$text.base",
          "$disabled": "$hue.neutral.400",
          "$focused": "$hue.neutral.800",
          "$selected": "$hue.interactive.200"
        },
        "secondary": {
          "base": "$text.muted",
          "$hovered": "$text.base"
        },
        "destructive": {
          "base": "$hue.neutral.800",
          "$disabled": "$hue.neutral.400"
        }
      },
      "formfield": {
        "base": "$hue.neutral.200",
        "$hovered": "$hue.interactive.200",
        "$focused": "$hue.interactive.200",
        "$pressed": "$hue.interactive.200",
        "$disabled": "$hue.neutral.400",
        "$selected": "$hue.interactive.200"
      },
      "feedback": {
        "error": {
          "base": "$hue.red.200"
        },
        "warning": {
          "base": "#f5a742"
        },
        "success": {
          "base": "$hue.green.200"
        },
        "info": {
          "base": "$hue.cyan.200"
        }
      }
    },
    "background": {
      "base": "$hue.neutral.800",
      "raised": {
        "base": "$hue.neutral.700",
        "high": "$hue.neutral.600",
        "max": "$hue.neutral.500"
      },
      "action": {
        "primary": {
          "base": "transparent",
          "$hovered": "$hue.neutral.700",
          "$focused": "$hue.interactive.200",
          "$selected": "transparent"
        },
        "secondary": {
          "base": "transparent"
        },
        "destructive": {
          "base": "$hue.red.200"
        }
      },
      "formfield": {
        "base": "$background.base"
      },
      "feedback": {
        "error": {
          "base": "$background.base"
        },
        "warning": {
          "base": "$background.base"
        },
        "success": {
          "base": "$background.base"
        },
        "info": {
          "base": "$background.base"
        }
      }
    },
    "border": {
      "base": "#484848"
    },
    "scrollbar": {
      "base": "#606060"
    },
    "diff": {
      "text": {
        "added": "#4fd6be",
        "removed": "#c53b53",
        "context": "#828bb8",
        "hunkHeader": "#828bb8"
      },
      "background": {
        "added": "#20303b",
        "removed": "#37222c",
        "context": "$hue.neutral.700"
      },
      "highlight": {
        "added": "#b8db87",
        "removed": "#e26a75"
      },
      "lineNumber": {
        "text": "#8f8f8f",
        "background": {
          "added": "#1b2b34",
          "removed": "#2d1f26"
        }
      }
    },
    "syntax": {
      "comment": "$hue.neutral.400",
      "keyword": "$hue.accent.200",
      "function": "$hue.interactive.200",
      "variable": "$hue.red.200",
      "string": "$hue.green.200",
      "number": "#f5a742",
      "type": "#e5c07b",
      "operator": "$hue.cyan.200",
      "punctuation": "$hue.neutral.200"
    },
    "markdown": {
      "text": "$hue.neutral.200",
      "heading": "$hue.accent.200",
      "link": "$hue.interactive.200",
      "linkText": "$hue.cyan.200",
      "code": "$hue.green.200",
      "blockQuote": "#e5c07b",
      "emphasis": "#e5c07b",
      "strong": "#f5a742",
      "horizontalRule": "$hue.neutral.400",
      "listItem": "$hue.interactive.200",
      "listEnumeration": "$hue.cyan.200",
      "image": "$hue.interactive.200",
      "imageText": "$hue.cyan.200",
      "codeBlock": "$hue.neutral.200"
    },
    "@dialog": {
      "background": {
        "base": "$background.raised.base",
        "action": {
          "primary": {
            "$hovered": "$background.raised.high"
          }
        }
      }
    }
  },
  "light": {
    "hue": {
      "gray": {
        "100": "#000000",
        "200": "#1a1a1a",
        "300": "#4e4e4e",
        "400": "#8a8a8a",
        "500": "#bebebe",
        "600": "#f5f5f5",
        "700": "#fafafa",
        "800": "#ffffff",
        "900": "#ffffff"
      },
      "red": {
        "100": "#c20f26",
        "200": "#d1383d",
        "300": "#e05352",
        "400": "#ee6b68",
        "500": "#fc837d",
        "600": "#fda19b",
        "700": "#fcbeb9",
        "800": "#fed8d5",
        "900": "#fff2f0"
      },
      "orange": {
        "100": "#c7811e",
        "200": "#d68c27",
        "300": "#df9a44",
        "400": "#e8a85c",
        "500": "#f1b671",
        "600": "#fac386",
        "700": "#fcd4a8",
        "800": "#fee3c7",
        "900": "#fef3e7"
      },
      "green": {
        "100": "#1f8c44",
        "200": "#3d9a57",
        "300": "#55a96a",
        "400": "#6cb77c",
        "500": "#81c68f",
        "600": "#96d5a3",
        "700": "#ace4b6",
        "800": "#c1f3ca",
        "900": "#e1fee6"
      },
      "cyan": {
        "100": "#077786",
        "200": "#318795",
        "300": "#4c97a4",
        "400": "#64a8b4",
        "500": "#7bb8c4",
        "600": "#92c9d4",
        "700": "#a8dbe4",
        "800": "#bfecf4",
        "900": "#e3fafe"
      },
      "blue": {
        "100": "#226bcc",
        "200": "#3b7dd8",
        "300": "#518ee4",
        "400": "#67a0f0",
        "500": "#7db1fc",
        "600": "#9cc3fa",
        "700": "#b7d4fd",
        "800": "#d4e5fd",
        "900": "#f0f6fe"
      },
      "purple": {
        "100": "#6b47a7",
        "200": "#7b5bb6",
        "300": "#8c6fc5",
        "400": "#9d83d4",
        "500": "#af98e3",
        "600": "#c1acf2",
        "700": "#d3c3fc",
        "800": "#e4dbfd",
        "900": "#f6f3ff"
      },
      "accent": "$hue.orange",
      "interactive": "$hue.blue",
      "neutral": "$hue.gray"
    },
    "categorical": [
      "purple",
      "orange",
      "green",
      "blue",
      "red"
    ],
    "text": {
      "action": {
        "primary": {
          "$focused": "$hue.neutral.900"
        },
        "destructive": {
          "base": "$hue.neutral.900"
        }
      },
      "feedback": {
        "warning": {
          "base": "$hue.accent.200"
        }
      }
    },
    "border": {
      "base": "#b8b8b8"
    },
    "scrollbar": {
      "base": "#a0a0a0"
    },
    "diff": {
      "text": {
        "added": "#1e725c",
        "context": "#7086b5",
        "hunkHeader": "#7086b5"
      },
      "background": {
        "added": "#d5e5d5",
        "removed": "#f7d8db"
      },
      "highlight": {
        "added": "#4db380",
        "removed": "#f52a65"
      },
      "lineNumber": {
        "text": "#595959",
        "background": {
          "added": "#c5d5c5",
          "removed": "#e7c8cb"
        }
      }
    },
    "syntax": {
      "number": "$hue.accent.200",
      "type": "#b0851f"
    },
    "markdown": {
      "blockQuote": "#b0851f",
      "emphasis": "#b0851f",
      "strong": "$hue.accent.200"
    }
  },
  "dark": {
    "hue": {
      "gray": {
        "100": "#ffffff",
        "200": "#eeeeee",
        "300": "#b5b5b5",
        "400": "#808080",
        "500": "#4c4c4c",
        "600": "#1e1e1e",
        "700": "#141414",
        "800": "#0a0a0a",
        "900": "#030303"
      },
      "red": {
        "100": "#fd7e87",
        "200": "#e06c75",
        "300": "#c35a63",
        "400": "#a74952",
        "500": "#8c3941",
        "600": "#722931",
        "700": "#591921",
        "800": "#410a13",
        "900": "#280207"
      },
      "orange": {
        "100": "#fddac5",
        "200": "#fab283",
        "300": "#d8976c",
        "400": "#b67c56",
        "500": "#966341",
        "600": "#774a2c",
        "700": "#593319",
        "800": "#3d1d06",
        "900": "#1f0b01"
      },
      "green": {
        "100": "#96f7a7",
        "200": "#7fd88f",
        "300": "#68b977",
        "400": "#539c61",
        "500": "#3d7f4b",
        "600": "#296336",
        "700": "#144922",
        "800": "#00300f",
        "900": "#011705"
      },
      "cyan": {
        "100": "#68d0dd",
        "200": "#56b6c2",
        "300": "#449da7",
        "400": "#33848e",
        "500": "#226c75",
        "600": "#0f555d",
        "700": "#093e44",
        "800": "#02292e",
        "900": "#001518"
      },
      "blue": {
        "100": "#82b4fb",
        "200": "#5c9cf5",
        "300": "#4c86d6",
        "400": "#3c70b8",
        "500": "#2c5b9b",
        "600": "#1d477f",
        "700": "#0f3364",
        "800": "#02214a",
        "900": "#01112b"
      },
      "purple": {
        "100": "#b38ff4",
        "200": "#9d7cd8",
        "300": "#8869bd",
        "400": "#7357a2",
        "500": "#5f4688",
        "600": "#4b356f",
        "700": "#392557",
        "800": "#271640",
        "900": "#17072b"
      },
      "accent": "$hue.purple",
      "interactive": "$hue.orange",
      "neutral": "$hue.gray"
    }
  }
}
```

### Open questions (themes)

1. OpenTUI internals not in this checkout: exact OSC sequences/timeouts used by `renderer.getPalette({size:16})`, `waitForThemeMode(ms)`, `renderer.themeMode` and `CliRenderEvents.THEME_MODE` (presumably OSC 4;n / 10 / 11 queries and DEC mode 2031 `CSI ?997;{1=dark,2=light} n` notifications), and `paletteDetectionStatus`. Rust port must re-implement: query OSC 10/11 + OSC 4 0..15 after first paint; subscribe to mode 2031.
2. `SyntaxStyle.fromTheme` scope matching semantics (exact vs. dotted-prefix fallback, e.g. does `keyword.return.foo` fall back to `keyword.return` then `keyword`?) and conflict resolution between rules are OpenTUI behavior; not verifiable here.
3. `RGBA.fromHex` leniency (e.g. V1 `vesper.json` uses `#fff`; whether invalid V1 hex like `#zzzzzz` throws or yields black) is OpenTUI behavior.
4. In `interpolate` (`v1-migrate.ts:438-439`) `Number.isFinite(h)` is always true for achromatic OKLCH colors (atan2 of near-zero values), so gray-to-gray interpolation uses noisy hues; output depends on floating-point details — exact byte-identical neutral steps 100/300/500/900 in Rust need the same float math (f64, same constants).
5. `extmark.paste` relies on `text.action.primary.focused` being the V1 selected foreground; for native V2 themes it is whatever the theme sets (dark opencode: `#0a0a0a`).
6. Single-mode detection for built-ins was computed by script from background equality; `detectMode` agreement was assumed (text brighter than bg) for aura/ayu/catppuccin-frappe/catppuccin-macchiato/nightowl.
7. `<channel>` segment of the storage path comes from `useTuiApp().channel` (`tui/src/context/storage.tsx:146`); its value (e.g. `latest`/`dev`) not traced here.
8. The mini/run-mode renderer (`tui/src/mini/theme.ts:260-321`) has its own theme resolution (256-color palette query, transparent background, mono variants); out of scope here but shares `parseTheme`/`generateSystem`/discovery.

---

## 7. TUI-local state, config and server communication

Paths below are relative to `packages/` unless prefixed `src/` (then relative to `packages/tui/`).

### 7.1 Filesystem roots

XDG roots come from `util/src/global-roots.ts:5-8`, app dir name `opencode` (`util/src/global.ts:12`):

| Root | Default | Override |
|---|---|---|
| config | `~/.config/opencode` | `$XDG_CONFIG_HOME/opencode` |
| state | `~/.local/state/opencode` | `$XDG_STATE_HOME/opencode` |
| data | `~/.local/share/opencode` | `$XDG_DATA_HOME/opencode` |
| cache | `~/.cache/opencode` | `$XDG_CACHE_HOME/opencode` |

`TuiPaths = { cwd, home, state, worktree }` (`src/context/runtime.tsx:9-14`); `state` is the global state root (`src/app.tsx:317`). `TuiApp = { name, version, channel }` (`src/context/runtime.tsx:3-7`); `channel` = `$OPENCODE_TUI_CHANNEL ?? OPENCODE_CHANNEL` build constant (`cli/src/commands/handlers/default.ts:100`).

### 7.2 Config file (`cli.json`)

- The TUI config lives in **`<config>/cli.json`** (`cli/src/config/config.ts:31`), JSONC-parsed, `$schema: "https://opencode.ai/v2/cli.json"` (`cli/src/config/schema.ts:4`). The schema is exactly `Config.Info` from the TUI (`cli/src/config/schema.ts:6-9`). Env `OPENCODE_CLI_CONFIG_CONTENT` is merged on top (`cli/src/config/config.ts:32-34`). Writes are atomic (`.tmp` + rename, mode `0o600`) under a flock in `<state>/locks` (`cli/src/config/config.ts:42-61`).
- **Migration** (`cli/src/config/migrate.ts`): if `cli.json` exists, renames legacy `terminal.copy_on_select: bool` → `terminal.copy: "select"|"manual"` and rewrites v1 keybind names via `TuiKeybind.CommandMap` (`migrate.ts:43-95`). If absent, builds it from legacy `<config>/tui.json` (v1 TUI config) + `<state>/kv.json` (v1 kv store) (`migrate.ts:100-118`).
- **Hot reload**: `ConfigProvider` watches the config file's directory with `fs.watch` and re-`get()`s + reconciles on any change (`src/config/index.tsx:356-365`). `update(draft => …)` persists through the host then re-applies (`src/config/index.tsx:350-355`). Commands such as "Disable animations" write config this way (`src/app.tsx:1134-1200`).

#### Schema (`src/config/index.tsx:69-249`) with resolved defaults (`src/config/index.tsx:281-333`)

| Key | Type / values | Default (resolved) | Meaning |
|---|---|---|---|
| `theme.name` | string | `opencode` (see §6) | Theme name; the theme picker writes it on every cursor move (§6) |
| `theme.mode` | `system`\|`dark`\|`light` | `system` behaviour | "Color mode; 'system' follows the terminal" |
| `keybinds` | `{actionId: "keys"}` | defaults §5 | Overrides; when terminal suspend unsupported (Windows), `terminal.suspend` forced `"none"` and `ctrl+z` prepended to `input.undo` (`:288-296`) |
| `plugins` | `(string \| {package, options})[]` | — | Plugin enablement/declarations |
| `leader.timeout` | int > 0 ms | **2000** (`:310`) | Leader wait |
| `scroll.speed` | number ≥ 0.001 | (unset → renderer default) | Per-tick scroll distance |
| `scroll.acceleration` | bool | unset | Accelerated scrolling |
| `attention.notifications` | bool | **false** (`:301`) | System notifications |
| `attention.sound` | bool | **false** (`:302`) | Attention sounds |
| `attention.volume` | 0..1 | **0.4** (`:303`) | |
| `attention.sound_pack` | string | **`"opencode.default"`** (`:304`) | |
| `attention.sounds` | `{default\|question\|permission\|error\|done\|subagent_done: path}` | `{}` | Per-event file overrides (`:18-25`) |
| `diffs.source` | VCS mode \| `"turn"` | `"branch"` | Diff viewer source |
| `diffs.wrap` | `word`\|`none` | `word` (`src/app.tsx:1178`) | |
| `diffs.tree` | bool | — | Show file tree |
| `diffs.single` | bool | — | Only selected file patch |
| `diffs.view` | `auto`\|`split`\|`unified` | `auto` | |
| `terminal.title` | bool | true (see `src/app.tsx:1134`) | Update terminal title |
| `terminal.copy` | `manual`\|`select` | **`select`** on non-Windows, `manual` on Windows (`src/app.tsx:548`, `src/ui/dialog.tsx:222`) | Copy-on-select |
| `prompt.editor` | bool | true (`src/app.tsx:1164`) | Include active editor file/selection |
| `prompt.paste` | `compact`\|`full` | compact | Large-paste placeholder |
| `prompt.image_preview` | bool | — | Image previews above prompt |
| `session.sidebar` | `auto`\|`hide` | auto | |
| `session.scrollbar` | bool | — | Transcript scrollbar |
| `session.thinking` | `show`\|`hide` | — | Reasoning default |
| `session.grouping` | `auto`\|`none` | — | Transcript grouping |
| `session.verbosity` | `low`\|`medium`\|`high` | — | Detail level |
| `session.image_preview` | bool | — | |
| `session.tps` | bool | **true** (`:322`) | Tokens/sec display |
| `session.markdown` | `source`\|`rendered` | — | Conceal markdown markers |
| `session.new_location` | `launch`\|`inherit` | **`launch`** (`:320`) | |
| `session.permissions` | `prompt`\|`autoaccept` | **`prompt`** (`:321`) | |
| `tabs.mode` | `auto`\|`on`\|`off` | `auto` (legacy `tabs.enabled` maps to on/off) (`:285-286`) | `enabled = on \|\| (auto && $HERDR_ENV !== "1")` (`:327`) |
| `tabs.scope` | `global`\|`cwd` | **`cwd`** (`:328`) | |
| `tabs.layout` | `horizontal`\|`vertical` | **`horizontal`** (`:329`) | |
| `tabs.indicators` | `status`\|`numbers` | **`status`** (`:330`) | |
| `mini.*` | thinking/tools/shell_output/turn_summary/footer/splash (`show`\|`hide`), `work_spinner` (14 names `:29-44`, default `block-soft-slide`), `mono`, `replay`, `replay_limit` | | Mini mode only (§7.9) |
| `debug.devtools` / `debug.timing` / `debug.turn_tokens` | bool / bool / bool\|`"verbose"` | — | Debug bar, TTFD, per-turn tokens |
| `experimental` | `{string: bool}` | — | Experiments registry (`src/component/dialog-experiments.tsx`) |
| `animations` | bool | true | |
| `mouse` | bool | **true** (`:311`) | Mouse capture |
| `cursor.style` / `cursor.blinking` | `block`\|`underline`\|`line`\|`default` / bool | `block` / `true` only when `cursor` present (`:312-317`) | |

Unset optional keys stay `undefined`; consumers apply their own fallback (e.g. `config.data.animations ?? true`, `src/app.tsx:1150`).

### 7.3 Persistent TUI state files

**Keyed JSON store** (`src/context/storage.tsx`): `<state>/<channel>/tui/<key>.json` (`:196, :202`), key segments must match `^[a-zA-Z0-9][a-zA-Z0-9._-]*$` (`:187`). Each update: take a flock (`<state>/<channel>/locks`), re-read file, apply mutation, atomic write `file.<pid>.<uuid>.tmp` → rename (`src/util/persistence.ts:326-337`), then reconcile store (`:216-234`). Directory is `fs.watch`ed; reload all entries 50 ms after the last event (cross-process live sync) (`:258-266`). Unreadable/missing file → `initial`. `memory(key)` gives an in-process store only.

| Key (file) | Shape | Writer |
|---|---|---|
| `tabs` | `{global: TabsState, cwd: {[cwd]: TabsState}}`, reconcile key `sessionID` | `src/context/session-tabs.tsx:72-79` |
| `layout` | `{verticalTabsWidth?}` and `{paneWidth?, terminalWidth?}` (same file, two users) | `src/app.tsx:501`, `src/component/session-frame.tsx:42` |
| `session-list` | dialog prefs | `src/component/dialog-session-list.tsx:42` |
| `update-notifications` | `{versions: string[]}` (already-notified versions) | `src/context/update-notification.tsx:37` |
| `session-terminal-selection` | selected PTY per session | `src/context/session-terminals.tsx:22` |
| `system-theme` | `{colors?: TerminalColors}` (cached palette) | `src/context/theme.tsx:129` |
| `directory-recents` | recent directories | `src/prompt/directory-recents.ts:13` |
| `plugin.<pluginId>.<key>` | plugin-owned (e.g. `plugin.<sidebar-mcp>.view` `{open:true}`, `…getting-started`) | `src/plugin/api.tsx:201-202`, `src/feature-plugins/sidebar/mcp.tsx:6`, `src/feature-plugins/sidebar/footer.tsx:11` |
| memory `subagents-filter` | `{active: true}` | `src/routes/session/composer/subagents-tab.tsx:37` |

**Direct files in `<state>` (no channel subdir):**

| File | Content | Source |
|---|---|---|
| `model.json` | `{recent: [{providerID, modelID}] (max 10, MRU, dedup by "provider/model"), favorite: [...] (newest first), variant: {...}}` + unknown keys preserved; flock + fs.watch | `src/context/local.tsx:154`, `src/model-preference.ts:49-65,103-160` |
| `session.json` | `{pinned: string[]}`; quick slots = first 9 pinned root sessions that still exist | `src/context/local.tsx:514-547` |
| `prompt-history.jsonl` | prompt history (§4) | `src/prompt/history.tsx:93` |
| `prompt-stash.jsonl` | stashed prompts | `src/prompt/stash.tsx:40` |
| `frecency.jsonl` | @-mention frecency | `src/prompt/frecency.tsx:42` |

### 7.4 Local selection state (`src/context/local.tsx`)

- **Agents**: list = server agents for the location with `mode !== "subagent" && !hidden` (`:57-59`). Current = per-session draft → CLI `--agent` → session's server agent → global current → first (`:84-93`). `set(id)` on unknown id toasts `Agent not found: ${id}` warning 3000 ms (`:96-101`). `move(±1)` wraps (`:110-120`). Agent color: `agent.color` hex if set, else `theme.categorical[i][200]` deduped, indexed by position among visible agents (`:75-81,121-128`).
- **Models**: fallback order = CLI `--model` → last config document's `model` → first valid recent (`:173-200`). `cycle(±1)` walks `recent` (`:412-423`); `cycleFavorite(±1)` walks favorites, toast "Add a favorite model to use this shortcut" when none (`:424-450`); `set(model, {recent})` pushes MRU (`:452-460`); favorite toggle (`:465-468`). Variant cycle: named variants (excluding `"default"`) in order, after last → back to default/undefined (`src/model-preference.ts:67-75`). Selections are tracked per (session, agent) and per (location, agent) for new sessions; committed to server via `session.switchModel` / `switchAgent` on submit (`src/component/prompt/index.tsx:1273-1292`).
- **Session pins**: `togglePin`, `quickSwitch(slot 1..9)` (`src/context/local.tsx:584-605`).

### 7.5 Session tabs model (`src/context/session-tabs.tsx`, `src/context/session-tabs-model.ts`)

- State scoped per `tabs.scope` (`cwd` keyed by launch cwd) (`session-tabs.tsx:103-114`). Tabs = `{sessionID (root session), title?}`; opening dedups by root; close selects right neighbour else left (`session-tabs-model.ts:39-55`). Closed tabs kept in memory for "Reopen closed tab"; tab nav history in memory (`session-tabs.tsx:83-87`).
- Unread = `time.idle > time.viewed` (`session-tabs.tsx:122-125`); unread kinds `"activity" | "error"` (`session-tabs-model.ts:6`). Viewing a session calls `session.view` (`session-tabs.tsx:221`).
- Constants: `NEW_SESSION_TAB_TITLE = "New session"`, tab width 22, max 32, min 8; overflow markers `‹12 ` / ` 12›` (`session-tabs-model.ts:8,33-37`). Detail line `"${project} ⎇ ${branch}"` when in a non-default-branch worktree (`:14-22`).
- **Retention**: only the current, kept (open tabs) and the **3** most-recent other root sessions stay loaded; others are `evict`ed from the data store (`src/context/session-retention.ts`, `limit: 3` at `src/context/session-tabs.tsx:144`).

### 7.6 Clipboard (`src/clipboard.ts`)

Delegates to OpenTUI `createClipboard({host: createHostClipboard(), terminal: createRendererClipboardAdapter(renderer)})` (`:13-19`).
- **Read**: prefer `image/png` then `text/plain`, selection `clipboard` (`:25-28`); PNG → base64 `{data, mime}`; text decoded via `decodePasteBytes`; empty text → undefined; errors "Clipboard read timed out", "Clipboard content exceeded configured read or image conversion limits" (`:29-51`).
- **Write**: strip NUL, destination `all-available` (host tool AND terminal OSC 52), success if either host written or terminal attempted (`:53-62`). Toast on copy: `"Copied to clipboard"` info (`src/app.tsx:568`).
- Rust port: host = `pbcopy/pbpaste` (macOS), `wl-copy/wl-paste` or `xclip/xsel` (Linux), PowerShell/`clip` (Windows); terminal = `ESC ] 52 ; c ; <base64> BEL` (OSC 52). Host mechanisms are inside `@opentui/core`, not in this repo (see open questions).

### 7.7 Attention: notifications and sounds

- Host: `src/attention.ts`. Disabled unless `attention.notifications || attention.sound` (`:131`). Tracks terminal focus via renderer `focus`/`blur` events (focus reporting, `CSI ?1004h`) — state `unknown|focused|blurred` (`:22,90-102`).
- Text normalised: strip ANSI, collapse newlines to spaces, drop control chars, title ≤ 80 chars (default `"OpenCode"`), message ≤ 240 chars (`:35-37,56-63`).
- Notification: `renderer.triggerNotification(message, title)` (OpenTUI emits a terminal desktop-notification escape — OSC 9 / OSC 777, see open questions), default `when: "blurred"`; skipped when focus unknown (`:78-83,137-153`).
- Sound: plays first loadable of [config override, builtin] via OpenTUI `Audio` (`src/audio.ts`), volume clamp 0..1; default `when: "always"` (`:104-126,154-161`). Built-in files (`src/attention-sounds.bun.ts:2-6`, from `packages/ui/audio/`): default/done `bip-bop-01.mp3`, question `bip-bop-03.mp3`, permission `staplebops-06.mp3`, error `nope-03.mp3`, subagent_done `yup-01.mp3`.
- Triggers (`src/feature-plugins/system/notifications.ts`): `form.created` → "Input needs response" (sound `question`, title = form title) (`:45-49`); `permission.asked` → "Permission needs input" (`permission`) (`:52-56`); execution succeeded/interrupted → "Session done" (`done`, or `subagent_done` for child sessions) (`:33-42`); execution failed → error message (`error`) + toast title "Session failed" variant error (`:61-71`). Subagent sessions never raise desktop notifications (`:12-16`). Each id/session notifies once until re-armed (`:24-32`).

### 7.8 Server communication

**Endpoint & auth.** CLI passes `server.endpoint = {url, auth?: {type:"basic", username, password}}` (`client/src/service.ts:2-14`); TUI builds `OpenCode.make({baseUrl, headers: Service.headers(endpoint)})` (HTTP Basic) (`src/app.tsx:211-212`). Managed local service: discovered via registration file in XDG state, spawned as `opencode serve --service` (`client/src/service.ts:16-35`); `reconnect(signal)` re-resolves the endpoint, `restart()` restarts it (command "Restart service", toasts "Restarting service…" 30000 ms → "Service restarted") (`src/app.tsx:221-230,1003-1013`).

**Bootstrap.** `api.file.list({location:{directory: cwd}})` resolves the location (fallback `api.location.get()`) before rendering (`src/app.tsx:213-216`). Then `createData` (`client/src/solid/data.ts`) lazily loads per-location resources: `agent.list`, `command.list`, `config.get`, `integration.list`, `mcp.list`, `mcp.resource.catalog`, `model.list`, `provider.list`, `reference.list`, `skill.list`, `websearch.providers`, `vcs.get`, `shell.list` (`data.ts:1331-1333,1925-1959`), sessions via `session.get`/`session.list`, messages via `message.list` (`:1618-1719`), pending `permission.list`, `form.list`, `session.inbox.list`. All caches invalidate whenever the connection is not `connected` (`data.ts:1963-1965`).

**Event stream** (`client/src/solid/connection.ts`): SSE `GET /api/event` (`client/src/promise/generated/client.ts:1705-1708`). First event must be `server.connected` else error (`connection.ts:106-107`). Connect timeout 2000 ms, reconnect delay 1000 ms (attempt counter resets if connection lived ≥ 1 s), idle watchdog 45 000 ms (server keepalive every 15 s) (`:32-35,147-183`). Events are batched and flushed every 10 ms (`:65-74`). Status `connecting|connected|reconnecting` drives `src/component/reconnecting.tsx` (§1). On each reconnect the managed `reconnect` hook may swap the API client (`src/context/client.tsx:25-34`).

**Events handled by the data store** (`client/src/solid/data.ts:601-1299`): `server.connected`, `project.updated`, `session.created|deleted|usage.updated|agent.selected|model.selected|renamed|permissions|moved|viewed|synthetic|instructions.updated`, `worktree.resolved`, `session.inbox.enqueued|delivered|delivery.changed|cancelled`, `session.shell.started|ended`, `session.step.started|streamed|ended|failed`, `session.text.started|delta|ended`, `session.tool.input.started|delta|ended`, `session.tool.called|progress|success|failed`, `session.reasoning.started|delta|ended`, `session.retry.scheduled`, `session.execution.started|succeeded|failed|interrupted`, `session.compaction.started|delta|ended|failed`, `session.revert.staged|cleared|committed`, `permission.asked|replied`, `form.created|replied|cancelled`, `provider|model|agent|command|skill|reference|integration|config|websearch.updated`, `vcs.branch.updated`, `shell.created|exited|deleted`, `mcp.status.changed`, `mcp.resources.changed`. The TUI additionally listens directly (via `useEvent`, `src/context/event.ts`) for e.g. `session.deleted`, `session.agent.selected` (`src/context/local.tsx:67-74`).

**Direct API calls from the TUI** (excluding mini):

| Call | Where | Purpose |
|---|---|---|
| `session.prompt` (via `data.session.prompt`, optimistic row, rollback on reject) | `src/component/prompt/index.tsx:1350+`, `client/src/solid/data.ts:1590-1612` | Send user prompt (`delivery` default `"steer"`) |
| `session.create` (via `data.session.create`) | `src/component/prompt/index.tsx:1231` | New session on first submit; toast "Creating a session failed"/"Failed to set up session" |
| `session.environment` | `prompt/index.tsx:1247` | Push terminal env vars to new session |
| `session.switchAgent` / `session.switchModel` | `prompt/index.tsx:1280,1286` | Commit selection before send; error "Failed to switch model: …" |
| `session.shell` | `prompt/index.tsx:1307` | `!` shell mode submit |
| `session.command` | `prompt/index.tsx:1315` | Slash command submit (`name`, `text`, files, agents, skills, delivery) |
| `session.revert.commit` / `.stage` / `.clear` | `prompt/index.tsx:1336`, `routes/session/index.tsx:1010,1028,2150` | Undo/redo |
| `session.synthetic` | `prompt/index.tsx:1342` | Hidden editor-context message |
| `session.interrupt` | `prompt/index.tsx:522`, `routes/session/index.tsx:1008`, `composer/subagents-tab.tsx:190` | Stop generation |
| `session.wait` | `routes/session/index.tsx:1009` | Wait for idle (before revert) |
| `session.background` | `prompt/index.tsx:542`, `routes/session/index.tsx:1270` | Background a session |
| `session.update({title:""})` | `src/context/data.tsx:100` | Regenerate title |
| `session.update` | `routes/session/index.tsx:927` | Rename etc. |
| `session.export` | `routes/session/index.tsx:1210,1236` | Export transcript |
| `session.inbox.update` / `.cancel` | `routes/session/index.tsx:631-634` | Queued message edit/cancel |
| `session.move` | `prompt/index.tsx:300`, `dialog-workspaces.tsx:317`, `location-missing.tsx:51` | Move session to another directory/worktree |
| `session.list` | `dialog-session-list.tsx:62` | Session list dialog |
| `session.view` | `context/session-tabs.tsx:221` | Mark viewed (unread) |
| `session.log` | `devtools-bar.tsx:165` | Debug |
| `session.compact` (via data) | `client/src/solid/data.ts:1549` | Compact |
| `session.form.reply` / `.cancel` (via data) | `data.ts:1822-1825` | Question/form answers |
| `file.find` | `component/prompt/autocomplete.tsx:361` | @-mention file search |
| `location.get` | `prompt/index.tsx:287`, `app.tsx:215` | Resolve directory |
| `worktree.list` / `.refresh` / `.create` | `dialog-workspaces.tsx:83,107`, `prompt/move.tsx:41` | Workspaces |
| `shell.get` / `.output` / `.remove` | `dialog-shell-output.tsx:41-51`, `composer/shell-tab.tsx:104` | Background shells |
| `mcp.connect` / `.disconnect` | `dialog-mcp.tsx` | MCP toggle |
| `server.info` / `server.pair` | `devtools-bar.tsx:57`, `dialog-pair.tsx:22`, `dialog-integration.tsx:219` | Pairing/QR |
| `credential.activate`, `integration.command.cancel`, `integration.oauth.cancel` | `dialog-integration.tsx:414-759`, `dialog-open.tsx:122` | Provider auth |
| `experimental.persistentPty.list/create/snapshot` | `context/session-terminals.tsx:31,93`, `terminal-pane.tsx:176` | Embedded terminal pane |

Permission replies go through `src/context/permission.tsx` (see §2). Errors from calls are normalised by `src/util/error.ts` and usually shown as error toasts.

### 7.9 Mini mode (`src/mini/`, `opencode mini`)

- Separate frontend launched by `opencode mini` ("Start the minimal interactive interface", `cli/src/commands/commands.ts:340-341`) via `runMiniFrontend` → `runInteractiveDeferredMode` (`src/mini/index.ts:1-7`, `cli/src/mini.ts:38`). It does not use the alt screen: transcript is written as immutable terminal scrollback (OpenTUI `ScrollbackWriter`) and only a live footer (prompt, permission/form, command menu, subagent tabs, status) is redrawn at the bottom (`src/mini/footer*.tsx`, `scrollback.*.ts`). Reads stdin prompt when piped (`cli/src/mini.ts:37`).
- Entry header (single row): `▪ oc mini` (mono: `[O] oc mini`), then ` v<version>` and ` · <path>` (mono separator ` - `, ellipsis `...` vs `…`) (`src/mini/splash.ts:280-298`). Exit banner: the `go` logo (`src/logo.ts:6-9`, mono `[O]`) beside `Session  <title>` (title fallback `"Untitled session"`, limit 50) and `Continue opencode mini -s <session_id>` (`src/mini/splash.ts:22-23,179-184`).
- Logo cell encoding shared with full TUI: `_` = space painted with full shadow bg, `^` = `▀` "mix", `~` = `▀` top-only (`src/mini/splash.ts:45-66`).
- Controlled by `mini.*` config (§7.2); session replay on resume/resize (`replay`, `replay_limit`). A faithful port needs its own spec (≈20k lines); see open questions.

### Open questions (state)

1. `@opentui/core` internals (host clipboard commands, OSC 52 chunking, `triggerNotification` escape sequence — OSC 9 vs OSC 777 vs OSC 99, focus reporting, Kitty keyboard flags, `Audio` decoder for mp3) are outside this repo; need the OpenTUI source to replicate byte-for-byte.
2. The data/sync layer (`packages/client/src/solid/data.ts`, ~2k lines) owns event→store projection, optimistic admission and per-session sync; it should get its own spec (client) — this section only enumerates it.
3. `scroll.speed` / `scroll.acceleration` defaults when unset are renderer defaults (OpenTUI `ScrollBox`); confirm values.
4. `layout` storage key is shared by `src/app.tsx:501` and `src/component/session-frame.tsx:42` with different shapes — confirm both merge into one object on disk.
5. Mini mode (`src/mini/`, 40 files) is only summarised; decide whether the Rust port needs `opencode mini` parity.

---


## 8. Consolidated open questions (cross-cutting)

Each section above ends with its own detailed list. The items below are the ones that block an exact replica, roughly by impact.

1. **OpenTUI internals are not in this checkout.** Markdown rendering (`MarkdownRenderable`), `<diff>` rendering, textarea/extmark chip editing semantics, tree-sitter highlight scope fallback, alpha blending of overlays (black @150/@70), alt-screen/mouse defaults, Kitty keyboard flags, clipboard host commands + OSC 52, `triggerNotification` escape (OSC 9/777/99), terminal palette queries/timeouts, `Audio` mp3 playback, ScrollBox default scroll speed/acceleration. Fetch `@opentui/core` / `@opentui/solid` at the pinned catalog version (keymap 0.5.17 was fetched to `scratchpad/dl-opentui-keymap`, cited `KM:` in §5) and spec it separately.
2. **Data/sync layer** (`packages/client/src/solid/data.ts`, connection, optimistic admission, event projection) drives everything the transcript shows; it needs its own spec (client), §7.8 only enumerates it.
3. **Source quirks: replicate or fix?** History back-navigation only from an empty prompt or one equal to the oldest entry (§4); frecency never affects ranking (§4); `/` mid-sentence opens an empty "No matching commands" popup (§4); `ctrl+v` may double-paste (§4); `ctrl+c` with a selection under copy-on-select is not consumed (§5); update dialog `left`/`shift+tab` move forward (§2); permission prompt checks raw `shell`/`edit` names while presentation maps `bash`→`shell` (§2); `--continue` with no prior session leaves route on placeholder `"dummy"` (§1).
4. **Non-integer geometry:** dialog top offset `height/4` is unfloored (§2); fullscreen permission uses a negative top in a Portal (§2); Yoga flex rounding generally — the Rust layout must choose a rounding rule and verify against the original with screenshots.
5. **Command palette order** depends on layer registration order and reshuffles at runtime (§5); pick and document a deterministic order.
6. **Theme math fidelity:** neutral-scale interpolation and `increase`/`decrease` depend on object identity (hue, step) and f64 math (§6); port must carry color provenance.
7. **Inert keybind ids** (23, §5) and unused animation code (knight-rider scanner, one-cell spinners outside mini, §1): decide whether to port.
8. **Mini mode** (`opencode mini`, ~20k lines in `src/mini/`) is only summarised (§7.9); decide whether parity is required and, if so, write a dedicated spec.
9. **Plugins:** external TUI plugins (`plugins` config, `src/plugin/*`) are JS modules; the Rust port cannot load them. Decide whether to port only the built-in feature plugins as native code (recommended) and which slot API, if any, to expose.
