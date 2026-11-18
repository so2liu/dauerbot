//! Project-scoped native CLI model preferences and read-only discovery.
//!
//! A configured selection is never evidence of the model used by a live turn.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Fence a selection made in an asynchronously rendered desktop or Feishu card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub agent: String,
    pub generation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub provider: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub agent: String,
    pub options: Vec<ModelOption>,
    pub configured: Option<Selection>,
    pub native_default: Option<Selection>,
    pub effective: Option<Selection>,
    pub effective_source: String,
    pub warnings: Vec<String>,
}

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_model_preferences (
      project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
      agent TEXT NOT NULL, selection TEXT NOT NULL, updated_at INTEGER NOT NULL,
      PRIMARY KEY(project_id,agent));",
    )?;
    Ok(())
}

/// Explicit IDs are accepted even when a CLI cannot enumerate its catalogue.
/// Shell syntax is never evaluated: values become individual argv elements.
pub fn validate(agent: &str, selection: &Selection) -> Result<Selection> {
    ensure!(
        ["codex", "claude", "pi", "agy"].contains(&agent),
        "Model selection is unavailable for this CLI"
    );
    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 256
            && !s.starts_with('-')
            && s.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "._-/+:[]".contains(ch))
    };
    let mut value = selection.clone();
    value.model = value.model.trim().to_owned();
    value.provider = value
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    ensure!(
        valid(&value.model),
        "Provide a model ID without spaces or control characters"
    );
    if let Some(provider) = value.provider.as_deref() {
        ensure!(
            matches!(agent, "pi" | "codex"),
            "This CLI uses its native provider settings; select a model without a provider override"
        );
        ensure!(
            valid(provider) && !provider.contains('/'),
            "Provide a valid provider ID"
        );
        if agent == "pi" {
            // Strip a matching provider prefix; remaining slashes may be part
            // of a routing group's nested model ID.
            if let Some(model) = value.model.strip_prefix(&format!("{provider}/")) {
                value.model = model.to_owned();
            }
            ensure!(!value.model.is_empty(), "Provide a model ID");
        }
    } else if agent == "pi"
        && let Some((provider, model)) = value.model.split_once('/')
    {
        ensure!(!model.is_empty(), "Provide a model ID after the provider");
        let (provider, model) = (provider.to_owned(), model.to_owned());
        value.provider = Some(provider);
        value.model = model;
    }
    Ok(value)
}

pub fn load(c: &Connection, project: &str, agent: &str) -> Result<Option<Selection>> {
    let value: Option<String> = c
        .query_row(
            "SELECT selection FROM project_model_preferences WHERE project_id=? AND agent=?",
            params![project, agent],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|s| serde_json::from_str(&s).context("Invalid saved model preference"))
        .transpose()
}

/// Call from the same transaction which accepts the model-change request.
/// None means return to the CLI's own default, without changing global files.
pub fn save(
    c: &Connection,
    project: &str,
    agent: &str,
    selection: Option<&Selection>,
) -> Result<()> {
    if let Some(selection) = selection {
        let selection = validate(agent, selection)?;
        c.execute("INSERT INTO project_model_preferences(project_id,agent,selection,updated_at) VALUES(?,?,?,?)
          ON CONFLICT(project_id,agent) DO UPDATE SET selection=excluded.selection,updated_at=excluded.updated_at",
          params![project,agent,serde_json::to_string(&selection)?,crate::db::now()])?;
    } else {
        c.execute(
            "DELETE FROM project_model_preferences WHERE project_id=? AND agent=?",
            params![project, agent],
        )?;
    }
    Ok(())
}

pub fn selection_args(agent: &str, selection: &Selection) -> Result<Vec<String>> {
    let selection = validate(agent, selection)?;
    let mut args = vec!["--model".into(), selection.model];
    if let Some(provider) = selection.provider {
        match agent {
            "pi" => args.extend(["--provider".into(), provider]),
            "codex" => args.extend([
                "-c".into(),
                format!("model_provider={}", serde_json::to_string(&provider)?),
            ]),
            _ => unreachable!("provider was validated"),
        }
    }
    Ok(args)
}

/// Remove only previous model arguments when an explicit project preference is
/// active. Keep every unrelated argument, including native permission settings.
pub fn extra_args(
    agent: &str,
    args: &[String],
    selection: Option<&Selection>,
) -> Result<Vec<String>> {
    let Some(selection) = selection else {
        return Ok(args.to_vec());
    };
    let selection = validate(agent, selection)?;
    remove_model_args(agent, args, selection.provider.is_some())
}

/// Removing a project override follows native CLI and saved-session settings.
/// The switch source snapshot retains the full original arguments for rollback.
pub fn clear_model_args(agent: &str, args: &[String]) -> Result<Vec<String>> {
    ensure!(
        ["codex", "claude", "pi", "agy"].contains(&agent),
        "Model selection is unavailable for this CLI"
    );
    remove_model_args(agent, args, true)
}

fn remove_model_args(agent: &str, args: &[String], remove_provider: bool) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--model"
            || agent == "agy" && arg == "-model"
            || agent == "codex" && arg == "-m"
            || agent == "pi" && arg == "--provider" && remove_provider
        {
            ensure!(
                i + 1 < args.len(),
                "Existing model argument is missing its value"
            );
            i += 2;
            continue;
        }
        if arg.starts_with("--model=")
            || agent == "agy" && arg.starts_with("-model=")
            || agent == "pi" && remove_provider && arg.starts_with("--provider=")
        {
            i += 1;
            continue;
        }
        if agent == "codex" {
            let assignment = arg
                .strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").filter(|s| !s.is_empty()));
            if assignment
                .and_then(|s| s.split_once('='))
                .is_some_and(|(key, _)| {
                    key.trim() == "model" || remove_provider && key.trim() == "model_provider"
                })
            {
                i += 1;
                continue;
            }
            if arg.starts_with("-m") && arg.len() > 2 {
                i += 1;
                continue;
            }
        }
        if agent == "codex"
            && matches!(arg.as_str(), "-c" | "--config")
            && let Some(value) = args.get(i + 1)
            && let Some((key, _)) = value.split_once('=')
            && (key.trim() == "model" || remove_provider && key.trim() == "model_provider")
        {
            i += 2;
            continue;
        }
        result.push(arg.clone());
        i += 1;
    }
    Ok(result)
}

fn json_file(path: &Path) -> Option<Value> {
    // Configuration may contain credentials. Only whitelisted model fields are
    // projected below, and no parse error includes file content.
    serde_json::from_slice(&model_file(path)?).ok()
}

fn model_file(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4_000_001)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= 4_000_000).then_some(bytes)
}

/// Read only simple top-level string assignments; unsupported TOML forms remain
/// unknown rather than being approximated as an effective native setting.
fn toml_string(path: &Path, key: &str) -> Option<String> {
    let text = String::from_utf8(model_file(path)?).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        let Some((name, rhs)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let rhs = rhs.trim();
        let mut stream = serde_json::Deserializer::from_str(rhs).into_iter::<String>();
        if let Some(Ok(value)) = stream.next() {
            return Some(value);
        }
        if let Some(rest) = rhs.strip_prefix('\'') {
            return rest.split_once('\'').map(|(s, _)| s.to_owned());
        }
    }
    None
}

fn add(catalog: &mut Catalog, id: &str, label: &str, provider: Option<&str>, source: &str) {
    let selection = Selection {
        model: id.into(),
        provider: provider.map(str::to_owned),
    };
    if let Ok(selection) = validate(&catalog.agent, &selection)
        && !catalog
            .options
            .iter()
            .any(|o| o.id == selection.model && o.provider == selection.provider)
    {
        catalog.options.push(ModelOption {
            id: selection.model,
            label: label.to_owned(),
            provider: selection.provider,
            source: source.into(),
        });
    }
}

fn native_files(agent: &str, cwd: &Path, home: &Path, configured: Option<Selection>) -> Catalog {
    let mut catalog = Catalog {
        agent: agent.into(),
        options: vec![],
        configured,
        native_default: None,
        effective: None,
        effective_source: "unverified".into(),
        warnings: vec![],
    };
    match agent {
        "codex" => {
            let dir = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            let config = dir.join("config.toml");
            let provider = toml_string(&config, "model_provider");
            if let Some(model) = toml_string(&config, "model") {
                add(
                    &mut catalog,
                    &model,
                    &model,
                    provider.as_deref(),
                    "native_config",
                );
                catalog.native_default = Some(Selection {
                    model,
                    provider: provider.clone(),
                });
            }
            let paths = if let Some(path) = toml_string(&config, "model_catalog_json") {
                let path = PathBuf::from(path);
                vec![if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                }]
            } else {
                vec![dir.join("models_cache.json")]
            };
            for path in paths {
                if let Some(value) = json_file(&path)
                    && let Some(models) = value.get("models").and_then(Value::as_array)
                {
                    for model in models {
                        if model["visibility"] == "hide" {
                            continue;
                        }
                        if let Some(id) = model["slug"].as_str() {
                            add(
                                &mut catalog,
                                id,
                                model["display_name"].as_str().unwrap_or(id),
                                provider.as_deref(),
                                "native_catalog",
                            );
                        }
                    }
                }
            }
            catalog.warnings.push("The native catalogue may be cached. CLI profiles and project configuration can override the native default; the running model is unverified.".into());
        }
        "claude" => {
            let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            for path in [
                dir.join("settings.json"),
                cwd.join(".claude/settings.json"),
                cwd.join(".claude/settings.local.json"),
            ] {
                if let Some(settings) = json_file(&path) {
                    if let Some(model) = settings["model"].as_str() {
                        add(&mut catalog, model, model, None, "native_config");
                        catalog.native_default = Some(Selection {
                            model: model.into(),
                            provider: None,
                        });
                    }
                    if let Some(env) = settings["env"].as_object() {
                        for (key, value) in env {
                            if (key == "ANTHROPIC_MODEL"
                                || key.starts_with("ANTHROPIC_DEFAULT_") && key.ends_with("_MODEL"))
                                && let Some(model) = value.as_str()
                            {
                                add(&mut catalog, model, model, None, "native_config");
                            }
                        }
                    }
                }
            }
            catalog.warnings.push("Claude Code has no supported read-only CLI model-list command. Configured native models are shown; an explicit model ID or native alias is also accepted.".into());
        }
        "pi" => {
            let dir = std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pi/agent"));
            for path in [dir.join("settings.json"), cwd.join(".pi/settings.json")] {
                if let Some(settings) = json_file(&path)
                    && let Some(model) = settings["defaultModel"].as_str()
                {
                    let provider = settings["defaultProvider"].as_str();
                    add(&mut catalog, model, model, provider, "native_config");
                    catalog.native_default = Some(Selection {
                        model: model.into(),
                        provider: provider.map(str::to_owned),
                    });
                }
            }
        }
        "agy" => {
            if let Some(settings) = json_file(&home.join(".gemini/antigravity-cli/settings.json")) {
                if let Some(model) = settings["model"].as_str() {
                    add(&mut catalog, model, model, None, "native_config");
                    catalog.native_default = Some(Selection {
                        model: model.into(),
                        provider: None,
                    });
                }
                if let Some(models) = settings["customModelsConfig"]["customModels"].as_object() {
                    for (id, model) in models {
                        add(
                            &mut catalog,
                            id,
                            model["modelName"].as_str().unwrap_or(id),
                            None,
                            "native_config",
                        );
                    }
                }
            }
            catalog.warnings.push("Provider and login remain controlled by Antigravity's native settings. Custom model IDs require an existing native custom-model configuration.".into());
        }
        _ => catalog
            .warnings
            .push("This CLI does not support managed model selection.".into()),
    }
    if let Some(selection) = catalog.configured.clone() {
        add(
            &mut catalog,
            &selection.model,
            &selection.model,
            selection.provider.as_deref(),
            "project_preference",
        );
    }
    catalog
}

/// Read native configuration off the async executor, then invoke only native
/// catalogue commands. No authentication command or model turn is executed.
pub async fn discover(
    project: &crate::db::Project,
    configured: Option<Selection>,
) -> Result<Catalog> {
    let home = directories::BaseDirs::new()
        .context("Home directory is unavailable")?
        .home_dir()
        .to_owned();
    let agent = project.agent.clone();
    let cwd = PathBuf::from(&project.path);
    let (read_agent, read_cwd) = (agent.clone(), cwd.clone());
    let mut catalog = tokio::task::spawn_blocking(move || {
        native_files(&read_agent, &read_cwd, &home, configured)
    })
    .await?;
    let args: &[&str] = match agent.as_str() {
        "pi" => &[
            "pi",
            "--offline",
            "--no-extensions",
            "--no-mcp",
            "--list-models",
        ],
        "agy" => &["agy", "models"],
        _ => &[],
    };
    if !args.is_empty() {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
        let mut command = tokio::process::Command::new(shell);
        command
            .args(["-lic", "exec \"$@\"", "dauerbot-model-list"])
            .args(args)
            .current_dir(&cwd)
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(12), command.output()).await {
            Ok(Ok(output)) if output.status.success() && output.stdout.len() < 4_000_000 => {
                let text = String::from_utf8_lossy(&output.stdout);
                if agent == "pi" { parse_pi(&mut catalog, &text); }
                else {
                    let before = catalog.options.len();
                    parse_agy(&mut catalog, &text);
                    if catalog.options.len() == before {
                        catalog.warnings.push("Antigravity's model-list format could not be read. Native configured models are shown; explicit model IDs are accepted.".into());
                    }
                }
            }
            _ => catalog.warnings.push(format!("{} model discovery could not complete. Check its native login or supply an explicit model ID.", agent)),
        }
    }
    if catalog.options.is_empty() {
        catalog.warnings.push("No native model catalogue is available. Enter an explicit model ID, or keep the CLI default.".into());
    }
    Ok(catalog)
}

fn parse_pi(catalog: &mut Catalog, text: &str) {
    let mut table = false;
    for line in text.lines() {
        let columns: Vec<_> = line.split_whitespace().collect();
        if columns.starts_with(&["provider", "model"]) {
            table = true;
            continue;
        }
        if table && columns.len() >= 4 {
            add(
                catalog,
                columns[1],
                columns[1],
                Some(columns[0]),
                "native_cli",
            );
        }
    }
}

fn parse_agy(catalog: &mut Catalog, text: &str) {
    // Prefer structured output when exposed by a native version. A changed
    // human table format must not turn headings or errors into model choices.
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let models = value.as_array().or_else(|| value["models"].as_array());
        if let Some(models) = models {
            for model in models {
                if let Some(id) = model["id"]
                    .as_str()
                    .or(model["modelId"].as_str())
                    .or(model.as_str())
                {
                    add(
                        catalog,
                        id,
                        model["displayName"].as_str().unwrap_or(id),
                        None,
                        "native_cli",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn explicit_native_default_clears_only_model_and_provider_overrides() {
        let codex = args(&[
            "--model",
            "old",
            "-mcompact",
            "-c",
            "model=\"older\"",
            "--config=model_provider=\"local\"",
            "--config",
            "model_reasoning_effort=\"high\"",
            "--profile",
            "work",
            "--add-dir",
            "/project/other",
        ]);
        assert_eq!(
            clear_model_args("codex", &codex).unwrap(),
            args(&[
                "--config",
                "model_reasoning_effort=\"high\"",
                "--profile",
                "work",
                "--add-dir",
                "/project/other"
            ])
        );
        assert_eq!(
            clear_model_args(
                "pi",
                &args(&["--provider=local", "--model", "old", "--thinking", "high"])
            )
            .unwrap(),
            args(&["--thinking", "high"])
        );
        assert_eq!(
            clear_model_args("agy", &args(&["-model=old", "--settings", "native.json"])).unwrap(),
            args(&["--settings", "native.json"])
        );
        assert_eq!(
            clear_model_args(
                "claude",
                &args(&["--model=old", "--append-system-prompt", "keep me"])
            )
            .unwrap(),
            args(&["--append-system-prompt", "keep me"])
        );
        assert!(clear_model_args("codex", &args(&["--model"])).is_err());
    }

    #[test]
    fn new_model_keeps_existing_provider_unless_the_user_selects_another() {
        let original = args(&[
            "-c",
            "model_provider=\"existing\"",
            "-mold",
            "--add-dir",
            "/other",
        ]);
        let selection = Selection {
            model: "new-model".into(),
            provider: None,
        };
        assert_eq!(
            extra_args("codex", &original, Some(&selection)).unwrap(),
            args(&["-c", "model_provider=\"existing\"", "--add-dir", "/other"])
        );
        let selected = Selection {
            provider: Some("selected".into()),
            ..selection
        };
        assert_eq!(
            extra_args("codex", &original, Some(&selected)).unwrap(),
            args(&["--add-dir", "/other"])
        );
        assert_eq!(extra_args("codex", &original, None).unwrap(), original);
    }
}
