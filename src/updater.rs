#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::process::Command;
use std::{
    env, fs,
    io::{self, Cursor},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/OpenMouse-Project/OpenMouse-Bridge/releases/latest";
const UPDATE_URL_ENV: &str = "OPENMOUSE_BRIDGE_UPDATE_API_URL";
const USER_AGENT: &str = "OpenMouse-Bridge-Updater";
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    archive_url: String,
    checksum_url: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    let url = env::var(UPDATE_URL_ENV).unwrap_or_else(|_| LATEST_RELEASE_URL.to_owned());
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .context("could not create the update client")?;
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not check {url}"))?
        .error_for_status()
        .with_context(|| format!("update check failed for {url}"))?;
    let bytes = read_bounded_response(response, MAX_RELEASE_BYTES).await?;
    let release = serde_json::from_slice::<Release>(&bytes)
        .context("could not parse the latest Bridge release")?;
    select_update(release, crate::BRIDGE_VERSION)
}

pub async fn download_and_stage(update: &UpdateInfo) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("could not create the update download client")?;
    let (archive, checksum) = tokio::try_join!(
        download(&client, &update.archive_url, MAX_ARCHIVE_BYTES),
        download(&client, &update.checksum_url, MAX_CHECKSUM_BYTES),
    )?;
    verify_checksum(&archive, &checksum)?;
    let version = update.version.clone();
    tokio::task::spawn_blocking(move || stage_archive(&archive, &version))
        .await
        .context("update staging task stopped unexpectedly")??;
    Ok(())
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("update download failed for {url}"))?;
    read_bounded_response(response, limit).await
}

pub(crate) async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    if let Some(length) = response.headers().get(reqwest::header::CONTENT_LENGTH) {
        let length = length
            .to_str()?
            .parse::<u64>()
            .context("invalid download content length")?;
        ensure!(length <= limit as u64, "download exceeds byte limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("could not read bounded download")?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "download exceeds byte limit"
        );
        if bytes.capacity() - bytes.len() < chunk.len() {
            let capacity =
                limit.min((bytes.len() + chunk.len()).max(bytes.capacity().saturating_mul(2)));
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .context("could not allocate bounded download buffer")?;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn select_update(release: Release, current: &str) -> Result<Option<UpdateInfo>> {
    let current = Version::parse(current).context("Bridge has an invalid current version")?;
    let version_text = release.tag_name.trim_start_matches('v');
    let available = Version::parse(version_text)
        .with_context(|| format!("release {} has an invalid version", release.tag_name))?;
    if available <= current {
        return Ok(None);
    }

    let archive_name = platform_archive_name()?;
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_url(&release.assets, archive_name)?;
    let checksum_url = asset_url(&release.assets, &checksum_name)?;
    Ok(Some(UpdateInfo {
        version: available.to_string(),
        archive_url,
        checksum_url,
    }))
}

fn asset_url(assets: &[ReleaseAsset], name: &str) -> Result<String> {
    assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| anyhow!("release does not contain {name}"))
}

fn platform_archive_name() -> Result<&'static str> {
    #[cfg(target_os = "macos")]
    return Ok("openmouse-bridge-macos-universal.zip");
    #[cfg(target_os = "windows")]
    return Ok("openmouse-bridge-windows-x64.zip");
    #[cfg(target_os = "linux")]
    return Ok("openmouse-bridge-linux-x64.zip");
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    anyhow::bail!("automatic Bridge updates are available only on macOS, Windows, and Linux");
}

fn verify_checksum(archive: &[u8], checksum_file: &[u8]) -> Result<()> {
    let checksum = std::str::from_utf8(checksum_file)
        .context("update checksum is not UTF-8")?
        .split_whitespace()
        .next()
        .context("update checksum is empty")?;
    ensure!(
        checksum.len() == 64,
        "update checksum has an invalid length"
    );
    let actual = format!("{:x}", Sha256::digest(archive));
    ensure!(
        actual.eq_ignore_ascii_case(checksum),
        "update checksum does not match"
    );
    Ok(())
}

struct StagingDirectory {
    path: std::path::PathBuf,
    handed_off: bool,
}

impl StagingDirectory {
    fn new_in(parent: &Path) -> Result<Self> {
        use std::hash::BuildHasher;
        verify_directory_chain(parent)?;
        for _ in 0..32 {
            let nonce = std::collections::hash_map::RandomState::new()
                .hash_one((std::process::id(), std::time::SystemTime::now()));
            let path = parent.join(format!("openmouse-update-{nonce:016x}"));
            match private_directory_builder().create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        handed_off: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("could not create private update staging"),
            }
        }
        anyhow::bail!("could not allocate unique update staging")
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn stage_archive(archive: &[u8], version: &str) -> Result<()> {
    Version::parse(version).context("invalid update version")?;
    #[cfg(windows)]
    let parent = directories::BaseDirs::new()
        .context("could not locate private user cache")?
        .cache_dir()
        .to_path_buf();
    #[cfg(not(windows))]
    let parent = env::temp_dir();
    let parent = fs::canonicalize(parent).context("could not resolve update staging parent")?;
    let mut staging = StagingDirectory::new_in(&parent)?;
    extract_archive(archive, &staging.path)?;
    verify_package(&staging.path)?;
    let current_executable =
        env::current_exe().context("could not locate the Bridge executable")?;
    let install_directory = current_executable
        .parent()
        .context("Bridge executable has no parent directory")?;
    ensure!(
        current_executable.file_name() == Some(std::ffi::OsStr::new(platform_binary_name())),
        "Bridge executable has an unexpected name"
    );
    verify_install_directory(install_directory)?;
    spawn_install_helper(&staging.path, install_directory, &current_executable)?;
    staging.handed_off = true;
    Ok(())
}

fn verify_package(staging: &Path) -> Result<()> {
    verify_tree(staging)?;
    let license = staging.join("DM-Sans-OFL.txt");
    let expected = include_bytes!("../assets/fonts/DM-Sans-OFL.txt");
    ensure!(
        reject_link(&license)?.is_file() && fs::metadata(&license)?.len() == expected.len() as u64,
        "update package requires the complete DM Sans license"
    );
    ensure!(
        fs::read(&license)? == expected,
        "update package contains an incomplete DM Sans license"
    );
    let node = if cfg!(windows) {
        "native-hid/node/node.exe"
    } else {
        "native-hid/node/bin/node"
    };
    for name in [
        platform_binary_name(),
        "native-hid/src/apply.mjs",
        "native-hid/src/brands.mjs",
        "native-hid/src/hid-device-adapter.mjs",
        "native-hid/src/attack-shark-x11.mjs",
        "native-hid/package.json",
        node,
        "native-hid/node_modules/node-hid/package.json",
        "native-hid/node_modules/@openmouse/protocol/package.json",
        "native-hid/node_modules/@openmouse/keyboard-protocol/package.json",
    ] {
        let path = staging.join(name);
        verify_directory_chain(path.parent().context("missing package parent")?)?;
        let metadata =
            reject_link(&path).with_context(|| format!("update package is missing {name}"))?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "update package file is empty or invalid: {name}"
        );
        #[cfg(unix)]
        if name == platform_binary_name() || name == node {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o500 == 0o500,
                "update executable requires owner read and execute access: {name}"
            );
        }
    }
    Ok(())
}

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 4096;
const MAX_RELEASE_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 8192;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;

fn extract_archive(archive: &[u8], staging: &Path) -> Result<()> {
    extract_archive_with_limits(
        archive,
        staging,
        MAX_ARCHIVE_ENTRIES,
        MAX_FILE_BYTES,
        MAX_OUTPUT_BYTES,
    )
}

fn safe_entry_path(name: &str) -> Result<&Path> {
    ensure!(
        !name.is_empty() && name.len() <= 1024 && !name.starts_with('/'),
        "unsafe archive path"
    );
    ensure!(!name.contains(['\\', ':', '\0']), "unsafe archive path");
    let name = name.strip_suffix('/').unwrap_or(name);
    let mut count = 0;
    for part in name.split('/') {
        count += 1;
        ensure!(
            !part.is_empty() && part != "." && part != ".." && !part.ends_with(['.', ' ']),
            "unsafe archive path component"
        );
        ensure!(
            !part
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')),
            "unsafe archive path component"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()),
            "reserved archive path component"
        );
    }
    ensure!(count <= 64, "archive path is too deep");
    Ok(Path::new(name))
}

fn reject_link(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "update path contains a symbolic link"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "update path contains a reparse point"
        );
    }
    Ok(metadata)
}

fn verify_directory_chain(directory: &Path) -> Result<()> {
    ensure!(directory.is_absolute(), "update directory must be absolute");
    for ancestor in directory.ancestors() {
        ensure!(
            reject_link(ancestor)?.is_dir(),
            "update ancestor is not a directory"
        );
    }
    Ok(())
}

fn private_directory_builder() -> fs::DirBuilder {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder
}

fn create_entry_directories(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(_) => ensure!(
                reject_link(&path)?.is_dir(),
                "archive parent is not a directory"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let builder = private_directory_builder();
                builder.create(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        fs::canonicalize(&path)?.starts_with(fs::canonicalize(root)?),
        "archive path escapes staging"
    );
    Ok(())
}

const MAX_ZIP_METADATA_READ_BYTES: usize = 16 * 1024 * 1024;

struct ArchiveReader<'a> {
    inner: Cursor<&'a [u8]>,
    metadata_budget: &'a std::cell::Cell<Option<usize>>,
}

impl io::Read for ArchiveReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.metadata_budget.get();
        let limit = remaining.unwrap_or(buffer.len()).min(buffer.len());
        if !buffer.is_empty() && remaining == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP metadata read budget exceeded",
            ));
        }
        let read = self.inner.read(&mut buffer[..limit])?;
        if let Some(remaining) = remaining {
            self.metadata_budget.set(Some(remaining - read));
        }
        Ok(read)
    }
}

impl io::Seek for ArchiveReader<'_> {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

fn extract_archive_with_limits(
    archive: &[u8],
    staging: &Path,
    entries: usize,
    per_file: u64,
    total: u64,
) -> Result<()> {
    use std::io::Read;
    ensure!(
        archive.len() <= MAX_ARCHIVE_BYTES,
        "update archive exceeds download limit"
    );
    verify_directory_chain(staging)?;
    let metadata_budget = std::cell::Cell::new(Some(MAX_ZIP_METADATA_READ_BYTES));
    let reader = ArchiveReader {
        inner: Cursor::new(archive),
        metadata_budget: &metadata_budget,
    };
    let mut zip = zip::ZipArchive::new(reader)
        .context("update archive is invalid or exceeds metadata budget")?;
    metadata_budget.set(None);
    ensure!(zip.len() <= entries, "update archive has too many entries");
    let mut declared_total = 0u64;
    let mut names = std::collections::BTreeSet::new();
    let mut output_paths = std::collections::BTreeSet::new();
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .context("could not read update archive entry")?;
        let name = entry.name()?;
        let relative = safe_entry_path(&name)?;
        let enclosed = entry
            .enclosed_name()
            .context("update archive contains an unsafe path")?;
        ensure!(
            relative == enclosed,
            "archive path normalization is not allowed"
        );
        ensure!(
            names.insert(relative.to_string_lossy().to_lowercase()),
            "duplicate archive path"
        );
        for path in relative
            .ancestors()
            .filter(|path| !path.as_os_str().is_empty())
        {
            output_paths.insert(path.to_string_lossy().to_lowercase());
            ensure!(
                output_paths.len() <= entries,
                "archive creates too many filesystem entries"
            );
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0 || kind == if entry.is_dir() { 0o040000 } else { 0o100000 },
                "unsupported archive entry type"
            );
        }
        ensure!(entry.size() <= per_file, "archive entry exceeds size limit");
        ensure!(
            !entry.is_dir() || entry.size() == 0,
            "archive directory has file data"
        );
        declared_total = declared_total
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(declared_total <= total, "archive exceeds output limit");
        let destination = staging.join(relative);
        ensure!(
            destination.starts_with(staging),
            "archive path escapes staging"
        );
        let mut cursor = destination.as_path();
        while cursor != staging {
            match fs::symlink_metadata(cursor) {
                Ok(_) => {
                    let metadata = reject_link(cursor)?;
                    ensure!(
                        metadata.is_dir() && (cursor != destination || entry.is_dir()),
                        "archive would overwrite an existing file"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            cursor = cursor.parent().context("archive path escapes staging")?;
        }
    }
    let mut written_total = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let relative = safe_entry_path(&entry.name()?)?.to_path_buf();
        let destination = staging.join(&relative);
        if entry.is_dir() {
            create_entry_directories(staging, &relative)?;
            continue;
        }
        if let Some(parent) = relative.parent() {
            create_entry_directories(staging, parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&destination)
            .with_context(|| format!("could not create {}", destination.display()))?;
        let limit = per_file.min(total - written_total);
        let copied = io::copy(&mut (&mut entry).take(limit), &mut output)?;
        let mut extra = [0u8; 1];
        ensure!(
            entry.read(&mut extra)? == 0 && copied == entry.size(),
            "archive entry exceeds output limit or declared size"
        );
        written_total += copied;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    ensure!(
        reject_link(&staging.join(platform_binary_name()))?.is_file(),
        "update archive does not contain the Bridge executable"
    );
    Ok(())
}

fn verify_install_directory(directory: &Path) -> Result<()> {
    verify_directory_chain(directory)?;
    for name in [
        platform_binary_name(),
        "native-hid",
        "openmouse-app-icon.png",
        "DM-Sans-OFL.txt",
    ] {
        let path = directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => verify_tree(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let _probe =
        StagingDirectory::new_in(directory).context("Bridge install directory is not writable")?;
    Ok(())
}

fn verify_tree(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(path) = pending.pop() {
        count += 1;
        ensure!(
            count <= MAX_ARCHIVE_ENTRIES,
            "update tree has too many entries"
        );
        let metadata = reject_link(&path)?;
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "unsupported update file type"
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
                ensure!(
                    count + pending.len() <= MAX_ARCHIVE_ENTRIES,
                    "update tree has too many entries"
                );
            }
        }
    }
    Ok(())
}

fn write_helper(path: &Path, script: &str) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(script.as_bytes())
        .context("could not write the update helper")
}

fn platform_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    return "openmouse-bridge.exe";
    #[cfg(not(target_os = "windows"))]
    return "openmouse-bridge";
}

#[cfg(any(unix, test))]
const UNIX_TRANSACTION: &str = r#"install_package() (
set -eu
stage="$1"
destination="$2"
binary="$3"
[ -f "$stage/$binary" ] && [ -d "$stage/native-hid" ] || exit 1
for tree_root in "$stage" "$destination"; do
  parent="$tree_root"
  while [ "$parent" != / ]; do
    [ -d "$parent" ] && [ ! -L "$parent" ] || exit 1
    parent=$(dirname "$parent") || exit 1
  done
done
special=$(find "$stage" ! -type f ! -type d -print) || exit 1
[ -z "$special" ] || exit 1
names="native-hid $binary DM-Sans-OFL.txt"
if [ -f "$stage/openmouse-app-icon.png" ]; then names="openmouse-app-icon.png $names"; fi
lock="$destination/.openmouse-update.lock"
(umask 077; mkdir "$lock") || exit 1
transaction=$(umask 077; mktemp -d "$destination/.openmouse-update.XXXXXXXX") || { rmdir "$lock"; exit 1; }
committed=no
rollback() {
  status=$?
  trap - 0 1 2 15
  failed=no
  if [ "$committed" != yes ]; then
    for name in $names; do
      if [ -e "$transaction/old/$name" ]; then
        rm -rf "$destination/$name" && mv "$transaction/old/$name" "$destination/$name" || failed=yes
      elif [ -f "$transaction/installed/$name" ]; then
        rm -rf "$destination/$name" || failed=yes
      fi
    done
  fi
  if [ "$failed" = yes ]; then
    printf '%s\n' "Update rollback failed; backups retained at $transaction" >&2
    exit 1
  fi
  rm -rf "$transaction"
  rmdir "$lock"
  if [ "$committed" != yes ]; then exit 2; fi
  exit "$status"
}
trap rollback 0
trap 'exit 1' 1 2 15
mkdir "$transaction/new" "$transaction/old" "$transaction/installed" || exit 1
for name in $names; do
  if [ -e "$destination/$name" ] || [ -L "$destination/$name" ]; then
    special=$(find "$destination/$name" ! -type f ! -type d -print) || exit 1
    [ -z "$special" ] || exit 1
  fi
  cp -R "$stage/$name" "$transaction/new/$name" || exit 1
 done
chmod u+x "$transaction/new/$binary" || exit 1
for name in $names; do
  if [ -e "$destination/$name" ]; then mv "$destination/$name" "$transaction/old/$name" || exit 1; fi
 done
for name in $names; do
  : > "$transaction/installed/$name" || exit 1
  mv "$transaction/new/$name" "$destination/$name" || exit 1
 done
committed=yes
)
"#;

#[cfg(any(unix, test))]
const UNIX_COMPLETION: &str = r#"complete_update() {
  if install_package "$1" "$2" "$3"; then
    :
  else
    status=$?
    [ "$status" -eq 2 ] || return 1
  fi
  "$4" "$2" "$3"
}
"#;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn spawn_install_helper(staging: &Path, destination: &Path, executable: &Path) -> Result<()> {
    let script = staging.join("install-update.sh");
    let contents = format!(
        "#!/bin/sh\n{UNIX_TRANSACTION}\n{UNIX_COMPLETION}\npid=\"$1\"\nstage=\"$2\"\ndestination=\"$3\"\nbinary=\"$4\"\ntrap 'rm -rf \"$stage\"' 0\nwhile kill -0 \"$pid\" 2>/dev/null; do sleep 1; done\nlaunch_bridge() {{ cd \"$1\" || return 1; nohup \"$1/$2\" >/dev/null 2>&1 & }}\ncomplete_update \"$stage\" \"$destination\" \"$binary\" launch_bridge\n"
    );
    write_helper(&script, &contents)?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(staging)
        .arg(destination)
        .arg(
            executable
                .file_name()
                .context("Bridge executable has no file name")?,
        )
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

#[cfg(windows)]
const WINDOWS_TRANSACTION: &str = r#"function Assert-CleanTree([string]$Path) {
  $Pending = [System.Collections.Generic.Stack[string]]::new()
  $Pending.Push($Path)
  $Count = 0
  while ($Pending.Count -gt 0) {
    $Item = Get-Item -LiteralPath $Pending.Pop() -Force -ErrorAction Stop
    if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Update path contains a reparse point' }
    $Count++
    if ($Count -gt 8192) { throw 'Update tree has too many entries' }
    if ($Item.PSIsContainer) {
      foreach ($Child in Get-ChildItem -LiteralPath $Item.FullName -Force -ErrorAction Stop) {
        $Pending.Push($Child.FullName)
        if ($Count + $Pending.Count -gt 8192) { throw 'Update tree has too many entries' }
      }
    }
  }
}
function Install-BridgePackage([string]$Stage, [string]$Destination, [string]$Binary) {
  foreach ($Root in @($Stage, $Destination)) {
    $Parent = Get-Item -LiteralPath $Root -Force
    while ($null -ne $Parent) {
      if (($Parent.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Update parent contains a reparse point' }
      $Parent = $Parent.Parent
    }
  }
  Assert-CleanTree $Stage
  $Names = @('native-hid', $Binary, 'DM-Sans-OFL.txt')
  if (Test-Path -LiteralPath (Join-Path $Stage 'openmouse-app-icon.png')) { $Names = @('openmouse-app-icon.png') + $Names }
  foreach ($Name in $Names) {
    $Source = Get-Item -LiteralPath (Join-Path $Stage $Name) -Force
    if (($Name -eq 'native-hid') -ne $Source.PSIsContainer) { throw 'Invalid update package type' }
    if (Test-Path -LiteralPath (Join-Path $Destination $Name)) { Assert-CleanTree (Join-Path $Destination $Name) }
  }
  $Lock = Join-Path $Destination '.openmouse-update.lock'
  New-Item -ItemType Directory -Path $Lock -ErrorAction Stop | Out-Null
  $Transaction = Join-Path $Destination ('.openmouse-update.' + [guid]::NewGuid().ToString('N'))
  $Backed = @()
  $Installed = @()
  $RollbackFailed = $false
  try {
    New-Item -ItemType Directory -Path $Transaction -ErrorAction Stop | Out-Null
    $New = Join-Path $Transaction 'new'
    $Old = Join-Path $Transaction 'old'
    New-Item -ItemType Directory -Path $New, $Old -ErrorAction Stop | Out-Null
    foreach ($Name in $Names) {
      Copy-Item -LiteralPath (Join-Path $Stage $Name) -Destination (Join-Path $New $Name) -Recurse -ErrorAction Stop
    }
    foreach ($Name in $Names) {
      if (Test-Path -LiteralPath (Join-Path $Destination $Name)) {
        Move-Item -LiteralPath (Join-Path $Destination $Name) -Destination (Join-Path $Old $Name) -ErrorAction Stop
        $Backed += $Name
      }
    }
    foreach ($Name in $Names) {
      $Installed += $Name
      Move-Item -LiteralPath (Join-Path $New $Name) -Destination (Join-Path $Destination $Name) -ErrorAction Stop
    }
  } catch {
    $Failure = $_
    foreach ($Name in $Installed) {
      try {
        $Target = Join-Path $Destination $Name
        if (Test-Path -LiteralPath $Target) { Remove-Item -LiteralPath $Target -Recurse -Force -ErrorAction Stop }
      } catch { $RollbackFailed = $true }
    }
    foreach ($Name in $Backed) {
      try {
        $Target = Join-Path $Destination $Name
        if (Test-Path -LiteralPath $Target) { throw 'Cannot safely restore update backup over an existing target' }
        Move-Item -LiteralPath (Join-Path $Old $Name) -Destination $Target -ErrorAction Stop
      }
      catch { $RollbackFailed = $true }
    }
    if ($RollbackFailed) { throw "Update rollback failed; backups retained at $Transaction" }
    Write-Warning $Failure
    return $false
  } finally {
    if (-not $RollbackFailed) {
      Remove-Item -LiteralPath $Transaction -Recurse -Force -ErrorAction SilentlyContinue
      Remove-Item -LiteralPath $Lock -Force -ErrorAction SilentlyContinue
    }
  }
  return $true
}
"#;

#[cfg(windows)]
const WINDOWS_COMPLETION: &str = r#"function Complete-BridgeUpdate([string]$Stage, [string]$Destination, [string]$Binary, [scriptblock]$Launch) {
  $Updated = Install-BridgePackage $Stage $Destination $Binary
  & $Launch $Destination $Binary
  return $Updated
}
"#;

#[cfg(target_os = "windows")]
fn spawn_install_helper(staging: &Path, destination: &Path, executable: &Path) -> Result<()> {
    let script = staging.join("install-update.ps1");
    let contents = format!(
        "param([int]$BridgePid, [string]$Stage, [string]$Destination, [string]$Binary)\n$ErrorActionPreference = 'Stop'\n{WINDOWS_TRANSACTION}\n{WINDOWS_COMPLETION}\ntry {{\nwhile (Get-Process -Id $BridgePid -ErrorAction SilentlyContinue) {{ Start-Sleep -Seconds 1 }}\nComplete-BridgeUpdate $Stage $Destination $Binary {{ param($Directory, $Name) Start-Process -FilePath (Join-Path $Directory $Name) -WorkingDirectory $Directory }}\n}} finally {{ Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue }}\n"
    );
    write_helper(&script, &contents)?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-BridgePid")
        .arg(std::process::id().to_string())
        .arg("-Stage")
        .arg(staging)
        .arg("-Destination")
        .arg(destination)
        .arg("-Binary")
        .arg(
            executable
                .file_name()
                .context("Bridge executable has no file name")?,
        );
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn spawn_install_helper(_staging: &Path, _destination: &Path, _executable: &Path) -> Result<()> {
    anyhow::bail!("automatic Bridge updates are available only on macOS, Windows, and Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = env::temp_dir();
        #[cfg(unix)]
        let parent = fs::canonicalize(parent).unwrap();
        let root = parent.join(format!(
            "bridge-archive-test-{}-{}-literal $ O'Connor [x]",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn extraction_never_overwrites_existing_files() {
        let root = fixture();
        let executable = root.join(platform_binary_name());
        fs::write(&executable, b"original fixture").unwrap();
        let bytes = archive(&[(platform_binary_name(), b"replacement")]);
        assert!(extract_archive(&bytes, &root).is_err());
        assert_eq!(fs::read(executable).unwrap(), b"original fixture");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_archive_types_and_portable_paths_are_rejected() {
        for name in [
            "../escape",
            "/absolute",
            "C:/escape",
            "dir\\escape",
            "file:stream",
            "CON",
            "trailing.",
        ] {
            let root = fixture();
            let bytes = archive(&[(name, b"bad"), (platform_binary_name(), b"bridge")]);
            assert!(extract_archive(&bytes, &root).is_err(), "accepted {name}");
            fs::remove_dir_all(root).unwrap();
        }
        for kind in [0o120777u32, 0o010644, 0o020644, 0o060644, 0o140644] {
            let root = fixture();
            let mut bytes = archive(&[(platform_binary_name(), b"bridge")]);
            let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
            bytes[central + 5] = 3;
            bytes[central + 38..central + 42].copy_from_slice(&(kind << 16).to_le_bytes());
            assert!(
                extract_archive(&bytes, &root).is_err(),
                "accepted type {kind:o}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn extraction_does_not_follow_existing_directory_links() {
        let root = fixture();
        let outside = fixture();
        fs::write(outside.join("sentinel"), b"untouched").unwrap();
        let link = root.join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        {
            let status = Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let bytes = archive(&[
            ("linked/escape", b"bad"),
            (platform_binary_name(), b"bridge"),
        ]);
        assert!(extract_archive(&bytes, &root).is_err());
        assert!(!outside.join("escape").exists());
        assert!(extract_archive(&bytes, &link).is_err());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        let executable = root.join(platform_binary_name());
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("sentinel"), &executable).unwrap();
        #[cfg(windows)]
        fs::hard_link(outside.join("sentinel"), &executable).unwrap();
        let bytes = archive(&[(platform_binary_name(), b"replacement")]);
        assert!(extract_archive(&bytes, &root).is_err());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn extraction_masks_all_special_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        let mut bytes = archive(&[(platform_binary_name(), b"bridge")]);
        let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
        bytes[central + 5] = 3;
        bytes[central + 38..central + 42].copy_from_slice(&(0o107755u32 << 16).to_le_bytes());
        extract_archive(&bytes, &root).unwrap();
        assert_eq!(
            fs::metadata(root.join(platform_binary_name()))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn downloads_are_bounded_without_network_or_content_length() {
        let response = reqwest::Response::from(axum::http::Response::new(vec![b'x'; 17]));
        assert!(read_bounded_response(response, 16).await.is_err());
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .header("content-length", "1")
                .body(vec![b'x'; 17])
                .unwrap(),
        );
        assert!(read_bounded_response(response, 16).await.is_err());
        let response = reqwest::Response::from(axum::http::Response::new(vec![b'x'; 16]));
        assert_eq!(
            read_bounded_response(response, 16).await.unwrap(),
            vec![b'x'; 16]
        );
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .header("content-length", "9999999999")
                .body(Vec::<u8>::new())
                .unwrap(),
        );
        assert!(
            read_bounded_response(response, MAX_CHECKSUM_BYTES)
                .await
                .is_err()
        );
    }

    #[test]
    fn archive_entry_and_decompression_limits_preserve_existing_fixtures() {
        let bytes = archive(&[(platform_binary_name(), b"12345"), ("other", b"12345")]);
        for (entries, file, total) in [(1, 5, 10), (2, 4, 10), (2, 5, 9)] {
            let root = fixture();
            fs::write(root.join("sentinel"), b"untouched").unwrap();
            assert!(extract_archive_with_limits(&bytes, &root, entries, file, total).is_err());
            assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"untouched");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
            fs::remove_dir_all(root).unwrap();
        }
        let root = fixture();
        extract_archive_with_limits(&bytes, &root, 2, 5, 10).unwrap();
        assert_eq!(fs::read(root.join("other")).unwrap(), b"12345");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_staging_is_unique_and_cleans_failed_preparation() {
        let parent = fixture();
        let path;
        {
            let first = StagingDirectory::new_in(&parent).unwrap();
            let second = StagingDirectory::new_in(&parent).unwrap();
            assert_ne!(first.path, second.path);
            path = first.path.clone();
            let bytes = archive(&[(platform_binary_name(), b"bridge")]);
            extract_archive(&bytes, &first.path).unwrap();
            assert!(verify_package(&first.path).is_err());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn zip_metadata_reader_stops_before_unbounded_directory_work() {
        let bytes = archive(&[(platform_binary_name(), b"bridge")]);
        let budget = std::cell::Cell::new(Some(8));
        let reader = ArchiveReader {
            inner: Cursor::new(bytes.as_slice()),
            metadata_budget: &budget,
        };
        assert!(zip::ZipArchive::new(reader).is_err());
        assert_eq!(budget.get(), Some(0));
        let budget = std::cell::Cell::new(Some(MAX_ZIP_METADATA_READ_BYTES));
        let reader = ArchiveReader {
            inner: Cursor::new(bytes.as_slice()),
            metadata_budget: &budget,
        };
        assert_eq!(zip::ZipArchive::new(reader).unwrap().len(), 1);
    }

    #[test]
    fn decompression_limit_does_not_trust_declared_entry_size() {
        let root = fixture();
        fs::write(root.join("sentinel"), b"untouched").unwrap();
        let mut bytes = archive(&[(platform_binary_name(), &[b'A'; 1024])]);
        let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1u32.to_le_bytes());
        assert!(extract_archive_with_limits(&bytes, &root, 1, 3, 3).is_err());
        assert!(
            fs::metadata(root.join(platform_binary_name()))
                .is_ok_and(|metadata| metadata.len() <= 3)
        );
        assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"untouched");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_unix_transaction_commits_or_restores_fixture_package() {
        for case in [
            "success",
            "copy-failure",
            "commit-failure",
            "rollback-failure",
        ] {
            let root = fixture();
            let stage = root.join("stage");
            let destination = root.join("install");
            for directory in [&stage, &destination] {
                fs::create_dir(directory).unwrap();
                fs::create_dir(directory.join("native-hid")).unwrap();
            }
            fs::write(root.join("sandbox-marker"), b"bridge-update-fixture-only").unwrap();
            fs::write(stage.join("openmouse-bridge"), b"new-binary").unwrap();
            fs::write(stage.join("native-hid/helper"), b"new-helper").unwrap();
            fs::write(destination.join("openmouse-bridge"), b"old-binary").unwrap();
            fs::write(destination.join("native-hid/helper"), b"old-helper").unwrap();
            fs::write(destination.join("sentinel"), b"untouched").unwrap();
            fs::write(
                stage.join("DM-Sans-OFL.txt"),
                include_bytes!("../assets/fonts/DM-Sans-OFL.txt"),
            )
            .unwrap();
            fs::write(destination.join("DM-Sans-OFL.txt"), b"old-license").unwrap();
            let script = root.join("sandbox.sh");
            let contents = format!(
                "{UNIX_TRANSACTION}\n{UNIX_COMPLETION}\n{}",
                r#"
root="$1"
fixture_case="$2"
[ "$(cat "$root/sandbox-marker")" = bridge-update-fixture-only ] || exit 90
cp() {
  if [ "$fixture_case" = copy-failure ] && [ "$2" = "$root/stage/native-hid" ]; then return 1; fi
  command cp "$@"
}
mv() {
  case "$1" in */new/openmouse-bridge) if [ "$fixture_case" = commit-failure ] || [ "$fixture_case" = rollback-failure ]; then return 1; fi ;; esac
  case "$1" in */old/openmouse-bridge) if [ "$fixture_case" = rollback-failure ]; then return 1; fi ;; esac
  command mv "$@"
}
record_launch() { cat "$1/$2" > "$root/launch-record"; }
complete_update "$root/stage" "$root/install" openmouse-bridge record_launch
"#
            );
            assert!(!contents.contains("nohup") && !contents.contains("kill -0"));
            write_helper(&script, &contents).unwrap();
            let root_argument = root.to_string_lossy().replace('\\', "/");
            #[cfg(windows)]
            let root_argument = format!(
                "/{}{}",
                root_argument[..1].to_ascii_lowercase(),
                &root_argument[2..]
            );
            let output = Command::new(if cfg!(windows) { "sh" } else { "/bin/sh" })
                .arg(&script)
                .arg(&root_argument)
                .arg(case)
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                case != "rollback-failure",
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                root.join("launch-record").exists(),
                case != "rollback-failure",
                "{case}: relaunch eligibility"
            );
            if case == "rollback-failure" {
                assert!(destination.join(".openmouse-update.lock").exists());
                fs::remove_dir_all(root).unwrap();
                continue;
            }
            let new = case == "success";
            assert_eq!(
                fs::read(root.join("launch-record")).unwrap(),
                if new { b"new-binary" } else { b"old-binary" }
            );
            assert_eq!(
                fs::read(destination.join("openmouse-bridge")).unwrap(),
                if new { b"new-binary" } else { b"old-binary" }
            );
            assert_eq!(
                fs::read(destination.join("DM-Sans-OFL.txt"))
                    .unwrap()
                    .as_slice(),
                if new {
                    include_bytes!("../assets/fonts/DM-Sans-OFL.txt").as_slice()
                } else {
                    b"old-license"
                }
            );
            assert_eq!(
                fs::read(destination.join("native-hid/helper")).unwrap(),
                if new { b"new-helper" } else { b"old-helper" }
            );
            assert_eq!(
                fs::read(destination.join("sentinel")).unwrap(),
                b"untouched"
            );
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 4);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn generated_windows_transaction_commits_or_restores_fixture_package() {
        use std::os::windows::fs::OpenOptionsExt;
        for case in [
            "success",
            "copy-failure",
            "locked-binary",
            "commit-failure",
            "existing-lock",
            "rollback-failure",
        ] {
            let root = fixture();
            let stage = root.join("stage");
            let destination = root.join("install");
            for directory in [&stage, &destination] {
                fs::create_dir(directory).unwrap();
                fs::create_dir(directory.join("native-hid")).unwrap();
            }
            fs::write(root.join("sandbox-marker"), b"bridge-update-fixture-only").unwrap();
            fs::write(stage.join(platform_binary_name()), b"new-binary").unwrap();
            fs::write(stage.join("native-hid/helper"), b"new-helper").unwrap();
            fs::write(stage.join("openmouse-app-icon.png"), b"new-icon").unwrap();
            fs::write(destination.join(platform_binary_name()), b"old-binary").unwrap();
            fs::write(destination.join("native-hid/helper"), b"old-helper").unwrap();
            fs::write(destination.join("openmouse-app-icon.png"), b"old-icon").unwrap();
            fs::write(destination.join("sentinel"), b"untouched").unwrap();
            fs::write(
                stage.join("DM-Sans-OFL.txt"),
                include_bytes!("../assets/fonts/DM-Sans-OFL.txt"),
            )
            .unwrap();
            fs::write(destination.join("DM-Sans-OFL.txt"), b"old-license").unwrap();
            if case == "existing-lock" {
                fs::create_dir(destination.join(".openmouse-update.lock")).unwrap();
                fs::write(
                    destination.join(".openmouse-update.lock/owner"),
                    b"other-transaction",
                )
                .unwrap();
            }
            let lock = if case == "locked-binary" {
                Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(destination.join(platform_binary_name()))
                        .unwrap(),
                )
            } else {
                None
            };
            let script = root.join("sandbox.ps1");
            let contents = format!("param([string]$Root, [string]$Case)\n$ErrorActionPreference = 'Stop'\n{WINDOWS_TRANSACTION}\n{WINDOWS_COMPLETION}\n{}
", r#"
if (-not [IO.Path]::IsPathRooted($Root) -or [IO.File]::ReadAllText((Join-Path $Root 'sandbox-marker')) -ne 'bridge-update-fixture-only') { throw 'Not a fixture' }
$Stage = Join-Path $Root 'stage'
$Destination = Join-Path $Root 'install'
$Binary = 'openmouse-bridge.exe'
function Copy-Item([string]$LiteralPath, [string]$Destination, [switch]$Recurse, [string]$ErrorAction) {
  if ($Case -eq 'copy-failure' -and $LiteralPath.EndsWith('native-hid')) { throw 'Injected fixture copy failure' }
  Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
}
function Move-Item([string]$LiteralPath, [string]$Destination, [string]$ErrorAction) {
  if (($Case -eq 'commit-failure' -or $Case -eq 'rollback-failure') -and $LiteralPath.EndsWith(('new\' + $Binary))) { throw 'Injected fixture commit failure' }
  if ($Case -eq 'rollback-failure' -and $LiteralPath.EndsWith(('old\' + $Binary))) { throw 'Injected fixture rollback failure' }
  Microsoft.PowerShell.Management\Move-Item @PSBoundParameters
}
$Failed = $false
try { $Updated = Complete-BridgeUpdate $Stage $Destination $Binary { param($Directory, $Name) [IO.File]::WriteAllText((Join-Path $Root 'launch-record'), [IO.File]::ReadAllText((Join-Path $Directory $Name))) }; $Failed = -not $Updated } catch { $Failed = $true }
if (($Case -eq 'success') -eq $Failed) { throw 'Unexpected fixture transaction result' }
"#);
            assert!(!contents.contains("Start-Process") && !contents.contains("Get-Process"));
            write_helper(&script, &contents).unwrap();
            let output = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script)
                .arg(&root)
                .arg(case)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            drop(lock);
            let new = case == "success";
            assert_eq!(
                root.join("launch-record").exists(),
                !matches!(case, "existing-lock" | "rollback-failure"),
                "{case}: relaunch eligibility"
            );
            if case == "rollback-failure" {
                assert!(fs::read_dir(&destination).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".openmouse-update.")
                }));
                fs::remove_dir_all(root).unwrap();
                continue;
            }
            if case != "existing-lock" {
                assert_eq!(
                    fs::read(root.join("launch-record")).unwrap(),
                    if new { b"new-binary" } else { b"old-binary" }
                );
            }
            assert_eq!(
                fs::read(destination.join(platform_binary_name())).unwrap(),
                if new { b"new-binary" } else { b"old-binary" }
            );
            assert_eq!(
                fs::read(destination.join("DM-Sans-OFL.txt"))
                    .unwrap()
                    .as_slice(),
                if new {
                    include_bytes!("../assets/fonts/DM-Sans-OFL.txt").as_slice()
                } else {
                    b"old-license"
                }
            );
            assert_eq!(
                fs::read(destination.join("native-hid/helper")).unwrap(),
                if new { b"new-helper" } else { b"old-helper" }
            );
            assert_eq!(
                fs::read(destination.join("openmouse-app-icon.png")).unwrap(),
                if new { b"new-icon" } else { b"old-icon" }
            );
            assert_eq!(
                fs::read(destination.join("sentinel")).unwrap(),
                b"untouched"
            );
            assert_eq!(
                fs::read_dir(&destination).unwrap().count(),
                if case == "existing-lock" { 6 } else { 5 }
            );
            if case == "existing-lock" {
                assert_eq!(
                    fs::read(destination.join(".openmouse-update.lock/owner")).unwrap(),
                    b"other-transaction"
                );
            }
            assert_eq!(
                fs::read(stage.join(platform_binary_name())).unwrap(),
                b"new-binary"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn helper_write_is_exclusive_and_package_preflight_is_complete() {
        let root = fixture();
        let script = root.join("install-update.ps1");
        fs::write(&script, b"fixture").unwrap();
        assert!(write_helper(&script, "replacement").is_err());
        assert_eq!(fs::read(&script).unwrap(), b"fixture");
        let node = if cfg!(windows) {
            "native-hid/node/node.exe"
        } else {
            "native-hid/node/bin/node"
        };
        let names = [
            platform_binary_name(),
            "native-hid/src/apply.mjs",
            "native-hid/src/brands.mjs",
            "native-hid/src/hid-device-adapter.mjs",
            "native-hid/src/attack-shark-x11.mjs",
            "native-hid/package.json",
            node,
            "native-hid/node_modules/node-hid/package.json",
            "native-hid/node_modules/@openmouse/protocol/package.json",
            "native-hid/node_modules/@openmouse/keyboard-protocol/package.json",
        ];
        for name in names {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"inert-fixture").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        assert!(verify_package(&root).is_err(), "font license is mandatory");
        fs::write(
            root.join("DM-Sans-OFL.txt"),
            include_bytes!("../assets/fonts/DM-Sans-OFL.txt"),
        )
        .unwrap();
        verify_package(&root).unwrap();
        fs::write(root.join("DM-Sans-OFL.txt"), b"truncated notice").unwrap();
        assert!(
            verify_package(&root).is_err(),
            "font license must be complete"
        );
        fs::write(
            root.join("DM-Sans-OFL.txt"),
            include_bytes!("../assets/fonts/DM-Sans-OFL.txt"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in [platform_binary_name(), node] {
                let path = root.join(name);
                for mode in [0o641, 0o111, 0o400, 0o600, 0o050, 0o005] {
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                    let result = verify_package(&root);
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                    assert!(result.is_err(), "accepted {name} mode {mode:o}");
                }
            }
        }
        for name in names {
            fs::write(root.join(name), []).unwrap();
            assert!(verify_package(&root).is_err(), "accepted empty {name}");
            fs::write(root.join(name), b"inert-fixture").unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn release(version: &str) -> Release {
        Release {
            tag_name: format!("v{version}"),
            assets: vec![
                ReleaseAsset {
                    name: platform_archive_name().unwrap_or("unsupported.zip").into(),
                    browser_download_url: "https://example.test/bridge.zip".into(),
                },
                ReleaseAsset {
                    name: format!(
                        "{}.sha256",
                        platform_archive_name().unwrap_or("unsupported.zip")
                    ),
                    browser_download_url: "https://example.test/bridge.zip.sha256".into(),
                },
            ],
        }
    }

    #[test]
    fn newer_stable_release_is_selected() {
        if platform_archive_name().is_err() {
            return;
        }
        let update = select_update(release("2.0.0"), "1.9.0")
            .expect("release should parse")
            .expect("new release should be selected");
        assert_eq!(update.version, "2.0.0");
    }

    #[test]
    fn current_or_older_release_is_ignored() {
        assert!(select_update(release("1.3.2"), "1.3.2").unwrap().is_none());
        assert!(select_update(release("1.2.9"), "1.3.2").unwrap().is_none());
    }

    #[test]
    fn checksum_must_match_archive() {
        let archive = b"bridge";
        let checksum = format!("{:x}  bridge.zip\n", Sha256::digest(archive));
        verify_checksum(archive, checksum.as_bytes()).expect("checksum should match");
        assert!(
            verify_checksum(
                archive,
                b"0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
    }
}
