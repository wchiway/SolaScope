use std::{
    collections::BTreeMap,
    env::VarError,
    fmt,
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use cap_std::{ambient_authority, fs::Dir};
use solascope_core::{Error, Result};

const MAX_ENV_BYTES: u64 = 64 * 1024;

/// Configuration-local defaults, never injected into the process environment.
#[derive(Clone, Default)]
pub struct Environment {
    values: Arc<BTreeMap<String, String>>,
}

impl fmt::Debug for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Environment { dotenv: [REDACTED] }")
    }
}

impl Environment {
    /// Resolve live process values first, including explicitly empty values.
    pub fn var(&self, name: &str) -> std::result::Result<String, VarError> {
        match std::env::var(name) {
            Err(VarError::NotPresent) => self.values.get(name).cloned().ok_or(VarError::NotPresent),
            value => value,
        }
    }

    /// Include file defaults even when a process value overrides the same name.
    pub fn values_for_redaction(&self, name: &str) -> Vec<String> {
        std::env::var(name)
            .ok()
            .into_iter()
            .chain(self.values.get(name).cloned())
            .filter(|value| !value.is_empty())
            .collect()
    }

    pub(crate) fn overlay(&self, other: &Self) -> Self {
        let mut values = (*self.values).clone();
        values.extend(
            other
                .values
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        Self {
            values: Arc::new(values),
        }
    }

    pub(crate) fn file_values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    pub(crate) fn load(directory: &Path) -> Result<Self> {
        let directory = match Dir::open_ambient_dir(directory, ambient_authority()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => {
                return Err(Error::Config(
                    "could not inspect configuration .env directory".into(),
                ))
            }
        };
        let metadata = match directory.symlink_metadata(".env") {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err(Error::Config("could not inspect configuration .env".into())),
        };
        if metadata.file_type().is_symlink() {
            return Err(Error::Privacy(
                "configuration .env cannot be a symbolic link".into(),
            ));
        }
        if !metadata.is_file() {
            return Err(Error::Config(
                "configuration .env must be a regular file".into(),
            ));
        }
        if metadata.len() > MAX_ENV_BYTES {
            return Err(Error::Config("configuration .env exceeds 64 KiB".into()));
        }
        let file = directory
            .open(".env")
            .map_err(|_| Error::Config("could not open configuration .env".into()))?;
        if !file.metadata()?.is_file() {
            return Err(Error::Config(
                "configuration .env must be a regular file".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.into_std()
            .take(MAX_ENV_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Config("could not read configuration .env".into()))?;
        if bytes.len() as u64 > MAX_ENV_BYTES {
            return Err(Error::Config("configuration .env exceeds 64 KiB".into()));
        }
        let source = String::from_utf8(bytes).map_err(|_| {
            Error::Config("configuration .env must be UTF-8 (contents redacted)".into())
        })?;
        let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
        let mut values = BTreeMap::new();
        for item in dotenvy::from_read_iter(source.as_bytes()) {
            let (name, value) = item.map_err(|_| {
                Error::Config("invalid configuration .env syntax (contents redacted)".into())
            })?;
            values.insert(name, value);
        }
        Ok(Self {
            values: Arc::new(values),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_exports_and_multiline_values_without_changing_process_state() {
        let directory = tempfile::tempdir().unwrap();
        let name = "SOLASCOPE_DOTENV_ONLY_TEST_7B44";
        let before = std::env::var_os(name);
        std::fs::write(directory.path().join(".env"), format!(
            "\u{feff}# credential defaults\nexport {name}='quoted # secret'\nMULTILINE=\"first\nsecond\"\nDUPLICATE=first\nDUPLICATE=last\n"
        )).unwrap();
        let environment = Environment::load(directory.path()).unwrap();
        assert_eq!(environment.file_values()[name], "quoted # secret");
        assert_eq!(environment.file_values()["MULTILINE"], "first\nsecond");
        assert_eq!(environment.file_values()["DUPLICATE"], "last");
        assert_eq!(std::env::var_os(name), before);
        assert!(!format!("{environment:?}").contains("quoted # secret"));
    }

    #[test]
    fn process_values_win_and_both_sources_are_available_for_redaction() {
        let directory = tempfile::tempdir().unwrap();
        let name = "SOLASCOPE_DOTENV_PROCESS_TEST_391C";
        let previous = std::env::var_os(name);
        std::fs::write(
            directory.path().join(".env"),
            format!("{name}=file-value\n"),
        )
        .unwrap();
        let environment = Environment::load(directory.path()).unwrap();
        std::env::set_var(name, "process-value");
        assert_eq!(environment.var(name).unwrap(), "process-value");
        assert_eq!(
            environment.values_for_redaction(name),
            ["process-value", "file-value"]
        );
        std::env::set_var(name, "");
        assert_eq!(environment.var(name).unwrap(), "");
        match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn missing_files_are_optional_and_invalid_contents_are_redacted() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Environment::load(directory.path())
            .unwrap()
            .file_values()
            .is_empty());
        for bytes in [b"API_KEY='secret-without-closing-quote".as_slice(), &[0xff]] {
            std::fs::write(directory.path().join(".env"), bytes).unwrap();
            let error = Environment::load(directory.path()).unwrap_err();
            assert!(matches!(error, Error::Config(_)));
            assert!(!format!("{error:?} {error}").contains("secret-without-closing-quote"));
        }
    }

    #[test]
    fn rejects_directories_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".env")).unwrap();
        assert!(matches!(
            Environment::load(directory.path()),
            Err(Error::Config(_))
        ));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(".env"),
            vec![b'a'; MAX_ENV_BYTES as usize + 1],
        )
        .unwrap();
        assert!(matches!(
            Environment::load(directory.path()),
            Err(Error::Config(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_dotenv_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("key-file"), "KEY=outside-secret").unwrap();
        symlink(
            outside.path().join("key-file"),
            directory.path().join(".env"),
        )
        .unwrap();
        assert!(matches!(
            Environment::load(directory.path()),
            Err(Error::Privacy(_))
        ));
    }
}
