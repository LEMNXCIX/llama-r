use clap::{Parser, Subcommand};
use dotenvy::dotenv;
use serde_json;
use std::fs;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(author, version, about = "Llama-R: High-Performance Personal AI Gateway", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize the default project agent configuration using the current directory name
    Init,
    /// Initialize a new editable project agent configuration
    InitAgent { name: String },
    /// Generate AI context for a project (requires a running server)
    Analyze {
        #[arg(default_value = ".")]
        path: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "http://localhost:3000")]
        server: String,
    },
    /// Refresh an existing project context (requires a running server)
    Reanalyze {
        project_id: String,
        #[arg(long, default_value = "http://localhost:3000")]
        server: String,
    },
    /// Export project context as rules for specific AI tools
    ExportRules {
        project_id: String,
        #[arg(default_value = ".")]
        path: String,
        #[arg(long, default_value = "all")]
        format: String,
    },
    /// Run the Llama-R server and TUI
    Run,
}

pub async fn handle_cli() -> bool {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Init) => {
            init_default_agent().await;
            true
        }
        Some(Commands::InitAgent { name }) => {
            init_named_agent(name).await;
            true
        }
        Some(Commands::Analyze {
            path,
            id,
            agent,
            server,
        }) => {
            run_analyze(path, id.as_deref(), agent.as_deref(), server).await;
            true
        }
        Some(Commands::Reanalyze { project_id, server }) => {
            run_reanalyze(project_id, server).await;
            true
        }
        Some(Commands::ExportRules {
            project_id,
            path,
            format,
        }) => {
            run_export_rules(project_id, path, format).await;
            true
        }
        Some(Commands::Run) | None => false,
    }
}

fn current_project_id() -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|err| format!("Could not read current directory: {}", err))?;
    let project_id = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Could not infer project name from current directory".to_string())?
        .trim()
        .to_string();

    crate::services::validation::validate_identifier(&project_id, "project name")
        .map_err(|err| err.to_string())?;
    Ok(project_id)
}

async fn init_default_agent() {
    let project_id = match current_project_id() {
        Ok(project_id) => project_id,
        Err(err) => {
            println!("{}", err);
            return;
        }
    };

    let prompt = format!(
        "You are the general-purpose assistant for the project '{}'. Help the user clearly, accurately, and concisely.",
        project_id
    );

    init_agent_file(
        &project_id,
        &project_id,
        &prompt,
        Some(&project_id),
        &[],
    )
    .await;
}

async fn init_named_agent(name: &str) {
    if let Err(err) = crate::services::validation::validate_identifier(name, "agent name") {
        println!("{}", err);
        return;
    }

    let project_id = match current_project_id() {
        Ok(project_id) => project_id,
        Err(err) => {
            println!("{}", err);
            return;
        }
    };

    let prompt = format!(
        "You are a specialized agent named '{}' for the project '{}'.",
        name, project_id
    );
    init_agent_file(name, name, &prompt, Some(&project_id), &[]).await;
}

async fn init_agent_file(
    name: &str,
    display_name: &str,
    system_prompt: &str,
    context_project: Option<&str>,
    optimize_rules: &[&str],
) {
    dotenv().ok();
    let default_model = std::env::var("DEFAULT_MODEL").unwrap_or_default();

    let agents_dir = match context_project {
        Some(project_id) => crate::core::paths::get_project_agents_dir(project_id),
        None => crate::core::paths::get_agents_dir(),
    };
    let filename = format!("{}.toml", name);
    let path = agents_dir.join(&filename);

    if path.exists() {
        println!("Agent config '{}' already exists in {}.", filename, agents_dir.display());
        println!("You can edit it directly; the file is meant to stay editable.");
        return;
    }

    if let Err(err) = fs::create_dir_all(&agents_dir) {
        println!("Failed to create agents directory: {}", err);
        return;
    }

    let model_line = if default_model.is_empty() {
        "# model = \"\"  # Falls back to DEFAULT_MODEL from .env".to_string()
    } else {
        format!("model = \"{}\"", default_model)
    };

    let context_project_line = context_project
        .map(|project_id| format!("context_project = \"{}\"\n", project_id))
        .unwrap_or_default();

    let optimize_rules_line = if optimize_rules.is_empty() {
        "rules = []".to_string()
    } else {
        format!(
            "rules = [{}]",
            optimize_rules
                .iter()
                .map(|rule| format!("\"{}\"", rule))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let config = format!(
        r#"name = "{display_name}"
{model_line}
system_prompt = """{system_prompt}"""
{context_project_line}rules = []
skills = []

[optimize]
enabled = true
{optimize_rules_line}
"#
    );

    match fs::write(&path, config) {
        Ok(_) => {
            println!("Created editable agent config at '{}'", path.display());
            if default_model.is_empty() {
                println!("No DEFAULT_MODEL configured yet. Run `cargo run` first to set one.");
            } else {
                println!("Model: {}", default_model);
            }
            println!("Edit the TOML file whenever you want to customize it.");
        }
        Err(err) => println!("Error writing agent config: {}", err),
    }
}

async fn run_export_rules(project_id: &str, target_path: &str, format: &str) {
    let project_context_dir = crate::core::paths::get_project_context_dir(project_id);
    let context_file = project_context_dir.join("context.json");

    if !context_file.exists() {
        println!("Context for '{}' not found at {}.", project_id, context_file.display());
        println!("Hint: run `cargo run -- analyze <path> --id {}` or `cargo run -- reanalyze {}` first.", project_id, project_id);
        return;
    }

    let content = match fs::read_to_string(&context_file) {
        Ok(content) => content,
        Err(err) => {
            println!("Error reading context: {}", err);
            return;
        }
    };

    let ctx: crate::context::store::ProjectContext = match serde_json::from_str(&content) {
        Ok(context) => context,
        Err(err) => {
            println!("Error parsing context: {}", err);
            return;
        }
    };

    let target_dir = Path::new(target_path);
    if !target_dir.exists() {
        println!("Target path '{}' does not exist.", target_path);
        return;
    }

    let formats = if format == "all" {
        vec!["cursor", "gemini", "claude"]
    } else {
        vec![format]
    };

    for selected_format in formats {
        let (filename, header) = match selected_format {
            "cursor" => (".cursorrules", "## Cursor Rules"),
            "gemini" => ("GEMINI.md", "## Gemini Context"),
            "claude" => (".clauderules", "## Claude Rules"),
            _ => {
                println!("Unsupported format: {}", selected_format);
                continue;
            }
        };

        let file_path = target_dir.join(filename);
        let final_content = format!(
            "# {}\n# Generado por Llama-R para el proyecto: {}\n\n{}",
            header, ctx.project_id, ctx.context_md
        );

        match fs::write(&file_path, final_content) {
            Ok(_) => println!("Exported rules to {}", file_path.display()),
            Err(err) => println!("Failed to write {}: {}", filename, err),
        }
    }
}

async fn run_analyze(path: &str, id: Option<&str>, agent: Option<&str>, server: &str) {
    let canonical_path = match crate::services::validation::canonicalize_project_path(path) {
        Ok(path) => path,
        Err(err) => {
            println!("{}", err);
            return;
        }
    };

    let project_name = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let project_id = id.unwrap_or(project_name);

    if let Err(err) = crate::services::validation::validate_identifier(project_id, "project_id") {
        println!("{}", err);
        return;
    }

    println!("Analyzing project '{}' at '{}'...", project_id, canonical_path.display());
    println!("Server: {}", server);
    println!("This may take 30-60s while the LLM generates context and selects skills.");
    println!();

    let body = serde_json::json!({
        "project_id": project_id,
        "project_path": canonical_path.to_string_lossy().to_string(),
        "auto_analyze": true,
        "agent_id": agent,
    });

    let client = reqwest::Client::new();
    let url = format!("{}/api/contexts", server);

    match client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    if status.is_success() {
                        let project_dir = crate::core::paths::get_project_dir(project_id);
                        println!("Context generated for '{}'", project_id);
                        println!("Saved to: {}", project_dir.display());
                        if let Some(message) = json.get("message").and_then(|value| value.as_str()) {
                            println!("{}", message);
                        }
                        if let Some(agent_id) = agent {
                            println!("Context linked to agent '{}'", agent_id);
                            println!("Use it with headers X-Project: {} and X-Agent: {}", project_id, agent_id);
                        }
                    } else if status == reqwest::StatusCode::CONFLICT {
                        println!("Context for '{}' already exists.", project_id);
                        println!("Refresh it with `cargo run -- reanalyze {}`.", project_id);
                    } else {
                        println!("Server error ({}): {:?}", status, json);
                    }
                }
                Err(_) => println!("Server error ({})", status),
            }
        }
        Err(err) if err.is_connect() => {
            println!("Could not connect to server at {}", server);
            println!("Start it first with `cargo run` or `cargo run -- run`.");
        }
        Err(err) if err.is_timeout() => {
            println!("Request timed out. The LLM may be cold-starting or loading a model.");
            println!("Error: {}", err);
        }
        Err(err) => println!("Request failed: {}", err),
    }
}

async fn run_reanalyze(project_id: &str, server: &str) {
    if let Err(err) = crate::services::validation::validate_identifier(project_id, "project_id") {
        println!("{}", err);
        return;
    }

    let client = reqwest::Client::new();
    let url = format!("{}/api/contexts/{}/analyze", server, project_id);
    println!("Refreshing context '{}' via {}...", project_id, url);

    match client
        .post(&url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            println!("Context '{}' refreshed successfully.", project_id);
        }
        Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
            println!("Context '{}' was not found. Create it first with `cargo run -- analyze <path> --id {}`.", project_id, project_id);
        }
        Ok(resp) => {
            println!("Server returned {} while refreshing '{}'.", resp.status(), project_id);
        }
        Err(err) if err.is_connect() => {
            println!("Could not connect to server at {}", server);
            println!("Start it first with `cargo run` or `cargo run -- run`.");
        }
        Err(err) if err.is_timeout() => {
            println!("Reanalyze timed out. The model may still be working.");
            println!("Error: {}", err);
        }
        Err(err) => println!("Request failed: {}", err),
    }
}


