use gpui::{AssetSource, SharedString};
use std::path::PathBuf;

pub(crate) struct Assets;

impl Assets {
    fn resolve(path: &str) -> Option<PathBuf> {
        if let Some(dir) = option_env!("CARGO_MANIFEST_DIR") {
            let path = PathBuf::from(dir).join(path);
            if path.exists() {
                return Some(path);
            }
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(bin_dir) = exe.parent() {
                let path = bin_dir.join(path);
                if path.exists() {
                    return Some(path);
                }

                let path = bin_dir.join("../Resources").join(path);
                if path.exists() {
                    return Some(path);
                }
            }
        }

        None
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match Self::resolve(path) {
            Some(full) => Ok(Some(std::fs::read(&full)?.into())),
            None => Ok(None),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        match Self::resolve(path) {
            Some(dir) if dir.is_dir() => Ok(std::fs::read_dir(dir)?
                .filter_map(|entry| {
                    Some(SharedString::from(
                        entry.ok()?.path().to_string_lossy().into_owned(),
                    ))
                })
                .collect()),
            _ => Ok(vec![]),
        }
    }
}
