import { mount } from 'svelte'
import App from './App.svelte'
import { captureTokenFromUrl } from './lib/identity'
import './app.css'

// Must run before anything renders: strips ?token=… from the address bar
// (docs/06-security.md#token-on-the-websocket-upgrade).
captureTokenFromUrl()

const app = mount(App, {
  target: document.getElementById('app')!,
})

export default app
