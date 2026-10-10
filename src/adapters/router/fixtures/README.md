# systemone_response.json

**CAPTURED** from a real `llama-server`, not hand-written.

Captured from:

    llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0
    # llama.cpp, built from source at the commit this branch was tested against
    # listening on http://127.0.0.1:9931

    curl -s http://127.0.0.1:9931/v1/systemone \
      -H 'Content-Type: application/json' \
      -d '{"state":"necesito cancelar el pedido 42 urgentemente",
           "questions":{
             "agent":{"type":"choice",
                      "instructions":"Cual agente debe atender esta peticion? Elige el general si ninguna especializacion encaja.",
                      "criteria":{
                        "fudi::ops":"Fudi Ops — Cancela pedidos, gestiona el menu de Fudi y consulta el ERP. NO responde preguntas generales.",
                        "fudi::fudi":"Fudi General — Asistente general de Fudi. Responde preguntas generales sobre el negocio."}},
             "specialized":{"type":"noul",
                             "instructions":"Esta peticion necesita un agente especializado?"}}}'

What this closes: the design doc listed the response envelope as its one open
risk, because Liquid publishes the *request* shape but no example of a
`llama-server` *response*. The fixture was therefore fitted to the hosted API's
documented field names and could not be distinguished from a guess.

It is now an observation. The fields `decode_response` reads — `answers.<name>.choice`,
`.confidence`, `.probabilities` for a Choice and `.noul` for a Noul — are the
ones the local server actually emits, so the adapter's structs are pinned by
reality rather than by a cross-implementation inference.

Two things worth keeping in mind about the shape:

- Each answer carries an explicit `"type"` tag — `"choice"` or `"noul"` —
  matching the question that was asked. `decode_answer` derives the kind from
  the fields present and only *checks* a string `type` when one is present, so a
  server that omitted the tag would still parse; this capture simply confirms
  that it does not omit it.
- `model` echoes the full `-hf` spec (`LiquidAI/d1-3B-GGUF:Q8_0`), not the bare
  `d1-3b` label `ROUTER_MODEL` defaults to. `decode_response` ignores it — the
  model is whatever the running server was started with, which is why the
  label is log-only.