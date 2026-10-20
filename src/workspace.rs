//! The directories `import` paths resolve against.
//!
//! `pine_lang::DirLoader` takes a list of roots, but the editor's LSP providers
//! (`HoverProvider`, `DefinitionProvider`) are handed nothing but the buffer text
//! and an offset — they cannot carry a path. The roots are therefore recorded
//! here once, and every analysis asks for them.
//!
//! There is more than one root on purpose. A library rarely sits *next to* the
//! script that imports it: pinecone's own test data keeps `import/*.pine` beside
//! a `libraries/` folder, and a real project keeps its libraries wherever it
//! likes. So the roots are, in order:
//!
//! 1. directories given on the command line (`--lib`, `PINEDITOR_LIB_DIR`),
//! 2. the open file's own directory,
//! 3. every `libraries/` directory found walking up from it.
//!
//! `DirLoader` tries each root in turn until a file exists, so order only decides
//! which copy wins when a library name is duplicated.
//!
//! This is process-wide state: the editor is single-window and single-document,
//! so there is exactly one open file to describe.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use pine_lang::DirLoader;

/// A place in a file the reader can be sent back to.
///
/// `offset` is a position, not a byte index: it is what the editor's own
/// `cursor_position()` reports and what `set_cursor_position()` consumes, so a
/// jump back needs no conversion on either end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpSite {
    /// `None` while the bundled fixture is showing.
    pub path: Option<PathBuf>,
    pub position: lsp_types::Position,
    /// The buffer exactly as it was when the jump started.
    ///
    /// Restoring the text instead of re-reading the file keeps unsaved edits —
    /// and the fixture, which has no file to re-read at all.
    pub source: String,
}

/// Jump origins, most recent last.
static JUMP_SITES: RwLock<Vec<JumpSite>> = RwLock::new(Vec::new());

/// How many origins are kept.
///
/// Each entry holds a full buffer snapshot, so an uncapped stack would grow with
/// every edit-and-jump cycle for the life of the process. Fifty jumps back is far
/// more than anyone walks.
const MAX_JUMP_SITES: usize = 50;

/// Remember where a jump is starting from.
///
/// Pressing the key twice without moving queues the same site once: the stack is
/// a trail of places, not a log of keystrokes.
pub fn push_jump_site(site: JumpSite) {
    if let Ok(mut sites) = JUMP_SITES.write() {
        if sites.last() == Some(&site) {
            return;
        }
        if sites.len() == MAX_JUMP_SITES {
            sites.remove(0);
        }
        sites.push(site);
    }
}

/// Take the most recent origin, if there is one.
pub fn pop_jump_site() -> Option<JumpSite> {
    JUMP_SITES.write().ok()?.pop()
}

/// Number of remembered origins.
#[cfg(test)]
pub fn jump_site_count() -> usize {
    JUMP_SITES
        .read()
        .map(|sites| sites.len())
        .unwrap_or_default()
}

/// The process's working directory when it started.
///
/// `hover`'s reference lookup changes the process CWD (pine_reference resolves its
/// spec directory relatively, and that crate is not ours to change). Anything that
/// wants "where the user started" — the save-as dialog — must read this instead of
/// `current_dir()`, or it opens inside the pinecone checkout after the first hover.
static STARTUP_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

pub fn set_startup_dir(dir: PathBuf) {
    if let Ok(mut current) = STARTUP_DIR.write() {
        *current = Some(dir);
    }
}

/// Where the process started, if it was recorded.
pub fn startup_dir() -> Option<PathBuf> {
    STARTUP_DIR.read().ok().and_then(|dir| dir.clone())
}

/// Roots named by the user, independent of which file is open.
static CONFIGURED: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());
/// The effective root list: configured roots followed by the document's own.
static ROOTS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Record the roots the user asked for (`--lib`, `PINEDITOR_LIB_DIR`).
///
/// Called once at startup, before any file is open.
pub fn set_configured_roots(roots: Vec<PathBuf>) {
    if let Ok(mut current) = CONFIGURED.write() {
        *current = roots;
    }
}

/// Point subsequent analyses at `roots` — the open document's, in the order it
/// wants them tried.
pub fn set_document_roots(roots: Vec<PathBuf>) {
    let configured = CONFIGURED
        .read()
        .map(|configured| configured.clone())
        .unwrap_or_default();

    let mut merged = configured;
    for root in roots {
        if !merged.contains(&root) {
            merged.push(root);
        }
    }

    if let Ok(mut current) = ROOTS.write() {
        *current = merged;
    }
}

/// The roots libraries are looked up in, if any are known.
pub fn roots() -> Vec<PathBuf> {
    ROOTS.read().map(|roots| roots.clone()).unwrap_or_default()
}

/// A fresh loader over the current roots, or `None` when there are none.
///
/// `DirLoader` caches what it reads in a `RefCell`, so it is neither `Sync` nor
/// worth sharing: building one is just cloning the root list, and a per-analysis
/// instance keeps the cache from outliving the file it was built from.
pub fn loader() -> Option<DirLoader> {
    let roots = roots();
    (!roots.is_empty()).then(|| DirLoader::new(roots))
}

/// Roots named outside the document: `--lib <dir>` (repeatable), `--lib=<dir>`,
/// and `PINEDITOR_LIB_DIR` (colon-separated).
///
/// Empty entries are ignored, so an unset or empty environment variable is not a
/// root pointing at the current directory — which would silently make every name
/// resolvable from wherever the process happens to have started.
pub fn parse_roots(args: &[String], lib_dir_env: Option<&str>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let dir = match arg.as_str() {
            "--lib" => args.next().map(String::as_str),
            other => other.strip_prefix("--lib="),
        };
        if let Some(dir) = dir.filter(|dir| !dir.is_empty()) {
            roots.push(absolute(dir));
        }
    }

    if let Some(env) = lib_dir_env {
        for dir in env.split(':').filter(|dir| !dir.is_empty()) {
            roots.push(absolute(dir));
        }
    }

    roots
}

/// Anchor a root to the filesystem now, while the process CWD is still the one
/// the user started in.
///
/// `DirLoader` keeps roots verbatim and only calls `is_file()` when resolving, so
/// a relative root is resolved against whatever the CWD is at that moment — and
/// hovering changes it (see [`crate::hover`]). An absolute root is immune, and it
/// also keeps the analysis cache key stable.
fn absolute(dir: &str) -> PathBuf {
    std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir))
}

/// The file the bundled fixture was compiled from, if it is still on disk.
///
/// `include_str!` proves it existed at build time; this checks at run time,
/// because the roots are derived from where it lives.
pub fn fixture_path() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/example.pine");
    path.is_file().then_some(path)
}

/// The roots for the bundled fixture: the same rules as an opened file, applied
/// to the file it was compiled from.
pub fn fixture_roots() -> Vec<PathBuf> {
    fixture_path()
        .map(|path| roots_for_file(&path))
        .unwrap_or_default()
}

/// The roots to try for a script at `path`: its own directory first, then any
/// `libraries/` above it.
pub fn roots_for_file(path: &Path) -> Vec<PathBuf> {
    let Some(directory) = path.parent() else {
        return vec![];
    };

    let mut roots = vec![directory.to_path_buf()];
    for library in libraries_above(directory) {
        if !roots.contains(&library) {
            roots.push(library);
        }
    }
    roots
}

/// The `libraries/` directories to search, walking up from `directory`.
///
/// Four levels covers the layouts that actually occur (`<project>/libraries`,
/// `tests/testdata/libraries` from `tests/testdata/import/…`) without turning a
/// typo in a path into a scan of the filesystem.
pub fn libraries_above(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut current = Some(directory);
    for _ in 0..4 {
        let Some(dir) = current else { break };
        let candidate = dir.join("libraries");
        if candidate.is_dir() {
            found.push(candidate);
        }
        current = dir.parent();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// These tests share process-wide state, so they must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn reset() {
        set_configured_roots(vec![]);
        set_document_roots(vec![]);
    }

    #[test]
    fn no_roots_means_no_loader() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        reset();
        assert!(roots().is_empty());
        assert!(loader().is_none());
    }

    #[test]
    fn configured_roots_come_first_and_survive_the_next_file() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        reset();

        set_configured_roots(vec![PathBuf::from("/opt/pine-libs")]);
        set_document_roots(vec![PathBuf::from("/tmp/project")]);

        assert_eq!(
            roots(),
            vec![
                PathBuf::from("/opt/pine-libs"),
                PathBuf::from("/tmp/project")
            ]
        );

        // Opening another file replaces only the document's own root.
        set_document_roots(vec![PathBuf::from("/tmp/other")]);
        assert_eq!(
            roots(),
            vec![PathBuf::from("/opt/pine-libs"), PathBuf::from("/tmp/other")]
        );

        reset();
    }

    #[test]
    fn a_duplicate_root_is_only_listed_once() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        reset();

        set_configured_roots(vec![PathBuf::from("/tmp/project")]);
        set_document_roots(vec![PathBuf::from("/tmp/project")]);
        assert_eq!(roots(), vec![PathBuf::from("/tmp/project")]);

        reset();
    }

    #[test]
    fn libraries_directories_are_found_walking_up() {
        let base = std::env::temp_dir().join(format!("pineditor-roots-{}", std::process::id()));
        let script_dir = base.join("tests/testdata/import");
        let libraries = base.join("tests/testdata/libraries");
        std::fs::create_dir_all(&script_dir).expect("create script dir");
        std::fs::create_dir_all(&libraries).expect("create libraries dir");

        let found = libraries_above(&script_dir);
        assert_eq!(found.first(), Some(&libraries), "{found:?}");
        // `import/` itself has no `libraries/` child, so it must not appear.
        assert!(!found.contains(&script_dir.join("libraries")));

        std::fs::remove_dir_all(&base).ok();
    }

    fn site(path: &str, line: u32, character: u32) -> JumpSite {
        JumpSite {
            path: Some(PathBuf::from(path)),
            position: lsp_types::Position::new(line, character),
            source: format!("// {path}\n"),
        }
    }

    #[test]
    fn jump_sites_come_back_in_reverse_order() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        while pop_jump_site().is_some() {}

        push_jump_site(site("/tmp/a.pine", 1, 0));
        push_jump_site(site("/tmp/a.pine", 5, 4));
        assert_eq!(jump_site_count(), 2);

        assert_eq!(pop_jump_site(), Some(site("/tmp/a.pine", 5, 4)));
        assert_eq!(pop_jump_site(), Some(site("/tmp/a.pine", 1, 0)));
        assert_eq!(pop_jump_site(), None, "an empty stack yields nothing");
    }

    #[test]
    fn the_same_place_is_only_remembered_once() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        while pop_jump_site().is_some() {}

        push_jump_site(site("/tmp/a.pine", 1, 0));
        push_jump_site(site("/tmp/a.pine", 1, 0));
        assert_eq!(jump_site_count(), 1, "a repeated origin is not a new stop");

        // A different position in the same file is a different stop.
        push_jump_site(site("/tmp/a.pine", 2, 0));
        assert_eq!(jump_site_count(), 2);

        while pop_jump_site().is_some() {}
    }

    #[test]
    fn the_fixture_contributes_roots_from_its_own_directory() {
        let roots = fixture_roots();
        let fixture = fixture_path().expect("fixtures/example.pine should exist");

        assert_eq!(roots.first().map(PathBuf::as_path), fixture.parent(), "{roots:?}");
    }

    #[test]
    fn the_startup_directory_survives_a_changed_cwd() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let started_at = std::env::current_dir().expect("a cwd");

        set_startup_dir(started_at.clone());
        assert_eq!(startup_dir(), Some(started_at));
    }

    #[test]
    fn the_jump_stack_is_capped() {
        let _guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        while pop_jump_site().is_some() {}

        for line in 0..(MAX_JUMP_SITES as u32 + 10) {
            push_jump_site(site("/tmp/a.pine", line, 0));
        }
        assert_eq!(jump_site_count(), MAX_JUMP_SITES);

        // The oldest entries went first, so the newest is still on top.
        let newest = pop_jump_site().expect("an origin");
        assert_eq!(newest.position.line, MAX_JUMP_SITES as u32 + 9);

        while pop_jump_site().is_some() {}
    }

    #[test]
    fn library_roots_come_from_flags_and_the_environment() {
        let args = vec![
            "--lib".to_string(),
            "/opt/libs".to_string(),
            "--lib=/srv/more".to_string(),
            "--dump".to_string(),
        ];

        assert_eq!(
            parse_roots(&args, Some("/env/one:/env/two")),
            vec![
                PathBuf::from("/opt/libs"),
                PathBuf::from("/srv/more"),
                PathBuf::from("/env/one"),
                PathBuf::from("/env/two"),
            ]
        );
    }

    #[test]
    fn a_relative_library_root_is_anchored_at_startup() {
        // The CWD moves later (hover changes it), so a relative root would start
        // pointing somewhere else. It must be absolute from the start.
        let roots = parse_roots(&["--lib".to_string(), "vendor/libs".to_string()], None);

        assert_eq!(roots.len(), 1);
        assert!(roots[0].is_absolute(), "{:?}", roots[0]);
        assert!(roots[0].ends_with("vendor/libs"), "{:?}", roots[0]);
    }

    #[test]
    fn an_empty_lib_argument_is_not_a_root() {
        // `--lib ""` and `PINEDITOR_LIB_DIR=""` must not turn into the CWD.
        let args = vec!["--lib".to_string(), String::new()];
        assert!(parse_roots(&args, Some("")).is_empty());
        assert!(parse_roots(&[], None).is_empty());
    }

    #[test]
    fn a_scripts_roots_start_with_its_own_directory() {
        let base = std::env::temp_dir().join(format!("pineditor-file-{}", std::process::id()));
        let script_dir = base.join("tests/testdata/import");
        let libraries = base.join("tests/testdata/libraries");
        std::fs::create_dir_all(&script_dir).expect("create script dir");
        std::fs::create_dir_all(&libraries).expect("create libraries dir");

        let roots = roots_for_file(&script_dir.join("basic_import.pine"));
        assert_eq!(roots.first(), Some(&script_dir), "{roots:?}");
        assert_eq!(roots.get(1), Some(&libraries), "{roots:?}");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_directory_without_libraries_above_yields_nothing() {
        let root = std::env::temp_dir().join(format!("pineditor-bare-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create temp dir");

        assert!(libraries_above(&root).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }
}
