# ui/

Primitives that know no domain: markup plus class composition over the
shared blocks in `app.css` (`.btn`, `.dot`, `.badge`, `.banner`, `.notice`,
`.toast`). Same layer as codefort's `src/ui/` (ts-quality docs/UI.md rules
19, 24).

Empty on purpose. In Svelte a shared block is usually just the class on the
element (UI.md rule 13: compose, don't wrap), so a primitive appears here
only when two features need the same markup, not just the same class. The
first one promoted lands here with a row in `web/CLAUDE.md`'s shared-block
table (UI.md rule 28).
