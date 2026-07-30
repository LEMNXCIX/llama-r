use crate::error::AppError;
use dotenvy::dotenv;
use std::env;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub ollama_url: String,
    pub default_model: String,
    /// Embedding model for RAG (Ollama). Default: `nomic-embed-text`.
    pub embedding_model: String,
    /// Expected embedding vector size (must match model output).
    pub embedding_dimensions: usize,
    /// When false, RAG store is not built (chat works without retrieval).
    pub rag_enabled: bool,
    /// History retention period in days (default: 90).
    pub history_retention_days: u32,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        dotenv().ok();

        let raw_port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
        let port = raw_port.parse().map_err(|_| {
            AppError::Config(format!(
                "PORT must be a valid u16 number, received '{}'",
                raw_port
            ))
        })?;

        let ollama_url = env::var("OLLAMA_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string())
            .trim()
            .to_string();
        if ollama_url.is_empty() {
            return Err(AppError::Config(
                "OLLAMA_URL cannot be empty when provided".to_string(),
            ));
        }

        let default_model = env::var("DEFAULT_MODEL")
            .unwrap_or_else(|_| String::new())
            .trim()
            .to_string();

        let embedding_model = env::var("EMBEDDING_MODEL")
            .unwrap_or_else(|_| "nomic-embed-text".to_string())
            .trim()
            .to_string();
        if embedding_model.is_empty() {
            return Err(AppError::Config(
                "EMBEDDING_MODEL cannot be empty when provided".to_string(),
            ));
        }

        let raw_dims = env::var("EMBEDDING_DIMENSIONS").unwrap_or_else(|_| "768".to_string());
        let embedding_dimensions = raw_dims.parse().map_err(|_| {
            AppError::Config(format!(
                "EMBEDDING_DIMENSIONS must be a positive usize, received '{}'",
                raw_dims
            ))
        })?;
        if embedding_dimensions == 0 {
            return Err(AppError::Config(
                "EMBEDDING_DIMENSIONS must be greater than 0".to_string(),
            ));
        }

        let rag_enabled = env::var("RAG_ENABLED")
            .map(|value| {
                let lower = value.trim().to_ascii_lowercase();
                !(lower == "0" || lower == "false" || lower == "no" || lower == "off")
            })
            .unwrap_or(true);

        let history_retention_days = env::var("HISTORY_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);

        Ok(Self {
            port,
            ollama_url,
            default_model,
            embedding_model,
            embedding_dimensions,
            rag_enabled,
            history_retention_days,
        })
    }

    pub fn is_configured(&self) -> bool {
        !self.default_model.is_empty()
    }

    pub fn save_to_env(&self) -> Result<(), AppError> {
        let env_path = Path::new(".env");
        let mut content = if env_path.exists() {
            fs::read_to_string(env_path)?
        } else {
            String::new()
        };

        let updates = [
            ("PORT", self.port.to_string()),
            ("OLLAMA_URL", self.ollama_url.clone()),
            ("DEFAULT_MODEL", self.default_model.clone()),
        ];

        for (key, value) in &updates {
            let mut found = false;
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            for line in &mut lines {
                if line.starts_with(&format!("{}=", key)) {
                    *line = format!("{}={}", key, value);
                    found = true;
                    break;
                }
            }
            if found {
                content = lines.join("\n");
            } else {
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(&format!("{}={}", key, value));
            }
            env::set_var(key, value);
        }

        fs::write(env_path, &content)?;
        Ok(())
    }
}
