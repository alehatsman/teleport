# shell/

App chrome that is not a feature: layout, navigation, global status strips,
cross-feature helpers (a `timeAgo`, keyboard navigation). Same layer as
codefort's `src/shell/` (ts-quality docs/UI.md rule 19).

Empty on purpose. Teleport's only chrome today is the hash router in
`App.svelte`, which stays at the root like codefort's `App.tsx`. The first
piece of chrome shared by two features lands here; nothing is moved here
ahead of that (UI.md rule 25).
