use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::keymap::KeyBindings;

#[derive(Debug, Clone)]
pub struct Config {
    pub token: Option<String>,
    pub database_path: PathBuf,
    pub keybindings: KeyBindings,
    pub visual_line_navigation: bool,
    pub config_path: PathBuf,
}

impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let token = match environment_token()? {
            Some(token) => Some(token),
            None => token_from_file(&root.join("API_TOKEN"))?,
        };
        let database_path = std::env::var_os("TODOIST_TUI_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| root.to_path_buf())
                    .join("todoist-tui")
                    .join("todoist-tui.db")
            });
        let explicit_config = std::env::var_os("TODOIST_TUI_CONFIG_PATH").map(PathBuf::from);
        let config_path = explicit_config.clone().unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| root.to_path_buf())
                .join("todoist-tui")
                .join("config.toml")
        });
        let (keybindings, visual_line_navigation) =
            KeyBindings::load_with_editor_options(&config_path, explicit_config.is_some())?;
        Ok(Self {
            token,
            database_path,
            keybindings,
            visual_line_navigation,
            config_path,
        })
    }
}

fn environment_token() -> Result<Option<String>> {
    let Some(value) = std::env::var_os("TODOIST_API_TOKEN") else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("TODOIST_API_TOKEN is not valid UTF-8"))?;
    Ok(non_empty_trimmed(&value))
}

fn token_from_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(non_empty_trimmed(&value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read API token from {}", path.display())),
    }
}

fn non_empty_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_tokens_and_rejects_empty_values() {
        assert_eq!(non_empty_trimmed("  token\n"), Some("token".into()));
        assert_eq!(non_empty_trimmed(" \n\t"), None);
    }

    #[test]
    fn missing_token_file_is_local_only_and_present_token_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("API_TOKEN");
        assert_eq!(token_from_file(&path).unwrap(), None);

        fs::write(&path, "  secret-token\n").unwrap();
        assert_eq!(token_from_file(&path).unwrap(), Some("secret-token".into()));
    }
}
