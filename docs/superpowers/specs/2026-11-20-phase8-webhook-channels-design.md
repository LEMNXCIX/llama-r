# Fase 8 — Webhooks y canales por agente

**Fecha:** 2026-11-20
**Estado:** aprobado en brainstorming (diseño de 3 secciones), pendiente de revisión del spec
**Alcance:** endpoint de webhook firmado que enruta a un agente configurado, con modo sync/async
por canal y extracción del mensaje mediante una plantilla mínima de marcadores `{{campo.ruta}}` sobre el JSON.

## Contexto

El roadmap global incluía Fase 8 como condicional ("solo si aparece un caso de uso real"). El
usuario eligió **webhooks/channels por agente** como caso de uso concreto. El mapeo de seams
confirmó que casi todo el plumbing ya existe: rutas axum con `Arc<AppState>`
(`src/runtime.rs:332-389`), `AgentSelection` compartido por HTTP y gRPC
(`src/api/route_resolution.rs:14-25`, `src/api/grpc.rs:65-77`), `execute_chat` con solo
`ChatRequest` + selection (`src/api/chat_core.rs:818-824`), `execution_id` durable observable en
`GET /api/executions/:id` (`src/api/execution_api.rs:131-173`), el patrón `auth_env` que guarda
**nombres** de variables y nunca secretos (`src/adapters/mcp/registry.rs:35-54`). El repo **no**
trae un motor de plantillas: las menciones a "Liquid" en el código son Liquid AI (el modelo d1 de
decisiones), no un lenguaje de plantillas. Por eso se implementa un renderizador mínimo de
marcadores `{{campo.ruta}}` sobre `serde_json::Value` (ya presente), sin añadir dependencias.

Lo genuinamente nuevo: el endpoint, el mapeo payload→mensaje, la verificación HMAC, la config de
canal y la semántica sync/async. No hay hoy ningún concepto de "channel" ni "webhook" como
feature (solo canales internos `mpsc`).

Decisiones del brainstorming:

- **Respuesta:** ambos modos, elegidos por canal (`sync` / `async`).
- **Autenticación:** HMAC-SHA256 del body crudo con secreto por `auth_env`, comparado en tiempo
  constante; rechazo `401` antes de parsear.
- **Payload:** plantilla opcional de marcadores `{{campo.ruta}}` sobre el JSON; sin plantilla, el body crudo es el mensaje.
- **Enfoque A:** `channels.toml` + un endpoint genérico, sin cola nueva.

## Goal

Que un webhook firmado llegue al agente configurado con su mensaje extraído, respondiendo en el
mismo request (`sync`) o con un `execution_id` observable después (`async`) — sin secretos en
logs/errores y sin poder escalar el scope del agente.

## Non-goals

- Cola durable de entregas, reintentos o deduplicación de eventos entrantes.
- Firmas específicas por proveedor más allá de HMAC-SHA256 (`sha256=<hex>` y `<hex>`).
- Auth de usuario en el endpoint: la firma **es** la autenticación.
- Cambios en la TUI o en endpoints existentes.
- Compaction/routing/historial (Fases 5 y anteriores, ya entregadas).

## Decisiones de diseño

1. **`ChannelConfig`** (`src/domain/channel.rs`): `id`, `agent_id`, `project_id: Option<String>`,
   `mode: ChannelMode { Sync | Async }` (default `Async`), `template: Option<String>` (marcadores `{{campo.ruta}}`),
   `signature_header: String`, `auth_env: String` (nombre de la variable, nunca el secreto),
   `enabled: bool` (default `true`). `channels.toml` en la base dir: ausente = sin canales;
   malformado → warning con ruta y sin canales; boot continúa. Se relee en cada request (archivo
   pequeño, mismo trade-off restart-free de `models.toml`), así que editar un canal no requiere
   reinicio.
2. **Puerto `ChannelSignatureVerifier`** (`src/ports/channel.rs`): `fn verify(&self, body: &[u8],
   header: &str, secret: &str) -> bool`. Adapter HMAC-SHA256 que acepta `sha256=<hex>` y `<hex>`,
   compara en tiempo constante, y devuelve `false` si falta header/secreto o el formato es
   inválido. Puerto nuevo y aditivo; ningún puerto sagrado cambia.
3. **Endpoint** `POST /api/webhooks/:channel` (`src/api/webhook_api.rs`): lee el body **crudo**
   antes de parsear JSON, resuelve el canal (deshabilitado/inexistente → `404`), verifica HMAC →
   `401` antes de cualquier parsing, renderiza los marcadores `{{campo.ruta}}` sobre el JSON (o usa el body
   crudo como mensaje) y construye `AgentSelection { project_id, agent_id, conversation_id: None,
   debug: false }` desde la config.
4. **Modo `sync`:** llama a `execute_chat` y devuelve `200` con `ChatResponse`, o `502` si el
   proveedor falla, `400` si payload/plantilla/agente es inválido. El emisor ve la respuesta en la
   misma petición.
5. **Modo `async` (default):** crea el turno con `begin_turn_execution` para obtener el
   `execution_id` **antes** de responder, lanza `execute_chat` en `tokio::spawn` con un
   `Arc<AppState>` clonado, y devuelve `202 Accepted { execution_id }`. El estado se consulta en
   `GET /api/executions/:id` (ya existente). Sin cola nueva: el patrón de spawn ya lo usan purge,
   hot-reload y los forwarders.
6. **Errores sin secretos:** firma inválida/ausente → `401` genérico; JSON inválido o plantilla que
   falla → `400` nombrando el canal pero sin eco del body; agente no encontrado → `400` (misma
   validación estricta que `/api/chat`); proveedor caído en sync → `502`; en async el error queda en
   el `AgentExecution` (`Failed`) y el `202` ya fue entregado. Ningún error ni log incluye la firma,
   el secreto ni el body crudo.
7. **Aislamiento:** `debug: false` siempre — un canal nunca expone prompt ni routing. `project_id`
   y `agent_id` salen **solo** de la config: un canal no puede activar `X-Debug` ni seleccionar un
   agente de otro proyecto. El turno hereda el scope del agente configurado igual que una request
   HTTP con esos headers.

## Arquitectura

- `src/domain/channel.rs`, `src/adapters/channel/` (config loader + verifier HMAC),
  `src/ports/channel.rs`, `src/api/webhook_api.rs`.
- `src/runtime.rs`: registrar la ruta en el mismo `Router` con `Arc<AppState>`; sin middleware
  nuevo — la firma se verifica dentro del handler.
- Reutilizar `execute_chat`, `begin_turn_execution`, `AgentSelection` y el store de ejecución sin
  cambios de firma.

## Flujo de datos

1. `POST /api/webhooks/:channel` con body crudo y cabecera de firma.
2. Resolver canal desde `channels.toml` → `404` si no existe o está deshabilitado.
3. Verificar HMAC sobre los bytes exactos → `401` si falla (antes de parsear).
4. Parsear JSON, sustituir marcadores `{{campo.ruta}}` (o usar el body como mensaje).
5. Construir `AgentSelection` desde la config + `ChatRequest` con un único mensaje user.
6. Sync: `execute_chat` → `200` con la respuesta. Async: `begin_turn_execution` → spawn → `202`
   con `execution_id`.

## Manejo de errores

- Toda falla es `AppError` con status apropiado (`400`/`401`/`404`/`502`), mensaje genérico en
  `401`, sin secretos nunca.
- `channels.toml` malformado → warning con la ruta, tabla vacía, boot continúa.
- Un error en la plantilla no revela el body; se nombra solo el canal.
- En async, un fallo del turno se registra en el `AgentExecution`; el `202` ya fue entregado.

## Testing

`tests/webhook_tests.rs` (nuevo; router axum real, body crudo, fake provider):

- `hmac_valid_signature_reaches_agent`; `invalid_or_missing_signature_is_401_before_parsing`
  (body modificado con header válido se rechaza; formatos `sha256=` y hex).
- `marker_template_extracts_message` (p. ej. `{{sender.login}}`); `absent_template_uses_raw_body` (payload tipo GitHub); `unknown_marker_fails_without_echoing_body`.
- `sync_mode_returns_agent_answer`; `async_mode_returns_202_with_execution_id` + el turno aparece
  en `GET /api/executions/:id` con `Completed`/`Failed`.
- `channel_cannot_force_debug_or_cross_project_agent`; `unknown_or_disabled_channel_is_404`;
  `malformed_channels_toml_is_warned_and_empty`; `secret_never_appears_in_error_or_log`.

Gates: fmt / clippy `-D warnings` / audit / suite completa (910+) / build `--no-default-features`.

## Riesgos

- **Firma falsificable por canonicalización:** mitigado calculando HMAC sobre los bytes **crudos**
  recibidos, nunca sobre un JSON re-serializado.
- **Secreto filtrado en logs/errores:** mitigado con mensajes genéricos en `401` y sin eco del body
  ni de la firma; el patrón `auth_env` guarda solo nombres.
- **Escalada de scope:** mitigado construyendo `AgentSelection` solo desde config, con `debug:false`
  fijo y sin headers del emisor.
- **Async sin cola:** un `spawn` no sobrevive a un crash del proceso; se acepta explícitamente
  (sin reintentos de entrega) y el `execution_id` permite observar el resultado mientras el
  proceso siga vivo. Fase 8 no pide durabilidad de entrega.
- **Relectura de config por request:** archivo pequeño; mismo trade-off restart-free que
  `models.toml`, documentado.

## Exit criteria

- Un webhook con firma válida llega al agente configurado con el mensaje extraído; sync devuelve la
  respuesta y async devuelve `202` con un `execution_id` observable.
- Firma inválida → `401` antes de parsear; ningún secreto aparece en logs/errores; un canal no puede
  forzar debug ni salirse de su agente/proyecto.
- Puerto nuevo aditivo, ningún puerto sagrado cambiado, sin nueva dependencia; gates verdes.
