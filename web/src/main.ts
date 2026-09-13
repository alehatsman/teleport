import { mount } from "svelte"
import { captureTokenFromUrl } from "@/api/identity"
import App from "./App.svelte"
import "./app.css"

// Must run before anything renders: strips ?token=… from the address bar
// (docs/06-security.md#token-on-the-websocket-upgrade).
captureTokenFromUrl()

const target = document.getElementById("app")
if (!target) throw new Error("index.html has no #app element to mount into")

const app = mount(App, { target })

export default app
