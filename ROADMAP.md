# Llama-R Project Roadmap (2026 Edition)

## Phase 1: Foundation & Agent System (Current Priority)
- [x] Basic Rust service structure.
- [x] Ollama provider implementation.
- [ ] Agent Manager: One .toml per agent in `agents/`.
- [ ] Hot-reload: Watcher for `agents/` and `contextos/` using `notify`.
- [ ] Context Loader: Support for variables and folder recursion.

## Phase 2: Native TUI & Observability
- [ ] TUI Implementation: `ratatui` + `crossterm`.
- [ ] Real-time monitoring: Logs, uptime, model cache status, tokens/s.
- [ ] Interactive Agent Management: Create, edit, and restart agents from the TUI.
- [ ] Tracing: `tracing-subscriber` with JSON layers and OpenTelemetry.

## Phase 3: Performance & Multi-Protocol
- [ ] Streaming: OpenAI-compatible byte-a-byte streaming.
- [ ] Zero-copy: Optimization of buffers using `bytes` and `Cow`.
- [ ] gRPC Interface: High-performance endpoint with `tonic`.
- [ ] MCP Support: Model Context Protocol for tool integration.

## Phase 4: Multi-Provider & Advanced Logic
- [ ] Expansion: Support for OpenAI, Anthropic, vLLM, llama.cpp.
- [ ] Persistencia: SQLite for agent memory and history.
- [ ] Security: Basic rate-limiting and secret management.
- [ ] Advanced Context: Vector DB support (optional) or RAG integration.

## Phase 5: Ecosystem & Deployment
- [ ] Multi-stage Docker: Optimization for < 30MB images.
- [ ] Linux Integration: Systemd service unit.
- [ ] CI/CD: Multi-arch builds and automated security audits.
- [ ] Documentation: Public API coverage and auto-generated docs.
