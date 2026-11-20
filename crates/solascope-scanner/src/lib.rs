//! Source discovery and context reads share the same root, ignore, size and
//! privacy rules. Capability-relative opens keep source reads inside the root
//! even when another process replaces a directory with a symlink during a scan.
use cap_std::{ambient_authority, fs::Dir};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    WalkBuilder,
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use solascope_config::Config;
use solascope_core::{hash_bytes, validate_relative_path, Error, Result, SourceFile};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::mpsc::{self, UnboundedReceiver};

#[derive(Debug)]
pub struct ScanResult {
    pub files: Vec<SourceFile>,
    /// Counts rejected entries, including pruned directories as one entry;
    /// descendants of a pruned directory are never enumerated.
    pub skipped: usize,
    pub warnings: Vec<String>,
}

/// A failed directory traversal, source read or ignore-rule read aborts the scan.
/// Callers must never publish deletions from an incomplete scan.
pub fn scan(config: &Config) -> Result<ScanResult> {
    let policy = Arc::new(Policy::new(config)?);
    let skipped = Arc::new(AtomicUsize::new(0));
    let errors: Arc<Mutex<Vec<Error>>> = Arc::new(Mutex::new(Vec::new()));
    let mut walk = WalkBuilder::new(&policy.root);
    // Load ignore files ourselves through the confined directory handle. This
    // also prevents reading parent/global ignore files outside the project.
    walk.standard_filters(false)
        .follow_links(config.privacy.symlink_policy == "within_root")
        .sort_by_file_path(|left, right| left.cmp(right));
    let filter_policy = Arc::clone(&policy);
    let filter_skipped = Arc::clone(&skipped);
    let filter_errors = Arc::clone(&errors);
    walk.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let decision = entry
            .path()
            .strip_prefix(&filter_policy.root)
            .map_err(|_| Error::Privacy("scan entry escaped project root".into()))
            .and_then(|relative| {
                filter_policy.allowed(
                    relative,
                    entry.file_type().is_some_and(|ty| ty.is_dir()),
                    false,
                )
            });
        match decision {
            Ok(true) => true,
            Ok(false) => {
                filter_skipped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(error) => {
                filter_errors.lock().unwrap().push(error);
                false
            }
        }
    });
    let mut files = Vec::new();
    for entry in walk.build() {
        let entry = entry.map_err(|_| {
            Error::Io(io::Error::other(
                "source directory traversal failed; scan aborted",
            ))
        })?;
        if let Some(error) = errors.lock().unwrap().pop() {
            return Err(error);
        }
        if entry.depth() == 0 || entry.file_type().is_some_and(|ty| ty.is_dir()) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&policy.root)
            .map_err(|_| Error::Privacy("scan entry escaped project root".into()))?;
        match policy.read(relative)? {
            Some(file) => files.push(file),
            None => {
                skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if let Some(error) = errors.lock().unwrap().pop() {
        return Err(error);
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(ScanResult {
        files,
        skipped: skipped.load(Ordering::Relaxed),
        warnings: vec![],
    })
}

/// Read only a valid, allowed UTF-8 source file. Both slash styles are accepted
/// as separators; Windows drive paths, UNC paths and `..` are always rejected.
pub fn read_file(config: &Config, relative_path: &str) -> Result<SourceFile> {
    validate_relative_path(relative_path)?;
    let normalized = relative_path.replace('\\', "/");
    let policy = Policy::new(config)?;
    policy.read(Path::new(&normalized))?.ok_or_else(|| {
        Error::Privacy(
            "requested path is excluded, sensitive, binary, oversized or not a regular file".into(),
        )
    })
}

/// Start a watcher only when explicitly called. The returned handle must be
/// retained by the caller; dropping it stops delivery. No task is spawned here.
pub fn watch(
    root: &Path,
) -> Result<(RecommendedWatcher, UnboundedReceiver<notify::Result<Event>>)> {
    let root = fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(Error::InvalidInput("watch root must be a directory".into()));
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .map_err(|_| Error::Io(io::Error::other("failed to initialize filesystem watcher")))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|_| Error::Io(io::Error::other("failed to watch project directory")))?;
    Ok((watcher, receiver))
}

/// Removed paths are checked lexically and through their existing ancestors;
/// an event can still matter after its source file has disappeared.
pub fn event_is_relevant(config: &Config, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let Ok(policy) = Policy::new(config) else {
        return false;
    };
    event.paths.iter().any(|path| {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            policy.root.join(path)
        };
        let Ok(relative) = path.strip_prefix(&policy.root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return true;
        }
        // Changes to ignore rules can add/remove many files from the index.
        let ignore_changed = matches!(
            relative.file_name().and_then(|name| name.to_str()),
            Some(".gitignore" | ".solascopeignore")
        );
        if ignore_changed {
            if validate_relative_path(&relative.to_string_lossy()).is_err()
                || policy.hard_filtered(relative)
            {
                return false;
            }
            let parent = relative.parent().unwrap_or(Path::new(""));
            return parent.as_os_str().is_empty()
                || policy.allowed(parent, true, true).unwrap_or(false);
        }
        let is_dir = path.is_dir() || !path.exists();
        policy.allowed(relative, is_dir, true).unwrap_or(false)
    })
}

struct Policy {
    root: PathBuf,
    directory: Dir,
    index: PathBuf,
    log: Option<PathBuf>,
    include: GlobSet,
    exclude: GlobSet,
    sensitive: GlobSet,
    follow_links: bool,
    max_file_size: u64,
    ignores: Mutex<BTreeMap<PathBuf, Vec<Gitignore>>>,
}

impl Policy {
    fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let root = fs::canonicalize(&config.project.root)?;
        let directory = Dir::open_ambient_dir(&root, ambient_authority())?;
        let index = match fs::canonicalize(&config.project.index_dir) {
            Ok(index) => index,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                config.project.index_dir.clone()
            }
            Err(error) => return Err(error.into()),
        };
        if root.starts_with(&index) {
            return Err(Error::Config(
                "resolved index directory must not contain the project root".into(),
            ));
        }
        Ok(Self {
            root,
            directory,
            index,
            log: Some({
                let path = config.diagnostic_log_path();
                path.canonicalize().unwrap_or(path)
            }),
            include: globs(&config.project.include)?,
            exclude: globs(&config.project.exclude)?,
            sensitive: globs(&config.privacy.sensitive_path_patterns)?,
            follow_links: config.privacy.symlink_policy == "within_root",
            max_file_size: config.project.max_file_size,
            ignores: Mutex::new(BTreeMap::new()),
        })
    }

    fn allowed(&self, relative: &Path, is_dir: bool, allow_missing: bool) -> Result<bool> {
        let Some(text) = relative.to_str() else {
            return Ok(false);
        };
        validate_relative_path(text)?;
        if self.filtered(relative, is_dir)? {
            return Ok(false);
        }
        let mut current = PathBuf::new();
        for component in relative.components() {
            current.push(component);
            let absolute = self.root.join(&current);
            match fs::symlink_metadata(&absolute) {
                Ok(meta) if meta.file_type().is_symlink() && !self.follow_links => {
                    return Ok(false)
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => break,
                Err(error) => return Err(error.into()),
            }
            // Check every existing ancestor, so a missing child of an escaping
            // directory symlink cannot become a relevant watcher event.
            let canonical = fs::canonicalize(&absolute)?;
            let Ok(target_relative) = canonical.strip_prefix(&self.root) else {
                return Ok(false);
            };
            if canonical.starts_with(&self.index) || self.hard_filtered(target_relative) {
                return Ok(false);
            }
        }
        let absolute = self.root.join(relative);
        match fs::canonicalize(&absolute) {
            Ok(canonical) => {
                let Ok(target_relative) = canonical.strip_prefix(&self.root) else {
                    return Ok(false);
                };
                if target_relative != relative && self.filtered(target_relative, is_dir)? {
                    return Ok(false);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => {}
            Err(error) => return Err(error.into()),
        }
        Ok(true)
    }

    fn hard_filtered(&self, relative: &Path) -> bool {
        let Some(text) = relative.to_str() else {
            return true;
        };
        let text = text.replace('\\', "/");
        let mut prefix = PathBuf::new();
        for component in text.split('/') {
            prefix.push(component);
            let name = component.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                ".git"
                    | ".hg"
                    | ".svn"
                    | ".solascope"
                    | "target"
                    | "node_modules"
                    | "vendor"
                    | "deps"
                    | "build"
                    | "dist"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".mypy_cache"
                    | ".pytest_cache"
                    | ".next"
                    | "coverage"
                    | ".cache"
                    | ".ssh"
                    | ".aws"
                    | ".gnupg"
                    | ".kube"
                    | "secrets"
                    | "credentials"
                    | ".solascope.toml"
                    | ".npmrc"
                    | ".pypirc"
                    | ".netrc"
                    | "credentials.json"
                    | "id_rsa"
                    | "id_dsa"
                    | "id_ecdsa"
                    | "id_ed25519"
            ) || name == ".env"
                || name.starts_with(".env.")
                || [".pem", ".key", ".p12", ".pfx", ".keystore"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                || self.sensitive.is_match(&prefix)
                || self.exclude.is_match(&prefix)
            {
                return true;
            }
        }
        let absolute = self.root.join(relative);
        absolute.starts_with(&self.index) || self.log.as_ref().is_some_and(|log| log == &absolute)
    }

    fn filtered(&self, relative: &Path, is_dir: bool) -> Result<bool> {
        if self.hard_filtered(relative) {
            return Ok(true);
        }
        if !is_dir && !self.include.is_empty() && !self.include.is_match(relative) {
            return Ok(true);
        }
        self.ignored(relative, is_dir)
    }

    fn ignored(&self, relative: &Path, is_dir: bool) -> Result<bool> {
        let mut directories: Vec<_> = relative
            .parent()
            .unwrap_or(Path::new(""))
            .ancestors()
            .collect();
        directories.reverse();
        let mut ignored = false;
        let mut cache = self.ignores.lock().unwrap();
        for directory in directories {
            if !cache.contains_key(directory) {
                let mut rules = Vec::new();
                for name in [".gitignore", ".solascopeignore"] {
                    rules.push(self.load_ignore(directory, name)?);
                }
                cache.insert(directory.to_path_buf(), rules);
            }
            for rule in &cache[directory] {
                let matched = rule.matched_path_or_any_parents(self.root.join(relative), is_dir);
                if matched.is_ignore() {
                    ignored = true;
                } else if matched.is_whitelist() {
                    ignored = false;
                }
            }
        }
        Ok(ignored)
    }

    fn load_ignore(&self, directory: &Path, name: &str) -> Result<Gitignore> {
        let path = directory.join(name);
        let mut builder = GitignoreBuilder::new(self.root.join(directory));
        // The lexical path and resolved target must both stay inside the root.
        // Absolute symlinks within the root are resolved to relative paths
        // before the capability open, which still protects against races.
        match fs::symlink_metadata(self.root.join(&path)) {
            Ok(meta) if meta.file_type().is_symlink() && !self.follow_links => {}
            Ok(_) => {
                let canonical = fs::canonicalize(self.root.join(&path))?;
                let relative = canonical
                    .strip_prefix(&self.root)
                    .map_err(|_| Error::Privacy("ignore file escapes project root".into()))?;
                if self.hard_filtered(relative) {
                    return Err(Error::Privacy(
                        "ignore file resolves to an excluded path".into(),
                    ));
                }
                let meta = self.directory.metadata(relative)?;
                if !meta.is_file() || meta.len() > 1_048_576 {
                    return Err(Error::Config(
                        "ignore rules must be a regular file below the size limit".into(),
                    ));
                }
                let mut text = String::new();
                self.directory
                    .open(relative)?
                    .take(1_048_577)
                    .read_to_string(&mut text)?;
                if text.len() > 1_048_576 {
                    return Err(Error::Config("ignore file exceeds size limit".into()));
                }
                for line in text.lines() {
                    builder
                        .add_line(None, line)
                        .map_err(|_| Error::Config("invalid ignore rule; scan aborted".into()))?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        builder
            .build()
            .map_err(|_| Error::Config("invalid ignore rules; scan aborted".into()))
    }

    fn read(&self, relative: &Path) -> Result<Option<SourceFile>> {
        if !self.allowed(relative, false, false)? {
            return Ok(None);
        }
        let canonical = fs::canonicalize(self.root.join(relative))?;
        let target = canonical
            .strip_prefix(&self.root)
            .map_err(|_| Error::Privacy("source escaped project root".into()))?;
        // This open is capability-relative, rather than a check-then-open on
        // the ambient path. A concurrent symlink swap cannot read outside root.
        let preliminary = self.directory.metadata(target)?;
        if !preliminary.is_file() || preliminary.len() > self.max_file_size {
            return Ok(None);
        }
        let file = self.directory.open(target)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() > self.max_file_size {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(self.max_file_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > self.max_file_size || binary(&bytes) {
            return Ok(None);
        }
        let Ok(content) = String::from_utf8(bytes) else {
            return Ok(None);
        };
        if content
            .lines()
            .any(|line| line.starts_with("-----BEGIN ") && line.contains("PRIVATE KEY-----"))
        {
            return Ok(None);
        }
        // Recheck policy after reading to catch ordinary symlink and ignore
        // changes. Capability confinement is what enforces the root boundary.
        if !self.allowed(relative, false, false)? {
            return Ok(None);
        }
        let after = self.directory.metadata(target)?;
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err(Error::SourceChanged(
                relative.to_string_lossy().into_owned(),
            ));
        }
        Ok(Some(SourceFile {
            relative_path: relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            absolute_path: self.root.join(relative),
            hash: hash_bytes(content.as_bytes()),
            content,
        }))
    }
}

fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|_| Error::Config("invalid path glob".into()))?);
    }
    builder
        .build()
        .map_err(|_| Error::Config("invalid path glob set".into()))
}

fn binary(bytes: &[u8]) -> bool {
    // UTF-8 alone is insufficient: binary headers and control-only payloads can
    // be valid UTF-8. Permit ordinary source whitespace, reject other controls.
    bytes.iter().any(|byte| {
        *byte == 0
            || (*byte < 0x20 && !matches!(*byte, b'\n' | b'\r' | b'\t' | 0x0c))
            || *byte == 0x7f
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{tempdir, TempDir};

    fn fixture() -> (TempDir, Config) {
        let root = tempdir().unwrap();
        let config = Config::default_for_root(root.path()).unwrap();
        (root, config)
    }

    fn write(root: &Path, path: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn paths(result: &ScanResult) -> Vec<&str> {
        result
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect()
    }

    #[test]
    fn stable_text_scan_and_hash() {
        let (root, config) = fixture();
        write(root.path(), "z.rs", "fn z() {}\n");
        write(root.path(), "a/hello.py", "print('你好')\n");
        write(root.path(), "empty.txt", "");
        let result = scan(&config).unwrap();
        assert_eq!(paths(&result), ["a/hello.py", "empty.txt", "z.rs"]);
        assert_eq!(result.files[2].hash, hash_bytes(b"fn z() {}\n"));
        assert_eq!(result.files[2].absolute_path, root.path().join("z.rs"));
        assert_eq!(paths(&scan(&config).unwrap()), paths(&result));
        assert_eq!(
            read_file(&config, "a\\hello.py").unwrap().relative_path,
            "a/hello.py"
        );
    }

    #[test]
    fn honors_gitignore_and_custom_ignore_without_a_git_repository() {
        let (root, config) = fixture();
        write(root.path(), ".gitignore", "*.tmp\nignored/\n!keep.tmp\n");
        write(root.path(), ".solascopeignore", "custom.rs\n");
        write(root.path(), "nested/.gitignore", "local.rs\n");
        for path in [
            "drop.tmp",
            "keep.tmp",
            "ignored/hidden.rs",
            "custom.rs",
            "nested/local.rs",
            "nested/ok.rs",
        ] {
            write(root.path(), path, "source");
        }
        let result = scan(&config).unwrap();
        assert!(paths(&result).contains(&"keep.tmp"));
        assert!(paths(&result).contains(&"nested/ok.rs"));
        for path in [
            "drop.tmp",
            "ignored/hidden.rs",
            "custom.rs",
            "nested/local.rs",
        ] {
            assert!(!paths(&result).contains(&path), "{path}");
            assert!(read_file(&config, path).is_err(), "{path}");
        }
    }

    #[test]
    fn builtin_privacy_and_noise_cannot_be_unignored() {
        let (root, config) = fixture();
        write(root.path(), ".solascopeignore", "!*\n");
        for path in [
            ".env",
            ".env.local",
            "nested/.env.production",
            "cert.pem",
            "private.key",
            ".ssh/id_rsa",
            "id_ed25519",
            ".aws/credentials",
            ".git/config",
            "target/code.rs",
            "node_modules/code.js",
            ".solascope/index.db",
            "deps/code.go",
            "secrets/token.txt",
        ] {
            write(root.path(), path, "secret");
            assert!(read_file(&config, path).is_err(), "{path}");
        }
        write(
            root.path(),
            "disguised.txt",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n",
        );
        assert!(read_file(&config, "disguised.txt").is_err());
        write(root.path(), "src/lib.rs", "pub fn ok() {}\n");
        let result = scan(&config).unwrap();
        assert!(paths(&result).contains(&"src/lib.rs"));
        assert!(result
            .files
            .iter()
            .all(|file| !file.content.contains("secret")));
    }

    #[test]
    fn configured_include_exclude_sensitive_and_index_filters() {
        let (root, mut config) = fixture();
        config.project.include = vec!["**/*.rs".into()];
        config.project.exclude = vec!["generated".into()];
        config.privacy.sensitive_path_patterns = vec!["private/**".into()];
        config.project.index_dir = root.path().join("custom-index");
        for path in [
            "src/lib.rs",
            "notes.txt",
            "generated/code.rs",
            "private/data.rs",
            "custom-index/chunks.rs",
        ] {
            write(root.path(), path, "text");
        }
        assert_eq!(paths(&scan(&config).unwrap()), ["src/lib.rs"]);
        for path in [
            "notes.txt",
            "generated/code.rs",
            "private/data.rs",
            "custom-index/chunks.rs",
        ] {
            assert!(read_file(&config, path).is_err());
        }
    }

    #[test]
    fn skips_non_utf8_binary_and_large_files() {
        let (root, mut config) = fixture();
        config.project.max_file_size = 8;
        write(root.path(), "valid.txt", "12345678");
        write(root.path(), "large.rs", "123456789");
        write(root.path(), "null.bin", b"hi\0");
        write(root.path(), "control.bin", [1, 2, 3]);
        write(root.path(), "invalid.bin", [0xff, 0xfe]);
        assert_eq!(paths(&scan(&config).unwrap()), ["valid.txt"]);
        for path in ["large.rs", "null.bin", "control.bin", "invalid.bin"] {
            assert!(read_file(&config, path).is_err());
        }
    }

    #[test]
    fn rejects_escape_and_windows_absolute_paths() {
        let (_, config) = fixture();
        for path in [
            "../secret",
            "a/../../secret",
            "a\\..\\secret",
            "/etc/passwd",
            "C:\\Users\\secret",
            "C:secret",
            "\\\\host\\share\\secret",
            "",
            "a\0b",
        ] {
            assert!(read_file(&config, path).is_err(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_default_skip_and_within_root_never_escape_or_alias_secrets() {
        use std::os::unix::fs::symlink;
        let (root, mut config) = fixture();
        let outside = tempdir().unwrap();
        write(outside.path(), "secret.rs", "outside");
        write(root.path(), "real.rs", "inside");
        write(root.path(), ".env", "sensitive");
        write(root.path(), "real_dir/inner.rs", "nested");
        symlink(
            outside.path().join("secret.rs"),
            root.path().join("escape.rs"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("escape_dir")).unwrap();
        symlink(root.path().join("real.rs"), root.path().join("link.rs")).unwrap();
        symlink(".env", root.path().join("alias.rs")).unwrap();
        symlink("real_dir", root.path().join("linked_dir")).unwrap();
        assert!(read_file(&config, "link.rs").is_err());
        assert!(read_file(&config, "linked_dir/inner.rs").is_err());
        assert!(!paths(&scan(&config).unwrap()).contains(&"link.rs"));
        config.privacy.symlink_policy = "within_root".into();
        assert_eq!(read_file(&config, "link.rs").unwrap().content, "inside");
        assert_eq!(
            read_file(&config, "linked_dir/inner.rs").unwrap().content,
            "nested"
        );
        for path in ["escape.rs", "escape_dir/secret.rs", "alias.rs"] {
            assert!(read_file(&config, path).is_err());
        }
        let result = scan(&config).unwrap();
        assert!(paths(&result).contains(&"link.rs"));
        assert!(!result
            .files
            .iter()
            .any(|file| file.content.contains("outside") || file.content.contains("sensitive")));
    }

    #[cfg(unix)]
    #[test]
    fn ignore_symlinks_cannot_read_outside_root() {
        use std::os::unix::fs::symlink;
        let (root, mut config) = fixture();
        let outside = tempdir().unwrap();
        write(outside.path(), "rules", "*.rs");
        write(root.path(), "ok.rs", "text");
        symlink(outside.path().join("rules"), root.path().join(".gitignore")).unwrap();
        assert!(paths(&scan(&config).unwrap()).contains(&"ok.rs"));
        config.privacy.symlink_policy = "within_root".into();
        assert!(scan(&config).is_err());
    }

    #[test]
    fn invalid_ignore_and_missing_sources_are_errors_not_partial_success() {
        let (root, config) = fixture();
        write(root.path(), ".gitignore", "[z-a]");
        write(root.path(), "ok.rs", "source");
        assert!(scan(&config).is_err());
        assert!(read_file(&config, "missing.rs").is_err());
    }

    #[test]
    fn watch_event_filter_covers_deletions_noise_and_ignore_changes() {
        let (root, config) = fixture();
        let event = |path: &str| Event::new(EventKind::Any).add_path(root.path().join(path));
        assert!(event_is_relevant(&config, &event("src/deleted.rs")));
        assert!(event_is_relevant(&config, &event(".gitignore")));
        assert!(event_is_relevant(&config, &event(".solascopeignore")));
        for path in [
            ".git/index",
            "target/a.rs",
            "deps/a.rs",
            ".solascope/data.db",
            ".env",
            "../outside.rs",
        ] {
            assert!(!event_is_relevant(&config, &event(path)), "{path}");
        }
        let event = Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(root.path().join("src.rs"));
        assert!(!event_is_relevant(&config, &event));
    }

    #[test]
    fn automatic_configuration_logs_cannot_be_indexed_or_read() {
        let (root, mut config) = fixture();
        write(root.path(), "src/lib.rs", "fn read() {}");
        write(
            root.path(),
            "logs/solascope.log",
            "private diagnostic event",
        );
        config.log.directory = Some(root.path().to_path_buf());
        config.project.include = vec!["**/*".into()];
        assert_eq!(paths(&scan(&config).unwrap()), ["src/lib.rs"]);
        assert!(read_file(&config, "logs/solascope.log").is_err());
        config.log.persist = false;
        assert_eq!(paths(&scan(&config).unwrap()), ["src/lib.rs"]);
        assert!(read_file(&config, "logs/solascope.log").is_err());
    }

    #[test]
    fn configured_diagnostic_logs_cannot_be_included_or_read_as_source() {
        let (root, mut config) = fixture();
        write(root.path(), "src/lib.rs", "fn read() {}");
        write(root.path(), "notes.txt", "project documentation");
        write(root.path(), "diagnostics.txt", "private diagnostic event");
        config.project.include = vec!["**/*".into()];
        config.log.output = root
            .path()
            .join("diagnostics.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(paths(&scan(&config).unwrap()), ["notes.txt", "src/lib.rs"]);
        assert!(read_file(&config, "diagnostics.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn aliases_of_a_configured_log_are_also_excluded() {
        use std::os::unix::fs::symlink;
        let (root, mut config) = fixture();
        write(
            root.path(),
            "logs/diagnostics.txt",
            "private diagnostic event",
        );
        write(root.path(), "source.rs", "fn source() {}");
        symlink("logs/diagnostics.txt", root.path().join("alias.rs")).unwrap();
        config.privacy.symlink_policy = "within_root".into();
        config.log.output = root
            .path()
            .join("logs/diagnostics.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(paths(&scan(&config).unwrap()), ["source.rs"]);
        assert!(read_file(&config, "alias.rs").is_err());
    }
}
