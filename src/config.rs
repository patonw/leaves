use std::{borrow::Cow, collections::BTreeMap, env};

#[derive(Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    Mono,

    #[default]
    Fall,

    Spring,
}

impl ColorScheme {
    pub fn mono(&self) -> bool {
        *self == Self::Mono
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum DirStyle {
    #[default]
    Plain,

    Thick,
}

#[derive(Debug, Default)]
pub struct Config {
    pub colors: ColorScheme,

    /// Render directories with heavy lines
    pub dir_style: DirStyle,
}

#[derive(Debug, Default)]
struct EnvWrap(BTreeMap<String, String>);

impl EnvWrap {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|it| it.as_str())
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
    pub fn new(env: impl IntoIterator<Item = (String, String)>) -> Self {
        let namespace = env!("CARGO_CRATE_NAME").to_uppercase(); // need compile-time uppercase

        let vars = EnvWrap::from(env);

        let colors = match vars.lower(&format!("{namespace}_COLORS")).as_deref() {
            Some("none" | "mono" | "monochrome") => ColorScheme::Mono,
            Some("spring" | "swap") => ColorScheme::Spring,
            _ => Default::default(),
        };

        let dir_style = match vars.lower(&format!("{namespace}_DIR_STYLE")).as_deref() {
            Some("thick") => DirStyle::Thick,
            _ => Default::default(),
        };

        Self { colors, dir_style }
    }
}
