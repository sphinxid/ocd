# Catching up with opencode 2.3.2

[← Compatibility](COMPATIBILITY.md)

Status: done (2026-11-22); ocd now reports 2.3.2, and its releases and web UI are built from the `v2.3.2` tag
(commit `c0ad18b`). Before, ocd tracked opencode 2.3.1 ([UPGRADE-2.3.1.md](UPGRADE-2.3.1.md)); upstream released
2.3.2 on Nov 18, about 80 minutes after 2.3.1.

`v2.3.1..v2.3.2` is five commits: three changes, one Code Mode addition and the release itself. Every change is
ported and checked against an upstream 2.3.2 build from the tag and against upstream's own tests.

## What changes upstream

- **GPT-6.2 prompt caching** (#64542, #64544): every Responses route sends the system prompt as a leading developer
  message instead of top-level `instructions`, which GPT-6.2 models never cache, so every step was billed for the
  whole system prompt again. This covers OpenAI (API key and ChatGPT sign-in), GitHub Copilot, Zen, Azure,
  OpenRouter and the other Open Responses routes (xAI, Alibaba, Meta, DigitalOcean, Venice and gateways).
  Upstream's `/responses/compact` request leads with the same message; ocd does not send that request (native
  compaction is not supported, see COMPATIBILITY), so only the step requests apply.
- **`webfetch` output** (#64534): the structured output and its schema no longer repeat the `url` and `format` the
  tool was called with: `{ contentType, output }`.
- **Code Mode** (#64516): an extension can name the modules that load its globals (`modules: { bun: "Bun" }` makes
  `import { file } from "bun"` load `Bun`); a later extension's name wins, and only the extensions a run gets
  count. No released extension uses it yet; ocd's `GlobalsExtension` takes `modules`.
- **Version**: 2.3.2 everywhere; no migration, API or config change.

## Checks

| Check | Result |
|---|---|
| Responses request bodies | ocd's request for upstream's new `gpt-6.2-sol` cache recording is identical to the recorded one; the six re-recorded fixtures ocd keeps a copy of are updated and match |
| Code Mode | Upstream's two new `extensions.test.ts` cases ported |
| `webfetch` | Upstream's updated registration test ported |
| Plugin helper | `host.js` rebuilt from the 2.3.2 pin is byte-identical to the 2.3.1 one |
| Tests | 3,118 passed, none failed |
| Side by side | 282 of 288 identical against the 2.3.2 build, the six known differences left ([PARITY.md](PARITY.md)) |
