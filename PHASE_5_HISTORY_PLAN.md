# Fase 5 — Historial + aprendizaje continuo

> **Documento de ejecución para un agente de IA implementador**  
> Fuente: [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md) § Fase 5 + §6.8 + `MemoryConfig`  
> Audiencia: agente **menos capaz** — seguir este plan **en orden**, sin saltarse pasos, sin “mejoras creativas” fuera de alcance.  
> Ubicación: raíz del repo (`PHASE_5_HISTORY_PLAN.md`)  
> Planes hermanos: [`PHASE_3_RIG_ENGINE_PLAN.md`](./PHASE_3_RIG_ENGINE_PLAN.md), [`PHASE_4_RAG_PLAN.md`](./PHASE_4_RAG_PLAN.md)

---

## 0. Cómo usar este plan (léelo completo antes de tocar código)

### 0.1 Rol del implementador

Eres un implementador Rust en el repo **llama-r**. Tu trabajo es completar la **Fase 5: Historial + aprendizaje continuo** para que:

1. Cada turno de agente (user + assistant) se persista en SQLite cuando `memory.persist_history = true`.
2. El cliente pueda continuar una conversación con `X-Conversation-Id` (o metadata gRPC equivalente).
3. Cada N turnos (`summarize_every_n_turns`) se genere un resumen LLM y, si `index_summaries = true`, se indexe en la collection RAG de memoria del agente.
4. Exista API de list/get/export/delete de conversaciones.
5. Haya retención/purge por `retention_days` / `HISTORY_RETENTION_DAYS`.
6. Un agente **nunca** liste ni recupere conversaciones de otro agente.

### 0.2 Reglas obligatorias

1. **Trabaja solo en Fase 5.** No implementes rate limits (Fase 6), CLI ecosistema (Fase 7), ni LanceDB si aún falta (eso es Fase 4). Puedes **usar** el port `RagStore` ya existente para indexar resúmenes.
2. **No rompas el port `ConversationStore`** en `src/ports/history.rs` salvo bugs o campos mínimos necesarios. Extiende el trait si falta un método del DoD; actualiza **todas** las implementaciones y mocks.
3. **Defense in depth:** list/get/export/delete deben filtrar por `agent_qualified_id` (o devolver 404). Nunca listes “todas las conversaciones del mundo” sin filtro de agente.
4. **No pongas secretos en TOML ni en filas SQLite** (no persistir API keys, tokens, ni tool args sensibles si `observability.log_tool_args = false`).
5. **Después de cada subtarea (5.x):** `cargo check` y, cuando haya tests, `cargo test --target-dir target-tests`.
6. **No edites** `target/`, `target-tests/`, logs, ni `data/history.db`.
7. Si la API de `rusqlite` no coincide con los snippets, **adapta al crate real**, pero **mantén el port estable**.
8. Commits: solo si el usuario lo pide. No hagas `git push`.
9. Comentarios de código en inglés. Mensajes de error en inglés.
10. Si Fase 3/4 ya cablearon `AgentRuntime.history` / `rag`, **no reescribas** el engine ni el RAG store. Completa el store + wiring + API + tests.

### 0.3 Criterio de salida de toda la fase (Definition of Done)

La fase está **completa** solo si **todas** estas condiciones se cumplen:

| # | Condición |
|---|-----------|
| D1 | Feature Cargo `history` existe y activa `rusqlite` (bundled). |
| D2 | Schema SQLite con `conversations`, `messages`, `tool_events`, `summaries` (migraciones idempotentes). |
| D3 | `SqliteConversationStore` implementa `ConversationStore` (append, get, list, delete, export, purge, summarize). |
| D4 | Path de datos: `{base}/data/history.db` (helper en `paths.rs`); `data/` en `.gitignore`. |
| D5 | `AgentRuntime.chat` / `chat_stream` persisten user+assistant cuando `persist_history` y store `Some`. |
| D6 | `X-Conversation-Id` (HTTP) y metadata gRPC reanudan la misma conversación; si falta, se crea UUID. |
| D7 | Si el body no trae historial, el runtime carga últimos N mensajes del store y los pasa a Rig. |
| D8 | Summarizer corre cada `summarize_every_n_turns` **de esa conversación** (no “la última del agente”). |
| D9 | Si `index_summaries`, el resumen se hace `upsert_scoped` a `summary_collection` o `agent.memory_source_id()`, respetando `rag_write`. |
| D10 | API: `GET/DELETE /api/conversations`, get, messages, export. |
| D11 | Purge al boot + periódico usando `HISTORY_RETENTION_DAYS` (y/o `memory.retention_days` por agente). |
| D12 | Tests de round-trip + aislamiento entre agentes **sin Ollama obligatorio** (summarizer mockeable). |
| D13 | `agent_id` persistido es el id de archivo (`agent.id`), no el display `config.name`. |
| D14 | `cargo fmt`, `cargo check`, `cargo test --target-dir target-tests` pasan. |
| D15 | `AGENTS.md` y `ROADMAP.md` actualizados con estado Fase 5. |

### 0.4 Qué YA existe (no reimplementar desde cero)

> **Importante:** otro agente ya empezó Fase 5. **Verifica y completa**; no borres el adapter para reescribirlo salvo bug estructural.

| Pieza | Ruta | Estado típico |
|-------|------|----------------|
| `MemoryConfig` (`persist_history`, `summarize_every_n_turns`, `index_summaries`, `summary_collection`, `retention_days`) | `src/domain/agent.rs` | ✅ listo |
| Port `ConversationStore` + tipos export | `src/ports/history.rs` | ✅ extendido — **congelar salvo bugs** |
| `SqliteConversationStore` + schema + CRUD tests | `src/adapters/history/sqlite.rs` | 🔄 **existe — revisar bugs de la §0.7** |
| Feature `history` + `rusqlite` bundled | `Cargo.toml` | ✅ |
| Path `get_history_db_path()` | `src/core/paths.rs` | ✅ `{base}/data/history.db` |
| `data/` gitignored | `.gitignore` | ✅ |
| Wiring `build_history_store` + `AgentRuntime.history` | `src/runtime.rs` | ✅ |
| Persist + hydrate history en runtime | `src/services/agent_runtime.rs` | 🔄 **existe — falta conversation_id en summarize** |
| Header `X-Conversation-Id` | `src/api/handlers.rs`, `grpc.rs` | ✅ parseado |
| API `/api/conversations*` | `src/api/history_api.rs` | 🔄 **existe — verificar aislamiento 404** |
| Purge al boot + intervalo | `src/runtime.rs` | 🔄 **existe — verificar intervalo y docs** |
| `Config.history_retention_days` | `src/config.rs` | ✅ env `HISTORY_RETENTION_DAYS` |
| Historial in-request (body) → Rig | Fase 3: `chat_core` + `builder::to_rig_history` | ✅ no tocar salvo bugs |
| RAG `upsert_scoped` | Fase 4: `ports/rag.rs` | ✅ usar, no reimplementar |

### 0.5 Flujo objetivo (después de Fase 5)

```text
POST /api/chat  (+ X-Project / X-Agent + X-Conversation-Id opcional)
  → prepare_request (enrich system prompt)          [ya existe]
  → AgentRuntime.chat
       1. resolve agent + scope
       2. SI persist_history:
            ConversationStore.append_user(...) → conversation_id
       3. SI req.history vacío:
            ConversationStore.get_history(id, N) → prior turns (sin el user recién append)
       4. AgentEngine.run (Rig + tools + RAG retrieve)
       5. SI persist_history:
            append_assistant(...)
       6. SI index_summaries y turn_count % N == 0:
            summarize(LLM) → INSERT summaries
            SI rag Some y allows_rag_write(memory):
                 RagStore.upsert_scoped(summary chunk)
  → ChatResponse
       (ideal) header X-Conversation-Id: <id>

Más tarde, retrieve RAG del mismo agente incluye resúmenes → aprendizaje continuo.
```

**Aislamiento:**

```text
Agent A (proj/a) append → conversations.agent_qualified_id = "proj/a"
Agent B list            → 0 filas de A
GET /api/conversations/:id de A con filtro B → 404
RAG query B             → no ve chunks summary-* de A
```

### 0.6 Orden de implementación (NO reordenar)

```text
5.1 Confirmar/congelar port + tipos
  → 5.2 Paths + feature Cargo + spike SQLite (si aún no compila)
    → 5.3 Schema + migraciones + SqliteConversationStore CRUD
      → 5.4 Wiring runtime (append + hydrate + conversation_id)
        → 5.5 Header X-Conversation-Id + devolver id al cliente
          → 5.6 Summarizer (mockeable) cada N turnos de ESA conversación
            → 5.7 Indexar resumen en RAG (scope write)
              → 5.8 API list/get/messages/export/delete
                → 5.9 Purge / retención
                  → 5.10 Tests aislamiento + docs
```

### 0.7 Bugs / huecos conocidos a corregir (si el código ya está)

Revisa estos puntos **antes** de marcar DoD. Si ya están arreglados, tilda y sigue.

1. **`agent_id` en INSERT usa `agent.config.name`** (`sqlite.rs` `ensure_conversation`). Debe ser `agent.id` (filename sin `.toml`). `name` es display y puede colisionar.
2. **`maybe_summarize_and_index` elige “la conversación más reciente del agente”** (`ORDER BY updated_at DESC LIMIT 1`) en vez de recibir `conversation_id`. Pásale el id del run actual (extiende el trait **solo si hace falta**; preferible añadir argumento `conversation_id: Option<&str>` o un método nuevo con default).
3. **Summarizer pega a Ollama real** — unit tests no cubren el camino feliz sin red. Extrae `Summarizer` trait / fn inyectable (o feature test hook) y cubre con mock.
4. **Tabla `tool_events` no se escribe.** Mínimo: documentar como P2 **o** persistir nombre de tool + success **sin** args si `log_tool_args=false`. No bloquea D1–D15 si lo dejas explícito en ROADMAP como pendiente Fase 6/audit.
5. **`ChatResponse` no devuelve `conversation_id`.** El cliente no puede reanudar salvo que genere el UUID él. Añade campo opcional al DTO **o** header de respuesta `X-Conversation-Id`. Preferible **ambos** (header + campo serde skip_if none) sin romper clientes viejos.
6. **`chat_stream` puede no persistir assistant** al completar. Verifica que el texto final se `append_assistant` (hook en `Completed` o después de drenar el stream).
7. **Purge global ignora `memory.retention_days` por agente.** Aceptable en Fase 5 usar solo `HISTORY_RETENTION_DAYS` global; documentarlo.
8. **Lock `Mutex<Connection>` en async.** Está bien para Fase 5 (un proceso, WAL). No introduzcas `tokio::sync::Mutex` + `block_in_place` salvo deadlock real. No uses `spawn_blocking` en cada query a menos que midas contención.

---

## 1. Contexto de arquitectura (solo lo necesario)

### 1.1 Capas

- **ports** (`src/ports/history.rs`): contrato `ConversationStore`.
- **adapters** (`src/adapters/history/`): única capa que puede `use rusqlite::...`.
- **services** (`src/services/agent_runtime.rs`): orquesta persist + engine + summarize.
- **api** (`src/api/history_api.rs`, `chat_core.rs`): headers + REST.

**Regla:** ningún módulo fuera de `adapters/history` debe hacer `use rusqlite::...`.

### 1.2 Config de manifiesto (ya existe)

```toml
[memory]
persist_history = true
summarize_every_n_turns = 10
index_summaries = false
# summary_collection = "agent:demo/ops/memory"   # default: agent.memory_source_id()
retention_days = 90
```

Defaults en `MemoryConfig::default()`: persist on, summarize cada 10, index off, 90 días.

### 1.3 Relación con Fase 3 y 4

| Fase | Qué aporta a F5 |
|------|-----------------|
| 3 | Engine + `AgentRunRequest.history` + `conversation_id` + hydrate desde el **body** |
| 4 | `RagStore.upsert_scoped` / `query_scoped` + `own_memory_source_id()` |
| 5 | Persistencia SQLite + hydrate desde **DB** + resúmenes → RAG |

Sin store (`history` feature off o open falla): el runtime se comporta como Fase 3 (solo history del body). **No debe romper chat.**

---

## 2. Tarea 5.1 — Congelar el port

**Objetivo:** contrato estable usado por runtime + API + tests.  
**Estimación:** ~0.5 d  
**Prioridad:** P1  
**Estado base:** ✅ trait ya grande

### 2.1 Métodos requeridos

```rust
#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn append_user(...) -> Result<String, String>;      // returns conversation_id
    async fn append_assistant(...) -> Result<(), String>;
    async fn get_history(conversation_id, limit) -> Result<Vec<ChatMessage>, String>;
    async fn list_conversations(agent_qualified_id, page, page_size) -> Result<Vec<ConversationRecord>, String>;
    async fn get_conversation(id) -> Result<Option<ConversationRecord>, String>;
    async fn delete_conversation(id) -> Result<(), String>;
    async fn export_conversation(id) -> Result<ConversationExport, String>;
    async fn purge_old_conversations(retention_days) -> Result<u64, String>;
    async fn maybe_summarize_and_index(agent, scope, rag) -> Result<Option<ConversationSummary>, String>;
}
```

### 2.2 Cambio permitido (recomendado)

Si `maybe_summarize_and_index` no recibe `conversation_id`, **añádelo**:

```rust
async fn maybe_summarize_and_index(
    &self,
    conversation_id: Option<&str>,
    agent: &Agent,
    scope: &AgentScope,
    rag: Option<Arc<dyn RagStore>>,
) -> Result<Option<ConversationSummary>, String>;
```

Actualiza **todas** las implementaciones y call sites (`agent_runtime.rs`).

### 2.3 Verificación 5.1

```bash
cargo check
rg "impl ConversationStore for" -n src/
```

### 2.4 Checklist 5.1

- [ ] Trait documentado en el módulo
- [ ] Call sites de runtime compilan
- [ ] No hay segunda trait “HistoryRepo” paralela

**STOP:** no abras SQLite aún si el port no está alineado.

---

## 3. Tarea 5.2 — Feature Cargo + path

**Objetivo:** compilar con/sin historial.  
**Estimación:** ~0.5 d  
**Prioridad:** P1  
**Estado base:** ✅ suele existir

### 3.1 `Cargo.toml`

```toml
[features]
default = ["rig-engine", "rag", "history"]
history = ["dep:rusqlite"]

[dependencies]
rusqlite = { version = "0.31", features = ["bundled"], optional = true }
uuid = { version = "1", features = ["v4"] }
```

Si `uuid` ya está, no dupliques. Verifica versión estable en crates.io si 0.31 falla.

### 3.2 Paths

```rust
// src/core/paths.rs
pub fn get_history_db_path() -> PathBuf {
    get_data_dir().join("history.db")
}

// ensure_dirs() debe crear get_data_dir()
```

`.gitignore` ya tiene `/data` — no commitees `history.db`.

### 3.3 Verificación 5.2

```bash
cargo check
cargo check --no-default-features --features rig-engine
```

### 3.4 Checklist 5.2

- [ ] Feature opcional; sin ella el binario arranca
- [ ] Path documentado
- [ ] `data/` ignorado

---

## 4. Tarea 5.3 — Schema + `SqliteConversationStore`

**Objetivo:** persistencia real, migraciones `IF NOT EXISTS`.  
**Estimación:** ~1 d  
**Prioridad:** P1

### 4.1 Schema canónico

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS conversations (
    id                  TEXT PRIMARY KEY,
    agent_qualified_id  TEXT NOT NULL,
    project_id          TEXT,
    agent_id            TEXT NOT NULL,          -- Agent.id, NO config.name
    turn_count          INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,       -- unix seconds UTC
    updated_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conv_agent_updated
    ON conversations(agent_qualified_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id               TEXT PRIMARY KEY,
    conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role             TEXT NOT NULL CHECK(role IN ('user', 'assistant', 'system')),
    content          TEXT NOT NULL,
    created_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created
    ON messages(conversation_id, created_at ASC);

CREATE TABLE IF NOT EXISTS tool_events (
    id               TEXT PRIMARY KEY,
    conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id       TEXT,
    server_id        TEXT,
    tool_name        TEXT NOT NULL,
    is_error         INTEGER NOT NULL DEFAULT 0,
    preview          TEXT,                     -- recortado; nunca args secretos
    created_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS summaries (
    id                   TEXT PRIMARY KEY,
    conversation_id      TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    agent_qualified_id   TEXT NOT NULL,
    turn_range_start     INTEGER NOT NULL,
    turn_range_end       INTEGER NOT NULL,
    summary_text         TEXT NOT NULL,
    indexed_in_rag       INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL
);
```

### 4.2 Semántica de `append_user`

1. Si `conversation_id` Some y existe → úsalo.
2. Si Some y **no** existe → créalo con ese id (cliente-generated UUID está OK).
3. Si None / vacío → `Uuid::new_v4()`.
4. INSERT message role=`user`.
5. `turn_count += 1` (un “turn” = un mensaje user; assistant no incrementa, o incrementa de a 1 por par — **elige uno y documenta**).  
   **Decisión recomendada:** `turn_count` = número de mensajes **user** (como el código actual). Summarize usa ese número.
6. Return conversation_id.

### 4.3 Semántica de `append_assistant`

1. Requiere conversation existente (créala solo si hace falta para no perder el texto).
2. **No** incrementa `turn_count` (si seguiste la decisión de arriba).
3. Actualiza `updated_at`.

### 4.4 Aislamiento

`list_conversations` **siempre** filtra `WHERE agent_qualified_id = ?`.  
`get_conversation` / `export` / `delete` en la **API** deben comprobar que el record pertenece al agente del request (query `project_id`+`agent_id` o header). Si el store no filtra, la API sí.

### 4.5 Tests 5.3 (sin red)

En `sqlite.rs`:

- `crud_and_isolation` — dos agentes, list no se cruza, export, delete cascade.
- `append_none_creates_uuid` — id no vacío.
- `append_same_id_continues` — segundo user queda en la misma conv.
- `agent_id_column_is_agent_id_not_name` — `config.name = "Pretty"`, `id = "ops"` → columna `agent_id = "ops"`.
- `purge_old_conversations` — backdate `updated_at`.

### 4.6 Verificación 5.3

```bash
cargo test --target-dir target-tests sqlite_conversation -- --nocapture
```

### 4.7 Checklist 5.3

- [ ] Migraciones idempotentes (segundo `open` no falla)
- [ ] WAL + FK on
- [ ] Tests aislamiento verdes
- [ ] `agent.id` persistido

---

## 5. Tarea 5.4 — Wiring en `AgentRuntime`

**Objetivo:** cada chat de agente escribe y puede leer historial.  
**Estimación:** ~0.5–1 d  
**Prioridad:** P1  
**Estado base:** 🔄 parcialmente hecho

### 5.1 Orden en `chat` / `chat_stream`

Sigue el flujo de §0.5. Extrae un helper privado para no duplicar persist+hydrate entre `chat` y `chat_stream`.

```rust
async fn persist_user_and_hydrate(
    &self,
    req: &mut RuntimeChatRequest,
    agent: &Agent,
) -> Result<(), AppError> { /* ... */ }
```

### 5.2 Hydrate

Si `req.history` **ya** viene del body (`chat_core` lo llenó), **no lo pises** con la DB (el body es la fuente del cliente).  
Si está vacío y hay `conversation_id` + store: `get_history(id, 20)` y quita el último user (recién append).

Cap recomendado: 20 mensajes (const `HISTORY_WINDOW`).

### 5.3 `persist_history = false`

No abras writes. Hydrate desde DB tampoco (sesión efímera).

### 5.4 Stream

Tras `Completed` (o al mapear el stream en `chat_core`), `append_assistant` con el texto final. Si el stream solo emite Token+Completed, haz append en runtime **después no es posible** (el caller drena). Opciones:

- **A (recomendada):** `chat_stream` envuelve el `Receiver` y al ver `Completed` hace append (spawn + clone store).
- **B:** `chat_core::engine_events_to_chat_stream` llama un callback. Evítalo (acopla API al store).

### 5.5 Checklist 5.4

- [ ] Helper compartido chat / stream
- [ ] Body history gana sobre DB
- [ ] Stream persiste assistant
- [ ] Fallo de persist = warn, **no** 500 (chat sigue)

---

## 6. Tarea 5.5 — `X-Conversation-Id` de ida y vuelta

**Objetivo:** el cliente puede reanudar.  
**Estimación:** ~0.5 d  
**Prioridad:** P1

### 6.1 Request

Ya parseado en `handlers.rs` / `grpc.rs`:

```text
X-Conversation-Id: <uuid>
```

`chat_core` lo pasa a `RuntimeChatRequest.conversation_id`.

### 6.2 Response

1. Campo opcional en `ChatResponse`:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub conversation_id: Option<String>,
```

2. Header de respuesta `X-Conversation-Id` (axum `AppendHeaders` o middleware). Si es invasivo, **al menos el campo JSON**.

3. Stream: incluir `conversation_id` en el primer o último `ChatStreamEvent` (campo opcional) **o** solo header SSE comment. Preferible campo en el evento `done=true`.

### 6.3 Tests

- execute_chat con runtime + FakeHistoryStore: sin header → response trae id.
- segundo request con ese id → FakeHistory ve el mismo id.

Usa un `FakeHistory` en `chat_core` tests (Arc + Mutex de vecs). **No** abras SQLite en tests de API unitarios.

### 6.4 Checklist 5.5

- [ ] Request header documentado en AGENTS.md
- [ ] Cliente recibe el id
- [ ] gRPC metadata de respuesta si es barato; si no, documenta limitación

---

## 7. Tarea 5.6 — Summarizer

**Objetivo:** resumen cada N user-turns de **esa** conversación.  
**Estimación:** ~1 d  
**Prioridad:** P1

### 7.1 Cuándo corre

`index_summaries == true` **y** `turn_count > 0` **y** `turn_count % summarize_every_n_turns == 0`  
**y** no existe ya un summary con `turn_range_end = turn_count` para esa conv.

### 7.2 Input

Últimos `summarize_every_n_turns * 2` mensajes (user+assistant) de **esa** `conversation_id`.

Prompt (inglés, estable):

```text
Summarize the following conversation turns in 2-3 sentences.
Focus on decisions, facts, and context needed later.
Do not invent facts.

<role>: <content>
...
```

### 7.3 LLM

`POST {OLLAMA_URL}/api/chat` no-stream, modelo = `DEFAULT_MODEL` o `agent.config.model`.  
Timeout 30s. Error → `Err(...)`; el runtime ya ignora el error del summarize (`let _ =`).

### 7.4 Mock para tests

```rust
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, transcript: &str) -> Result<String, String>;
}
```

`SqliteConversationStore` guarda `Arc<dyn Summarizer>`. En prod: `OllamaSummarizer`. En tests: `FixedSummarizer("summary text")`.

Si no quieres trait público, `enum SummarizerKind { Http(...), Fixed(String) }` interno está OK.

### 7.5 Tests 5.6

- interval 2, 2 users → 1 summary.
- tercer user (count=3) → no segundo summary.
- cuarto user → segundo summary, rangos no solapan mal.
- **sin HTTP**.

### 7.6 Checklist 5.6

- [ ] Usa conversation_id del run
- [ ] Idempotente por `turn_range_end`
- [ ] Tests mock verdes

---

## 8. Tarea 5.7 — Indexar resumen en RAG

**Objetivo:** aprendizaje continuo visible en retrieve del mismo agente.  
**Estimación:** ~0.5 d  
**Prioridad:** P1  
**Dependencia:** Fase 4 `RagStore` (in-memory / file vale)

### 8.1 Target collection

```text
summary_collection.unwrap_or_else(|| agent.memory_source_id())
```

Ejemplos: `agent:ops/memory` o `agent:fudi/ops/memory`.

### 8.2 Upsert

```rust
RagUpsert {
    source_id: target,
    id: format!("summary-{summary_id}"),
    text: summary_text,
    metadata: json!({
        "conversation_id": conv_id,
        "turn_start": start,
        "turn_end": end,
        "kind": "conversation_summary",
    }),
}
```

Solo si `scope.allows_rag_write(&target)`. Si deniega: guarda summary en SQLite con `indexed_in_rag=0` (no falles el chat).

### 8.3 Test

`InMemoryRagStore` + `RagWritePolicy::OwnMemoryOnly` + agent matching `own_memory_source_id`.  
Tras summarize: `query_scoped` recupera el texto del summary.  
Segundo agente con otras `rag_sources`: 0 hits.

### 8.4 Checklist 5.7

- [ ] Isolation RAG
- [ ] `indexed_in_rag` refleja el upsert
- [ ] No llama RAG si `index_summaries=false`

---

## 9. Tarea 5.8 — API HTTP

**Objetivo:** inspeccionar y exportar historial.  
**Estimación:** ~0.5 d  
**Prioridad:** P2 (pero D10 es DoD — implementa)

### 9.1 Rutas (ya montadas en `runtime.rs` si el scaffold existe)

| Método | Ruta | Authz |
|--------|------|--------|
| GET | `/api/conversations?project_id=&agent_id=&page=&page_size=` | obligatorio agent o project |
| GET | `/api/conversations/:id` | 404 si no existe |
| GET | `/api/conversations/:id/messages` | 404 |
| DELETE | `/api/conversations/:id` | 404 |
| POST o GET | `/api/conversations/:id/export` | JSON completo |

Si falta filtro de pertenencia en get/delete: añade query `project_id`/`agent_id` **o** headers `X-Project`/`X-Agent` y compara `record.agent_qualified_id`.

### 9.2 Feature off

Handlers devuelven error claro: `"History feature is disabled or uninitialized"`.

### 9.3 Tests

Unitarios con FakeHistory en el router (como `api_integration` o módulo `history_api` tests). Al menos: list vacío, append vía store + list 1, delete 404 after.

### 9.4 Checklist 5.8

- [ ] OpenAPI/utoipa tags `"History"`
- [ ] Documentado en AGENTS.md
- [ ] Sin listado global sin filtro

---

## 10. Tarea 5.9 — Retención / purge

**Objetivo:** no crecer `history.db` sin límite.  
**Estimación:** ~0.5 d  
**Prioridad:** P2 (D11)

### 10.1 Política Fase 5

- Env `HISTORY_RETENTION_DAYS` (default 90) → `Config.history_retention_days`.
- `purge_old_conversations(days)` borra `WHERE updated_at < now - days*86400` (cascade messages/summaries/tool_events).
- Corre **una vez al boot** y **cada 24h** (tokio interval). Loguea `deleted=N`.

Por-agente `memory.retention_days` queda **documentado como no aplicado** (Fase 6 o follow-up). No implementes N queries por agente ahora.

### 10.2 `retention_days = 0`

Significa “no borrar” (guard). `purge` con 0 → `Ok(0)` inmediato.

### 10.3 Checklist 5.9

- [ ] Test backdate + purge
- [ ] 0 días no borra
- [ ] Boot no falla si la DB está vacía

---

## 11. Tarea 5.10 — Tests + docs + verificación final

**Objetivo:** DoD D12–D15.  
**Estimación:** ~0.5 d  
**Prioridad:** P1

### 11.1 Matriz mínima de tests

| Test | Dónde |
|------|--------|
| CRUD + isolation agentes | `adapters/history/sqlite.rs` |
| purge | idem |
| summarize mock + no HTTP | sqlite o módulo summarizer |
| RAG index isolation | sqlite + InMemoryRagStore |
| runtime persist + hydrate | `agent_runtime` con FakeStore + FakeEngine |
| header conversation_id | `chat_core` |
| API list/get/delete | `history_api` o integration |
| `--no-default-features --features rig-engine` check | CI manual |

### 11.2 Docs

**`AGENTS.md`** — sección nueva:

```markdown
## Conversation history

- Feature: `history` (default ON). Disable: `--no-default-features --features rig-engine,rag`
- DB: `{LLAMA_R_DIR}/data/history.db` (gitignored)
- Continue a thread: header `X-Conversation-Id`
- Response includes `conversation_id` (JSON) so clients can send it back
- Persist follows agent `[memory] persist_history`
- Summaries: `index_summaries = true` + RAG write policy
- Admin: GET/DELETE `/api/conversations`, export
- Retention: `HISTORY_RETENTION_DAYS` (default 90), purge on boot + daily
```

**`ROADMAP.md`:** Fase 5 ✅ o 🔄 según DoD real. Enlaza este archivo.

**`.env.example`:** `HISTORY_RETENTION_DAYS=90`

### 11.3 Verificación final

```bash
cargo fmt
cargo check
cargo check --no-default-features --features rig-engine
cargo test --target-dir target-tests
```

Si usas WSL desde Windows: `wsl -d archlinux -- bash -lc 'cd /home/leonardo/Repositories/llama-r && cargo test --target-dir target-tests'`.

### 11.4 Checklist 5.10

- [ ] D1–D15 tildados con evidencia
- [ ] ROADMAP / AGENTS alineados
- [ ] No secrets en fixtures

---

## 12. Fuera de alcance (NO implementar)

- Postgres / multi-instance sync (`ports/history.rs` lo permite después).
- UI TUI de historial completa (un indicador del `conversation_id` es opcional P3).
- Encriptación at-rest de `history.db`.
- Rate limit / audit log formal (Fase 6).
- Re-index masivo de summaries viejos.
- `tool_events` ricos con args (P2; Fase 6 audit).
- Cambiar el contrato de `AgentEngine`.

---

## 13. Archivos a tocar (mapa)

| Archivo | Acción |
|---------|--------|
| `src/ports/history.rs` | Congelar / añadir `conversation_id` a summarize |
| `src/adapters/history/mod.rs` | Exports + cfg |
| `src/adapters/history/sqlite.rs` | Store + tests + bugs §0.7 |
| `src/services/agent_runtime.rs` | Helper persist/hydrate; stream append; summarize id |
| `src/api/chat_core.rs` | conversation_id in/out |
| `src/api/handlers.rs` | header response |
| `src/api/grpc.rs` | metadata |
| `src/api/history_api.rs` | isolation 404 |
| `src/domain/models.rs` | campo opcional `conversation_id` |
| `src/runtime.rs` | wiring + purge (si falta) |
| `src/core/paths.rs` | `get_history_db_path` |
| `src/config.rs` / `.env.example` | retention |
| `Cargo.toml` | feature `history` |
| `AGENTS.md` / `ROADMAP.md` | estado |
| `tests/api_integration.rs` | opcional 1 test HTTP |

---

## 14. Decisiones (anota aquí si te desvías)

| Tema | Decisión del plan | Si cambias, escribe por qué |
|------|-------------------|-----------------------------|
| Crate SQL | `rusqlite` bundled (no sqlx async) | |
| Turn count | # de mensajes user | |
| Ventana hydrate | 20 msgs | |
| Summarizer | trait/enum mockeable | |
| ID al cliente | JSON `conversation_id` + header | |
| Purge | global env, no per-agent | |
| tool_events write | P2 opcional | |

---

## 15. Definition of Done — checklist final del implementador

Copia y tilda al terminar:

- [ ] D1 feature `history`
- [ ] D2 schema 4 tablas
- [ ] D3 store completo
- [ ] D4 path + gitignore
- [ ] D5 persist en runtime
- [ ] D6 header request
- [ ] D7 hydrate si body vacío
- [ ] D8 summarize de **esa** conv
- [ ] D9 RAG index scoped
- [ ] D10 API
- [ ] D11 purge
- [ ] D12 tests mock
- [ ] D13 `agent.id` en columna
- [ ] D14 cargo fmt/check/test
- [ ] D15 docs

Cuando **todos** estén tildados, actualiza `ROADMAP.md` Fase 5 a ✅ y para. No empieces Fase 6.
