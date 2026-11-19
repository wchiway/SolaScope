use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cap_std::fs::{Dir, OpenOptions};
use solascope_config::Config;
use solascope_core::{Error, Result};
use tracing_subscriber::fmt::writer::MakeWriter;

const HEADER: &[u8] = b"# SolaScope diagnostic log\n";

pub(crate) fn initialize(config: &Config, verbose: bool) -> Result<()> {
    let level = if verbose { "debug" } else { &config.log.level };
    // Detailed application logs must not implicitly enable protocol libraries
    // to print complete model or MCP request/response bodies.
    let defaults = if level == "off" {
        "off".to_owned()
    } else {
        let targets = [
            "solascope_cli",
            "solascope_core",
            "solascope_config",
            "solascope_scanner",
            "solascope_parser",
            "solascope_storage",
            "solascope_models",
            "solascope_engine",
        ];
        format!(
            "warn,{}",
            targets
                .iter()
                .map(|target| format!("{target}={level}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let filter = if verbose {
        tracing_subscriber::EnvFilter::new(defaults)
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(defaults))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(LogWriter::new(config)?)
        .with_ansi(false)
        .try_init()
        .map_err(|_| Error::Config("could not initialize diagnostic logging".into()))
}

#[derive(Clone)]
enum Destination {
    Stderr,
    File(Arc<Mutex<File>>),
}

#[derive(Clone)]
struct LogWriter {
    destination: Destination,
    key_names: Arc<Vec<String>>,
    redact_bearer: bool,
}

impl LogWriter {
    fn new(config: &Config) -> Result<Self> {
        let destination = if config.log.output == "stderr" {
            Destination::Stderr
        } else {
            Destination::File(Arc::new(Mutex::new(open_log(config)?)))
        };
        Ok(Self {
            destination,
            // Store names, not secrets. Resolve the values for each event so
            // rotated credentials are not accidentally recorded.
            key_names: Arc::new(vec![
                config.embedding.api_key_env.clone(),
                config.rerank.api_key_env.clone(),
            ]),
            redact_bearer: config.log.redact_secrets,
        })
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogEntry;

    fn make_writer(&'a self) -> Self::Writer {
        LogEntry {
            destination: self.destination.clone(),
            key_names: self.key_names.clone(),
            known_values: secret_values(&self.key_names),
            redact_bearer: self.redact_bearer,
            bytes: Vec::new(),
        }
    }
}

struct LogEntry {
    destination: Destination,
    key_names: Arc<Vec<String>>,
    known_values: Vec<String>,
    redact_bearer: bool,
    bytes: Vec<u8>,
}

impl Write for LogEntry {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        self.known_values.extend(secret_values(&self.key_names));
        let rendered = scrub(
            &String::from_utf8_lossy(&self.bytes),
            &self.known_values,
            self.redact_bearer,
        );
        self.bytes.clear();
        match &self.destination {
            Destination::Stderr => io::stderr().lock().write_all(rendered.as_bytes()),
            Destination::File(file) => {
                let mut file = file
                    .lock()
                    .map_err(|_| io::Error::other("log lock poisoned"))?;
                file.write_all(rendered.as_bytes())?;
                file.flush()
            }
        }
    }
}

impl Drop for LogEntry {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            eprintln!("warning: could not write diagnostic log: {error}");
        }
    }
}

fn secret_values(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
        .collect()
}

fn scrub(text: &str, known: &[String], redact_bearer: bool) -> String {
    let mut output = text.to_owned();
    // Configured API keys are always protected, even when general bearer-token
    // detection is disabled for diagnostic work.
    for value in known.iter().filter(|value| !value.is_empty()) {
        output = output.replace(value, "[REDACTED]");
    }
    if redact_bearer {
        let mut cursor = 0;
        while let Some(offset) = output[cursor..].to_ascii_lowercase().find("bearer ") {
            let start = cursor + offset + "bearer ".len();
            let length = output.as_bytes()[start..]
                .iter()
                .take_while(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(**byte, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
                })
                .count();
            if length == 0 {
                cursor = start;
            } else {
                output.replace_range(start..start + length, "[REDACTED]");
                cursor = start + "[REDACTED]".len();
            }
        }
    }
    output
}

// Resolve existing ancestors before choosing a capability-relative path. This
// handles missing directories, symlink aliases and Windows extended paths.
fn canonical_candidate(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path.to_path_buf();
    let mut tail = Vec::new();
    while !ancestor.try_exists()? {
        tail.push(
            ancestor
                .file_name()
                .ok_or_else(|| Error::Config("invalid log path".into()))?
                .to_os_string(),
        );
        if !ancestor.pop() {
            return Err(Error::Config("invalid log path".into()));
        }
    }
    let mut output = ancestor.canonicalize()?;
    for component in tail.into_iter().rev() {
        output.push(component);
    }
    Ok(output)
}

fn open_log(config: &Config) -> Result<File> {
    let root = config.project.root.canonicalize()?;
    let path = Path::new(&config.log.output);
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(Error::Privacy("log file cannot be a symbolic link".into()));
    }
    let candidate = canonical_candidate(path)?;
    let relative = candidate
        .strip_prefix(&root)
        .map_err(|_| Error::Privacy("log file must stay inside the project root".into()))?;
    solascope_core::validate_relative_path(&relative.to_string_lossy())?;
    if relative
        .components()
        .any(|component| component.as_os_str() == ".git")
    {
        return Err(Error::Privacy("cannot write logs into Git metadata".into()));
    }
    let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
    if let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        directory.create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match directory.open_with(relative, &options) {
        Ok(file) => {
            let mut file = file.into_std();
            file.write_all(HEADER)?;
            return Ok(file);
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut options = OpenOptions::new();
            options.read(true).append(true);
            directory.open_with(relative, &options)?.into_std()
        }
        Err(error) => return Err(error.into()),
    };
    let mut header = vec![0; HEADER.len()];
    if file.read_exact(&mut header).is_err() || header != HEADER {
        return Err(Error::Config(
            "existing log file is not a SolaScope log; choose a new path (file left unchanged)"
                .into(),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> Config {
        let mut config = Config::default_for_root(root).unwrap();
        config.log.output = root
            .join("logs/diagnostics.log")
            .to_string_lossy()
            .into_owned();
        config
    }

    #[test]
    fn protects_configured_secrets_and_optional_bearer_tokens() {
        let known = vec!["configured-secret".to_owned()];
        let text = "key=configured-secret Authorization: Bearer other-token.123";
        let protected = scrub(text, &known, true);
        assert!(!protected.contains("configured-secret"));
        assert!(!protected.contains("other-token"));
        let minimal = scrub(text, &known, false);
        assert!(!minimal.contains("configured-secret"));
        assert!(minimal.contains("other-token"));
    }

    #[test]
    fn buffers_events_so_split_writes_cannot_leak_secrets() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let writer = LogWriter::new(&config).unwrap();
        let mut entry = writer.make_writer();
        entry.known_values.push("split-secret".into());
        entry.write_all(b"key=split-").unwrap();
        entry.write_all(b"secret\n").unwrap();
        drop(entry);
        let content = std::fs::read_to_string(&config.log.output).unwrap();
        assert!(content.contains("[REDACTED]"));
        assert!(!content.contains("split-secret"));
    }

    #[test]
    fn appends_owned_logs_and_never_modifies_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        LogWriter::new(&config)
            .unwrap()
            .make_writer()
            .write_all(b"first\n")
            .unwrap();
        LogWriter::new(&config)
            .unwrap()
            .make_writer()
            .write_all(b"second\n")
            .unwrap();
        let text = std::fs::read_to_string(&config.log.output).unwrap();
        assert!(text.ends_with("first\nsecond\n"));
        std::fs::write(&config.log.output, "unrelated content").unwrap();
        assert!(LogWriter::new(&config).is_err());
        assert_eq!(
            std::fs::read_to_string(&config.log.output).unwrap(),
            "unrelated content"
        );
    }

    #[test]
    fn never_creates_logs_outside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        config.log.output = outside
            .path()
            .join("new.log")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(LogWriter::new(&config), Err(Error::Privacy(_))));
        assert!(!outside.path().join("new.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn parent_symlinks_cannot_escape_and_new_logs_are_private() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("logs")).unwrap();
        let config = config(root.path());
        assert!(LogWriter::new(&config).is_err());
        assert!(!outside.path().join("diagnostics.log").exists());
        std::fs::remove_file(root.path().join("logs")).unwrap();
        LogWriter::new(&config).unwrap();
        assert_eq!(
            std::fs::metadata(&config.log.output)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
