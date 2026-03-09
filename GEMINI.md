# Llama-R: High-Performance Personal AI Agent Gateway

## Application Context
Llama-R es un gateway AI ligero y ultrarrápido escrito en Rust. Su propósito principal es actuar como proxy unificado y personal para Ollama (con soporte futuro para OpenAI, Anthropic, vLLM, llama.cpp, etc.). Está diseñado exclusivamente para uso personal y proyectos de desarrolladores que quieran múltiples “agentes” especializados (resúmenes, nutrición, emails profesionales, código Rust, etc.) con su propio system prompt y contexto pre-cargado.

### Core Objectives
1. **Low Latency extremo:** Minimizar overhead mediante streaming byte-a-byte, connection pooling y zero-copy (Rust 2026 standards).
2. **Sistema de Agentes Personalizados:** Cada agente se define en un archivo `.toml` independiente dentro de `agents/` con su propio system prompt y contextos.
3. **Hot-reload total:** Recarga instantánea de configuraciones y archivos de contexto sin reiniciar el servidor (usando `notify`).
4. **TUI Nativa:** Interfaz terminal interactiva (`ratatui` + `crossterm`) para gestión de agentes, monitoreo de logs y estado del sistema.
5. **Library-First:** El núcleo reside en `llama-r-core` para ser reutilizado como librería en otros proyectos Rust.
6. **Unified Interface:** Streaming compatible con OpenAI, gRPC (`tonic`) y Model Context Protocol (MCP).
7. **Multi-Provider:** Abstracción total de proveedores mediante el trait `LLMProvider`.

## Architectural Patterns (Rust 2026)
- **Domain-Driven Design:** Estructura clara: `domain/`, `agents/`, `providers/`, `api/`, `config/`, `tui/`.
- **Zero-copy & Eficiencia:** Uso intensivo de `bytes::Bytes`, `Cow<str>` y `smallvec` en el hot path.
- **Error Handling:** `thiserror` para errores internos, `anyhow` para nivel de aplicación y `tracing` para logs estructurados.
- **Concurrency:** `tokio` para async total, `mpsc` para comunicación entre el servidor y la TUI.

## Model Validation Strategy
- **Caché en Memoria:** Carga de modelos al inicio + refresh periódico.
- **Validación Instantánea:** El hot path nunca consulta al proveedor; valida contra la caché y devuelve 404/Invalid Model de inmediato si no existe.

## Development Workflow
- **Hot-reload:** Cualquier cambio en `agents/*.toml` o `contextos/` se refleja al instante.
- **TUI/Server:** `cargo run` inicia tanto el servidor como la interfaz terminal en paralelo.
- **Testing:** Meta de 90%+ coverage con `wiremock` para simular proveedores.
