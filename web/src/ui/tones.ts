// Tone vocabularies for the primitives, in a plain module so a feature's
// .ts helpers can import them: tsc cannot see a type exported from a
// .svelte module script, only svelte-check can.

/** StatusDot: which .dot modifier. null is the base gray ("unknown / inactive"). */
export type DotTone = "success" | "warning" | "warning-strong" | null
