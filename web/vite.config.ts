import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'

// docs/09-frontend.md#dev-workflow default is teleportd's own default port,
// 7337. Override when this machine already has a different teleportd
// instance (e.g. a provisioned one) live on 7337 -- scripts/mobile-dev sets
// this for you rather than hardcoding a second port here.
const apiPort = process.env.TELEPORTD_DEV_PORT ?? '7337'

// Set only when reaching this dev server through Tailscale Serve from a
// phone (scripts/mobile-dev/README.md#why-this-exists). Vite rejects
// unrecognized Host headers by default; this is the matching allowlist
// entry. Undefined (the default) leaves Vite's own default in place.
const tailnetHost = process.env.TELEPORT_TAILNET_HOST

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
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
    allowedHosts: tailnetHost ? [tailnetHost] : undefined,
  },
})
