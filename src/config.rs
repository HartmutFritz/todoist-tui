use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{editor::EditorOptions, keymap::KeyBindings};

#[derive(Debug, Clone)]
pub struct Config {
    pub token: Option<String>,
    pub database_path: PathBuf,
    pub keybindings: KeyBindings,
    pub editor: EditorOptions,
    pub config_path: PathBuf,
}

impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let explicit_token = std::env::var_os("TODOIST_TUI_TOKEN_PATH").map(PathBuf::from);
        let config_dir = dirs::config_dir();
        let token = load_token(
            environment_token()?,
            explicit_token.as_deref(),
            root,
            config_dir.as_deref(),
        )?;
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
        let (keybindings, editor) =
            KeyBindings::load_with_editor_options(&config_path, explicit_config.is_some())?;
        Ok(Self {
            token,
            database_path,
            keybindings,
            editor,
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

fn load_token(
    environment: Option<String>,
    explicit_path: Option<&Path>,
    root: &Path,
    config_dir: Option<&Path>,
) -> Result<Option<String>> {
    if let Some(token) = environment {
        return Ok(Some(token));
    }
    if let Some(path) = explicit_path {
        return token_from_file(path)?.map(Some).with_context(|| {
            format!(
                "API token file {} must exist and contain a token",
                path.display()
            )
        });
    }
    if let Some(token) = token_from_file(&root.join("API_TOKEN"))? {
        return Ok(Some(token));
    }
    match config_dir {
        Some(dir) => token_from_file(&dir.join("todoist-tui/API_TOKEN")),
        None => Ok(None),
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

    #[test]
    fn token_sources_have_explicit_and_backwards_compatible_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let config = dir.path().join("config");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(config.join("todoist-tui")).unwrap();
        let explicit = dir.path().join("explicit");
        fs::write(&explicit, " explicit-token\n").unwrap();
        fs::write(config.join("todoist-tui/API_TOKEN"), " xdg-token\n").unwrap();
        assert_eq!(
            load_token(None, None, &root, Some(&config))
                .unwrap()
                .as_deref(),
            Some("xdg-token")
        );
        fs::write(root.join("API_TOKEN"), "legacy-token").unwrap();
        assert_eq!(
            load_token(None, None, &root, Some(&config))
                .unwrap()
                .as_deref(),
            Some("legacy-token")
        );
        assert_eq!(
            load_token(None, Some(&explicit), &root, Some(&config))
                .unwrap()
                .as_deref(),
            Some("explicit-token")
        );
        assert_eq!(
            load_token(
                Some("environment-token".into()),
                Some(&explicit),
                &root,
                Some(&config)
            )
            .unwrap()
            .as_deref(),
            Some("environment-token")
        );
    }

    #[test]
    fn explicit_token_file_errors_do_not_fall_back_or_expose_contents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("API_TOKEN"), "legacy-secret").unwrap();
        let explicit = dir.path().join("missing");
        assert!(load_token(None, Some(&explicit), dir.path(), None).is_err());
        fs::write(&explicit, " \n").unwrap();
        assert!(load_token(None, Some(&explicit), dir.path(), None).is_err());
        fs::write(&explicit, b"private-token\xff").unwrap();
        let error = format!(
            "{:#}",
            load_token(None, Some(&explicit), dir.path(), None).unwrap_err()
        );
        assert!(!error.contains("private-token"));
        assert!(!error.contains("legacy-secret"));
        assert!(load_token(None, Some(dir.path()), dir.path(), None).is_err());
    }
}
