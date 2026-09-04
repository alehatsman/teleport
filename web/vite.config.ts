import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  server: {
    // docs/09-frontend.md#dev-workflow: forward both /api HTTP and the
    // WebSocket upgrade to teleportd. Same-origin from the browser's point
    // of view, so api.ts/stream.ts never need a configurable base URL.
    proxy: {
      "/api": {
        target: "http://127.0.0.1:7337",
        ws: true,
      },
    },
  },
})
