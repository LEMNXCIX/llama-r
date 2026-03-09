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

        Ok(Self {
            port,
            ollama_url,
            default_model,
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
