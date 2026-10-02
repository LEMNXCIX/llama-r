# Llama-R Project Roadmap

> **Fuente de verdad de la plataforma de agentes:** [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md)  
> Este archivo resume el estado y apunta a las fases del plan.

## Estado actual (base)

- [x] Servicio Rust (HTTP axum, gRPC tonic, TUI)
- [x] Provider Ollama + streaming OpenAI-compatible
- [x] Agentes TOML (global + proyecto) con hot reload
- [x] Contextos de proyecto (`analyze` / `reanalyze`)
- [x] MCP **server** gateway (`/api/mcp`)
- [x] **Fase 0:** Dominio + campos de scope en TOML (AgentConfig, scope.rs, ports, ADR, ejemplos)
- [x] **Scaffold v2:** scopes, ports, MCP client HTTP, RAG in-memory, registry, Rig adapter stub

## Plataforma extensible de agentes (plan v2)

| Fase | Tema | Prioridad | Estado |
|------|------|-----------|--------|
| 0 | Dominio + campos de scope en TOML | P0 | ✅ completado |
| 1 | Registry dinámico + ScopeBuilder | P0 | ✅ scaffold |
| 2 | MCP Client (discovery + call) | P0 | 🔄 HTTP client listo; stdio pendiente |
| 3 | Integración Rig.rs | P0 | ✅ completado (engine non-stream + stream Nivel-A, history multi-turn, scope re-check; sub-agentes P2 pendiente) |
| 4 | RAG segmentado (FileRagStore) | P1 | ✅ completado (port, Ollama embeddings, FileRagStore persistente, ingest, API admin, aislamiento; LanceDB diferido por peso de deps) |
| 5 | Historial + resúmenes → RAG | P1 | ✅ completado (SqliteConversationStore, multi-turn, summarizer, retention job, export/history API) |
| 6 | Seguridad + observabilidad | P1 | ⬜ parcial (deny-all defaults) |
| 7 | Ecosistema (CLI, Docker, docs) | P2 | ⬜ |

Planes de ejecución por fase: [`PHASE_3_RIG_ENGINE_PLAN.md`](./PHASE_3_RIG_ENGINE_PLAN.md) · [`PHASE_4_RAG_PLAN.md`](./PHASE_4_RAG_PLAN.md) · [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md).  
Fuente de arquitectura: **[ARCHITECTURE_PLAN.md](./ARCHITECTURE_PLAN.md)**.

## Principio de extensibilidad

- **Nuevo agente** → solo archivo TOML.
- **Nueva app externa (Fudi, etc.)** → solo MCP Server + `mcp-servers/<id>.toml` + referencias en manifiestos.
- **Sin modificar el core** de llama-r.

## Comandos de desarrollo

```bash
cargo fmt
cargo check
cargo test --target-dir target-tests
cargo run
```
