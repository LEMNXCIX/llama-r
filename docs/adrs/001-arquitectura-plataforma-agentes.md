# ADR 001: Arquitectura de plataforma extensible de agentes

**Estado:** Aceptado  
**Fecha:** 2026-07-23  
**Contexto:** ARCHITECTURE_PLAN.md v0.2.0

## Decisión: Rig-core como engine de agentes

- **Opción:** Rig ya está en el árbol de dependencias; expone traits de agente,
  tool-use, y streaming que cubren el 80% del caso de uso sin escribir un loop
  propio de LLM + tool calls.
- **Riesgo menor:** Rig está en evolución activa. Se aísla tras el trait
  `AgentEngine` en `src/ports/engine.rs` para poder cambiar de implementación
  sin tocar el core de llama-r.

## Decisión: LanceDB para RAG vectorial

- **Opción:** LanceDB embebido (formato columnar, cero servicios externos).
- **Alternativas descartadas:**
  - Qdrant / Milvus: requieren servidor aparte.
  - SQLite + `sqlite-vec`: extensión no oficial, menor madurez.
  - In-memory HashMap: válido para tests, insuficiente para producción.
- **Ruta:** `data/lancedb/<namespace>/<collection>` — cada agente/proyecto
  es un namespace aislado.

## Decisión: SQLite para historial de conversaciones

- **Opción:** SQLite vía `rusqlite` (sin ORM).
- **Alternativas descartadas:**
  - PostgreSQL: dependencia externa innecesaria para single-node.
  - Redis: no relacional, difícil modelar joins conversación + eventos.
- **Esquema mínimo:** `conversations`, `messages`, `tool_events`.

## Consecuencias

- El runtime de llama-r sigue siendo un solo binario sin servicios externos
  obligatorios (Ollama es el único provider por defecto).
- Cada fase del plan añade una dependencia Cargo opcional (feature flag).
- El scaffold actual (Fase 0-1) es 100% compilable con `cargo test`.
