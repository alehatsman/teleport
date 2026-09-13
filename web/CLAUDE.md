# Frontend conventions

Read [../docs/09-frontend.md](../docs/09-frontend.md) first — architecture, the offset
contract, control-lease UI, geometry, mobile rules. That file says what the code does.

How it is split into pieces and styled is the fleet's
[ts-quality docs/UI.md](https://github.com/alehatsman/ts-quality/blob/main/docs/UI.md):
BEM strictly, shared blocks in one base stylesheet, tokens on `:root`, motion and
a11y rules, no UI framework / component library / CSS-in-JS / state or router library.
This file is only the teleport delta: where things live here, and the gate.

## Structure

[09-frontend.md#structure](../docs/09-frontend.md#structure) has the file tree and the
`SessionLauncher`/`DirectoryBrowser` interfaces — same `api/` / `ui/` split as codefort's
web app, without a `features/` layer (teleport has one feature, not several) and with
`ui/` still unborn (first promoted primitive creates it). Imports
use the `@/` alias for `src/` (`@/api/api`, `@/Session.svelte`) — no relative `../`
across top-level files.

## Gate

- `npm run lint && npm run typecheck && npm run build && npm test` before calling
  anything done, or `provision apply tasks/ui-ci.yml` from the repo root for the full
  gate. All must come back clean — 0 errors, 0 warnings. `svelte-check` also flags
  unused CSS selectors, which is the cheapest signal that a rename missed a template
  reference.
- Lint and format are Biome (`biome.jsonc`, extending ts-quality's `biome.base.json`).
  `npm run lint:fix` applies the safe fixes and the formatter; don't hand-sort imports
  or hand-format. Style: no semicolons, double quotes, 100 columns. A deliberate
  exception gets an inline `// biome-ignore lint/<group>/<rule>: <reason>`; never demote
  a rule in `biome.jsonc` without a comment saying why.
- Biome sees only the `<script>` block of a `.svelte` file, so three rules that need
  the template are off for `**/*.svelte` (unused variables/imports, undeclared deps).
  `svelte-check` covers those. Biome does not read a `<style>` block at all; the
  gate's ui-lint step does (BEM class names, raw color/radius/duration literals),
  and it is what checks the UI.md rules in component CSS.
- For a visual change, look at it: `npm run dev` and screenshot the affected views
  (headless Chromium works — `chromium-browser --headless=new --screenshot=out.png
  '<url>'`). A clean build proves the CSS parses, not that it looks right.

## Where a block lives here

**Shared blocks live in `src/app.css`.** Anything that appears in more than one
component is a block there, not duplicated per component:

| Block | Modifiers | Used for |
|---|---|---|
| `.btn` | `--primary`, `--danger` | every button |
| `.dot` | `--success`, `--warning`, `--warning-strong`, `--pulse` | status indicators (pair with a `.sr-only` label — a dot is `aria-hidden`) |
| `.badge` | `--controlling` | filled pill labels |
| `.banner` | `--error` | full-width inline alerts |
| `.notice` | elements `__link`, `__dismiss` | dismissible strip (e.g. "scrollback truncated") |
| `.toast` | | transient corner message |

Before adding a new button/badge/dot color, check this table first — reuse a modifier
or add one here rather than hand-rolling colors in a component's `<style>`.

**Component-scoped blocks stay in that component's `<style>`**, scoped by Svelte
automatically (`.sessions`, `.launcher`, `.empty`, `.session-list`, `.session-row`,
`.session`, `.key-bar`, `.terminal`, …). Don't promote to `app.css` until a second
component actually needs it. Conditional classes use Svelte's `class:` directive.

## Tokens

On `:root` in `app.css`. Spacing scale is `--space-1` (0.25rem) through `--space-4`
(1rem); radii are `--radius-sm`/`-md`/`-lg`; one transition duration; `--font-mono`
for commands, paths and ids (never spell the stack out in a component). The
reduced-motion kill switch and the global `:focus-visible` ring are in `app.css` too.

## Dark-only

`color-scheme: dark` on `:root`; there is no light theme and no toggle. Don't add
`prefers-color-scheme` branching — nothing here reads it.
