#!/usr/bin/env node
// Approximate mobile rendering check, via headless Chrome + real CDP -- no
// deps beyond Node's built-in fetch/WebSocket. Read README.md's "Chrome vs
// real device" section before trusting this for anything Safari-specific.
//
// Why not `chrome --headless=new --screenshot=out.png <url>`? That one-shot
// CLI flag races the window resize against the first paint and can render a
// stale/wrong box for some elements while everything else looks fine --
// found the hard way, cost a false bug report on a layout that was actually
// correct. This tool instead: sets device metrics BEFORE navigating, polls
// document.readyState, then waits a fixed settle window before capturing --
// because Vite's dev server injects CSS via JS (not a <link> tag), so even
// a "loaded" page can be a beat away from fully styled.
//
// Usage:
//   chrome.mjs launch                              start headless Chrome w/ CDP on :9333
//   chrome.mjs shot <url> [w] [h] [outFile]         mobile-viewport screenshot
//   chrome.mjs eval <url> <jsExpression> [w]        run JS on the page, print the JSON result
//
// `shot`/`eval` require `chrome.mjs launch` already running in the background.

const CDP = "http://127.0.0.1:9333";
const SETTLE_MS = 500;

function connect() {
  return (async () => {
    const targets = await (await fetch(`${CDP}/json`)).json();
    const target = targets.find((t) => t.type === "page");
    if (!target) {
      throw new Error("No page target found -- is `chrome.mjs launch` running?");
    }
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    const pending = new Map();
    let id = 0;
    await new Promise((resolve, reject) => {
      ws.onopen = resolve;
      ws.onerror = () => reject(new Error("CDP websocket connect failed"));
    });
    ws.onmessage = (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && pending.has(msg.id)) {
        pending.get(msg.id)(msg.result);
        pending.delete(msg.id);
      }
    };
    const send = (method, params = {}) =>
      new Promise((resolve) => {
        const msgId = ++id;
        pending.set(msgId, resolve);
        ws.send(JSON.stringify({ id: msgId, method, params }));
      });
    return { ws, send };
  })();
}

async function gotoAndSettle(send, url, width, height) {
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", {
    width, height, deviceScaleFactor: 3, mobile: true,
  });
  await send("Page.navigate", { url });
  await new Promise((resolve) => {
    const check = setInterval(async () => {
      const r = await send("Runtime.evaluate", { expression: "document.readyState" });
      if (r?.result?.value === "complete") {
        clearInterval(check);
        resolve();
      }
    }, 200);
  });
  await new Promise((r) => setTimeout(r, SETTLE_MS));
}

const [, , cmd, ...args] = process.argv;

if (cmd === "launch") {
  const { spawn } = await import("node:child_process");
  const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
  const child = spawn(
    chrome,
    ["--headless=new", "--disable-gpu", "--remote-debugging-port=9333", "about:blank"],
    { detached: true, stdio: "ignore" },
  );
  child.unref();
  console.log(`launched, pid ${child.pid}`);
} else if (cmd === "shot") {
  const [url, width = "390", height = "844", outFile = "/tmp/mobile-render.png"] = args;
  if (!url) { console.error("usage: chrome.mjs shot <url> [w] [h] [outFile]"); process.exit(1); }
  const { send, ws } = await connect();
  await gotoAndSettle(send, url, parseInt(width, 10), parseInt(height, 10));
  const shot = await send("Page.captureScreenshot", { format: "png" });
  const fs = await import("node:fs");
  fs.writeFileSync(outFile, Buffer.from(shot.data, "base64"));
  console.log(outFile);
  ws.close();
} else if (cmd === "eval") {
  const [url, expr, width = "390"] = args;
  if (!url || !expr) { console.error("usage: chrome.mjs eval <url> <jsExpression> [w]"); process.exit(1); }
  const { send, ws } = await connect();
  await gotoAndSettle(send, url, parseInt(width, 10), 844);
  const result = await send("Runtime.evaluate", { expression: expr, returnByValue: true });
  console.log(JSON.stringify(result?.result?.value ?? null, null, 2));
  ws.close();
} else {
  console.error("usage: chrome.mjs launch | shot <url> [w] [h] [outFile] | eval <url> <js> [w]");
  process.exit(1);
}
process.exit(0);
