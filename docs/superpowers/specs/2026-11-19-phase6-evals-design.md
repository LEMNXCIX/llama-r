# Fase 6 (pieza 6b) — Evals declarativas reutilizables

**Fecha:** 2026-11-19
**Estado:** propuesto — aprobado en brainstorming, pendiente de revisión del spec
**Alcance:** primera pieza de la Fase 6. Convierte el smoke test multi-provider en un
runner de evals declarativo y reutilizable con aserciones ricas sobre fixtures. NO cubre
el decision model de `Auto` (6a) ni OpenTelemetry/cost tracking (6c) — esos van en un
segundo spec porque tocan el runtime.

## Contexto

Ya existe un harness de smoke (`tests/provider_eval_smoke.rs` + `evals/providers/smoke.eval.toml`):
lee casos TOML, corre cada caso contra cada provider con fixtures servidas por un `TcpListener`
local, e imprime una tabla pass/fail. Es fixture-only (sin llaves reales en CI) y compara
principalmente la forma del wire de los adapters.

Tres hechos del código condicionan esta pieza:
1. `AgentRunEvent::{Token, ToolCall, ToolResult, Completed, Error, ApprovalRequired}` ya
   existen y el e2e de Fase 7 (`tests/execution_recovery_tests.rs`, `agent_runtime_tests.rs`)
   ya demostró cómo interceptar el tool-loop con un fake Ollama socket. Eso hace alcanzable
   `calledTool` sin un `mockModel` nuevo.
2. El puerto `DecisionRouter` ya existe y el router d1 (`LlamaCppRouter`/`OpenRouterRouter`/
   `OpenCodeRouter`) evalúa `Question::{Choice, Noul}` con probabilidades y degrada a
   `RouterUnavailable` cuando no hay router. Eso hace alcanzable `judge` con degradación a skip.
3. `ChatResponse.usage` ya reporta token counts y `ModelCapabilities` declara costes (hoy `None`).
   No se requiere nada de eso para esta pieza.

Se descartó la opción B (solo formalizar el smoke — deja `calledTool` sin hacer, que es lo más
valioso) y la C (framework con `mockModel` — scope creep, reimplementaría el engine de prueba).

## Goal

Que el equipo pueda definir casos de evaluación en TOML con aserciones de comportamiento
(`contains`, `notContains`, `calledTool`, `judge`) y ejecutarlos de forma repetible sobre
fixtures, con un modo estricto para CI, sin llaves reales y sin tocar el runtime.

## Non-goals

- Decision model para `Auto` (6a) — sigue `Auto ≡ Always`.
- OpenTelemetry, trazas, cost tracking (6c).
- `mockModel` o un engine simulado (se usan fixtures + el tool-loop real con fake socket).
- Llaves reales o network en CI — fixture-only, como el smoke actual.
- Cambiar endpoints, puertos ni el runtime de chat/agent.

## Decisiones de diseño

1. **Runner como librería reutilizable.** Extraer la lógica del smoke test a un módulo de
   biblioteca (`src/evals/`): carga de TOML, ejecución de casos, tabla de resultados, y
   aserciones. El smoke test existente se convierte en un consumidor delgado del runner,
   de modo que haya **una sola** implementación del harness.
2. **Tipos de aserción** (campo `check` en cada caso, con forma TOML):
   - `{ type = "contains", text = "..." }` (equivalente al `expect` actual).
   - `{ type = "not_contains", text = "..." }`.
   - `{ type = "called_tool", name = "server__tool" }` — verifica que apareció un
     `AgentRunEvent::ToolCall` con ese nombre calificado durante el turno.
   - `{ type = "judge", question = "..." }` — evalúa la respuesta con un `DecisionRouter`
     (d1). Si no hay router disponible (el caso común en CI), el caso se **omite con razón
     explícita** (`skipped: router_unavailable`), nunca se aprueba por defecto.
3. **`mode = "strict"` en el TOML** (o flag del runner): en modo estricto, cualquier caso
   fallido → salida no por el command/test. Los casos `skipped` no fallan, pero se reportan.
   El valor por defecto es estricto en CI.
4. **Dos clases de caso**: `provider` (chat directo contra adapters, como hoy) y `agent`
   (turno de agente con tool-loop contra un fake Ollama socket, para `calledTool`). Un caso
   `agent` nombra el agente y su manifiesto de fixture; se ejecuta con el mismo patrón e2e
   de Fase 7. Los fixtures siguen siendo JSON embebidos o archivos bajo `evals/fixtures/`.
5. **Sin secretos**: el runner nunca imprime args de tools ni contenido de prompts con
   secretos; reutiliza `redact_secrets` de Fase 7 si alguien pone un secreto en un fixture.

## Arquitectura

### Módulo `src/evals/` (nuevo)

- `mod.rs`: exports del runner y tipos.
- `cases.rs`: `EvalCase`, `EvalCheck` (enum de aserciones), `EvalMode`, `EvalSuite` —
  deserializables desde TOML. `EvalCase` gana `checks: Vec<EvalCheck>` (mantiene `expect`
  como azúcar hacia `contains` para compatibilidad con el TOML actual).
- `runner.rs`: `run_suite(suite, options) -> EvalReport`. Ejecuta cada caso según su clase:
  - `provider`: construye el adapter (patrón del smoke actual) contra un `TcpListener` con
    la fixture y aplica las aserciones de texto a la respuesta.
  - `agent`: construye un `AgentRuntime` con un engine apuntando a un fake Ollama, ejecuta
    el turno, recolecta los `AgentRunEvent` del canal y aplica `contains`/`calledTool` al
    texto final y a los eventos.
  - `judge`: invoca el `DecisionRouter` si está disponible (inyectado en `options`); si no,
    marca `skipped`.
- `report.rs`: `EvalReport { rows: Vec<EvalRow>, skipped: Vec<SkippedRow> }` con el formato
  de tabla y un método `all_passed(strict)`.

### Integración

- `tests/provider_eval_smoke.rs` se reescribe como un consumidor delgado: carga el TOML,
  llama `run_suite`, imprime el reporte y aserta `all_passed(strict)`.
- `evals/providers/smoke.eval.toml` se extiende con al menos: un caso `agent` con
  `called_tool`, un caso con `not_contains`, y un caso `judge` (que se marcará `skipped`
  en CI por no haber router). Se mantiene el comentario de live-smoke opcional.
- `src/lib.rs` (o `main.rs`) expone `pub mod evals;` — el módulo es siempre compilado, sin
  dependencias nuevas salvo las ya presentes (`toml`, `serde`, `rig-core`, `tokio`).

## Flujo de datos

1. Cargar `EvalSuite` desde TOML (`include_str!` en el test, ruta en la CLI futura).
2. Para cada caso, el runner elige la clase (`provider`/`agent`), monta la fixture local y
   ejecuta, recolectando texto final + eventos del tool-loop.
3. Aplica cada `EvalCheck` en orden; el primero que falla produce la celda `FAIL` con detalle.
4. `judge` sin router → fila `skipped` con razón, no `FAIL`.
5. El reporte se imprime como tabla y `all_passed(strict)` decide el exit del test.

## Manejo de errores

- Fixture malformada / caso inválido → error nombrando el id del caso; el suite continúa con
  los demás casos y reporta el fallo, no panic.
- `judge` sin router → `skipped: router_unavailable` (razón explícita). Un router que falla
  también produce `skipped`, nunca aprobación automática.
- `called_tool` con el nombre equivocado → `FAIL` con detalle del tool visto (redactado).
- Ningún secreto en la tabla ni en los detalles: se reutiliza `redact_secrets`.

## Testing

- `tests/evals_tests.rs` (nuevo): unit tests de `EvalCheck` sobre respuestas y eventos
  sintéticos (contains/not_contains/called_tool/judge-skip); test de que un caso inválido no
  detiene el suite; test de que un `judge` sin router se marca skipped; test de que el modo
  estricto falla con un caso malo y el no-estricto no.
- El smoke test reescrito sigue pasando con el TOML extendido (los casos `agent`/`judge`
  nuevos se ejecutan/skipean según corresponda).
- Gates: fmt/clippy/audit/738+ tests/`--no-default-features`.

## Riesgos

- **`calledTool` acoplado al formato de eventos del engine**: si el tool-loop cambia, el eval
  debe actualizarse. Mitigación: el runner consume `AgentRunEvent` (el contrato público), no
  detalles del bridge; un test pinnea el nombre calificado.
- **`judge` con router d1 en CI**: en la práctica nunca hay router, así que siempre se omite y
  podría dar falsa sensación de cobertura. Mitigación: el reporte muestra los omitidos con
  razón, y el spec exige al menos un `judge` con respuesta conocida que se pueda forzar con un
  router fake en un test, para probar la rama no-skipped.
- **Reescribir el smoke test**: riesgo de perder cobertura de wire que hoy existe. Mitigación:
  el runner conserva los casos `provider` existentes y el test reescrito debe seguir
  cubriendo los seis providers (el reviewer lo verificará).

## Exit criteria

- Un caso TOML con `called_tool` detecta una tool call real en el tool-loop; uno con
  `judge` se skipea con razón cuando no hay router.
- El smoke test consume el runner y sigue pasando para los seis providers.
- Modo estricto falla con caso malo; modo no-estricto reporta pero no falla.
- Ningún secreto en el reporte; runtime y puertos sin cambios; gates verdes; docs alineadas.
