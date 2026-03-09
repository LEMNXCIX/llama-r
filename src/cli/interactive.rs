use crate::providers::ollama::OllamaProvider;
use crate::providers::LLMProvider;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use std::error::Error;
use std::sync::Arc;

pub async fn run_interactive_setup(
    mut ollama_url: String,
) -> Result<(Arc<dyn LLMProvider + Send + Sync>, String), Box<dyn Error + Send + Sync>> {
    println!("Welcome to Llama-R Interactive Setup");

    // 1. Select Provider
    let providers = &["Ollama", "Explore more in future..."];
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose an LLM Provider")
        .items(providers)
        .default(0)
        .interact()?;

    if selection != 0 {
        return Err("Only Ollama is supported currently for interactive setup.".into());
    }

    // 1.1 Confirm/Edit URL
    ollama_url = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Ollama URL")
        .default(ollama_url)
        .interact_text()?;

    let mut provider: Arc<dyn LLMProvider + Send + Sync> =
        Arc::new(OllamaProvider::new(ollama_url.clone()));

    // 2. Check Health (with retry/edit loop)
    loop {
        print!("Checking provider health at {}... ", ollama_url);
        match provider.health_check().await {
            Ok(_) => {
                println!("✅ OK");
                break;
            }
            Err(e) => {
                println!("❌ FAILED");
                println!("Error: {}", e);

                let choices = &["Retry", "Edit URL", "Exit"];
                let retry = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Health check failed. What would you like to do?")
                    .items(choices)
                    .default(0)
                    .interact()?;

                match retry {
                    0 => continue,
                    1 => {
                        ollama_url = Input::with_theme(&ColorfulTheme::default())
                            .with_prompt("Ollama URL")
                            .default(ollama_url)
                            .interact_text()?;
                        provider = Arc::new(OllamaProvider::new(ollama_url.clone()));
                        continue;
                    }
                    _ => return Err("Setup cancelled by user.".into()),
                }
            }
        }
    }

    // 3. Select Model
    println!("Fetching available models...");
    let models = provider.list_models().await?;

    if models.is_empty() {
        return Err(
            "No models found in the selected provider. Please 'ollama pull <model>' first.".into(),
        );
    }

    let model_names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
    let model_selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a model to use")
        .items(&model_names)
        .default(0)
        .interact()?;

    let selected_model = model_names[model_selection].clone();
    println!("Selected model: {}", selected_model);

    Ok((provider, selected_model))
}
