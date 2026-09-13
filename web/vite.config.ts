import { fileURLToPath } from "node:url"
import { svelte } from "@sveltejs/vite-plugin-svelte"
import { defineConfig } from "vite"

// Destructured once: process.env is an index signature, and the baseline's
// noPropertyAccessFromIndexSignature would otherwise want bracket access,
// which Biome's useLiteralKeys then wants back as dot access.
const { TELEPORTD_DEV_PORT, TELEPORT_TAILNET_HOST } = process.env

// docs/09-frontend.md#dev-workflow default is teleportd's own default port,
// 7337. Override when this machine already has a different teleportd
// instance (e.g. a provisioned one) live on 7337 -- scripts/mobile-dev sets
// this for you rather than hardcoding a second port here.
const apiPort = TELEPORTD_DEV_PORT ?? "7337"

// Set only when reaching this dev server through Tailscale Serve from a
// phone (scripts/mobile-dev/README.md#why-this-exists). Vite rejects
// unrecognized Host headers by default; this is the matching allowlist
// entry. Undefined (the default) leaves Vite's own default in place.
const tailnetHost = TELEPORT_TAILNET_HOST

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  resolve: {
    // "@/..." resolves to src/... -- see tsconfig.app.json paths. Matches
    // codefort's web app.
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  server: {
    // docs/09-frontend.md#dev-workflow: forward both /api HTTP and the
    // WebSocket upgrade to teleportd. Same-origin from the browser's point
    // of view, so api.ts/stream.ts never need a configurable base URL.
    proxy: {
      "/api": {
        target: `http://127.0.0.1:${apiPort}`,
        ws: true,
      },
    },
    ...(tailnetHost ? { allowedHosts: [tailnetHost] } : {}),
  },
})
