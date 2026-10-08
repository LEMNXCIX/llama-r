# systemone_response.json

NOT CAPTURED. `llama-server` was unavailable when this fixture was written.
This is the hosted API's documented shape (docs.liquid.ai, decision-model
guide), not an observation of the local server.

Task 2's decode test passes either way, but only a captured fixture proves
the local envelope. Re-capture with the `curl` in Task 2 Step 1 before
trusting this adapter against a real `llama-server`.

## What is documented and what is invented

The **field names** are documented; the **values** are not observations —
they are plausible numbers for the example request in Task 2 Step 1 (state
"me cobraron dos veces este mes"; `agent` is a choice over
`fudi::ops`/`fudi::fudi`, `specialized` a noul).

- `answers.<name>.choice`, `answers.<name>.probabilities`,
  `answers.<name>.confidence` — the documented Choice answer: the winning
  label, the full distribution, and a confidence that need not equal the
  top probability.
- `answers.<name>.noul` — the documented Noul answer: a probability in
  [0, 1].
- `answers.<name>.type` — the documented per-answer discriminator.
  `decode_response` cross-checks it against the body when present but does
  not depend on it, so a local server that omits it still decodes.
- `model` and `usage` — documented top-level fields; `decode_response`
  ignores both. `usage.output_tokens` is `0` because d1 generates no
  tokens.
