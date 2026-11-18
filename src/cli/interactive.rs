use crate::config::{Config, ProviderKind};
use crate::runtime::build_provider_stack;
use crate::services::provider_stack::ProviderStack;
use dialoguer::{theme::ColorfulTheme, Input, Password, Select};
use std::error::Error;

/// Provider choices offered during first-run setup. Kept as a pure function so
/// the list can be pinned by a test without a terminal or an `AppState`.
fn provider_choices() -> &'static [&'static str] {
    &["Ollama", "OpenRouter", "OpenCode"]
}

/// Prompt for the credentials and origin specific to the chosen provider.
///
/// An empty key keeps the value already in `.env` (or no key at all for
/// OpenCode, whose password is optional). Called once before the first probe
/// and again whenever the user picks "Edit" in the health-check loop, so the
/// two paths can never drift apart.
fn prompt_provider_settings(config: &mut Config) -> Result<(), Box<dyn Error + Send + Sync>> {
    let theme = ColorfulTheme::default();
    match config.provider {
        ProviderKind::Ollama => {
            config.ollama_url = Input::with_theme(&theme)
                .with_prompt("Ollama URL")
                .default(config.ollama_url.clone())
                .interact_text()?;
        }
        ProviderKind::OpenRouter => {
            let key = Password::with_theme(&theme)
                .with_prompt("OpenRouter API key (leave empty to keep the value from .env)")
                .allow_empty_password(true)
                .interact()?;
            if !key.is_empty() {
                config.openrouter_api_key = Some(key);
            }
        }
        ProviderKind::OpenCode => {
            let key = Password::with_theme(&theme)
                .with_prompt("OpenCode server password (leave empty to keep the value from .env)")
                .allow_empty_password(true)
                .interact()?;
            if !key.is_empty() {
                config.opencode_api_key = Some(key);
            }
            config.opencode_url = Input::with_theme(&theme)
                .with_prompt("OpenCode URL")
                .default(config.opencode_url.clone())
                .interact_text()?;
        }
        // First-run setup only offers the three kinds above; the newly
        // opened kinds get their own prompts once their adapters are wired.
        ProviderKind::OpenAiCompatible | ProviderKind::Anthropic | ProviderKind::Gemini => {}
    }
    Ok(())
}

pub async fn run_interactive_setup(
    config: &mut Config,
) -> Result<(ProviderStack, String), Box<dyn Error + Send + Sync>> {
    println!("Welcome to Llama-R Interactive Setup");

    // 1. Select Provider. The list is the only source of truth for what setup
    // offers; the mapping below follows the same order as `provider_choices`.
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose an LLM Provider")
        .items(provider_choices())
        .default(0)
        .interact()?;

    config.provider = match selection {
        0 => ProviderKind::Ollama,
        1 => ProviderKind::OpenRouter,
        _ => ProviderKind::OpenCode,
    };

    // 1.1 Credentials / origin for the chosen provider.
    prompt_provider_settings(config)?;

    // 2. Check Health (with retry/edit loop). The stack is rebuilt from the
    // current values on every iteration so an "Edit" takes effect immediately.
    loop {
        let stack = build_provider_stack(config)?;
        println!("Checking {} provider health... ", config.provider);
        match stack.provider.health_check().await {
            Ok(_) => {
                println!("✅ OK");
                break;
            }
            Err(err) => {
                println!("❌ FAILED");
                println!("Error: {}", err);

                let choices = &["Retry", "Edit settings", "Exit"];
                let retry = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Health check failed. What would you like to do?")
                    .items(choices)
                    .default(0)
                    .interact()?;

                match retry {
                    0 => continue,
                    1 => {
                        prompt_provider_settings(config)?;
                        continue;
                    }
                    _ => return Err("Setup cancelled by user.".into()),
                }
            }
        }
    }

    // 3. Select Model. Ollama and OpenRouter expose a model list; OpenCode is a
    // one-shot text server with no discovery contract the gateway can rely on,
    // so its model id is taken as free text.
    let stack = build_provider_stack(config)?;
    let selected_model = if config.provider == ProviderKind::OpenCode {
        Input::with_theme(&ColorfulTheme::default())
            .with_prompt("OpenCode model id")
            .default(config.default_model.clone())
            .interact_text()?
    } else {
        println!("Fetching available models...");
        let models = stack.provider.list_models().await?;
        if models.is_empty() {
            return Err(format!(
                "No models found in the selected provider ({}).",
                config.provider
            )
            .into());
        }
        let model_names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
        let model_selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select a model to use")
            .items(&model_names)
            .default(0)
            .interact()?;
        model_names[model_selection].clone()
    };
    println!("Selected model: {}", selected_model);

    // Rebuild with the chosen model so the provider's own fallback model and
    // the stack's `default_model` both match what the user picked. The probe
    // stack above used the pre-existing (possibly empty) model, which is fine
    // for a health check but wrong for the stack the app will actually run on.
    config.default_model = selected_model.clone();
    let stack = build_provider_stack(config)?;

    Ok((stack, selected_model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_choices_offers_all_three_providers() {
        assert_eq!(provider_choices(), &["Ollama", "OpenRouter", "OpenCode"]);
    }
}
