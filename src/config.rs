use std::{borrow::Cow, collections::BTreeMap, env};

use color_eyre::Result;
use serde::Deserialize;

pub const PROJECT_NAME: &str = env!("CARGO_CRATE_NAME");

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub enum DirStyle {
    #[default]
    Plain,

    #[serde(alias = "heavy")]
    Thick,
}

#[derive(Debug, Default, Deserialize)]
pub struct ThemeSpec {
    pub dirs: Vec<String>,
    pub files: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_color_shift() -> f32 {
    0.2
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Render for a darker background
    #[serde(default = "default_true")]
    pub dark_mode: bool,

    /// Name of theme for color gradients to apply to files and directories
    #[serde(default)]
    pub colors: Option<String>,

    /// Apply lightening or darkening depending on dark_mode
    #[serde(default = "default_color_shift")]
    pub color_shift: f32,

    /// Render directories with heavy lines
    #[serde(default)]
    pub dir_style: DirStyle,

    #[serde(default)]
    pub themes: BTreeMap<String, ThemeSpec>,
}

#[derive(Debug, Default)]
struct EnvWrap(BTreeMap<String, String>);

impl EnvWrap {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|it| it.as_str())
    }

    pub fn one_of<S: AsRef<str>>(&self, keys: impl IntoIterator<Item = S>) -> Option<String> {
        keys.into_iter()
            .find_map(|k| self.get(k.as_ref()))
            .map(|s| s.to_lowercase())
    }

    pub fn lower(&self, key: &str) -> Option<Cow<'_, str>> {
        self.get(key).map(|s| Cow::Owned(s.to_lowercase()))
    }
}

impl<T: IntoIterator<Item = (String, String)>> From<T> for EnvWrap {
    fn from(value: T) -> Self {
        Self(value.into_iter().collect())
    }
}

impl Config {
    /// Apply overrides from environment
    pub fn with_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        let namespace = PROJECT_NAME.to_uppercase(); // need compile-time uppercase

        let vars = EnvWrap::from(env);

        self.dark_mode = match vars.lower(&format!("{namespace}_DARK_MODE")).as_deref() {
            Some("0" | "no" | "false") => false,
            Some("1" | "yes" | "true") => true,
            _ => self.dark_mode,
        };

        self.dark_mode = match vars.lower(&format!("{namespace}_BG")).as_deref() {
            Some("light") => false,
            Some("dark") => true,
            _ => self.dark_mode,
        };

        self.colors = match vars
            .one_of([format!("{namespace}_COLORS"), format!("{namespace}_THEME")])
            .as_deref()
        {
            Some("none" | "mono" | "monochrome") => Some("mono".into()),
            Some(value) => Some(value.into()),
            _ => self.colors,
        };

        self.color_shift = match vars.lower(&format!("{namespace}_COLOR_SHIFT")).as_deref() {
            Some("no" | "none" | "false") => 0.0,
            Some("yes" | "true") => 0.2,
            Some(value) if let Ok(num) = value.parse::<f32>() => num,
            _ => self.color_shift,
        };

        self.dir_style = match vars.lower(&format!("{namespace}_DIR_STYLE")).as_deref() {
            Some("heavy" | "thick" | "strong") => DirStyle::Thick,
            _ => self.dir_style,
        };

        self
    }

    /// Loads file from config directory
    pub fn load() -> Result<Self> {
        let path = dirs::config_dir()
            .unwrap_or_default()
            .join(PROJECT_NAME)
            .join("settings.toml");

        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(Default::default());
        };

        let config = toml::from_str(&text);
        tracing::debug!(?config, "Loaded settings from file");
        Ok(config?)
    }
}
