#!/usr/bin/env node
// Real-device mobile testing via Apple's own `safaridriver` (ships with
// macOS, implements standard WebDriver). No Xcode needed for a real device
// -- that's only required for `safari:useSimulator: true`. See README.md's
// "Real device (safaridriver)" section before using this; it covers the
// one-time setup (`safaridriver --enable`, pairing, Web Inspector toggle)
// this script can't do for you.
//
// IMPORTANT, read before typing/clicking: WebDriver's own `element/click`
// and key-sending are UNRELIABLE on a real iOS device here -- silent empty
// 400s, or synthetic touches landing as long-press/text-selection instead
// of a tap. Every write command below (`click`, `key`, `type`, `submit`)
// works around this by dispatching real DOM events via `execute/sync`
// script injection instead -- same code paths a real tap/keystroke takes,
// just triggered more reliably. Don't "fix" these to use the native
// WebDriver click/key endpoints; that's the less reliable path, not more
// idiomatic.
//
// Usage:
//   phone.mjs devices                       list paired iOS devices
//   phone.mjs start [port]                  start safaridriver (default 9445)
//   phone.mjs session <udid> [port]         create a session on a real device, persist it
//   phone.mjs nav <url>                     navigate
//   phone.mjs shot [outFile]                screenshot -> outFile (default /tmp/phone.png)
//   phone.mjs eval <js>                     run JS, print the JSON result
//   phone.mjs click <selector>              real click() via script (not WebDriver's touch sim)
//   phone.mjs fill <selector> <value>       set an input/select's value + dispatch input/change
//   phone.mjs submit <formSelector>         form.requestSubmit()
//   phone.mjs type <text>                   type into document.activeElement (e.g. xterm's
//                                            hidden textarea) via the same 'input' event path
//                                            a soft keyboard uses
//   phone.mjs key <name>                    dispatch a named key on document.activeElement:
//                                            Enter, Backspace, Escape, Tab, ArrowUp/Down/Left/Right
//
// State (session id, safaridriver port) persists in /tmp/mobile-dev-phone.json
// so you don't have to pass them to every call.

import { execSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const STATE_FILE = "/tmp/mobile-dev-phone.json";
const KEYS = {
  Enter: { key: "Enter", code: "Enter", keyCode: 13 },
  Backspace: { key: "Backspace", code: "Backspace", keyCode: 8 },
  Escape: { key: "Escape", code: "Escape", keyCode: 27 },
  Tab: { key: "Tab", code: "Tab", keyCode: 9 },
  ArrowUp: { key: "ArrowUp", code: "ArrowUp", keyCode: 38 },
  ArrowDown: { key: "ArrowDown", code: "ArrowDown", keyCode: 40 },
  ArrowLeft: { key: "ArrowLeft", code: "ArrowLeft", keyCode: 37 },
  ArrowRight: { key: "ArrowRight", code: "ArrowRight", keyCode: 39 },
};

function loadState() {
  if (!existsSync(STATE_FILE)) return {};
  return JSON.parse(readFileSync(STATE_FILE, "utf8"));
}
function saveState(s) {
  writeFileSync(STATE_FILE, JSON.stringify(s, null, 2));
}

async function wd(port, method, path, body) {
  const res = await fetch(`http://127.0.0.1:${port}${path}`, {
    method,
    headers: body ? { "Content-Type": "application/json" } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const json = await res.json();
  if (json.value?.error) {
    throw new Error(`${json.value.error}: ${json.value.message}`);
  }
  return json.value;
}

async function evalScript(port, sessionId, script, args = []) {
  return wd(port, "POST", `/session/${sessionId}/execute/sync`, { script, args });
}

const [, , cmd, ...args] = process.argv;
const state = loadState();

if (cmd === "devices") {
  const out = execSync("idevice_id -l", { encoding: "utf8" }).trim();
  if (!out) {
    console.log("No devices found. Plug the iPhone in via cable.");
  } else {
    for (const udid of out.split("\n")) {
      let name = "?";
      try {
        name = execSync(`ideviceinfo -u ${udid} -k DeviceName`, { encoding: "utf8" }).trim();
      } catch { /* device present but locked/untrusted -- still list the udid */ }
      console.log(`${udid}  ${name}`);
    }
  }
} else if (cmd === "start") {
  const port = args[0] || 9445;
  try {
    const res = await fetch(`http://127.0.0.1:${port}/status`);
    if (res.ok) {
      console.log(`safaridriver already up on :${port}`);
      saveState({ ...state, port });
      process.exit(0);
    }
  } catch { /* not up yet, fall through to start it */ }
  const { spawn } = await import("node:child_process");
  const child = spawn("safaridriver", ["-p", String(port)], {
    detached: true, stdio: "ignore",
  });
  child.unref();
  saveState({ ...state, port });
  await new Promise((r) => setTimeout(r, 1000));
  console.log(`started safaridriver on :${port}, pid ${child.pid}`);
  console.log(`(if this fails silently, run 'safaridriver --enable' yourself -- needs your password)`);
} else if (cmd === "session") {
  const [udid, port = state.port || 9445] = args;
  if (!udid) { console.error("usage: phone.mjs session <udid> [port]"); process.exit(1); }
  const result = await wd(port, "POST", "/session", {
    capabilities: {
      alwaysMatch: {
        browserName: "Safari",
        platformName: "iOS",
        "safari:useSimulator": false,
        "safari:deviceUDID": udid,
      },
    },
  });
  saveState({ port, sessionId: result.sessionId, udid });
  console.log(`session ${result.sessionId} on ${result.capabilities["safari:deviceName"]}`);
} else if (cmd === "nav") {
  const [url] = args;
  if (!url) { console.error("usage: phone.mjs nav <url>"); process.exit(1); }
  await wd(state.port, "POST", `/session/${state.sessionId}/url`, { url });
  console.log("navigated");
} else if (cmd === "shot") {
  const outFile = args[0] || "/tmp/phone.png";
  const shot = await wd(state.port, "GET", `/session/${state.sessionId}/screenshot`);
  writeFileSync(outFile, Buffer.from(shot, "base64"));
  console.log(outFile);
} else if (cmd === "eval") {
  const [expr] = args;
  if (!expr) { console.error("usage: phone.mjs eval <js>"); process.exit(1); }
  const result = await evalScript(state.port, state.sessionId, `return (${expr});`);
  console.log(JSON.stringify(result, null, 2));
} else if (cmd === "click") {
  const [selector] = args;
  if (!selector) { console.error("usage: phone.mjs click <selector>"); process.exit(1); }
  const result = await evalScript(
    state.port, state.sessionId,
    "const el = document.querySelector(arguments[0]); if (!el) return 'not found'; el.click(); return 'clicked';",
    [selector],
  );
  console.log(result);
} else if (cmd === "fill") {
  const [selector, value] = args;
  if (!selector || value === undefined) { console.error("usage: phone.mjs fill <selector> <value>"); process.exit(1); }
  const script = `
    const el = document.querySelector(arguments[0]);
    if (!el) return 'not found';
    const proto = el.tagName === 'SELECT' ? window.HTMLSelectElement.prototype : window.HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
    setter.call(el, arguments[1]);
    el.dispatchEvent(new Event('input', {bubbles: true}));
    el.dispatchEvent(new Event('change', {bubbles: true}));
    return el.value;
  `;
  const result = await evalScript(state.port, state.sessionId, script, [selector, value]);
  console.log(result);
} else if (cmd === "submit") {
  const [selector] = args;
  if (!selector) { console.error("usage: phone.mjs submit <formSelector>"); process.exit(1); }
  const result = await evalScript(
    state.port, state.sessionId,
    "const f = document.querySelector(arguments[0]); if (!f) return 'not found'; f.requestSubmit(); return 'submitted';",
    [selector],
  );
  console.log(result);
} else if (cmd === "type") {
  const text = args.join(" ");
  if (!text) { console.error("usage: phone.mjs type <text>"); process.exit(1); }
  // Prefer xterm's own input-capture textarea (a fresh page load has
  // nothing focused yet -- document.activeElement would be <body>, and
  // HTMLTextAreaElement's value setter throws on anything else). Falls
  // back to whatever IS focused for non-terminal inputs.
  const script = `
    const el = document.querySelector('.xterm-helper-textarea') || document.activeElement;
    el.focus();
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
    setter.call(el, arguments[0]);
    el.dispatchEvent(new InputEvent('input', {bubbles: true, data: arguments[0], inputType: 'insertText'}));
    return 'typed';
  `;
  const result = await evalScript(state.port, state.sessionId, script, [text]);
  console.log(result);
} else if (cmd === "key") {
  const [name] = args;
  const k = KEYS[name];
  if (!k) {
    console.error(`unknown key '${name}'. Known: ${Object.keys(KEYS).join(", ")}`);
    process.exit(1);
  }
  const script = `
    const el = document.querySelector('.xterm-helper-textarea') || document.activeElement;
    el.focus();
    const opts = {key: arguments[0], code: arguments[1], keyCode: arguments[2], which: arguments[2], bubbles: true, cancelable: true};
    el.dispatchEvent(new KeyboardEvent('keydown', opts));
    el.dispatchEvent(new KeyboardEvent('keyup', opts));
    return 'sent';
  `;
  const result = await evalScript(state.port, state.sessionId, script, [k.key, k.code, k.keyCode]);
  console.log(result);
} else {
  console.error(`usage: phone.mjs devices | start [port] | session <udid> [port] | nav <url> |
                shot [outFile] | eval <js> | click <selector> | fill <selector> <value> |
                submit <formSelector> | type <text> | key <name>`);
  process.exit(1);
}
