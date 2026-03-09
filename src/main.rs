use llama_r::cli::commands::handle_cli;
use llama_r::runtime::{build_runtime, start_grpc_server, start_http_server};
use llama_r::tui::app::TuiApp;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

struct TuiLogLayer {
    logs: Arc<Mutex<VecDeque<String>>>,
}

impl<S> tracing_subscriber::Layer<S> for TuiLogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Ok(mut logs) = self.logs.lock() else {
            eprintln!("Failed to acquire TUI log buffer; dropping log event");
            return;
        };
        let mut msg = String::new();
        let mut visitor = LogVisitor { msg: &mut msg };
        event.record(&mut visitor);

        let level = *event.metadata().level();
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        let entry = format!("[{}] {} {}", time, level, msg);

        logs.push_back(entry);
        if logs.len() > 100 {
            logs.pop_front();
        }
    }
}

struct LogVisitor<'a> {
    msg: &'a mut String,
}

impl<'a> tracing::field::Visit for LogVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.msg.push_str(&format!("{:?}", value));
        }
    }
}

#[tokio::main]
async fn main() {
    if handle_cli().await {
        return;
    }

    let logs_buffer = Arc::new(Mutex::new(VecDeque::new()));

    let file_appender = tracing_appender::rolling::daily("logs", "llama-r.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let tui_layer = TuiLogLayer {
        logs: logs_buffer.clone(),
    };

    tracing_subscriber::registry()
        .with(file_layer)
        .with(tui_layer)
        .init();

    tracing::info!("Llama-R starting up");
    let runtime = match build_runtime(logs_buffer.clone()).await {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "Startup failed");
            eprintln!("Startup failed: {}", err);
            return;
        }
    };

    tracing::info!(addr = %runtime.http_addr, "HTTP API listening");
    tracing::info!(addr = %runtime.grpc_addr, "gRPC server configured");

    let http_server = match start_http_server(&runtime).await {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(error = %err, "Failed to start HTTP server");
            eprintln!("Failed to start HTTP server: {}", err);
            return;
        }
    };
    let grpc_server = start_grpc_server(&runtime);

    let mut tui = TuiApp::new(runtime.state.clone());
    if let Err(err) = tui.run().await {
        tracing::error!(error = %err, "TUI exited with error");
    }

    http_server.abort();
    grpc_server.abort();
}
