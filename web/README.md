# teleport web

Svelte + TypeScript + Vite frontend for `teleportd`. See
[../docs/09-frontend.md](../docs/09-frontend.md) and
[../docs/11-mvp-plan.md](../docs/11-mvp-plan.md#m5--browser-terminal).

```bash
npm install
npm run dev     # local dev server
npm run build   # -> dist/, served by teleportd (docs/08-packaging.md#build-pipeline)
npm run lint    # biome: lint + format + import order (lint:fix to apply)
npm run typecheck  # svelte-check + tsc -b
npm test        # vitest
```

The lint and type baselines are [ts-quality](https://github.com/alehatsman/ts-quality)'s,
copied in as `biome.base.json` / `tsconfig.base.json` by `provision apply
tasks/ui-sync-config.yml` (run from the repo root) and extended, never edited.
`provision apply tasks/ui-ci.yml` is the full pre-push gate; `tasks/ui-fast.yml`
the pre-commit one. Same shape as `tasks/ci.yml` for the Rust crates.
