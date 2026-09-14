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
component interfaces. Codefort's four layers, UI.md rule 19: `api/`, `ui/`, `shell/`,
`features/<x>/`. Two features: `features/sessions/`, holding list and viewer both, and
`features/auth/`, holding the login screen and the passkey settings panel
([../docs/17-passkey-login.md](../docs/17-passkey-login.md)).
`shell/` is empty with a README saying what lands there; `ui/` holds the primitives
below; do not create a
third top-level layer, and do not put a component at `src/` root — `App.svelte`,
`main.ts` and `app.css` are the only root files. Imports use the `@/` alias for `src/`
(`@/api/api`, `@/features/sessions/Session.svelte`) — no relative `../` across layers.

## Gate

- `npm run lint && npm run typecheck && npm run build && npm test` before calling
  anything done, or `provision apply tasks/ui-ci.yml` from the repo root for the full
  gate. All must come back clean — 0 errors, 0 warnings. `svelte-check` also flags
  unused CSS selectors, which is the cheapest signal that a rename missed a template
  reference.
- `npm run test:e2e` (Playwright, `web/e2e/`) is separate from the gate above — it needs
  a real `teleportd` binary and real browsers, not just Node. See
  [docs/10-testing.md#web-e2e-playwright](../docs/10-testing.md#web-e2e-playwright), or
  `provision apply tasks/ui-e2e.yml` for the whole thing in one command.
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

## Logic lives in plain modules

Anything with no reactive state — formatting, filtering, request building — is a `.ts`
file beside the component that uses it (`features/sessions/sessionDisplay.ts`,
`launchRequest.ts`), with a `.test.ts` next to it (UI.md rule 27). A component's
`<script>` holds state, effects and handlers; if a function in it takes plain values and
returns plain values, it belongs in the module. Pass `now` in; never read `Date.now()`
inside a helper.

## Where a block lives here

**Shared blocks live in `src/app.css`.** Anything that appears in more than one
component is a block there, not duplicated per component:

| Block | Modifiers | Used for |
|---|---|---|
| `.btn` | `--primary`, `--danger` | every button |
| `.dot` | `--success`, `--warning`, `--warning-strong`, `--pulse` | status indicators (pair with a `.sr-only` label — a dot is `aria-hidden`) |
| `.chip` | `--active` | small toggleable options: filter tabs, recent-cwd picks |
| `.badge` | `--controlling` | filled pill labels |
| `.banner` | `--error` | full-width inline alerts |
| `.notice` | elements `__link`, `__dismiss` | dismissible strip (e.g. "scrollback truncated") |
| `.toast` | | transient corner message |

Before adding a new button/badge/dot color, check this table first — reuse a modifier
or add one here rather than hand-rolling colors in a component's `<style>`.

**Primitives live in `src/ui/`** (UI.md rules 24, 28) — a component only when the
shared thing is markup, not just a class (rule 13: a bare shared block goes straight on
the element):

| Primitive | Props | Wraps |
|---|---|---|
| `StatusDot` | `tone`, `pulse`, `label`, `showLabel` | `.dot` + its text twin (visible or `.sr-only`) — never place a bare `.dot` |
| `ErrorBanner` | `message` | `.banner--error` + `role="alert"`; placement is the caller's (wrap it) |

A primitive's prop vocabulary (`DotTone`) lives in `src/ui/tones.ts`, a plain module:
`tsc` cannot read a type exported from a `.svelte` module script, and feature helpers
in `.ts` need it.

**Component-scoped blocks stay in that component's `<style>`**, scoped by Svelte
automatically (`.sessions`, `.session-filters`, `.fab`, `.launcher`, `.browser`,
`.session-list`, `.session-row`, `.session`, `.session-header`, `.key-bar`,
`.terminal`). One component, one block (UI.md rule 23) — a part with parts of its own
gets its own file. Don't promote to `app.css` until a second
component actually needs it. Conditional classes use Svelte's `class:` directive.

## Tokens

On `:root` in `app.css`. Spacing scale is `--space-1` (0.25rem) through `--space-4`
(1rem); radii are `--radius-sm`/`-md`/`-lg`; one transition duration; `--font-mono`
for commands, paths and ids (never spell the stack out in a component). The
reduced-motion kill switch and the global `:focus-visible` ring are in `app.css` too.

## Dark-only

`color-scheme: dark` on `:root`; there is no light theme and no toggle. Don't add
`prefers-color-scheme` branching — nothing here reads it.
