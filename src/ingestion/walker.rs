//! Gitignore-aware parallel repo scanner (spec §2.2 `walker.rs`).

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use ignore::{WalkBuilder, WalkState};

/// Files larger than this are skipped; they are almost always generated or vendored.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Go,
    Ruby,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        if matches!(path.file_name().and_then(|n| n.to_str()), Some("Rakefile" | "Gemfile" | "Guardfile")) {
            return Some(Self::Ruby);
        }
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            "rb" | "rake" | "gemspec" => Some(Self::Ruby),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::Ruby => "ruby",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Forward-slash path relative to the repo root.
    pub rel_path: String,
    pub language: Language,
    pub source: String,
    pub blake3: String,
    pub line_count: usize,
}

/// Returns every supported source file under `repo_root`, sorted by path so downstream
/// ID assignment and linking are deterministic.
pub fn scan(repo_root: &Path) -> Result<Vec<SourceFile>> {
    let found = Mutex::new(Vec::new());

    WalkBuilder::new(repo_root)
        .filter_entry(|e| e.file_name() != ".code_atlas" && e.file_name() != ".git")
        .build_parallel()
        .run(|| {
            let found = &found;
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let Some(language) = Language::from_path(entry.path()) else {
                    return WalkState::Continue;
                };
                if entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                    return WalkState::Continue;
                }
                // Non-UTF-8 files can't be sliced by byte range safely; skip them.
                let Ok(source) = std::fs::read_to_string(entry.path()) else {
                    return WalkState::Continue;
                };
                let Ok(rel) = entry.path().strip_prefix(repo_root) else {
                    return WalkState::Continue;
                };
                let rel_path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");

                let file = SourceFile {
                    rel_path,
                    language,
                    blake3: blake3::hash(source.as_bytes()).to_hex().to_string(),
                    line_count: source.lines().count(),
                    source,
                };
                found.lock().expect("walker mutex poisoned").push(file);
                WalkState::Continue
            })
        });

    let mut files = found.into_inner().expect("walker mutex poisoned");
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn finds_supported_files_sorted_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/b.rs", "fn b() {}\n");
        write(dir.path(), "a.py", "def a():\n    pass\n");
        write(dir.path(), "web/c.tsx", "export const c = 1;\n");
        write(dir.path(), "README.md", "# hi");

        let files = scan(dir.path()).unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, ["a.py", "src/b.rs", "web/c.tsx"]);
        assert_eq!(files[1].language, Language::Rust);
        assert_eq!(files[1].line_count, 1);
    }

    #[test]
    fn respects_gitignore_and_skips_atlas_dir() {
        let dir = tempfile::tempdir().unwrap();
        // `ignore` only honours .gitignore inside a git work tree unless told otherwise.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        write(dir.path(), ".gitignore", "generated/\n");
        write(dir.path(), "generated/x.rs", "fn x() {}");
        write(dir.path(), ".code_atlas/dump.py", "x = 1");
        write(dir.path(), "keep.rs", "fn k() {}");

        let files = scan(dir.path()).unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, ["keep.rs"]);
    }

    #[test]
    fn hash_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "fn a() {}");
        let h1 = scan(dir.path()).unwrap()[0].blake3.clone();
        write(dir.path(), "a.rs", "fn a() { 1; }");
        let h2 = scan(dir.path()).unwrap()[0].blake3.clone();
        assert_ne!(h1, h2);
    }
}
