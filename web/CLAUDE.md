# Frontend conventions

Read [../docs/09-frontend.md](../docs/09-frontend.md) first — architecture, the offset
contract, control-lease UI, geometry, mobile rules. That file says what the code does.
This file says how to write and style it.

## Before touching anything

- No UI framework, no component library, no CSS-in-JS. Plain CSS only
  ([docs/09-frontend.md#explicitly-not-in-the-frontend](../docs/09-frontend.md#explicitly-not-in-the-frontend)).
  If a task seems to need one, it doesn't — ask, don't add a dependency.
- No state-management library, no router library, no SSR/SvelteKit.
- `npm run build && npm run check` before calling anything done. Both must come back
  clean — 0 errors, 0 warnings. `svelte-check` also flags unused CSS selectors, which
  is the cheapest signal that a rename missed a template reference.
- For a visual change, look at it: run `npm run dev` and screenshot the affected views
  (headless Chromium works fine — `chromium-browser --headless=new --screenshot=out.png
  '<url>'`). A clean build proves the CSS parses, not that it looks right.

## CSS: BEM, strictly

Every class is `block`, `block__element`, or a modifier —
`block--modifier` / `block__element--modifier`. No bare utility classes beyond
`.sr-only`. Don't nest elements inside elements — `block__element__sub-element` isn't
BEM; if a part has its own sub-parts, it's its own block (e.g. `session-row` is a
sibling block of `session-list`, not `session-list__row`).

**Shared blocks live in `src/app.css`.** Anything that appears in more than one
component — buttons, status dots, badges, banners, notices, toasts — is a block there,
not duplicated per component:

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

**Component-scoped blocks stay in that component's `<style>`.** A block used by only
one `.svelte` file (`.sessions`, `.launcher`, `.empty`, `.session-list`,
`.session-row`, `.session`, `.key-bar`, `.terminal`, …) is styled where it's used,
scoped by Svelte automatically. Don't promote something to `app.css` until a second
component actually needs it.

**Compose, don't wrap.** Apply a shared block directly on the element —
`<span class="dot dot--success">` — rather than adding a component element class that
just forwards to it. Plain CSS has no `@extend`, so a wrapper class would mean
duplicating the rule, not reusing it.

## Design tokens

Colors, spacing, radii, shadows, and the one transition duration are custom properties
on `:root` in `app.css`. Never hardcode a hex color, an ad hoc `border-radius: Npx`, or
a bespoke transition duration inside a component — add or reuse a token instead.
Spacing scale is `--space-1` (0.25rem) through `--space-4` (1rem); radii are
`--radius-sm`/`-md`/`-lg`.

## Motion and accessibility

- Every transition and `@keyframes` animation must go inert under
  `prefers-reduced-motion: reduce` — already handled globally in `app.css`. Don't
  bypass it with an inline `!important` duration.
- `:focus-visible` gets a ring globally (`app.css`); don't add `outline: none`
  anywhere.
- A color- or icon-only indicator (a `.dot`, an attention marker) needs a `.sr-only`
  text twin next to it — see `Sessions.svelte`'s session-state dot for the pattern.

## Dark-only

`color-scheme: dark` on `:root`; there is no light theme and no toggle. Don't add
`prefers-color-scheme` branching — nothing here reads it.
