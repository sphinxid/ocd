// Packs the upstream web app (packages/app) into the archive the CLI embeds, using upstream's own
// `buildAppArchive` (packages/cli/script/app-assets.ts): `vite build` with OPENCODE_CHANNEL and
// VITE_OPENCODE_SERVER_MODE=origin, every non-.map file of dist brotli-compressed at quality 11, sorted,
// in the `app-archive.ts` format. Usage: bun build-web-ui.ts <upstream-checkout> <output> <channel>
import path from "node:path"

const [upstream, output, channel] = process.argv.slice(2)
if (!upstream || !output || !channel) throw new Error("usage: build-web-ui.ts <upstream-checkout> <output> <channel>")
const { buildAppArchive } = await import(path.resolve(upstream, "packages/cli/script/app-assets.ts"))
const archive: Uint8Array = await buildAppArchive(channel)
await Bun.write(output, archive)
console.log(`wrote ${output} (${archive.length} bytes)`)
