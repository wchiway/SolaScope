use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};

use flate2::read::MultiGzDecoder;
use reqwest::{header, redirect, Client, Url};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use solascope_core::{Error, Result};

const SMALL_LIMIT: u64 = 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 256 * 1024 * 1024;
const UNPACKED_LIMIT: u64 = 512 * 1024 * 1024;
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
const PAX_LIMIT: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 32;
const REPOSITORY_PATH: &str = "/wchiway/SolaScope/releases/download/";
const LATEST_URL: &str = "https://api.github.com/repos/wchiway/SolaScope/releases/latest";
const DOCUMENTS: &[&str] = &[
    "LICENSE",
    "README.md",
    "README.zh-CN.md",
    "docs/configuration.md",
    "docs/mcp.md",
    "docs/architecture.md",
    "docs/benchmarks.md",
    "docs/implementation-status.md",
    "benchmarks/README.md",
    "docs/assets/solascope-overview.png",
    "docs/assets/solascope-search-flow.png",
];

#[derive(Debug, Clone)]
pub(super) struct Release {
    pub version: Version,
    pub tag: String,
    pub archive_name: String,
    pub archive_url: Url,
    pub checksum_url: Url,
    archive_size: u64,
    checksum_size: u64,
}

impl Release {
    pub(super) fn local(archive: &Path, version: Version, target: &str) -> Result<Self> {
        let suffix = archive_extension(target)?;
        let archive_name = format!("solascope-{version}-{target}.{suffix}");
        if archive.file_name().and_then(|name| name.to_str()) != Some(archive_name.as_str()) {
            return Err(failure(
                "local archive filename does not match its version and target",
            ));
        }
        let archive_size = fs::metadata(archive)?.len();
        if archive_size == 0 || archive_size > ARCHIVE_LIMIT || !version.pre.is_empty() {
            return Err(failure("local archive has an invalid version or size"));
        }
        let tag = format!("v{version}");
        Ok(Self {
            archive_url: Url::parse(&format!(
                "https://github.com{REPOSITORY_PATH}{tag}/{archive_name}"
            ))
            .map_err(|error| failure(error.to_string()))?,
            checksum_url: Url::parse(&format!(
                "https://github.com{REPOSITORY_PATH}{tag}/SHA256SUMS"
            ))
            .map_err(|error| failure(error.to_string()))?,
            version,
            tag,
            archive_name,
            archive_size,
            checksum_size: 0,
        })
    }
}

pub(super) struct Source {
    client: Client,
    #[cfg(test)]
    local: Option<Url>,
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    state: String,
    size: u64,
    browser_download_url: String,
}

impl Source {
    pub fn github() -> Result<Self> {
        let client = Client::builder()
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .user_agent("release-updater")
            .build()
            .map_err(|error| failure(format!("could not create update client: {error}")))?;
        Ok(Self {
            client,
            #[cfg(test)]
            local: None,
        })
    }

    #[cfg(test)]
    pub fn local(base: &str) -> Self {
        let mut source = Self::github().unwrap();
        source.client = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = Url::parse(base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert!(matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        ));
        source.local = Some(url);
        source
    }

    pub async fn latest(&self, target: &str) -> Result<Release> {
        let extension = archive_extension(target)?;
        let metadata_url = self.metadata_url()?;
        let mut bytes = Vec::new();
        self.fetch(&metadata_url, SMALL_LIMIT, None, true, &mut bytes)
            .await?;
        let metadata: Metadata = serde_json::from_slice(&bytes)
            .map_err(|error| failure(format!("invalid release metadata: {error}")))?;
        let version = metadata
            .tag_name
            .strip_prefix('v')
            .and_then(|value| Version::parse(value).ok())
            .ok_or_else(|| failure("release tag must be v followed by a semantic version"))?;
        if metadata.tag_name != format!("v{version}")
            || metadata.draft
            || metadata.prerelease
            || !version.pre.is_empty()
            || metadata.published_at.trim().is_empty()
        {
            return Err(failure("latest release must be a published stable version"));
        }
        let archive_name = format!("solascope-{version}-{target}.{extension}");
        let mut names = HashSet::new();
        let mut archive = None;
        let mut checksum = None;
        for asset in metadata.assets {
            if !safe_basename(&asset.name) || !names.insert(asset.name.clone()) {
                return Err(failure("release has unsafe or duplicate asset names"));
            }
            let limit = if asset.name == "SHA256SUMS" {
                SMALL_LIMIT
            } else {
                ARCHIVE_LIMIT
            };
            if asset.state != "uploaded" || asset.size == 0 || asset.size > limit {
                return Err(failure("release asset has an invalid state or size"));
            }
            let url = Url::parse(&asset.browser_download_url)
                .map_err(|error| failure(format!("invalid release asset URL: {error}")))?;
            self.validate_asset_url(&url, &metadata.tag_name, &asset.name)?;
            if asset.name == archive_name {
                archive = Some((url, asset.size));
            } else if asset.name == "SHA256SUMS" {
                checksum = Some((url, asset.size));
            }
        }
        let (archive_url, archive_size) =
            archive.ok_or_else(|| failure(format!("latest release is missing {archive_name}")))?;
        let (checksum_url, checksum_size) =
            checksum.ok_or_else(|| failure("latest release is missing SHA256SUMS"))?;
        Ok(Release {
            version,
            tag: metadata.tag_name,
            archive_name,
            archive_url,
            checksum_url,
            archive_size,
            checksum_size,
        })
    }

    pub async fn stage(&self, release: &Release, directory: &Path) -> Result<()> {
        self.validate_asset_url(&release.archive_url, &release.tag, &release.archive_name)?;
        self.validate_asset_url(&release.checksum_url, &release.tag, "SHA256SUMS")?;
        // Downloads and archive-derived output each have a new, private directory.
        let downloads = tempfile::Builder::new()
            .prefix(".download-")
            .tempdir_in(directory)?;
        let unpacked = tempfile::Builder::new()
            .prefix(".unpacked-")
            .tempdir_in(directory)?;
        let mut checksums = Vec::new();
        self.fetch(
            &release.checksum_url,
            SMALL_LIMIT,
            Some(release.checksum_size),
            false,
            &mut checksums,
        )
        .await?;
        let expected = checksum_for(&checksums, &release.archive_name)?;
        let archive_path = downloads.path().join("archive");
        let mut archive = File::create(&archive_path)?;
        self.fetch(
            &release.archive_url,
            ARCHIVE_LIMIT,
            Some(release.archive_size),
            false,
            &mut archive,
        )
        .await?;
        archive.sync_all()?;
        drop(archive);
        let output = unpacked.path().to_path_buf();
        let windows = release.archive_name.ends_with("-windows-msvc.zip");
        let legacy = release.version == Version::new(0, 1, 0);
        tokio::task::spawn_blocking(move || {
            let actual = hash_file(&archive_path)?;
            if actual != expected {
                return Err(failure("release archive SHA-256 does not match SHA256SUMS"));
            }
            if windows {
                extract_zip(&archive_path, &output, legacy)
            } else {
                extract_tar(&archive_path, &output, legacy)
            }
        })
        .await
        .map_err(|error| failure(format!("archive verification failed: {error}")))??;
        persist_binaries(unpacked.path(), directory, windows)
    }

    fn metadata_url(&self) -> Result<Url> {
        #[cfg(test)]
        if let Some(base) = &self.local {
            return base
                .join("/repos/wchiway/SolaScope/releases/latest")
                .map_err(|error| failure(format!("invalid test metadata URL: {error}")));
        }
        Url::parse(LATEST_URL).map_err(|error| failure(error.to_string()))
    }

    fn validate_asset_url(&self, url: &Url, tag: &str, name: &str) -> Result<()> {
        let path = format!("{REPOSITORY_PATH}{tag}/{name}");
        #[cfg(test)]
        if let Some(base) = &self.local {
            if url.origin() == base.origin()
                && url.path() == path
                && clean_url(url)
                && url.query().is_none()
            {
                return Ok(());
            }
            return Err(failure(
                "release asset URL does not match the frozen test release",
            ));
        }
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || url.port_or_known_default() != Some(443)
            || !clean_url(url)
            || url.query().is_some()
            || url.path() != path
        {
            return Err(failure(
                "release asset URL does not match the frozen GitHub release",
            ));
        }
        Ok(())
    }

    fn validate_redirect(&self, from: &Url, to: &Url) -> Result<()> {
        #[cfg(test)]
        if let Some(base) = &self.local {
            if from.origin() == base.origin() && to.origin() == base.origin() && clean_url(to) {
                return Ok(());
            }
            return Err(failure("update redirect left the test server"));
        }
        if to.scheme() != "https"
            || to.port_or_known_default() != Some(443)
            || !clean_url(to)
            || !matches!(
                to.host_str(),
                Some(
                    "github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                )
            )
            || (to.host_str() == Some("github.com")
                && (from.host_str() != Some("github.com")
                    || to.path() != from.path()
                    || to.query().is_some()))
        {
            return Err(failure(
                "update redirect is outside the approved HTTPS release hosts",
            ));
        }
        Ok(())
    }

    async fn fetch(
        &self,
        initial: &Url,
        limit: u64,
        expected_size: Option<u64>,
        metadata: bool,
        output: &mut impl Write,
    ) -> Result<()> {
        let mut url = initial.clone();
        for hop in 0..=5 {
            let mut response = self
                .client
                .get(url.clone())
                .header(
                    header::ACCEPT,
                    if metadata {
                        "application/vnd.github+json"
                    } else {
                        "application/octet-stream"
                    },
                )
                .send()
                .await
                .map_err(|error| failure(format!("update download failed: {error}")))?;
            if response.status().is_redirection() {
                if metadata || hop == 5 {
                    return Err(failure(
                        "unexpected metadata redirect or too many download redirects",
                    ));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| failure("update redirect has no valid Location header"))?;
                let next = url
                    .join(location)
                    .map_err(|error| failure(format!("invalid update redirect: {error}")))?;
                self.validate_redirect(&url, &next)?;
                url = next;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err(failure(format!(
                    "update download returned HTTP {}",
                    response.status()
                )));
            }
            if response.content_length().is_some_and(|size| {
                size > limit || expected_size.is_some_and(|expected| size != expected)
            }) {
                return Err(failure("update download has an invalid Content-Length"));
            }
            let mut size = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| failure(format!("update response could not be read: {error}")))?
            {
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| failure("update download size overflow"))?;
                if size > limit || expected_size.is_some_and(|expected| size > expected) {
                    return Err(failure("update download exceeds its size limit"));
                }
                output.write_all(&chunk)?;
            }
            if expected_size.is_some_and(|expected| size != expected) {
                return Err(failure(
                    "update download size differs from release metadata",
                ));
            }
            return Ok(());
        }
        Err(failure("too many update redirects"))
    }
}

fn archive_extension(target: &str) -> Result<&'static str> {
    match target {
        "x86_64-pc-windows-msvc" => Ok("zip"),
        "x86_64-unknown-linux-gnu"
        | concat!("a", "arch64-apple-darwin")
        | "x86_64-apple-darwin" => Ok("tar.gz"),
        _ => Err(Error::InvalidInput(format!(
            "self-update has no release package for {target}"
        ))),
    }
}

pub(super) fn stage_archive(
    archive: &Path,
    checksum: &Path,
    version: &Version,
    target: &str,
    directory: &Path,
) -> Result<()> {
    let release = Release::local(archive, version.clone(), target)?;
    let downloads = tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(directory)?;
    let unpacked = tempfile::Builder::new()
        .prefix(".unpacked-")
        .tempdir_in(directory)?;
    let mut checksums = Vec::new();
    File::open(checksum)?
        .take(SMALL_LIMIT + 1)
        .read_to_end(&mut checksums)?;
    if checksums.len() as u64 > SMALL_LIMIT {
        return Err(failure("local SHA256SUMS exceeds its size limit"));
    }
    let expected = checksum_for(&checksums, &release.archive_name)?;
    let archive_path = downloads.path().join("archive");
    let mut input = File::open(archive)?;
    let mut output = File::create(&archive_path)?;
    let copied = io::copy(
        &mut Read::by_ref(&mut input).take(ARCHIVE_LIMIT + 1),
        &mut output,
    )?;
    if copied != release.archive_size || copied > ARCHIVE_LIMIT {
        return Err(failure("local archive changed or exceeds its size limit"));
    }
    output.sync_all()?;
    drop(output);
    if hash_file(&archive_path)? != expected {
        return Err(failure("release archive SHA-256 does not match SHA256SUMS"));
    }
    let windows = target == "x86_64-pc-windows-msvc";
    let legacy = *version == Version::new(0, 1, 0);
    if windows {
        extract_zip(&archive_path, unpacked.path(), legacy)?;
    } else {
        extract_tar(&archive_path, unpacked.path(), legacy)?;
    }
    persist_binaries(unpacked.path(), directory, windows)
}

fn persist_binaries(unpacked: &Path, directory: &Path, windows: bool) -> Result<()> {
    let mut created = Vec::new();
    let result = (|| {
        for name in binary_names(windows) {
            let path = directory.join(name);
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            created.push(path);
            let mut input = File::open(unpacked.join(name))?;
            io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        for path in created {
            fs::remove_file(path)?;
        }
    }
    result
}

fn failure(message: impl Into<String>) -> Error {
    Error::Io(io::Error::other(message.into()))
}

fn clean_url(url: &Url) -> bool {
    url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
}

fn safe_basename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+_-".contains(&byte))
}

fn checksum_for(bytes: &[u8], archive_name: &str) -> Result<[u8; 32]> {
    let contents =
        std::str::from_utf8(bytes).map_err(|_| failure("SHA256SUMS must contain UTF-8 text"))?;
    let mut names = HashSet::new();
    let mut selected = None;
    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 67
            || !bytes[..64].iter().all(u8::is_ascii_hexdigit)
            || bytes[64] != b' '
            || !matches!(bytes[65], b' ' | b'*')
        {
            return Err(failure("malformed SHA256SUMS line"));
        }
        let name = &line[66..];
        if !safe_basename(name) || !names.insert(name) {
            return Err(failure("SHA256SUMS contains unsafe or duplicate filenames"));
        }
        let mut digest = [0; 32];
        hex::decode_to_slice(&line[..64], &mut digest)
            .map_err(|_| failure("malformed SHA256SUMS digest"))?;
        if name == archive_name {
            selected = Some(digest);
        }
    }
    selected.ok_or_else(|| failure("SHA256SUMS has no checksum for the release archive"))
}

fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            return Ok(hash.finalize().into());
        }
        hash.update(&bytes[..count]);
    }
}

fn binary_names(windows: bool) -> [&'static str; 2] {
    if windows {
        ["solascope.exe", "sas.exe"]
    } else {
        ["solascope", "sas"]
    }
}

fn allowed_entry(name: &str, windows: bool, legacy: bool) -> bool {
    if name.contains('\\')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    binary_names(windows).contains(&name)
        || DOCUMENTS.contains(&name)
        || (!legacy
            && (matches!(name, "docs/installation.md" | "docs/releases.md")
                || (windows && matches!(name, "install.ps1" | "install.cmd"))
                || (!windows && name == "install.sh")))
}

fn check_entry(
    name: &str,
    size: u64,
    windows: bool,
    legacy: bool,
    names: &mut HashSet<String>,
    total: &mut u64,
) -> Result<bool> {
    if !allowed_entry(name, windows, legacy)
        || !names.insert(name.to_owned())
        || names.len() > MAX_ENTRIES
    {
        return Err(failure(
            "archive contains an unsafe, unexpected, or duplicate entry",
        ));
    }
    let binary = binary_names(windows).contains(&name);
    if (binary && (size == 0 || size > BINARY_LIMIT)) || size > UNPACKED_LIMIT {
        return Err(failure("archive entry exceeds the unpacked size limit"));
    }
    *total = total
        .checked_add(size)
        .ok_or_else(|| failure("archive size overflow"))?;
    if *total > UNPACKED_LIMIT {
        return Err(failure("archive exceeds the total unpacked size limit"));
    }
    Ok(binary)
}

fn copy_entry(reader: &mut impl Read, size: u64, output: &mut impl Write) -> Result<()> {
    let actual = io::copy(&mut reader.take(size.saturating_add(1)), output)?;
    if actual != size {
        return Err(failure("archive entry size differs from its declared size"));
    }
    Ok(())
}

fn require_binaries(names: &HashSet<String>, windows: bool) -> Result<()> {
    if binary_names(windows)
        .iter()
        .all(|name| names.contains(*name))
    {
        Ok(())
    } else {
        Err(failure(
            "release archive must contain both solascope and sas binaries",
        ))
    }
}

// The raw pass bounds PAX allocation before tar's logical iterator resolves extensions.
// Normal Python-produced release archives use small local PAX timestamp headers.
fn validate_tar(path: &Path, legacy: bool) -> Result<()> {
    let mut archive = open_tar(path)?;
    let mut names = HashSet::new();
    let mut total = 0;
    let mut extensions = 0;
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_local_extensions() {
            extensions += 1;
            if entry.size() > PAX_LIMIT || extensions > MAX_ENTRIES {
                return Err(failure("archive PAX metadata exceeds its limit"));
            }
            let mut keys = HashSet::new();
            if let Some(values) = entry.pax_extensions()? {
                for value in values {
                    let value = value?;
                    let key = value.key().map_err(|_| failure("invalid PAX key"))?;
                    if !keys.insert(key.to_owned()) {
                        return Err(failure("duplicate PAX metadata key"));
                    }
                    match key {
                        "path" => {
                            let name = value.value().map_err(|_| failure("invalid PAX path"))?;
                            if !allowed_entry(name, false, legacy) {
                                return Err(failure("unsafe PAX archive path"));
                            }
                        }
                        "mtime" | "atime" | "ctime" | "uid" | "gid" | "uname" | "gname" => {}
                        // No published package needs sparse files, link names, or size overrides.
                        _ => return Err(failure("unexpected PAX metadata key")),
                    }
                }
            }
            continue;
        }
        if !kind.is_file() || entry.link_name_bytes().is_some() {
            return Err(failure("archive contains a link or non-regular file"));
        }
        let path = entry.path_bytes();
        let name = std::str::from_utf8(&path).map_err(|_| failure("invalid archive filename"))?;
        check_entry(name, entry.size(), false, legacy, &mut names, &mut total)?;
        let size = entry.size();
        copy_entry(&mut entry, size, &mut io::sink())?;
    }
    require_binaries(&names, false)?;
    // Consume the gzip trailer (including CRC) and reject appended archive contents.
    let mut tail = archive.into_inner();
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = tail.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        if buffer[..size].iter().any(|byte| *byte != 0) {
            return Err(failure("archive has data after its end marker"));
        }
    }
    Ok(())
}

struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = self.remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let size = self.inner.read(&mut buffer[..limit])?;
        self.remaining = self
            .remaining
            .checked_sub(size as u64)
            .ok_or_else(|| io::Error::other("archive exceeds the decompressed stream limit"))?;
        Ok(size)
    }
}

type TarReader = LimitedReader<MultiGzDecoder<BufReader<File>>>;

fn open_tar(path: &Path) -> Result<tar::Archive<TarReader>> {
    Ok(tar::Archive::new(LimitedReader {
        inner: MultiGzDecoder::new(BufReader::new(File::open(path)?)),
        remaining: UNPACKED_LIMIT,
    }))
}

fn extract_tar(path: &Path, output: &Path, legacy: bool) -> Result<()> {
    validate_tar(path, legacy)?;
    let mut archive = open_tar(path)?;
    let mut names = HashSet::new();
    let mut total = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() || entry.link_name_bytes().is_some() {
            return Err(failure("archive contains a link or non-regular file"));
        }
        let path = entry.path_bytes();
        let name = std::str::from_utf8(&path)
            .map_err(|_| failure("invalid archive filename"))?
            .to_owned();
        let size = entry.size();
        if check_entry(&name, size, false, legacy, &mut names, &mut total)? {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join(name))?;
            copy_entry(&mut entry, size, &mut file)?;
        } else {
            copy_entry(&mut entry, size, &mut io::sink())?;
        }
    }
    require_binaries(&names, false)
}

fn validate_zip_directory(file: &mut File) -> Result<()> {
    let length = file.metadata()?.len();
    let size = length.min(65_557) as usize;
    let start = length - size as u64;
    file.seek(SeekFrom::Start(start))?;
    let mut tail = vec![0; size];
    file.read_exact(&mut tail)?;
    let end = (0..size.saturating_sub(21))
        .rev()
        .find(|&offset| {
            tail[offset..].starts_with(b"PK\x05\x06")
                && offset + 22 + u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]) as usize
                    == size
        })
        .ok_or_else(|| failure("ZIP has no valid end-of-central-directory record"))?;
    let number = |offset| u16::from_le_bytes([tail[end + offset], tail[end + offset + 1]]);
    let word =
        |offset| u32::from_le_bytes(tail[end + offset..end + offset + 4].try_into().unwrap());
    let count = number(10);
    let central_size = word(12) as u64;
    let central_offset = word(16) as u64;
    if number(4) != 0
        || number(6) != 0
        || number(8) != count
        || count == 0
        || count as usize > MAX_ENTRIES
        || central_size > SMALL_LIMIT
        || central_offset.checked_add(central_size) != Some(start + end as u64)
    {
        return Err(failure("ZIP has an unsupported disk layout or entry count"));
    }
    // A ZIP64 locator overrides the count above; no supported package needs it.
    if start + end as u64 >= 20 {
        file.seek(SeekFrom::Start(start + end as u64 - 20))?;
        let mut signature = [0; 4];
        file.read_exact(&mut signature)?;
        if signature == *b"PK\x06\x07" {
            return Err(failure("ZIP64 release archives are not supported"));
        }
    }
    file.rewind()?;
    Ok(())
}

fn extract_zip(path: &Path, output: &Path, legacy: bool) -> Result<()> {
    let mut file = File::open(path)?;
    validate_zip_directory(&mut file)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| failure(format!("invalid release ZIP: {error}")))?;
    if archive.has_duplicate_names() || archive.len() > MAX_ENTRIES {
        return Err(failure("ZIP has duplicate names or too many entries"));
    }
    let mut names = HashSet::new();
    let mut total = 0;
    // Validate every member, including documents, before creating any output.
    for index in 0..archive.len() {
        let entry = archive
            .by_index_data(index)
            .map_err(|error| failure(format!("invalid ZIP entry: {error}")))?;
        let name =
            std::str::from_utf8(entry.name_raw()).map_err(|_| failure("invalid ZIP filename"))?;
        if !entry.is_file()
            || entry.encrypted()
            || entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000))
        {
            return Err(failure(
                "ZIP contains an encrypted, linked, or non-regular entry",
            ));
        }
        check_entry(name, entry.size(), true, legacy, &mut names, &mut total)?;
    }
    require_binaries(&names, true)?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| failure(format!("could not read ZIP entry: {error}")))?;
        let size = entry.size();
        let name =
            std::str::from_utf8(entry.name_raw()).map_err(|_| failure("invalid ZIP filename"))?;
        if binary_names(true).contains(&name) {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join(name))?;
            copy_entry(&mut entry, size, &mut file)?;
        } else {
            copy_entry(&mut entry, size, &mut io::sink())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use serde_json::{json, Value};
    use std::io::Cursor;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    use zip::{write::SimpleFileOptions, ZipWriter};

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";

    fn archive_name(version: &str, target: &str) -> String {
        format!(
            "solascope-{version}-{target}.{}",
            if target == WINDOWS { "zip" } else { "tar.gz" }
        )
    }

    fn metadata(
        base: &str,
        version: &str,
        target: &str,
        archive_size: usize,
        checksum_size: usize,
    ) -> Value {
        let name = archive_name(version, target);
        json!({
            "tag_name": format!("v{version}"), "draft": false, "prerelease": false,
            "published_at": "2026-01-01T00:00:00Z",
            "assets": [
                {"name": name, "state": "uploaded", "size": archive_size,
                 "browser_download_url": format!("{base}{REPOSITORY_PATH}v{version}/{name}")},
                {"name": "SHA256SUMS", "state": "uploaded", "size": checksum_size,
                 "browser_download_url": format!("{base}{REPOSITORY_PATH}v{version}/SHA256SUMS")}
            ]
        })
    }

    fn tar_bytes(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, content, kind) in entries {
            let mut header = tar::Header::new_ustar();
            assert!(name.len() < 100);
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_mode(0o755);
            header.set_size(content.len() as u64);
            header.set_entry_type(*kind);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("../outside").unwrap();
            }
            header.set_cksum();
            builder.append(&header, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn zip_bytes(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content, link) in entries {
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            if *link {
                writer.add_symlink(name, "../outside", options).unwrap();
            } else {
                writer.start_file(name, options).unwrap();
                writer.write_all(content).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    async fn serve_archive(
        server: &MockServer,
        version: &str,
        target: &str,
        archive: Vec<u8>,
        corrupt: bool,
    ) {
        let name = archive_name(version, target);
        let digest = if corrupt {
            "0".repeat(64)
        } else {
            hex::encode(Sha256::digest(&archive))
        };
        let checksum = format!("{digest}  {name}\n");
        Mock::given(method("GET"))
            .and(path("/repos/wchiway/SolaScope/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata(
                &server.uri(),
                version,
                target,
                archive.len(),
                checksum.len(),
            )))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{REPOSITORY_PATH}v{version}/SHA256SUMS")))
            .respond_with(ResponseTemplate::new(200).set_body_string(checksum))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{REPOSITORY_PATH}v{version}/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn published_stable_release_stages_only_both_binaries() {
        for (version, target) in [("0.1.0", TARGET), ("1.2.3", TARGET), ("1.2.3", WINDOWS)] {
            let server = MockServer::start().await;
            let bytes = if target == WINDOWS {
                zip_bytes(&[
                    ("solascope.exe", b"main", false),
                    ("sas.exe", b"alias", false),
                    ("README.md", b"docs", false),
                    ("install.ps1", b"installer", false),
                ])
            } else {
                tar_bytes(&[
                    ("solascope", b"main", tar::EntryType::Regular),
                    ("sas", b"alias", tar::EntryType::Regular),
                    ("README.md", b"docs", tar::EntryType::Regular),
                ])
            };
            serve_archive(&server, version, target, bytes, false).await;
            let source = Source::local(&server.uri());
            let release = source.latest(target).await.unwrap();
            assert_eq!(release.version, Version::parse(version).unwrap());
            assert_eq!(release.tag, format!("v{version}"));
            let directory = tempfile::tempdir().unwrap();
            source.stage(&release, directory.path()).await.unwrap();
            let names = binary_names(target == WINDOWS);
            assert_eq!(fs::read(directory.path().join(names[0])).unwrap(), b"main");
            assert_eq!(fs::read(directory.path().join(names[1])).unwrap(), b"alias");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
        }
    }

    #[tokio::test]
    async fn invalid_release_metadata_is_rejected() {
        let server = MockServer::start().await;
        let valid = metadata(&server.uri(), "1.2.3", TARGET, 50, 200);
        let mut cases = Vec::new();
        for field in ["draft", "prerelease"] {
            let mut value = valid.clone();
            value[field] = json!(true);
            cases.push(value);
        }
        for tag in ["1.2.3", "v1.2.3-rc.1", "v01.2.3", "v1.2.3/extra"] {
            let mut value = valid.clone();
            value["tag_name"] = json!(tag);
            cases.push(value);
        }
        for field in ["published_at", "assets"] {
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(field);
            cases.push(value);
        }
        let mut value = valid.clone();
        value["published_at"] = json!("");
        cases.push(value);
        let mut value = valid.clone();
        value["assets"].as_array_mut().unwrap().pop();
        cases.push(value);
        let mut value = valid.clone();
        value["assets"][0]["state"] = json!("new");
        cases.push(value);
        for size in [0, ARCHIVE_LIMIT + 1] {
            let mut value = valid.clone();
            value["assets"][0]["size"] = json!(size);
            cases.push(value);
        }
        let mut value = valid.clone();
        value["assets"][1]["size"] = json!(SMALL_LIMIT + 1);
        cases.push(value);
        let mut value = valid.clone();
        let duplicate = value["assets"][0].clone();
        value["assets"].as_array_mut().unwrap().push(duplicate);
        cases.push(value);
        for url in [
            "http://evil.invalid/file",
            "https://github.com/other/repo/file",
            "https://user:secret@github.com/file",
        ] {
            let mut value = valid.clone();
            value["assets"][0]["browser_download_url"] = json!(url);
            cases.push(value);
        }
        for value in cases {
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(value))
                .mount(&server)
                .await;
            assert!(Source::local(&server.uri()).latest(TARGET).await.is_err());
        }
        assert!(matches!(
            Source::local(&server.uri())
                .latest("x86_64-unknown-linux-musl")
                .await,
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn production_asset_urls_are_pinned_to_repository_and_tag() {
        let source = Source::github().unwrap();
        let name = archive_name("1.2.3", TARGET);
        let valid = format!("https://github.com{REPOSITORY_PATH}v1.2.3/{name}");
        assert!(source
            .validate_asset_url(&Url::parse(&valid).unwrap(), "v1.2.3", &name)
            .is_ok());
        for url in [
            valid.replace("https:", "http:"),
            valid.replace("github.com", "github.com.evil.invalid"),
            valid.replace("github.com", "user:secret@github.com"),
            valid.replace("github.com", "github.com:444"),
            valid.replace("v1.2.3", "v2.0.0"),
            format!("{valid}?token=secret"),
            format!("{valid}#fragment"),
            valid.replace("/download/", "/download%2f"),
        ] {
            assert!(
                source
                    .validate_asset_url(&Url::parse(&url).unwrap(), "v1.2.3", &name)
                    .is_err(),
                "{url}"
            );
        }
        let from = Url::parse(&valid).unwrap();
        for host in [
            "release-assets.githubusercontent.com",
            "objects.githubusercontent.com",
        ] {
            assert!(source
                .validate_redirect(
                    &from,
                    &Url::parse(&format!("https://{host}/download?signature=abc")).unwrap()
                )
                .is_ok());
        }
        for url in [
            "http://release-assets.githubusercontent.com/file",
            "https://evil.invalid/file",
            "https://api.github.com/file",
            "https://user:secret@objects.githubusercontent.com/file",
            "https://objects.githubusercontent.com:444/file",
        ] {
            assert!(source
                .validate_redirect(&from, &Url::parse(url).unwrap())
                .is_err());
        }
    }

    #[tokio::test]
    async fn redirects_and_download_bounds_are_enforced() {
        let server = MockServer::start().await;
        let source = Source::local(&server.uri());
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/body"))
            .mount(&server)
            .await;
        Mock::given(path("/body"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"12345"))
            .mount(&server)
            .await;
        let initial = Url::parse(&format!("{}/start", server.uri())).unwrap();
        let mut bytes = Vec::new();
        source
            .fetch(&initial, 5, Some(5), false, &mut bytes)
            .await
            .unwrap();
        assert_eq!(bytes, b"12345");
        assert!(source
            .fetch(&initial, 4, None, false, &mut Vec::new())
            .await
            .is_err());
        assert!(source
            .fetch(&initial, 8, Some(6), false, &mut Vec::new())
            .await
            .is_err());
        assert!(source
            .fetch(&initial, 8, None, true, &mut Vec::new())
            .await
            .is_err());
        server.reset().await;
        Mock::given(path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://example.invalid/body"),
            )
            .mount(&server)
            .await;
        assert!(source
            .fetch(&initial, 8, None, false, &mut Vec::new())
            .await
            .is_err());
        server.reset().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/start"))
            .mount(&server)
            .await;
        assert!(source
            .fetch(&initial, 8, None, false, &mut Vec::new())
            .await
            .is_err());
    }

    #[test]
    fn checksum_requires_exact_unique_well_formed_entry() {
        let name = archive_name("1.2.3", TARGET);
        let hash = "ab".repeat(32);
        let valid = format!("{hash}  {name}\n");
        assert_eq!(checksum_for(valid.as_bytes(), &name).unwrap(), [0xab; 32]);
        for text in [
            format!("{valid}{valid}"),
            format!("{hash}  ../{name}\n"),
            format!("{hash}  other.tar.gz\n"),
            format!("{hash} {name}\n"),
            format!("{}  {name}\n", "z".repeat(64)),
            format!("{valid}garbage\n"),
            format!("{}  other.tar.gz\n{}  other.tar.gz\n{valid}", hash, hash),
        ] {
            assert!(checksum_for(text.as_bytes(), &name).is_err(), "{text}");
        }
    }

    #[tokio::test]
    async fn corrupt_checksum_never_publishes_binaries() {
        let server = MockServer::start().await;
        let bytes = tar_bytes(&[
            ("solascope", b"main", tar::EntryType::Regular),
            ("sas", b"alias", tar::EntryType::Regular),
        ]);
        serve_archive(&server, "1.2.3", TARGET, bytes, true).await;
        let source = Source::local(&server.uri());
        let release = source.latest(TARGET).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        assert!(source.stage(&release, directory.path()).await.is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn tar_rejects_unsafe_documents_links_duplicates_and_traversal() {
        let directory = tempfile::tempdir().unwrap();
        for (name, kind) in [
            ("../outside", tar::EntryType::Regular),
            ("/absolute", tar::EntryType::Regular),
            ("docs/../README.md", tar::EntryType::Regular),
            ("docs\\mcp.md", tar::EntryType::Regular),
            ("docs/./mcp.md", tar::EntryType::Regular),
            ("unknown.md", tar::EntryType::Regular),
            ("README.md", tar::EntryType::Symlink),
            ("README.md", tar::EntryType::Link),
            ("README.md", tar::EntryType::Directory),
            ("solascope", tar::EntryType::Regular),
        ] {
            let output = tempfile::tempdir_in(directory.path()).unwrap();
            let archive = directory.path().join("archive.tar.gz");
            fs::write(
                &archive,
                tar_bytes(&[
                    ("solascope", b"main", tar::EntryType::Regular),
                    ("sas", b"alias", tar::EntryType::Regular),
                    (name, b"attack", kind),
                ]),
            )
            .unwrap();
            assert!(
                extract_tar(&archive, output.path(), false).is_err(),
                "{name:?} {kind:?}"
            );
            assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
            assert!(!directory.path().join("outside").exists());
        }
        let archive = directory.path().join("archive.tar.gz");
        fs::write(
            &archive,
            tar_bytes(&[("solascope", b"main", tar::EntryType::Regular)]),
        )
        .unwrap();
        let output = tempfile::tempdir().unwrap();
        assert!(extract_tar(&archive, output.path(), false).is_err());
    }

    #[test]
    fn zip_rejects_unsafe_documents_links_and_missing_binary() {
        let directory = tempfile::tempdir().unwrap();
        for (name, link) in [
            ("../outside", false),
            ("/absolute", false),
            ("docs/../README.md", false),
            ("docs\\mcp.md", false),
            ("docs/./mcp.md", false),
            ("unknown.md", false),
            ("README.md", true),
            ("README.md/", false),
        ] {
            let output = tempfile::tempdir_in(directory.path()).unwrap();
            let archive = directory.path().join("archive.zip");
            fs::write(
                &archive,
                zip_bytes(&[
                    ("solascope.exe", b"main", false),
                    ("sas.exe", b"alias", false),
                    (name, b"attack", link),
                ]),
            )
            .unwrap();
            assert!(
                extract_zip(&archive, output.path(), false).is_err(),
                "{name:?}"
            );
            assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
            assert!(!directory.path().join("outside").exists());
        }
        let archive = directory.path().join("archive.zip");
        fs::write(&archive, zip_bytes(&[("solascope.exe", b"main", false)])).unwrap();
        let output = tempfile::tempdir().unwrap();
        assert!(extract_zip(&archive, output.path(), false).is_err());
    }

    #[tokio::test]
    async fn streaming_limit_applies_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let read = socket.read(&mut request).await.unwrap();
            assert!(read > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n12345\r\n0\r\n\r\n").await.unwrap();
        });
        let source = Source::local(&base);
        let error = source
            .fetch(&Url::parse(&base).unwrap(), 4, None, false, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds its size limit"));
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_metadata_is_rejected_before_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; SMALL_LIMIT as usize + 1]),
            )
            .mount(&server)
            .await;
        let error = Source::local(&server.uri())
            .latest(TARGET)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Content-Length"));
    }

    #[test]
    fn zip_rejects_duplicate_names_oversized_entries_and_corrupt_data() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("archive.zip");
        let output = tempfile::tempdir().unwrap();
        let mut duplicate = zip_bytes(&[
            ("solascope.exe", b"main", false),
            ("sas.exe", b"alias", false),
            ("README.md", b"docs", false),
            ("README.xx", b"other", false),
        ]);
        let offsets: Vec<_> = duplicate
            .windows(9)
            .enumerate()
            .filter_map(|(offset, bytes)| (bytes == b"README.xx").then_some(offset))
            .collect();
        for offset in offsets {
            duplicate[offset..offset + 9].copy_from_slice(b"README.md");
        }
        fs::write(&archive, duplicate).unwrap();
        assert!(extract_zip(&archive, output.path(), false).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);

        let mut oversized = zip_bytes(&[
            ("solascope.exe", b"main", false),
            ("sas.exe", b"alias", false),
        ]);
        let central = oversized
            .windows(4)
            .position(|bytes| bytes == b"PK\x01\x02")
            .unwrap();
        oversized[central + 24..central + 28]
            .copy_from_slice(&((BINARY_LIMIT + 1) as u32).to_le_bytes());
        fs::write(&archive, oversized).unwrap();
        assert!(extract_zip(&archive, output.path(), false).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);

        let mut corrupt = zip_bytes(&[
            ("solascope.exe", b"main", false),
            ("sas.exe", b"alias", false),
        ]);
        let name_size = u16::from_le_bytes([corrupt[26], corrupt[27]]) as usize;
        let extra_size = u16::from_le_bytes([corrupt[28], corrupt[29]]) as usize;
        corrupt[30 + name_size + extra_size] ^= 0xff;
        fs::write(&archive, corrupt).unwrap();
        assert!(extract_zip(&archive, output.path(), false).is_err());
    }

    #[test]
    fn tar_pax_timestamp_is_supported_and_path_or_size_attacks_fail() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("archive.tar.gz");
        for (key, value, valid) in [
            ("mtime", "1700000000.5", true),
            ("path", "../outside", false),
            ("linkpath", "../outside", false),
            ("size", "9999999999999", false),
            ("GNU.sparse.map", "0,1000", false),
        ] {
            let encoder = GzEncoder::new(Vec::new(), Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            builder
                .append_pax_extensions([(key, value.as_bytes())])
                .unwrap();
            for name in ["solascope", "sas"] {
                let mut header = tar::Header::new_ustar();
                header.set_size(4);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, b"main".as_slice())
                    .unwrap();
            }
            fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
            let output = tempfile::tempdir().unwrap();
            assert_eq!(
                extract_tar(&archive, output.path(), false).is_ok(),
                valid,
                "{key}"
            );
            if !valid {
                assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
            }
        }
        let mut corrupt = tar_bytes(&[
            ("solascope", b"main", tar::EntryType::Regular),
            ("sas", b"alias", tar::EntryType::Regular),
        ]);
        let length = corrupt.len();
        corrupt[length - 8] ^= 0xff;
        fs::write(&archive, corrupt).unwrap();
        let output = tempfile::tempdir().unwrap();
        assert!(extract_tar(&archive, output.path(), false).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn staging_size_mismatch_and_existing_destination_leave_directory_unchanged() {
        let server = MockServer::start().await;
        let bytes = tar_bytes(&[
            ("solascope", b"main", tar::EntryType::Regular),
            ("sas", b"alias", tar::EntryType::Regular),
        ]);
        serve_archive(&server, "1.2.3", TARGET, bytes, false).await;
        let source = Source::local(&server.uri());
        let mut release = source.latest(TARGET).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        release.archive_size += 1;
        assert!(source.stage(&release, directory.path()).await.is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        release.archive_size -= 1;
        fs::write(directory.path().join("sas"), b"existing").unwrap();
        assert!(source.stage(&release, directory.path()).await.is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(fs::read(directory.path().join("sas")).unwrap(), b"existing");
    }

    #[test]
    fn tar_safe_pax_rename_cannot_hide_missing_or_duplicate_binaries() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["sas", "README.md"] {
            let encoder = GzEncoder::new(Vec::new(), Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            builder
                .append_pax_extensions([("path", name.as_bytes())])
                .unwrap();
            for binary in ["solascope", "sas"] {
                let mut header = tar::Header::new_ustar();
                header.set_size(4);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, binary, b"main".as_slice())
                    .unwrap();
            }
            let archive = directory.path().join("archive.tar.gz");
            fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
            let output = tempfile::tempdir().unwrap();
            assert!(extract_tar(&archive, output.path(), false).is_err());
        }
    }

    #[test]
    fn local_archives_preserve_destination_on_corruption_conflict_and_pax_rename() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join(archive_name("1.2.3", TARGET));
        let checksum = directory.path().join("SHA256SUMS");
        let output = tempfile::tempdir().unwrap();
        let version = Version::new(1, 2, 3);
        let valid = tar_bytes(&[
            ("solascope", b"main", tar::EntryType::Regular),
            ("sas", b"alias", tar::EntryType::Regular),
        ]);
        fs::write(&archive, &valid).unwrap();
        fs::write(
            &checksum,
            format!(
                "{}  {}\n",
                "0".repeat(64),
                archive.file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();
        assert!(stage_archive(&archive, &checksum, &version, TARGET, output.path()).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
        fs::write(
            &checksum,
            format!(
                "{}  {}\n",
                hex::encode(Sha256::digest(&valid)),
                archive.file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();
        fs::write(output.path().join("sas"), b"existing").unwrap();
        assert!(stage_archive(&archive, &checksum, &version, TARGET, output.path()).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 1);
        assert_eq!(fs::read(output.path().join("sas")).unwrap(), b"existing");
        fs::remove_file(output.path().join("sas")).unwrap();

        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append_pax_extensions([("path", b"README.md".as_slice())])
            .unwrap();
        for binary in ["solascope", "sas"] {
            let mut header = tar::Header::new_ustar();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, binary, b"main".as_slice())
                .unwrap();
        }
        let malicious = builder.into_inner().unwrap().finish().unwrap();
        fs::write(&archive, &malicious).unwrap();
        fs::write(
            &checksum,
            format!(
                "{}  {}\n",
                hex::encode(Sha256::digest(&malicious)),
                archive.file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();
        assert!(stage_archive(&archive, &checksum, &version, TARGET, output.path()).is_err());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
    }

    #[test]
    fn zip64_locator_cannot_override_bounded_zip32_entry_count() {
        let mut bytes = zip_bytes(&[
            ("solascope.exe", b"main", false),
            ("sas.exe", b"alias", false),
        ]);
        let end = bytes
            .windows(4)
            .rposition(|window| window == b"PK\x05\x06")
            .unwrap();
        let old_size = u32::from_le_bytes(bytes[end + 12..end + 16].try_into().unwrap());
        let mut locator = vec![0; 20];
        locator[..4].copy_from_slice(b"PK\x06\x07");
        bytes.splice(end..end, locator);
        bytes[end + 20 + 12..end + 20 + 16].copy_from_slice(&(old_size + 20).to_le_bytes());
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("archive.zip");
        fs::write(&archive, bytes).unwrap();
        let error = validate_zip_directory(&mut File::open(archive).unwrap()).unwrap_err();
        assert!(error.to_string().contains("ZIP64"));
    }

    #[test]
    fn supported_targets_match_archive_platforms() {
        let apple_arm = String::from_utf8(vec![
            97, 97, 114, 99, 104, 54, 52, 45, 97, 112, 112, 108, 101, 45, 100, 97, 114, 119, 105,
            110,
        ])
        .unwrap();
        assert_eq!(archive_extension(&apple_arm).unwrap(), "tar.gz");
        assert_eq!(archive_extension("x86_64-apple-darwin").unwrap(), "tar.gz");
        assert_eq!(archive_extension(TARGET).unwrap(), "tar.gz");
        assert_eq!(archive_extension(WINDOWS).unwrap(), "zip");
        assert!(matches!(
            Release::local(Path::new("missing"), Version::new(1, 2, 3), "unsupported"),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn unpacked_and_binary_bounds_are_independent() {
        let mut names = HashSet::new();
        let mut total = 0;
        assert!(check_entry(
            "solascope",
            BINARY_LIMIT + 1,
            false,
            false,
            &mut names,
            &mut total
        )
        .is_err());
        names.clear();
        total = UNPACKED_LIMIT;
        assert!(check_entry("README.md", 1, false, false, &mut names, &mut total).is_err());
        let mut reader = LimitedReader {
            inner: Cursor::new(vec![1, 2, 3]),
            remaining: 2,
        };
        assert!(io::copy(&mut reader, &mut io::sink()).is_err());
        assert!(copy_entry(&mut Cursor::new(vec![1, 2]), 1, &mut Vec::new()).is_err());
        assert!(!allowed_entry("install.sh", false, true));
        assert!(!allowed_entry("docs/installation.md", false, true));
        assert!(allowed_entry("install.sh", false, false));
        assert!(!allowed_entry("install.cmd", false, false));
    }
}
