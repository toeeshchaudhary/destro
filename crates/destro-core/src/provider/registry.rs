//! The provider catalogue: built-in descriptors, an optional config file, and
//! construction of a live [`Provider`] from an id.
//!
//! A user adds a backend without touching the agent loop: either drop a
//! `providers.json` file next to the app's config (see [`ProviderRegistry::path`])
//! or register a [`ProviderDescriptor`] in code. The loop only ever sees
//! [`Provider`].

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{MockProvider, OpenAiCompatProvider, Provider};

fn keys_file() -> Option<PathBuf> {
    crate::context::data_dir().map(|d| d.join("keys.json"))
}

/// An API key: the environment variable wins, then `keys.json` in the data
/// directory (written by [`save_key`], so a key can be entered in the TUI).
pub fn key_of(var: &str) -> Option<String> {
    if let Some(v) = env::var_os(var).filter(|v| !v.is_empty()) {
        return v.into_string().ok();
    }
    let text = fs::read_to_string(keys_file()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v[var]
        .as_str()
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// Remember a key for `var` in `keys.json` (plain text, user-only data dir).
pub fn save_key(var: &str, key: &str) -> Result<(), String> {
    let path = keys_file().ok_or("no data directory")?;
    let mut v: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v[var] = serde_json::Value::String(key.trim().to_string());
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// Which transport backs a descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    /// A deterministic, offline provider ([`MockProvider`]).
    #[default]
    Mock,
    /// An OpenAI chat-completions compatible HTTP endpoint.
    #[serde(
        alias = "openai-compatible",
        alias = "openai-compat",
        alias = "openai_compat"
    )]
    OpenAiCompat,
}

/// One provider a user may select.
///
/// This is the on-disk and in-code description; [`ProviderRegistry::build`]
/// turns it into a live provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    /// Stable selection id, e.g. `"deepseek"`.
    pub id: String,
    /// Human-readable name shown in clients.
    #[serde(default)]
    pub label: String,
    /// Transport kind.
    pub kind: ProviderKind,
    /// API root used by [`ProviderKind::OpenAiCompat`], e.g.
    /// `https://api.deepseek.com/v1`.
    #[serde(default)]
    pub base_url: String,
    /// Environment variable holding the API key; `None` means none is needed.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Model the harness starts with when the user does not choose one.
    #[serde(default)]
    pub default_model: String,
    /// Models this provider is known to serve.
    #[serde(default)]
    pub models: Vec<String>,
}

/// A descriptor as presented to a client: no secrets, just facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    /// Stable selection id.
    pub id: String,
    /// Human-readable name.
    pub label: String,
    /// Model used when the user does not choose one.
    pub default_model: String,
    /// Known model ids.
    pub models: Vec<String>,
    /// Whether an API key environment variable is declared.
    pub requires_key: bool,
    /// Whether that variable is currently set (or no key is needed).
    pub configured: bool,
    /// The environment variable that carries the key, if any.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The set of selectable providers.
///
/// [`ProviderRegistry::builtin`] always knows `mock`, `deepseek`, and
/// `openai-compatible`. [`ProviderRegistry::load`] merges a JSON config file on
/// top, by id, so a config entry can override a built-in (for example to point
/// `deepseek` at a proxy) and can add new providers.
///
/// # Config file
///
/// The file is a single JSON object with a `providers` array. Field names are
/// camelCase; `apiKeyEnv`, `label`, `baseUrl`, `defaultModel`, and `models` are
/// optional.
///
/// On Windows the path is `%APPDATA%\destro\providers.json`; elsewhere it is
/// `$XDG_CONFIG_HOME/destro/providers.json` or, failing that,
/// `~/.config/destro/providers.json`. A missing file is not an error.
///
/// ```json
/// {
///   "providers": [
///     {
///       "id": "local-llama",
///       "label": "llama.cpp (local)",
///       "kind": "openAiCompat",
///       "baseUrl": "http://127.0.0.1:8080/v1",
///       "defaultModel": "qwen2.5-coder",
///       "models": ["qwen2.5-coder"]
///     },
///     {
///       "id": "deepseek",
///       "label": "DeepSeek (proxy)",
///       "kind": "openAiCompat",
///       "baseUrl": "https://proxy.internal.example/v1",
///       "apiKeyEnv": "DEEPSEEK_API_KEY",
///       "defaultModel": "deepseek-chat",
///       "models": ["deepseek-chat", "deepseek-reasoner"]
///     }
///   ]
/// }
/// ```
#[derive(Clone, Debug, Default)]
pub struct ProviderRegistry {
    providers: Vec<ProviderDescriptor>,
}

impl ProviderRegistry {
    /// The providers every install starts with.
    pub fn builtin() -> Self {
        Self {
            providers: vec![
                ProviderDescriptor {
                    id: "mock".into(),
                    label: "Mock (offline)".into(),
                    kind: ProviderKind::Mock,
                    base_url: String::new(),
                    api_key_env: None,
                    default_model: "mock-echo".into(),
                    models: vec!["mock-echo".into()],
                },
                ProviderDescriptor {
                    id: "deepseek".into(),
                    label: "DeepSeek".into(),
                    kind: ProviderKind::OpenAiCompat,
                    base_url: "https://api.deepseek.com/v1".into(),
                    api_key_env: Some("DEEPSEEK_API_KEY".into()),
                    default_model: "deepseek-chat".into(),
                    models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
                },
                ProviderDescriptor {
                    id: "openai-compatible".into(),
                    label: "OpenAI compatible".into(),
                    kind: ProviderKind::OpenAiCompat,
                    base_url: "https://api.openai.com/v1".into(),
                    api_key_env: Some("OPENAI_API_KEY".into()),
                    default_model: "gpt-4o-mini".into(),
                    models: vec!["gpt-4o-mini".into(), "gpt-4o".into()],
                },
            ],
        }
    }

    /// Where the config file lives, when the platform offers a config directory.
    pub fn path() -> Option<PathBuf> {
        if cfg!(windows)
            && let Some(appdata) = env::var_os("APPDATA")
        {
            return Some(PathBuf::from(appdata).join("destro").join("providers.json"));
        }
        if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg).join("destro").join("providers.json"));
        }
        env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("destro")
                .join("providers.json")
        })
    }

    /// Built-ins merged with the config file at [`ProviderRegistry::path`], if
    /// one exists. Read or parse failures fall back to the built-ins.
    pub fn load() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(path).unwrap_or_else(|_| Self::builtin()),
            None => Self::builtin(),
        }
    }

    /// Built-ins merged with the config file at `path`. A missing file is not
    /// an error; malformed JSON is.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let mut registry = Self::builtin();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(registry),
            Err(error) => {
                return Err(format!("cannot read {}: {error}", path.display()));
            }
        };
        let file: ProviderFile = serde_json::from_str(&text)
            .map_err(|error| format!("invalid provider config {}: {error}", path.display()))?;
        for descriptor in file.providers {
            registry.merge(descriptor);
        }
        Ok(registry)
    }

    /// Add or replace a descriptor by id and return the registry.
    pub fn with(mut self, descriptor: ProviderDescriptor) -> Self {
        self.merge(descriptor);
        self
    }

    /// The descriptors, in display order.
    pub fn descriptors(&self) -> &[ProviderDescriptor] {
        &self.providers
    }

    /// The selectable providers, with key availability resolved.
    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|descriptor| ProviderInfo {
                id: descriptor.id.clone(),
                label: descriptor.label.clone(),
                default_model: descriptor.default_model.clone(),
                models: descriptor.models.clone(),
                requires_key: descriptor.api_key_env.is_some(),
                configured: descriptor
                    .api_key_env
                    .as_ref()
                    .is_none_or(|var| key_of(var).is_some()),
                api_key_env: descriptor.api_key_env.clone(),
            })
            .collect()
    }

    /// Build the provider with this id.
    ///
    /// `mock` yields [`MockProvider::echo`]. OpenAI-compatible kinds resolve
    /// their key from [`ProviderDescriptor::api_key_env`]; a missing variable
    /// is an error naming the variable. An unknown id lists the known ones.
    pub fn build(&self, id: &str) -> Result<Arc<dyn Provider>, String> {
        let descriptor = self.providers.iter().find(|p| p.id == id).ok_or_else(|| {
            let known = self
                .providers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown provider `{id}`; known providers: {known}")
        })?;

        match descriptor.kind {
            ProviderKind::Mock => Ok(Arc::new(MockProvider::echo())),
            ProviderKind::OpenAiCompat => {
                if descriptor.base_url.trim().is_empty() {
                    return Err(format!("provider `{id}` has no baseUrl configured"));
                }
                let api_key = match &descriptor.api_key_env {
                    None => String::new(),
                    Some(var) => key_of(var).ok_or_else(|| {
                        format!("provider `{id}` requires environment variable {var}")
                    })?,
                };
                Ok(Arc::new(OpenAiCompatProvider::new(
                    "openai-compat",
                    descriptor.base_url.clone(),
                    api_key,
                )))
            }
        }
    }

    /// The id to start with: the first configured non-mock provider, else
    /// `"mock"`.
    pub fn default_id(&self) -> String {
        self.providers
            .iter()
            .find(|descriptor| {
                descriptor.kind != ProviderKind::Mock
                    && descriptor
                        .api_key_env
                        .as_ref()
                        .is_none_or(|var| key_of(var).is_some())
            })
            .map(|descriptor| descriptor.id.clone())
            .unwrap_or_else(|| "mock".to_string())
    }

    fn merge(&mut self, descriptor: ProviderDescriptor) {
        match self
            .providers
            .iter_mut()
            .find(|existing| existing.id == descriptor.id)
        {
            Some(existing) => *existing = descriptor,
            None => self.providers.push(descriptor),
        }
    }
}

/// The top-level shape of `providers.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderFile {
    #[serde(default)]
    providers: Vec<ProviderDescriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Write `content` to a unique temp file and hand back its path.
    fn temp_config(content: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "destro-providers-test-{}-{unique}.json",
            std::process::id()
        ));
        fs::write(&path, content).expect("write temp config");
        path
    }

    #[test]
    fn builtin_contains_the_three_names() {
        let registry = ProviderRegistry::builtin();
        let ids: Vec<&str> = registry
            .descriptors()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, vec!["mock", "deepseek", "openai-compatible"]);
    }

    #[test]
    fn build_mock_succeeds() {
        let provider = ProviderRegistry::builtin()
            .build("mock")
            .expect("mock builds");
        assert_eq!(provider.name(), "mock");
    }

    #[test]
    fn unknown_id_error_lists_known_ids() {
        let error = ProviderRegistry::builtin()
            .build("does-not-exist")
            .err()
            .expect("unknown id fails");
        assert!(error.contains("does-not-exist"));
        assert!(error.contains("mock"));
        assert!(error.contains("deepseek"));
    }

    #[test]
    fn missing_key_error_names_the_environment_variable() {
        let path = temp_config(
            r#"{"providers":[{
                "id": "needs-secret",
                "kind": "openAiCompat",
                "baseUrl": "https://example.test/v1",
                "apiKeyEnv": "DESTRO_TEST_ABSENT_KEY_9F3A1",
                "defaultModel": "m"
            }]}"#,
        );
        let registry = ProviderRegistry::load_from(&path).expect("config loads");
        fs::remove_file(&path).ok();

        let error = registry.build("needs-secret").err().expect("no key");
        assert!(
            error.contains("DESTRO_TEST_ABSENT_KEY_9F3A1"),
            "message should name the env var: {error}"
        );

        let info = registry
            .list()
            .into_iter()
            .find(|p| p.id == "needs-secret")
            .expect("listed");
        assert!(info.requires_key);
        assert!(!info.configured);
    }

    #[test]
    fn config_merges_by_id_and_adds_new_providers() {
        let path = temp_config(
            &json!({
                "providers": [
                    {
                        "id": "deepseek",
                        "label": "DeepSeek via proxy",
                        "kind": "openAiCompat",
                        "baseUrl": "https://proxy.example/v1",
                        "apiKeyEnv": "DEEPSEEK_API_KEY",
                        "defaultModel": "deepseek-reasoner",
                        "models": ["deepseek-reasoner"]
                    },
                    {
                        "id": "local-llama",
                        "label": "Local llama",
                        "kind": "openAiCompat",
                        "baseUrl": "http://127.0.0.1:8080/v1",
                        "defaultModel": "qwen2.5-coder",
                        "models": ["qwen2.5-coder"]
                    }
                ]
            })
            .to_string(),
        );
        let registry = ProviderRegistry::load_from(&path).expect("config loads");
        fs::remove_file(&path).ok();

        let descriptors = registry.descriptors();
        let deepseek = descriptors
            .iter()
            .find(|d| d.id == "deepseek")
            .expect("deepseek present");
        assert_eq!(deepseek.label, "DeepSeek via proxy");
        assert_eq!(deepseek.base_url, "https://proxy.example/v1");
        assert_eq!(deepseek.default_model, "deepseek-reasoner");
        // The builtin entry was replaced, not duplicated.
        assert_eq!(descriptors.iter().filter(|d| d.id == "deepseek").count(), 1);

        let local = descriptors
            .iter()
            .find(|d| d.id == "local-llama")
            .expect("new provider added");
        assert_eq!(local.models, vec!["qwen2.5-coder"]);

        // A provider with no key requirement is configured and builds.
        assert!(registry.build("local-llama").is_ok());
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let path = env::temp_dir().join(format!(
            "destro-missing-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let registry = ProviderRegistry::load_from(&path).expect("missing file is fine");
        assert_eq!(registry.descriptors().len(), 3);
    }

    #[test]
    fn malformed_config_is_reported() {
        let path = temp_config("{ not json");
        let error = ProviderRegistry::load_from(&path).expect_err("invalid config fails");
        fs::remove_file(&path).ok();
        assert!(error.contains("invalid provider config"));
    }

    #[test]
    fn default_id_prefers_the_first_configured_non_mock_provider() {
        let path = temp_config(
            r#"{"providers":[{
                "id": "local-llama",
                "kind": "openAiCompat",
                "baseUrl": "http://127.0.0.1:8080/v1",
                "defaultModel": "qwen2.5-coder"
            }]}"#,
        );
        let registry = ProviderRegistry::load_from(&path).expect("config loads");
        fs::remove_file(&path).ok();
        assert_eq!(registry.default_id(), "local-llama");

        // With no configured non-mock provider, mock is the fallback.
        let only_mock = ProviderRegistry {
            providers: vec![ProviderDescriptor {
                id: "mock".into(),
                label: "Mock".into(),
                kind: ProviderKind::Mock,
                base_url: String::new(),
                api_key_env: None,
                default_model: "mock-echo".into(),
                models: Vec::new(),
            }],
        };
        assert_eq!(only_mock.default_id(), "mock");
    }

    #[test]
    fn descriptors_round_trip_through_json_camel_case() {
        let descriptor = ProviderDescriptor {
            id: "x".into(),
            label: "X".into(),
            kind: ProviderKind::OpenAiCompat,
            base_url: "https://example.test/v1".into(),
            api_key_env: Some("X_KEY".into()),
            default_model: "m".into(),
            models: vec!["m".into()],
        };
        let text = serde_json::to_string(&descriptor).expect("serialises");
        assert!(text.contains("\"baseUrl\""));
        assert!(text.contains("\"apiKeyEnv\""));
        assert!(text.contains("\"openAiCompat\""));
        let back: ProviderDescriptor = serde_json::from_str(&text).expect("deserialises");
        assert_eq!(back, descriptor);
    }

    #[test]
    fn path_uses_appdata_on_windows() {
        if !cfg!(windows) {
            return;
        }
        let expected = env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("destro").join("providers.json"));
        assert_eq!(ProviderRegistry::path(), expected);
    }
}
