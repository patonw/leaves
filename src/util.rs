use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::Color;

pub static SWAP_COLORS: AtomicBool = AtomicBool::new(false);

pub fn dir_color(dir_path: impl AsRef<Path>) -> Color {
    let mut h = DefaultHasher::default();
    format!(
        "{}",
        dir_path.as_ref().file_name().unwrap_or_default().display()
    )
    .hash(&mut h);
    let id = h.finish();

    let gradient = if SWAP_COLORS.load(Ordering::Relaxed) {
        &colorous::YELLOW_ORANGE_BROWN
    } else {
        &colorous::VIRIDIS
    };

    let color = gradient.eval_rational(id as usize, u64::MAX as usize);
    Color::from(color.into_tuple())
}

pub fn file_color(file_path: impl AsRef<Path>) -> Color {
    if let Some(ext) = file_path.as_ref().extension() {
        ext_color(ext)
    } else {
        Color::Reset
    }
}

pub fn ext_color(ext: &OsStr) -> Color {
    if ext.is_empty() {
        return Color::Reset;
    }

    let mut h = DefaultHasher::default();
    format!("{}", ext.display()).hash(&mut h);
    let id = h.finish();

    let gradient = if SWAP_COLORS.load(Ordering::Relaxed) {
        &colorous::VIRIDIS
    } else {
        &colorous::YELLOW_ORANGE_BROWN
    };

    let color = gradient.eval_rational(id as usize, u64::MAX as usize);
    Color::from(color.into_tuple())
}
