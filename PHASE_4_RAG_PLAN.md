# Fase 4 — RAG centralizado y segmentado

> **Documento de ejecución para un agente de IA implementador**  
> Fuente: [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md) § Fase 4 + §6.7  
> Audiencia: agente **menos capaz** — seguir este plan **en orden**, sin saltarse pasos, sin “mejoras creativas” fuera de alcance.  
> Ubicación: raíz del repo (`PHASE_4_RAG_PLAN.md`)  
> Plan hermano: [`PHASE_3_RIG_ENGINE_PLAN.md`](./PHASE_3_RIG_ENGINE_PLAN.md)

---

## 0. Cómo usar este plan (léelo completo antes de tocar código)

### 0.1 Rol del implementador

Eres un implementador Rust en el repo **llama-r**. Tu trabajo es completar la **Fase 4: RAG centralizado y segmentado** para que:

1. Cada agente solo lea collections listadas en `rag_sources`.
2. Cada agente solo escriba según `rag_write` (`none` | `own_memory_only` | `listed`).
3. Los vectores persistan en disco (LanceDB) bajo un path segmentado por `source_id`.
4. Exista un pipeline de ingest (archivos + output de analyze).
5. Haya endpoints admin de debug para ingest/query.
6. Los tests demuestren **aislamiento entre agentes**.

### 0.2 Reglas obligatorias

1. **Trabaja solo en Fase 4.** No implementes historial SQLite (Fase 5), rate limits (Fase 6), ni CLI ecosistema (Fase 7).
2. **No rompas el port `RagStore` / `EmbeddingProvider`** en `src/ports/rag.rs` salvo bugs o campos mínimos necesarios. El resto del sistema (Rig `prepare`, `AgentRuntime`) ya habla ese contrato.
3. **Mantén `InMemoryRagStore`** como implementación usable en tests y fallback sin feature LanceDB.
4. **Defense in depth:** `query_scoped` y `upsert_scoped` **siempre** llaman `scope.allows_rag_read` / `allows_rag_write` antes de tocar datos. Nunca confíes solo en el caller.
5. **No pongas secretos en TOML ni en metadata de chunks.**
6. **Después de cada subtarea (4.x):** `cargo check` y, cuando haya tests, `cargo test --target-dir target-tests`.
7. **No edites** `target/`, `target-tests/`, logs, ni dumps grandes de vectores en git.
8. Si la API del crate `lancedb` no coincide con los snippets, **adapta al crate real** (docs.rs / ejemplos oficiales), pero **mantén el port estable**.
9. Commits: solo si el usuario lo pide. No hagas `git push`.
10. Comentarios de código en inglés. Mensajes de error en inglés (consistente con el repo).
11. Si Fase 3 aún no cableó `RigAgentEngine.rag` / `AgentRuntime.rag`, **igual** implementa el store + wiring en `runtime.rs` / `AppState` para que quede listo; no reimplementes el engine.

### 0.3 Criterio de salida de toda la fase (Definition of Done)

| # | Condición |
|---|-----------|
| D1 | Traits `RagStore` + `EmbeddingProvider` estables y usados por adapters. |
| D2 | `OllamaEmbeddings` funciona (o se mejora) contra `POST {OLLAMA_URL}/api/embeddings`. |
| D3 | Existe `LanceRagStore` (o nombre equivalente) que implementa `RagStore` con persistencia en disco. |
| D4 | Path de datos: `{base}/data/lancedb/<encoded_source_id>/` (o equivalente documentado). |
| D5 | Namespaces documentados y parseados: `project:…`, `agent:…`, `global:…`, y forma corta `app/col`. |
| D6 | `query_scoped` solo lee `rag_sources`; deniega o ignora el resto. |
| D7 | `upsert_scoped` respeta `rag_write` (`None` / `OwnMemoryOnly` / `Listed`). |
| D8 | Pipeline de ingest: chunking + embed + upsert desde `context_files` y/o `context_md` de analyze. |
| D9 | API admin debug: `POST /api/rag/ingest` y `POST /api/rag/query` (gated o documentados como debug). |
| D10 | Tests de aislamiento: agente A no recupera chunks de collection de agente B. |
| D11 | Feature Cargo `rag` (o `rag-lancedb`) opcional; sin feature el build sigue compilando con in-memory o sin RAG. |
| D12 | `cargo fmt`, `cargo check`, `cargo test --target-dir target-tests` pasan. |
| D13 | `ROADMAP.md` + `AGENTS.md` actualizados con estado Fase 4. |
| D14 | `data/lancedb` (o `data/`) está en `.gitignore`. |

### 0.4 Qué YA existe (no reimplementar desde cero)

| Pieza | Ruta | Estado |
|-------|------|--------|
| Port `RagStore` + `EmbeddingProvider` + tipos | `src/ports/rag.rs` | ✅ listo |
| `OllamaEmbeddings` | `src/adapters/rag/embeddings.rs` | ✅ básico — **mejorar/robustecer** |
| `InMemoryRagStore` + tests aislamiento | `src/adapters/rag/store.rs` | ✅ listo — **mantener** |
| `HashEmbeddingProvider` (tests sin red) | `src/adapters/rag/store.rs` | ✅ listo |
| Scope `allows_rag_read` / `allows_rag_write` / `own_memory_source_id` | `src/domain/scope.rs` | ✅ listo |
| `RagWritePolicy` en manifiesto | `src/domain/agent.rs` | ✅ listo |
| `rag_sources` / `rag_write` en TOML | loader + `ScopeBuilder` | ✅ listo |
| Uso opcional de RAG en engine | `src/adapters/rig_engine/mod.rs` → `prepare()` | ✅ si `rag: Some(...)` |
| `AgentRuntime.rag` | `src/services/agent_runtime.rs` | ✅ campo existe, suele ser `None` |
| LanceDB adapter | `src/adapters/rag/lancedb.rs` | ❌ **no existe** |
| Paths `data/lancedb` | `src/core/paths.rs` | ❌ **falta helper** |
| Ingest service | `src/services/` | ❌ **falta** |
| API `/api/rag/*` | `src/api/` | ❌ **falta** |
| Feature Cargo `rag` / dep `lancedb` | `Cargo.toml` | ❌ **falta** |
| Wiring en `build_runtime` | `src/runtime.rs` | ❌ RAG no se construye hoy |

### 0.5 Flujo objetivo (después de Fase 4)

```text
Ingest (admin o analyze hook)
  → leer archivos / context_md
  → chunker (texto → chunks con ids estables)
  → EmbeddingProvider.embed(chunks)
  → RagStore.upsert_scoped(scope, docs)   // enforce rag_write

Chat / Agent run
  → AgentScope con rag_sources
  → RagStore.query_scoped(scope, user_message, top_k)
  → chunks inyectados en system prompt (Rig prepare ya lo hace)
  → LLM responde con conocimiento recuperado
```

**Aislamiento:**

```text
Agent A: rag_sources = ["agent:a/memory", "fudi/policies"]
Agent B: rag_sources = ["agent:b/memory"]

A escribe en agent:a/memory  → OK (si rag_write lo permite)
B query "secret of a"        → 0 hits (no ve agent:a/memory)
A query                      → hits solo de sus sources
```

### 0.6 Orden de implementación (NO reordenar)

```text
4.1 Confirmar/congelar ports (traits)
  → 4.2 Robustecer OllamaEmbeddings + config
    → 4.3 Paths + feature Cargo + spike LanceDB
      → 4.4 Namespaces / source_id encoding
        → 4.5 LanceRagStore con enforce scope (read/write)
          → 4.6 Ingest pipeline (chunk + upsert)
            → 4.7 API admin ingest/query
              → 4.8 Wiring runtime + engine/runtime.rag
                → 4.9 Tests aislamiento (+ lance si viable)
                  → 4.10 Docs + gitignore + verificación final
```

> Las filas 4.1–4.8 del `ARCHITECTURE_PLAN` se expanden aquí en subtareas más pequeñas.  
> **4.1 y parte de 4.2/4.5 ya están en código** — no las reescribas; verifica y completa.

---

## 1. Contexto de arquitectura (solo lo necesario)

### 1.1 Capas

| Capa | Responsabilidad RAG |
|------|---------------------|
| **domain** | `AgentScope`, `RagWritePolicy`, ids de source |
| **ports** | `RagStore`, `EmbeddingProvider`, `RagChunk`, `RagUpsert` |
| **adapters/rag** | Ollama embeddings, InMemory, LanceDB |
| **services** | Ingest pipeline, (opcional) service facade |
| **api** | Endpoints admin debug |
| **runtime** | DI: construir store y pasarlo a engine/runtime |

**Regla:** ningún módulo de `api/` o `services/` importa `lancedb` directamente. Solo el adapter.

### 1.2 Contrato del port (referencia — ya en código)

Archivo: `src/ports/rag.rs`

```rust
pub struct RagChunk {
    pub id: String,
    pub source_id: String,
    pub text: String,
    pub score: f32,
    pub metadata: Value,
}

pub struct RagUpsert {
    pub source_id: String,
    pub id: String,
    pub text: String,
    pub metadata: Value,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
    fn dimensions(&self) -> usize;
}

#[async_trait]
pub trait RagStore: Send + Sync {
    async fn query_scoped(...) -> Result<Vec<RagChunk>, String>;
    async fn upsert_scoped(...) -> Result<usize, String>;
    async fn delete_collection(source_id: &str) -> Result<(), String>;
}
```

**No renombres estos tipos** sin necesidad extrema.

### 1.3 Políticas de escritura (ya en dominio)

```text
rag_write = "none"              → ningún upsert
rag_write = "own_memory_only"   → solo own_memory_source_id() o agent:{id}/memory
rag_write = "listed"            → cualquier source_id ∈ rag_sources
```

Lectura: **solo** `source_id ∈ rag_sources` (`allows_rag_read`).

### 1.4 Relación con Fase 3

`RigAgentEngine::prepare` ya hace:

```rust
if let Some(rag) = &self.rag {
    if !scope.rag_sources.is_empty() {
        let hits = rag.query_scoped(scope, &req.user_message, 6).await...
        // append "## Retrieved knowledge" to system prompt
    }
}
```

Tu trabajo en Fase 4 es que `self.rag` sea un store **real y persistente**, no reescribir el engine.

---

## 2. Tarea 4.1 — Confirmar ports (traits)

**Objetivo:** dejar el contrato congelado; solo cambios mínimos si faltan helpers.  
**Estimación:** ~0.5 día (en la práctica: revisión + tests de compilación)  
**Prioridad:** P1  
**Estado base:** ✅ ya implementado

### 2.1 Acciones

1. Leer `src/ports/rag.rs` completo.
2. **No reescribir** el archivo si ya cumple el plan.
3. Opcional (solo si lo necesitas para admin/ingest):
   - Agregar método default o trait helper **no rompedor**, por ejemplo:

```rust
// Solo si hace falta — preferible poner helpers en services, no en el trait
```

4. Si agregas algo al trait, actualiza **todas** las implementaciones:
   - `InMemoryRagStore`
   - (futuro) `LanceRagStore`
   - mocks en tests

### 2.2 Verificación 4.1

```bash
cargo check
rg "impl RagStore for" -n src/
rg "impl EmbeddingProvider for" -n src/
```

### 2.3 Checklist 4.1

- [ ] Port estable y documentado en comentarios del módulo
- [ ] `InMemoryRagStore` sigue compilando
- [ ] No hay breaking change innecesario a callers (`rig_engine`, `agent_runtime`)

**STOP:** no agregues LanceDB aún.

---

## 3. Tarea 4.2 — Embeddings vía Ollama

**Objetivo:** provider de embeddings robusto y configurable.  
**Estimación:** ~0.5–1 día  
**Prioridad:** P1  
**Estado base:** ✅ stub funcional en `src/adapters/rag/embeddings.rs`

### 3.1 Archivos

| Archivo | Acción |
|---------|--------|
| `src/adapters/rag/embeddings.rs` | Mejorar |
| `src/config.rs` | Campos opcionales de embedding |
| `.env.example` | Documentar vars (si existe; si no, documentar en AGENTS.md) |

### 3.2 Comportamiento requerido

`OllamaEmbeddings`:

1. `POST {base_url}/api/embeddings` con body:

```json
{ "model": "<embedding_model>", "prompt": "<text>" }
```

2. Parsear `embedding: number[]` → `Vec<f32>`.
3. Batch: el trait recibe `&[String]`; puedes hacer N requests secuenciales (como ahora) o, si Ollama soporta batch en tu versión, usarlo. **Secuencial está OK** para Fase 4.
4. Errores HTTP claros: `"embeddings HTTP {status}: {body}"`.
5. `dimensions()`:  
   - Preferible: valor configurado (default razonable, p.ej. `768` o el del modelo).  
   - Opcional avanzado: probe al primer embed y cachear len (si lo haces, documenta race/first-call).

### 3.3 Config

Extiende `Config` (nombres sugeridos):

```rust
pub struct Config {
    pub port: u16,
    pub ollama_url: String,
    pub default_model: String,
    /// Embedding model for RAG (Ollama). Empty → disable real embeddings / use defaults.
    pub embedding_model: String,      // env: EMBEDDING_MODEL, default "nomic-embed-text" o similar
    pub embedding_dimensions: usize,  // env: EMBEDDING_DIMENSIONS, default 768
    pub rag_enabled: bool,            // env: RAG_ENABLED, default true si feature on
}
```

**Defaults seguros:**

- Si `EMBEDDING_MODEL` vacío → puedes defaultar a un modelo común de Ollama **o** dejar embeddings deshabilitados y usar solo in-memory+hash en tests.
- No fallar el boot del servidor solo porque el modelo de embed no está pullado; loguear warn y permitir que queries fallen con error claro.

### 3.4 Tests

En `embeddings.rs` o store tests:

- Mock HTTP **no es obligatorio** si es costoso.
- Mantén `HashEmbeddingProvider` para unit tests sin red.
- Opcional: test `#[ignore]` `ollama_embeddings_smoke` que llama Ollama real.

### 3.5 Verificación 4.2

```bash
cargo check
cargo test --target-dir target-tests hash_embedding -- --nocapture 2>/dev/null || true
```

### 3.6 Checklist 4.2

- [ ] `OllamaEmbeddings` maneja errores sin panic
- [ ] Config/env documentados
- [ ] `HashEmbeddingProvider` intacto para tests
- [ ] Dimensiones consistentes entre embed de docs y queries

---

## 4. Tarea 4.3 — Paths, feature Cargo, spike LanceDB

**Objetivo:** dependencia opcional + directorio de datos + compilar un open/create mínimo.  
**Estimación:** ~1 día  
**Prioridad:** P1

### 4.1 Feature en `Cargo.toml`

```toml
[features]
default = ["rag"]
# Si ya existe rig-engine de Fase 3, combínalos, por ejemplo:
# default = ["rig-engine", "rag"]
rag = ["dep:lancedb"]
# Si lancedb tira muchas deps de arrow, está bien que sea optional.

[dependencies]
# ... existentes ...
lancedb = { version = "0.16", optional = true }  # verificar versión actual en crates.io
# Puede requerir arrow-array / arrow-schema transitivos; agrega solo si el compile lo pide.
```

**Notas:**

1. Consulta [crates.io/crates/lancedb](https://crates.io/crates/lancedb) y elige la última estable compatible con el edition 2021 del proyecto.
2. Si `lancedb` es demasiado pesado o falla en el entorno, **plan B documentado**:
   - Feature `rag` habilita un `FileRagStore` (JSON/bincode por collection) con la **misma semántica de scope**.
   - Deja `LanceRagStore` como módulo `#[cfg(feature = "rag-lancedb")]` o TODO claro.
   - **Preferencia del plan:** intentar LanceDB primero; File store solo si bloquea >1 día de pelea de deps.
3. No pongas `lancedb` como dependencia no-opcional si rompe builds mínimos.

### 4.2 Paths en `src/core/paths.rs`

Agregar:

```rust
/// Runtime data root: `{base}/data`
pub fn get_data_dir() -> PathBuf {
    get_base_dir().join("data")
}

/// LanceDB root: `{base}/data/lancedb`
pub fn get_lancedb_dir() -> PathBuf {
    get_data_dir().join("lancedb")
}

/// Ensure data dirs exist (agents/contexts + data/lancedb)
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(get_agents_dir())?;
    std::fs::create_dir_all(get_contexts_dir())?;
    std::fs::create_dir_all(get_lancedb_dir())?;
    Ok(())
}
```

Actualiza el `ensure_dirs` **existente** (no dupliques la función).

### 4.3 `.gitignore`

Agregar:

```gitignore
/data
```

(o al menos `/data/lancedb`)

### 4.4 Módulo nuevo

```text
src/adapters/rag/
  mod.rs          # editar exports
  embeddings.rs   # existente
  store.rs        # InMemory — existente
  lancedb.rs      # NUEVO — detrás de cfg feature
  namespace.rs     # NUEVO — parse/encode source_id (puede ir en 4.4)
  chunker.rs      # NUEVO — en 4.6
```

`mod.rs`:

```rust
pub mod embeddings;
pub mod store;
pub mod namespace;

#[cfg(feature = "rag")]
pub mod lancedb;

pub use embeddings::OllamaEmbeddings;
pub use store::{HashEmbeddingProvider, InMemoryRagStore};

#[cfg(feature = "rag")]
pub use lancedb::LanceRagStore;
```

### 4.5 Spike mínimo en `lancedb.rs`

Solo compilar:

- `connect(path).execute().await`
- crear tabla vacía o abrir si existe
- un insert de 1 vector fake + query nearest

Puede vivir en `#[cfg(test)]` o función `LanceRagStore::smoke_open`.

### 4.6 Verificación 4.3

```bash
cargo check
cargo check --features rag
cargo check --no-default-features
```

### 4.7 Checklist 4.3

- [ ] Feature `rag` + dep opcional
- [ ] `get_lancedb_dir` + `ensure_dirs` crea el path
- [ ] `/data` en `.gitignore`
- [ ] Spike compila (aunque la lógica completa esté en 4.5)

---

## 5. Tarea 4.4 — Namespaces y encoding de `source_id`

**Objetivo:** convención única de ids y paths seguros en disco.  
**Estimación:** ~0.5 día  
**Prioridad:** P1

### 5.1 Archivo

Crear: `src/adapters/rag/namespace.rs`  
(o `src/domain/rag_source.rs` si prefieres dominio puro — **recomendado domain si es lógica pura sin I/O**)

**Recomendación:** poner parse/validate en `src/domain/` (puro) y encode path-safe en adapter.

### 5.2 Formas de `source_id` soportadas

| Forma | Ejemplo | Significado |
|-------|---------|-------------|
| `project:{project_id}/{collection}` | `project:fudi/policies` | knowledge del proyecto |
| `agent:{agent_id}/memory` | `agent:nutricion/memory` | memoria global del agente |
| `agent:{project}/{agent}/memory` | `agent:fudi/ops/memory` | memoria de agente en proyecto (alineado a `own_memory_source_id`) |
| `global/{collection}` | `global/docs` | corpus global |
| corta `app/col` | `fudi/policies` | legacy/simple — **permitida** como id opaco |

**No inventes más formas** en Fase 4. Trata ids desconocidos como **strings opacos** válidos mientras pasen validación básica.

### 5.3 Validación básica

Rechazar:

- string vacío
- `..` path traversal
- null bytes
- caracteres de control

Función sugerida:

```rust
pub fn validate_source_id(source_id: &str) -> Result<(), String> {
    if source_id.is_empty() {
        return Err("empty source_id".into());
    }
    if source_id.contains("..") || source_id.contains('\0') {
        return Err("invalid source_id".into());
    }
    // opcional: max len 256
    Ok(())
}
```

### 5.4 Encode path-safe (para LanceDB dir)

```rust
/// "fudi/policies" → "fudi__policies"
/// "agent:fudi/ops/memory" → "agent_fudi__ops__memory"
pub fn encode_source_id_for_path(source_id: &str) -> String {
    source_id.replace(':', "_").replace('/', "__")
}
```

Decode **no es obligatorio** si siempre guardas `source_id` original dentro de cada row.

### 5.5 Alineación con `AgentScope::own_memory_source_id`

Ya existe:

```rust
// project Some("fudi"), agent "ops" → "agent:fudi/ops/memory"
// project None, agent "ops" → "agent:ops/memory"
```

Los tests de write policy **deben** usar esos ids. No inventes otro formato de memoria.

### 5.6 Checklist 4.4

- [ ] `validate_source_id` + `encode_source_id_for_path`
- [ ] Tests unitarios de encode (casos de la tabla)
- [ ] Documentado en comentario de módulo o AGENTS.md

---

## 6. Tarea 4.5 — `LanceRagStore` + enforce scope

**Objetivo:** implementación persistente de `RagStore` con aislamiento.  
**Estimación:** ~2 días  
**Prioridad:** P1  
**Crítico.**

### 6.1 Archivo

`src/adapters/rag/lancedb.rs`

### 6.2 Estructura sugerida

```rust
pub struct LanceRagStore {
    base_dir: PathBuf,
    embeddings: Arc<dyn EmbeddingProvider>,
    /// Optional cache of open tables; keep simple first (open per call OK for MVP).
}

impl LanceRagStore {
    pub fn new(base_dir: impl Into<PathBuf>, embeddings: Arc<dyn EmbeddingProvider>) -> Self { ... }

    fn collection_path(&self, source_id: &str) -> PathBuf {
        self.base_dir.join(encode_source_id_for_path(source_id))
    }
}
```

### 6.3 Schema de tabla por collection

Una tabla (o DB) por `source_id`. Columnas mínimas:

| Columna | Tipo | Notas |
|---------|------|-------|
| `id` | Utf8 | id del chunk (estable) |
| `text` | Utf8 | contenido |
| `vector` | FixedSizeList\<Float32, DIM\> | embedding |
| `metadata` | Utf8 (JSON) | serializar `serde_json::Value` |

**DIM** = `embeddings.dimensions()`. Todas las rows de una collection deben usar la misma DIM. Si cambia el modelo/dim, documenta que hay que `delete_collection` + re-ingest.

### 6.4 `query_scoped` — algoritmo obligatorio

```text
1. Si scope.rag_sources vacío o top_k == 0 → Ok([])
2. query_vec = embeddings.embed([query])[0]
3. hits = []
4. Para cada source_id en scope.rag_sources:
     a. Si !scope.allows_rag_read(source_id) → Err(denied)  // o skip; preferible Err para misconfig
     b. Si collection no existe en disco → continue (0 docs)
     c. vector search top_k (o top_k * N y merge global)
     d. map rows → RagChunk { id, source_id, text, score, metadata }
5. Ordenar hits por score descendente (cosine/dot según métrica)
6. truncate(top_k)
7. Ok(hits)
```

**Métrica:** preferir **cosine** si LanceDB lo soporta; si solo L2, documentarlo y ser consistente con embeddings normalizados (HashEmbedding ya normaliza; Ollama puede no — aceptable).

### 6.5 `upsert_scoped` — algoritmo obligatorio

```text
1. Si docs vacío → Ok(0)
2. validate cada source_id
3. Para cada doc (o batch por source_id):
     a. Si !scope.allows_rag_write(&doc.source_id) → Err(denied)
4. texts = docs.texts; vectors = embeddings.embed(texts)
5. Si len mismatch → Err
6. Agrupar por source_id; open/create table; merge by id (upsert)
7. Ok(written_count)
```

**Upsert semántica:** mismo `id` en la misma collection **reemplaza** texto+vector+metadata (como `InMemoryRagStore`).

### 6.6 `delete_collection`

- Borrar directorio de la collection o drop table.
- No requiere scope en el trait actual (admin/internal). Si llamas desde API, valida permisos en la capa API.

### 6.7 Errores

- Siempre `Result<_, String>` o mapear a `AppError::Runtime` en la API.
- Nunca `unwrap` en paths de producción.
- Si LanceDB falla al abrir, mensaje: `"lancedb open '{source_id}': ..."`.

### 6.8 Paridad con InMemory

Los tests de aislamiento que ya existen en `store.rs` deben poder **reutilizarse** con un helper genérico:

```rust
async fn assert_isolation(store: Arc<dyn RagStore>) { ... }
```

Corre el mismo test contra `InMemoryRagStore` y, con feature, contra `LanceRagStore` en tempdir.

### 6.9 Implementación si LanceDB API es hostil

Orden de degradación aceptable:

1. **LanceDB full** (preferido)
2. **LanceDB sin índice ANN** (scan / nearest simple) — OK para volúmenes chicos
3. **FileRagStore** (JSONL + cosine en memoria al query) con misma interface — último recurso, documentar en decisiones

No dejes la fase a medias sin **alguna** persistencia.

### 6.10 Checklist 4.5

- [ ] `LanceRagStore` implementa `RagStore`
- [ ] Read/write enforcement idéntico en espíritu a InMemory
- [ ] Persistencia sobrevive reinicio de proceso (test manual o integración)
- [ ] `delete_collection` funciona
- [ ] cfg feature correcta; sin feature el crate compila

---

## 7. Tarea 4.6 — Ingest pipeline

**Objetivo:** convertir texto/archivos en chunks indexados.  
**Estimación:** ~1 día  
**Prioridad:** P1

### 7.1 Archivos nuevos

```text
src/adapters/rag/chunker.rs     # o src/services/rag_ingest.rs
src/services/rag_ingest.rs      # orquestación
```

Preferible:

- **chunker** puro en adapter o `services` sin I/O de red
- **RagIngestService** en `services` que usa `Arc<dyn RagStore>`

### 7.2 Chunking (MVP simple)

No uses un NLP sofisticado. MVP:

```rust
pub struct ChunkConfig {
    pub max_chars: usize,      // default 1200
    pub overlap_chars: usize,  // default 150
}

pub struct TextChunk {
    pub id: String,
    pub text: String,
    pub index: usize,
}

pub fn chunk_text(source_key: &str, text: &str, cfg: &ChunkConfig) -> Vec<TextChunk> {
    // split by max_chars with overlap
    // id = format!("{source_key}#chunk-{index}") or hash estable
}
```

Reglas:

- Normaliza newlines.
- Omite chunks vacíos/whitespace.
- Ids **estables** para el mismo texto+índice (re-ingest actualiza en lugar de duplicar sin control).

### 7.3 Fuentes de ingest

| Fuente | Origen | `source_id` destino típico |
|--------|--------|----------------------------|
| Analyze output | `ProjectContext.context_md` | `project:{project_id}/context` o el listado en el request |
| `context_files` del agente | paths relativos a `LLAMA_R_DIR` / base | collection elegida en request o convención `project:{id}/files` |
| Texto libre (API admin) | body | el `source_id` del request (si write policy lo permite) |

### 7.4 Servicio

```rust
// src/services/rag_ingest.rs
pub struct RagIngestService {
    pub store: Arc<dyn RagStore>,
}

pub struct IngestRequest {
    pub scope: AgentScope,          // o construir desde agent_id
    pub source_id: String,
    pub documents: Vec<IngestDocument>, // { id_hint?, text, metadata }
}

impl RagIngestService {
    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestResult, AppError> {
        // validate source_id
        // chunk each document
        // map to RagUpsert
        // store.upsert_scoped(&scope, upserts)
    }

    pub async fn ingest_project_context(
        &self,
        scope: &AgentScope,
        project_id: &str,
        context_md: &str,
        source_id: &str, // e.g. project:{id}/context
    ) -> Result<IngestResult, AppError> { ... }

    pub async fn ingest_files(
        &self,
        scope: &AgentScope,
        source_id: &str,
        paths: &[PathBuf],
    ) -> Result<IngestResult, AppError> {
        // read utf-8 text files only; skip binary
        // size limit per file e.g. 512 KiB
    }
}

pub struct IngestResult {
    pub chunks_written: usize,
    pub files_read: usize,
    pub source_id: String,
}
```

### 7.5 Hook opcional en analyze

Archivo: donde se guarda el context tras analyze (`context` API / analyzer).

**Mínimo aceptable:** no auto-hook; solo API admin.  
**Mejor:** tras `save_context` exitoso, si RAG está enabled y hay un scope/admin policy, indexar `context_md` en `project:{id}/context`.

Si auto-hook:

- No debe fallar el analyze si RAG falla — log `warn` y continúa.
- No bloquees el request HTTP más de lo razonable; si es lento, `tokio::spawn` fire-and-forget con tracing.

### 7.6 Checklist 4.6

- [ ] Chunker con tests (3–4 asserts de tamaño/overlap/ids)
- [ ] `RagIngestService` usa solo el port
- [ ] Lectura de archivos con límites y skip binarios
- [ ] Upsert respeta scope (test con write denied)

---

## 8. Tarea 4.7 — API admin debug

**Objetivo:** endpoints HTTP para ingest y query sin pasar por el chat.  
**Estimación:** ~1 día  
**Prioridad:** P2 (pero muy útiles; implementa tras 4.5–4.6)

### 8.1 Archivo nuevo

`src/api/rag_api.rs`

Registrar en `src/api/mod.rs` y rutas en `src/runtime.rs` → `build_router`.

### 8.2 Endpoints

```text
POST /api/rag/ingest
POST /api/rag/query
```

Opcional (nice-to-have, no bloqueante):

```text
DELETE /api/rag/collections/:source_id
GET    /api/rag/health
```

### 8.3 Auth / gating (simple)

Como es debug/admin en un gateway personal:

1. **Opción A (recomendada):** header `X-Debug: true` requerido (ya existe patrón en chat).
2. **Opción B:** env `RAG_ADMIN_ENABLED=true` (default false en prod mental model; default true en dev OK si lo documentas).
3. **No** implementes OAuth.

Si el gate falla → `403` o `404` (elige uno y documenta; preferible `403` con mensaje claro).

### 8.4 DTOs

```rust
#[derive(Deserialize)]
pub struct RagIngestBody {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    /// Target collection; must be allowed by agent scope write policy
    pub source_id: String,
    /// Raw texts to ingest
    pub texts: Vec<String>,
    /// Optional absolute/relative file paths (server-local) — cuidado: path allowlist
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Serialize)]
pub struct RagIngestResponse {
    pub chunks_written: usize,
    pub source_id: String,
}

#[derive(Deserialize)]
pub struct RagQueryBody {
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

#[derive(Serialize)]
pub struct RagQueryResponse {
    pub hits: Vec<RagHitDto>,
}

#[derive(Serialize)]
pub struct RagHitDto {
    pub id: String,
    pub source_id: String,
    pub text: String,
    pub score: f32,
}
```

### 8.5 Resolución de scope en la API

```text
1. Resolver RegisteredAgent vía agent_registry.resolve(project_id, agent_id)
2. Si no hay agente → 404
3. Usar registered.scope para query/upsert
4. Nunca aceptar un scope “forjado” desde el client sin pasar por el registry
```

### 8.6 Seguridad de `files`

Si aceptas paths de archivo:

- Solo paths bajo `get_base_dir()` **o** un allowlist.
- Rechaza `..` y symlinks fuera del base si es fácil de chequear.
- Si es demasiado riesgoso, **no implementes `files` en la API** y limita a `texts` + hook de analyze.

### 8.7 AppState

Agregar:

```rust
pub rag_store: Option<Arc<dyn RagStore>>,
pub rag_ingest: Option<Arc<RagIngestService>>,
```

Actualiza **todos** los constructores de `AppState` (grep `AppState {`).

### 8.8 Router

```rust
.route("/api/rag/ingest", post(rag_ingest))
.route("/api/rag/query", post(rag_query))
```

### 8.9 Checklist 4.7

- [ ] Endpoints montados
- [ ] Gate debug/admin
- [ ] Scope desde registry, no del client libre
- [ ] Errores de write/read denied → 400/403 con mensaje
- [ ] utoipa opcional (si el proyecto documenta otros endpoints, añade paths; si no, skip)

---

## 9. Tarea 4.8 — Wiring en runtime + uso en chat/engine

**Objetivo:** el servidor real construye el store y lo inyecta.  
**Estimación:** ~0.5–1 día  
**Prioridad:** P1

### 9.1 En `build_runtime` (`src/runtime.rs`)

Pseudocódigo:

```rust
let embeddings: Arc<dyn EmbeddingProvider> = Arc::new(OllamaEmbeddings::new(
    config.ollama_url.clone(),
    config.embedding_model.clone(),
    config.embedding_dimensions,
));

let rag_store: Option<Arc<dyn RagStore>> = if config.rag_enabled {
    #[cfg(feature = "rag")]
    {
        let dir = crate::core::paths::get_lancedb_dir();
        Some(Arc::new(LanceRagStore::new(dir, embeddings.clone())) as Arc<dyn RagStore>)
    }
    #[cfg(not(feature = "rag"))]
    {
        // Fallback: in-memory (no persistence) with warn log
        tracing::warn!("RAG feature disabled at compile time; using InMemoryRagStore");
        Some(Arc::new(InMemoryRagStore::new(embeddings.clone())) as Arc<dyn RagStore>)
    }
} else {
    None
};

// Si Fase 3 ya creó RigAgentEngine / AgentRuntime:
//   engine.rag = rag_store.clone()
//   runtime.rag = rag_store.clone()
// Si no, al menos deja rag_store en AppState para la API admin.
```

### 9.2 Comportamiento chat sin rag_sources

Si el agente tiene `rag_sources = []` (default):

- `query_scoped` no se llama o devuelve `[]`
- chat funciona igual que sin RAG

Esto ya es el default seguro del manifiesto.

### 9.3 Ejemplo de agente para pruebas manuales

Puedes documentar (no commitear secretos) un TOML de ejemplo:

```toml
name = "RAG Demo"
model = "llama3.2"
system_prompt = "Usa el conocimiento recuperado. Si no hay contexto, dilo."
rag_sources = ["project:demo/context", "agent:demo/memory"]
rag_write = "own_memory_only"
```

Colócalo en `examples/agents/` si no existe algo similar.

### 9.4 Checklist 4.8

- [ ] `build_runtime` construye embeddings + store
- [ ] `AppState` expone store/ingest
- [ ] Engine/runtime reciben `Some(rag)` cuando enabled
- [ ] Boot no crashea si Ollama embeddings no está listo

---

## 10. Tarea 4.9 — Tests de aislamiento y regresión

**Objetivo:** demostrar el criterio de salida de la fase.  
**Estimación:** ~1 día  
**Prioridad:** P1

### 10.1 Tests ya existentes (no borrar)

En `src/adapters/rag/store.rs`:

- `isolation_prevents_cross_agent_reads`
- `write_denied_outside_policy`

### 10.2 Tests nuevos obligatorios

| Test | Qué prueba |
|------|------------|
| `namespace_encode_roundtrip_safe` | encode path no contiene `/` ni `:` |
| `validate_source_id_rejects_traversal` | `../etc` falla |
| `chunk_text_respects_max_and_overlap` | chunker |
| `listed_write_policy_allows_rag_sources_only` | `RagWritePolicy::Listed` |
| `own_memory_write_matches_scope_helper` | ids de `own_memory_source_id` |
| `query_does_not_return_other_collection` | dos sources, scope solo una |
| `ingest_then_query_returns_chunk` | InMemory end-to-end con HashEmbedding |
| `lance_isolation_tempdir` (feature `rag`, puede `#[ignore]` si CI sin deps nativas) | misma isolation en disco |

### 10.3 Archivo de tests de integración opcional

`tests/rag_isolation_tests.rs` (mencionado en ARCHITECTURE_PLAN):

```rust
// Usa InMemory + HashEmbedding; no requiere Ollama ni LanceDB
```

### 10.4 Comandos

```bash
cargo test --target-dir target-tests
cargo test --target-dir target-tests --features rag
cargo test --target-dir target-tests --no-default-features
```

### 10.5 Checklist 4.9

- [ ] ≥ 6 tests nuevos o ampliados pasando sin Ollama
- [ ] Isolation cross-agent verde
- [ ] Write denied verde
- [ ] Tests existentes de scope en `domain/scope.rs` siguen verdes

---

## 11. Tarea 4.10 — Documentación y cierre

### 11.1 `ROADMAP.md`

Actualizar fila Fase 4:

```text
| 4 | RAG segmentado (LanceDB) | P1 | ✅ completado |
```

o `🔄 parcial` si usaste FileRagStore en lugar de LanceDB (explica en notas).

### 11.2 `AGENTS.md`

Agregar sección:

```markdown
## RAG

- Feature: `rag` (LanceDB). Build without: `cargo build --no-default-features` / sin feature rag
- Data path: `{LLAMA_R_DIR}/data/lancedb/` (gitignored)
- Env:
  - `EMBEDDING_MODEL` (default ...)
  - `EMBEDDING_DIMENSIONS` (default ...)
  - `RAG_ENABLED` (default true)
- Agent TOML: `rag_sources`, `rag_write`
- Debug API (requires X-Debug: true):
  - `POST /api/rag/ingest`
  - `POST /api/rag/query`
- Isolation: agents only read listed rag_sources; writes follow rag_write policy
```

### 11.3 No reescribir `ARCHITECTURE_PLAN.md`

Solo corrige inconsistencias graves.

### 11.4 Verificación final

```bash
cargo fmt
cargo check
cargo check --features rag
cargo check --no-default-features
cargo test --target-dir target-tests
```

### 11.5 Prueba manual (si Ollama + modelo embed disponibles)

```bash
# 1. pull embedding model (ejemplo)
ollama pull nomic-embed-text

# 2. run server
cargo run

# 3. ingest (ajusta agent/project a uno real)
curl -s http://127.0.0.1:3000/api/rag/ingest \
  -H 'Content-Type: application/json' \
  -H 'X-Debug: true' \
  -d '{
    "project_id": "demo",
    "agent_id": "demo",
    "source_id": "agent:demo/demo/memory",
    "texts": ["La política de cancelación permite reembolsos en 24h."]
  }'

# 4. query
curl -s http://127.0.0.1:3000/api/rag/query \
  -H 'Content-Type: application/json' \
  -H 'X-Debug: true' \
  -d '{
    "project_id": "demo",
    "agent_id": "demo",
    "query": "reembolsos",
    "top_k": 3
  }'
```

Ajusta `source_id` para que coincida con `own_memory_source_id` del agente y su `rag_write`.

---

## 12. Guía archivo por archivo (mapa de edición)

| Archivo | Crear/Editar | Qué hacer |
|---------|--------------|-----------|
| `Cargo.toml` | Editar | feature `rag`, dep opcional `lancedb` |
| `.gitignore` | Editar | `/data` |
| `src/ports/rag.rs` | Revisar | solo cambios mínimos |
| `src/adapters/rag/mod.rs` | Editar | exports + cfg |
| `src/adapters/rag/embeddings.rs` | Editar | robustez + config |
| `src/adapters/rag/store.rs` | Mantener | InMemory + tests |
| `src/adapters/rag/lancedb.rs` | Crear | `LanceRagStore` |
| `src/adapters/rag/namespace.rs` o domain | Crear | validate/encode |
| `src/adapters/rag/chunker.rs` | Crear | chunking |
| `src/services/rag_ingest.rs` | Crear | pipeline |
| `src/services/mod.rs` | Editar | `pub mod rag_ingest` |
| `src/api/rag_api.rs` | Crear | ingest/query handlers |
| `src/api/mod.rs` | Editar | `pub mod rag_api` |
| `src/api/handlers.rs` | Editar | campos rag en `AppState` |
| `src/runtime.rs` | Editar | DI + rutas |
| `src/core/paths.rs` | Editar | data/lancedb + ensure_dirs |
| `src/config.rs` | Editar | embedding/rag env |
| `src/domain/scope.rs` | Solo si bug | políticas ya existen |
| `src/adapters/rig_engine/mod.rs` | Wiring rag Option | no reescribir prepare |
| `src/services/agent_runtime.rs` | Pasar rag Some | si aplica |
| `tests/rag_isolation_tests.rs` | Crear opcional | isolation e2e |
| `examples/agents/*.toml` | Opcional | ejemplo con rag_sources |
| `ROADMAP.md` | Editar | estado fase 4 |
| `AGENTS.md` | Editar | docs RAG |
| `ARCHITECTURE_PLAN.md` | Evitar | — |

---

## 13. Pseudocódigo de `LanceRagStore::query_scoped`

```rust
async fn query_scoped(
    &self,
    scope: &AgentScope,
    query: &str,
    top_k: usize,
) -> Result<Vec<RagChunk>, String> {
    if scope.rag_sources.is_empty() || top_k == 0 {
        return Ok(vec![]);
    }

    let q = self
        .embeddings
        .embed(&[query.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "empty embedding for query".to_string())?;

    let mut hits = Vec::new();

    for source_id in &scope.rag_sources {
        if !scope.allows_rag_read(source_id) {
            return Err(format!(
                "RAG read denied for source '{source_id}' on agent '{}'",
                scope.agent_id
            ));
        }

        let path = self.collection_path(source_id);
        if !path.exists() {
            continue;
        }

        // open lance table at path
        // nearest_to(q).limit(top_k).execute()
        // for each row → RagChunk { source_id: source_id.clone(), score, ... }
        // hits.extend(...)
    }

    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top_k);
    Ok(hits)
}
```

Adapta la API exacta de `lancedb` a la versión elegida.

---

## 14. Errores comunes y cómo evitarlos

| Error | Causa | Solución |
|-------|-------|----------|
| Cross-agent leak | Query sin filtrar por `rag_sources` | Siempre iterar **solo** sources del scope |
| Write a collection ajena | No chequear `allows_rag_write` | Deny antes de embed (ahorra CPU) y antes de write |
| Dim mismatch | Modelo embed cambió | Documentar re-ingest; validar len del vector al insert |
| Path traversal | `source_id` con `..` | `validate_source_id` + encode |
| Boot falla sin Ollama embed | Hard fail al construir store | Soft: warn + errores en query |
| Tests flaky con red | Ollama en unit tests | Usar `HashEmbeddingProvider` |
| AppState no compila | Faltó campo en tests | `rg "AppState \{" -n` y actualizar todos |
| Arrow/lancedb compile hell | Versiones incompatibles | Pin versions; plan B FileRagStore |
| Duplicar chunks en re-ingest | ids no estables | ids `source#chunk-i` o hash de texto |
| Metadata gigante | Meten archivos enteros | Limitar tamaño de metadata JSON |
| Analyze roto por RAG | Hook síncrono que falla | warn + no fallar analyze |

---

## 15. Qué está FUERA de alcance (no implementar)

- Summarizer de conversaciones → index (Fase 5).
- SQLite history (Fase 5).
- Hybrid search BM25 + vector (nice-to-have futuro).
- UI TUI de browser RAG.
- Multi-tenant cloud remote LanceDB.
- Rate limiting (Fase 6).
- Cambiar el protocolo MCP.
- Reimplementar Rig/Fase 3.
- Embeddings locales con candle/ORT (Ollama basta).
- ACL finas por rol de usuario humano (solo scope de agente).

---

## 16. Checklist maestro (marcar al terminar)

### Ports y embeddings
- [ ] 4.1 Ports confirmados
- [ ] 4.2 OllamaEmbeddings + config

### Persistencia y namespaces
- [ ] 4.3 Feature + paths + spike
- [ ] 4.4 Namespace validate/encode
- [ ] 4.5 LanceRagStore (o File fallback documentado) + scope enforce

### Ingest y API
- [ ] 4.6 Chunker + RagIngestService
- [ ] 4.7 `/api/rag/ingest` + `/api/rag/query`
- [ ] 4.8 Wiring runtime/AppState/engine

### Calidad
- [ ] 4.9 Tests aislamiento + chunker + policies
- [ ] 4.10 Docs + gitignore + ROADMAP/AGENTS
- [ ] `cargo fmt && cargo check && cargo test --target-dir target-tests`

### DoD global
- [ ] D1–D14 de la sección 0.3 cumplidos

---

## 17. Orden de commits sugerido (si el usuario pide commits)

1. `feat(rag): confirm ports and harden Ollama embeddings config`
2. `feat(rag): add data paths, gitignore, and optional lancedb feature`
3. `feat(rag): implement source_id namespace helpers`
4. `feat(rag): add LanceRagStore with scoped query/upsert`
5. `feat(rag): add chunker and ingest service`
6. `feat(api): add debug RAG ingest/query endpoints`
7. `feat(runtime): wire RAG store into AppState and agent runtime`
8. `test(rag): isolation, policies, and ingest round-trip`
9. `docs: mark phase 4 status in ROADMAP and AGENTS`

Cada commit debe compilar.

---

## 18. Decisiones abiertas (rellenar al terminar)

| Decisión | Elección | Notas |
|----------|----------|-------|
| Versión exacta de `lancedb` | n/a | No se pinneó; stack arrow/datafusion demasiado pesado |
| ¿LanceDB o FileRagStore fallback? | **FileRagStore** | Misma semántica de scope; path `data/lancedb/` reutilizable |
| Modelo embed default | `nomic-embed-text` | Via `EMBEDDING_MODEL` |
| Dimensiones default | `768` | Via `EMBEDDING_DIMENSIONS` |
| Métrica distancia (cosine/L2) | **cosine** | InMemory + FileRagStore |
| Auto-ingest en analyze | no | Solo API admin en Fase 4 |
| Gate API (`X-Debug` vs env) | **`X-Debug: true`** | Header requerido |
| Feature name (`rag` vs `rag-lancedb`) | **`rag`** | Sin dep `lancedb`; feature vacía que habilita wiring |

---

## 19. Referencias internas

- Plan general: `ARCHITECTURE_PLAN.md` (Fase 4, §6.7)
- Plan Fase 3: `PHASE_3_RIG_ENGINE_PLAN.md`
- Roadmap: `ROADMAP.md`
- Workflows: `AGENTS.md`
- Port: `src/ports/rag.rs`
- InMemory + tests: `src/adapters/rag/store.rs`
- Embeddings: `src/adapters/rag/embeddings.rs`
- Scope policies: `src/domain/scope.rs`
- Manifiesto: `src/domain/agent.rs` (`rag_sources`, `rag_write`)
- Engine retrieve hook: `src/adapters/rig_engine/mod.rs` (`prepare`)
- Runtime hook: `src/services/agent_runtime.rs`
- Context analyze: `src/context/`

---

## 20. Mensaje de arranque para el agente implementador

Copia y pega esto al iniciar la sesión de implementación:

```text
Implementa la Fase 4 de llama-r siguiendo estrictamente PHASE_4_RAG_PLAN.md
en la raíz del repo. Trabaja en orden 4.1 → 4.10. No implementes Fase 5–7.
No rompas el port RagStore/EmbeddingProvider ni InMemoryRagStore.
Mantén enforce de rag_sources/rag_write en todo query/upsert.
Tras cada subtarea corre cargo check.
Al final: cargo fmt, cargo check (con y sin feature rag), cargo test --target-dir target-tests.
Criterio de salida: dos agentes con rag_sources distintos no recuperan chunks cruzados.
Si lancedb bloquea por dependencias, documenta fallback FileRagStore y sigue el port.
```

---

## 21. Dependencia con otras fases

```text
Fase 0–2  → scope + MCP (ya listos)
Fase 3    → engine puede inyectar RAG en prompt (prepare)
Fase 4    → ESTE PLAN (store persistente + ingest + API)
Fase 5    → usará upsert_scoped para indexar resúmenes de conversación
```

**No bloquees Fase 4 esperando Fase 3 completa:** el store y la API admin aportan valor solos; el wiring al engine puede ser `Option` y `None` si el engine aún no está listo.

---

*Fin del plan de ejecución — Fase 4 RAG centralizado y segmentado.*
