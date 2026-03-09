use crate::error::AppError;
use crate::providers::LLMProvider;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// ModelCache ensures model validation happens in-memory with ultra-low latency.
pub struct ModelCache {
    provider: Arc<dyn LLMProvider + Send + Sync>,
    available_models: RwLock<HashSet<String>>,
    last_update: RwLock<Option<Instant>>,
}

impl ModelCache {
    pub fn new(provider: Arc<dyn LLMProvider + Send + Sync>) -> Self {
        Self {
            provider,
            available_models: RwLock::new(HashSet::new()),
            last_update: RwLock::new(None),
        }
    }

    pub async fn is_model_valid(&self, model: &str) -> bool {
        let models = self.available_models.read().await;
        models.contains(model)
    }

    pub async fn refresh(&self) -> Result<(), AppError> {
        let models = self
            .provider
            .list_models()
            .await
            .map_err(|err| AppError::Provider(err.to_string()))?;
        let mut cache = self.available_models.write().await;
        cache.clear();
        for model in models {
            cache.insert(model.name);
        }
        let mut last_update = self.last_update.write().await;
        *last_update = Some(Instant::now());
        Ok(())
    }
}

pub fn validate_identifier(value: &str, field_name: &str) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(format!(
            "{} cannot be empty",
            field_name
        )));
    }

    let valid = trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if !valid {
        return Err(AppError::Validation(format!(
            "{} must contain only ASCII letters, numbers, '.', '_' or '-'",
            field_name
        )));
    }

    Ok(())
}

pub fn canonicalize_project_path(project_path: &str) -> Result<PathBuf, AppError> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return Err(AppError::Validation(
            "project_path cannot be empty".to_string(),
        ));
    }

    let path = Path::new(trimmed);
    if !path.exists() {
        return Err(AppError::Validation(format!(
            "Project path '{}' does not exist",
            trimmed
        )));
    }
    if !path.is_dir() {
        return Err(AppError::Validation(format!(
            "Project path '{}' is not a directory",
            trimmed
        )));
    }

    path.canonicalize().map_err(AppError::from)
}
