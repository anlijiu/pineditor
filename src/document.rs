//! The file behind the buffer: where it came from, and where its libraries live.
//!
//! The editor can also run on the bundled fixture, in which case there is no
//! path — and therefore no directory to resolve `import` against. Everything
//! here returns `Option` for that reason rather than inventing a path.

use std::path::{Path, PathBuf};

/// The on-disk file the buffer is attached to, if any.
#[derive(Default)]
pub struct Document {
    path: Option<PathBuf>,
}

impl Document {
    /// A document with no file yet — the bundled fixture.
    pub fn detached() -> Self {
        Self::default()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Attach the buffer to `path`. Called after a successful open or save.
    pub fn attach(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    /// Detach the buffer again, for the bundled fixture after a jump into a file.
    pub fn detach(&mut self) {
        self.path = None;
    }

    /// What the status bar shows: the file name, or a placeholder for the
    /// unsaved fixture.
    pub fn title(&self) -> String {
        match &self.path {
            Some(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            None => "未命名".to_string(),
        }
    }
}

/// Read `path` as UTF-8 text.
pub fn open(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Write `text` to `path` atomically.
///
/// `fs::write` truncates first, so a crash or a full disk mid-write leaves the
/// user's script half-destroyed — the worst failure mode an editor has. Writing a
/// sibling temp file and renaming it makes the swap atomic: the old contents stay
/// intact until the new file is complete. The temp file lives in the same
/// directory because `rename` is only atomic within one filesystem.
pub fn save(path: &Path, text: &str) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = directory.join(format!(
        ".{}.pineditor-{}",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "untitled".into()),
        std::process::id()
    ));

    std::fs::write(&temporary, text)?;
    // A failed rename leaves the temp file behind; removing it keeps the
    // directory clean, and the real file is untouched either way.
    if let Err(error) = std::fs::rename(&temporary, path) {
        std::fs::remove_file(&temporary).ok();
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_document_has_no_file_and_says_so() {
        let document = Document::detached();
        assert!(document.path().is_none());
        assert_eq!(document.title(), "未命名");
    }

    #[test]
    fn an_attached_document_shows_its_file_name() {
        let mut document = Document::detached();
        document.attach(PathBuf::from("/tmp/project/strategy.pine"));

        assert_eq!(document.title(), "strategy.pine");
        assert_eq!(document.path(), Some(Path::new("/tmp/project/strategy.pine")));
    }

    #[test]
    fn detaching_returns_the_buffer_to_unnamed() {
        let mut document = Document::detached();
        document.attach(PathBuf::from("/tmp/project/strategy.pine"));
        document.detach();

        assert!(document.path().is_none());
        assert_eq!(document.title(), "未命名");
    }

    #[test]
    fn a_failed_save_leaves_the_original_intact() {
        let dir = std::env::temp_dir().join(format!("pineditor-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("strategy.pine");
        save(&path, "original\n").expect("first save");

        // Saving into a path whose directory vanished must fail *and* not damage
        // the file that is already there.
        let missing = dir.join("gone").join("strategy.pine");
        assert!(save(&missing, "new\n").is_err());
        assert_eq!(open(&path).expect("read"), "original\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        let dir = std::env::temp_dir().join(format!("pineditor-temp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("strategy.pine");

        save(&path, "x\n").expect("save");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["strategy.pine".to_string()], "{entries:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_open_round_trips() {
        let dir = std::env::temp_dir().join(format!("pineditor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("round-trip.pine");

        let source = "//@version=5\nindicator(\"x\")\nplot(close)\n";
        save(&path, source).expect("save");
        assert_eq!(open(&path).expect("open"), source);

        std::fs::remove_dir_all(&dir).ok();
    }
}
