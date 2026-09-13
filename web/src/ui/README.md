# ui/

Primitives that know no domain: markup plus class composition over the
shared blocks in `app.css`. Same layer as codefort's `src/ui/` (ts-quality
docs/UI.md rules 19, 24).

In Svelte a shared block is usually just the class on the element (UI.md
rule 13: compose, don't wrap), so a primitive lands here only when the shared
thing is *markup* -- an a11y twin, a required role -- not just a class. Each
one gets a row in `web/CLAUDE.md`'s primitives table (UI.md rule 28). No
`api/` import, no feature import, no domain enum: the feature maps its state
to a `tone` at the call site (rule 12).

`tones.ts` holds the primitives' prop vocabularies as a plain module, so a
feature's `.ts` helpers can import them under `tsc` (which cannot see a type
exported from a `.svelte` module script).
