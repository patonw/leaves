use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::Path;

use colorgrad::preset::{greys, viridis, yl_or_br};
use colorgrad::{BasisGradient, Gradient as _, GradientBuilder};
use ratatui::style::Color;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct ColorScheme {
    color_shift: f32,

    dark_mode: bool,

    mono: bool,

    dir_grad: BasisGradient,

    ext_grad: BasisGradient,
}

static WHITE: colorgrad::Color = colorgrad::Color::new(1.0, 1.0, 1.0, 1.0);
static BLACK: colorgrad::Color = colorgrad::Color::new(0.0, 0.0, 0.0, 0.0);

impl ColorScheme {
    pub fn new(config: &Config) -> Self {
        let colors = config.colors.as_deref().unwrap_or("fall");
        let (dir_grad, ext_grad) = config
            .themes
            .get(colors)
            .and_then(|spec| -> Option<_> {
                let dir_grad = GradientBuilder::new()
                    .html_colors(&spec.dirs)
                    .build::<BasisGradient>();
                let ext_grad = GradientBuilder::new()
                    .html_colors(&spec.files)
                    .build::<BasisGradient>();

                tracing::debug!(?dir_grad, ?ext_grad, "Decoded gradients");

                Some((dir_grad.ok()?, ext_grad.ok()?))
            })
            .unwrap_or_else(|| match colors {
                "spring" => (yl_or_br(), viridis()),
                "mono" | "greys" => (greys(), greys()),
                _ => (viridis(), yl_or_br()),
            });

        Self {
            color_shift: config.color_shift,
            dark_mode: config.dark_mode,
            mono: colors == "mono",
            dir_grad,
            ext_grad,
        }
    }

    /// Lighten/darken colors to improve readability at the cost of saturation
    pub fn shift(&self, color: colorgrad::Color) -> colorgrad::Color {
        if self.color_shift == 0.0 {
            color
        } else if self.dark_mode {
            color.interpolate_oklab(&WHITE, self.color_shift)
        } else {
            color.interpolate_oklab(&BLACK, self.color_shift)
        }
    }

    pub fn dir_color(&self, dir_path: impl AsRef<Path>) -> Color {
        if self.mono {
            return Color::Reset;
        }

        let mut h = DefaultHasher::default();
        format!(
            "{}",
            dir_path.as_ref().file_name().unwrap_or_default().display()
        )
        .hash(&mut h);
        let id = h.finish();

        let color = self.shift(self.dir_grad.at(id as f32 / u64::MAX as f32));
        Color::from(color.to_rgba8())
    }

    pub fn file_color(&self, file_path: impl AsRef<Path>) -> Color {
        if let Some(ext) = file_path.as_ref().extension() {
            self.ext_color(ext)
        } else {
            Color::Reset
        }
    }

    pub fn ext_color(&self, ext: &OsStr) -> Color {
        if self.mono || ext.is_empty() {
            return Color::Reset;
        }

        let mut h = DefaultHasher::default();
        format!("{}", ext.display()).hash(&mut h);
        let id = h.finish();

        let color = self.shift(self.ext_grad.at(id as f32 / u64::MAX as f32));
        Color::from(color.to_rgba8())
    }
}
