# AI Agent Skills for Llama-R (2026 Edition)

Este documento detalla las habilidades y patrones especializados para el desarrollo en Llama-R.

## 1. Ultra-Low Latency Engineering (TTFT Focus)
**Objetivo:** Priorizar el *Time-To-First-Token* (TTFT) y el streaming real.
- **Acción:** Implementar streaming byte-a-byte en todas las capas.
- **Acción:** Usar `bytes::Bytes` y `Cow<str>` para evitar copias innecesarias en el buffer de red.
- **Acción:** Configurar adecuadamente `tokio::io::copy` y otros mecanismos de streaming directo.

## 2. Dynamic Agent & Context Management
**Objetivo:** Gestión eficiente de agentes con hot-reload.
- **Acción:** Utilizar el crate `notify` para observar cambios en `agents/` y `contextos/`.
- **Acción:** Implementar el `AgentManager` dentro de un `Arc<RwLock<...>>`.
- **Acción:** Diseñar cargadores de contexto que admitan variables (`{{edad}}`, `{{peso}}`) y carga recursiva de carpetas.

## 3. TUI & Parallel Service Design
**Objetivo:** Mantener la TUI fluida sin afectar el rendimiento del servidor.
- **Acción:** Implementar la TUI en un módulo separado (`src/tui/` o `src/cli_tui.rs`).
- **Acción:** Usar `tokio::sync::mpsc` para enviar métricas, logs y estados desde el servidor a la TUI.
- **Acción:** Utilizar `ratatui` + `crossterm` para una interfaz de terminal interactiva y estética.

## 4. Multi-Protocol & OpenAI Compatibility
**Objetivo:** Servir peticiones mediante múltiples protocolos unificados.
- **Acción:** Mantener compatibilidad total con el formato OpenAI Streaming en HTTP.
- **Acción:** Implementar cabeceras personalizadas como `X-Agent` para el ruteo dinámico de agentes.
- **Acción:** Seguir las especificaciones de gRPC (`tonic`) y MCP para la integración de herramientas.

## 5. Idiomatic Rust 2026 Patterns
**Objetivo:** Código profesional, seguro y mantenible.
- **Acción:** Usar `thiserror` para una definición exhaustiva de errores internos.
- **Acción:** Aplicar `anyhow` exclusivamente para errores de alto nivel en la ejecución.
- **Acción:** Implementar `tracing` con capas JSON y soporte para OpenTelemetry.
- **Acción:** Asegurar cobertura de tests con `wiremock` y `tokio::test`.

## 6. Persistence & Model Validation
**Objetivo:** Seguridad y eficiencia en la ruteo.
- **Acción:** Validar modelos contra la caché local cargada en el startup.
- **Acción:** Implementar persistencia opcional por agente mediante SQLite.
- **Acción:** Nunca permitir que una consulta al proveedor bloquee el hot path de validación.
