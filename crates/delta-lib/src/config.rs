use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainConfig {
    #[serde(default)]
    pub parser: ParserConfig,
    #[serde(default)]
    pub git: Option<GitConfig>,
    #[serde(default = "default_file_excludes")]
    pub file_exclude_patterns: Vec<String>,
    #[serde(default)]
    pub tripwire_patterns: Vec<String>,
    #[serde(flatten)]
    pub crate_configs: HashMap<String, ParserConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub remote_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[expect(clippy::struct_excessive_bools, reason = "configuration struct mirrors TOML schema")]
pub struct ParserConfig {
    #[serde(default = "default_true")]
    pub file_refs: bool,
    #[serde(default = "default_file_methods")]
    pub file_methods: HashSet<String>,
    #[serde(default = "default_true")]
    pub includes: bool,
    #[serde(default = "default_include_macros")]
    pub include_macros: HashSet<String>,
    #[serde(default = "default_true")]
    pub mods: bool,
    #[serde(default = "default_mod_macros")]
    pub mod_macros: HashSet<String>,
    #[serde(default = "default_false")]
    pub assume: bool,
    #[serde(default)]
    pub assume_patterns: HashSet<String>,
}

impl Default for ParserConfig {
    fn default() -> Self {
        // Use serde's deserialization to get the defaults.
        toml::from_str("").unwrap()
    }
}

const fn default_true() -> bool {
    true
}

fn default_file_excludes() -> Vec<String> {
    vec![".*".to_string(), "target".to_string()]
}

const fn default_false() -> bool {
    false
}

fn default_file_methods() -> HashSet<String> {
    ["file", "from_file", "load", "open", "read", "load_from"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn default_include_macros() -> HashSet<String> {
    ["include_str", "include_bytes"].iter().map(|s| (*s).to_string()).collect()
}

fn default_mod_macros() -> HashSet<String> {
    HashSet::new()
}

impl Default for MainConfig {
    fn default() -> Self {
        // Use serde's deserialization to get the defaults.
        toml::from_str("").unwrap()
    }
}

impl MainConfig {
    pub fn crate_config(&self, crate_name: &str) -> ParserConfig {
        let crate_key = format!("parser.{crate_name}");
        self.crate_configs.get(&crate_key).cloned().unwrap_or_else(|| self.parser.clone())
    }
}

pub fn load_config(config_path: Option<PathBuf>) -> Result<MainConfig> {
    match config_path {
        Some(path) => {
            let content = std::fs::read_to_string(&path).map_err(Error::ConfigRead)?;

            let config: MainConfig = toml::from_str(&content)?;

            Ok(config)
        }
        None => Ok(MainConfig::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_config_default_has_expected_values() {
        let config = ParserConfig::default();
        assert!(config.file_refs);
        assert!(config.includes);
        assert!(config.mods);
        assert!(!config.assume);
        assert!(config.assume_patterns.is_empty());
        assert!(config.file_methods.contains("open"));
        assert!(config.file_methods.contains("load"));
        assert!(config.include_macros.contains("include_str"));
        assert!(config.include_macros.contains("include_bytes"));
        assert!(config.mod_macros.is_empty());
    }

    #[test]
    fn main_config_default_has_file_excludes() {
        let config = MainConfig::default();
        assert!(config.file_exclude_patterns.contains(&".*".to_string()));
        assert!(config.file_exclude_patterns.contains(&"target".to_string()));
        assert!(config.tripwire_patterns.is_empty());
        assert!(config.git.is_none());
    }

    #[test]
    fn crate_config_returns_default_parser_when_no_override() {
        let config = MainConfig::default();
        let parser = config.crate_config("some-crate");
        assert!(parser.file_refs);
        assert!(parser.mods);
    }

    #[test]
    fn load_config_returns_default_when_none() {
        let config = load_config(None).unwrap();
        assert!(config.file_exclude_patterns.contains(&".*".to_string()));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn load_config_returns_error_for_missing_file() {
        let result = load_config(Some(PathBuf::from("nonexistent-config.toml")));
        assert!(matches!(result, Err(Error::ConfigRead(_))));
    }

    #[test]
    fn parse_toml_with_custom_values() {
        let toml_str = r#"
file_exclude_patterns = ["build"]
tripwire_patterns = ["Cargo.lock"]

[parser]
file_refs = false
mods = false
"#;
        let config: MainConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.file_exclude_patterns, vec!["build"]);
        assert_eq!(config.tripwire_patterns, vec!["Cargo.lock"]);
        assert!(!config.parser.file_refs);
        assert!(!config.parser.mods);
        assert!(config.parser.includes);
    }

    #[test]
    fn parse_toml_with_git_config() {
        let toml_str = r#"
[git]
remote_branch = "origin/develop"
"#;
        let config: MainConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.git.unwrap().remote_branch.unwrap(), "origin/develop");
    }
}
