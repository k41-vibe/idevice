//! streaming_zip_conduit service client
//!
//! Streams an app to the device and installs it the way Xcode does: the device extracts the
//! archive while it arrives instead of receiving an `.ipa` over AFC and unpacking it afterwards
//! (`installation_proxy`). This is the only install path that works on a watchOS device, which has
//! no AFC. The stream is a zip with no compression and no central directory, opened by a metadata
//! record; it ends where the central directory would begin.
//!
//! This is a straight port of pymobiledevice3's `streaming_zip_conduit.py` (11.43.1), which is
//! proven to install to the watch through the paired iPhone's companion_proxy.

use std::path::{Path, PathBuf};

use crate::{obf, Idevice, IdeviceError, IdeviceService};

const LOCAL_FILE_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_DIRECTORY_SIGNATURE: &[u8] = b"PK\x01\x02";
const ZIP_METADATA_PATH: &str = "META-INF/com.apple.ZipMetadata.plist";
// stat.S_IFDIR | 0o755 and stat.S_IFREG | 0o644
const STANDARD_DIRECTORY_MODE: u32 = 0o040_755;
const STANDARD_FILE_MODE: u32 = 0o100_644;
const MODE_EXTRA_ID: &[u8] = b"SZ";
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// Errors specific to the streaming zip conduit.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum StreamingZipConduitError {
    #[error("device refused the transfer: {0}")]
    TransferFailed(String),
    #[error("device failed to install the app: {0}")]
    InstallFailed(String),
    #[error("{0} is larger than 4 GiB, which the streaming format cannot carry")]
    EntryTooLarge(String),
}

/// One record of the stream: a directory, a regular file or a symlink. `data` holds the full
/// contents for files and the link target for symlinks (watch apps are small, so buffering a whole
/// entry is fine).
///
/// ponytail: whole entry in memory; stream per-chunk only if an app ever exceeds available memory.
struct Entry {
    name: String,
    mode: u32,
    crc: u32,
    data: Vec<u8>,
}

impl Entry {
    fn header(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let standard = if self.mode & S_IFMT == S_IFDIR {
            STANDARD_DIRECTORY_MODE
        } else {
            STANDARD_FILE_MODE
        };
        let extra: Vec<u8> = if self.mode == standard {
            Vec::new()
        } else {
            let mut e = MODE_EXTRA_ID.to_vec();
            e.extend_from_slice(&2u16.to_le_bytes());
            e.extend_from_slice(&((self.mode & 0xFFFF) as u16).to_le_bytes());
            e
        };
        let size = self.data.len() as u32;
        let mut out = Vec::with_capacity(30 + name.len() + extra.len());
        out.extend_from_slice(&LOCAL_FILE_SIGNATURE.to_le_bytes()); // signature
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method = stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
        out.extend_from_slice(&self.crc.to_le_bytes()); // crc32
        out.extend_from_slice(&size.to_le_bytes()); // compressed size
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&extra);
        out
    }
}

fn crc32_zlib(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn dir_entry(name: String, mode: u32) -> Entry {
    let name = if name.ends_with('/') { name } else { name + "/" };
    Entry { name, mode, crc: 0, data: Vec::new() }
}

fn bytes_entry(name: String, mode: u32, data: Vec<u8>) -> Entry {
    let crc = crc32_zlib(&data);
    Entry { name, mode, crc, data }
}

/// The mode to give `path` on the device, mirroring pymobiledevice3's `_local_mode`.
fn local_mode(path: &Path, is_dir: bool, is_symlink: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            return meta.mode();
        }
        let _ = (is_dir, is_symlink);
        if is_dir { STANDARD_DIRECTORY_MODE } else { STANDARD_FILE_MODE }
    }
    #[cfg(not(unix))]
    {
        // Windows keeps no permission bits. Dirs standard, files executable so binaries can run.
        let _ = is_symlink;
        if is_dir { STANDARD_DIRECTORY_MODE } else { 0o100_000 | 0o755 }
    }
}

/// Pre-order walk of an unpackaged `.app`, laid out under `Payload/` as in an `.ipa`. Order matches
/// Python's `os.walk` top-down: per directory, sorted subdirs then sorted files+symlinked-dirs,
/// descending in sorted order. The byte-for-byte test depends on this ordering.
fn app_directory_entries(app: &Path) -> Result<Vec<Entry>, IdeviceError> {
    let app_name = app
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| IdeviceError::UnexpectedResponse("app path has no name".into()))?;
    let root = format!("Payload/{app_name}");
    let mut entries = vec![
        dir_entry("Payload".into(), STANDARD_DIRECTORY_MODE),
        dir_entry(root.clone(), STANDARD_DIRECTORY_MODE),
    ];
    walk(app, &root, &mut entries)?;
    Ok(entries)
}

fn walk(dir: &Path, prefix: &str, entries: &mut Vec<Entry>) -> Result<(), IdeviceError> {
    // Classify children. A symlink to a directory is listed with files, not recursed into.
    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf, bool)> = Vec::new(); // (name, path, is_symlink)
    for child in std::fs::read_dir(dir)? {
        let child = child?;
        let name = match child.file_name().into_string() {
            Ok(n) => n,
            Err(_) => {
                return Err(IdeviceError::UnexpectedResponse("non-utf8 file name".into()))
            }
        };
        let path = child.path();
        let sym = std::fs::symlink_metadata(&path)?;
        let is_symlink = sym.file_type().is_symlink();
        let is_dir = !is_symlink && sym.is_dir();
        if is_dir {
            subdirs.push((name, path));
        } else {
            files.push((name, path, is_symlink));
        }
    }
    subdirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in &subdirs {
        entries.push(dir_entry(
            format!("{prefix}/{name}"),
            local_mode(path, true, false),
        ));
    }
    for (name, path, is_symlink) in &files {
        let mode = local_mode(path, false, *is_symlink);
        let entry_name = format!("{prefix}/{name}");
        if *is_symlink || mode & S_IFMT == S_IFLNK {
            let target = std::fs::read_link(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| IdeviceError::UnexpectedResponse("non-utf8 symlink".into()))?;
            entries.push(bytes_entry(entry_name, mode | S_IFLNK, target.as_bytes().to_vec()));
        } else {
            let data = std::fs::read(path)?;
            entries.push(bytes_entry(entry_name, mode, data));
        }
    }
    for (_, path) in &subdirs {
        walk(path, &format!("{prefix}/{}", path.file_name().unwrap().to_str().unwrap()), entries)?;
    }
    Ok(())
}

/// Prepend the metadata record that marks the stream as extractable while it arrives, and return
/// the full raw stream (every local-file record followed by the central-directory signature).
fn stream_bytes(entries: Vec<Entry>) -> Result<Vec<u8>, StreamingZipConduitError> {
    let entries: Vec<Entry> = entries
        .into_iter()
        .filter(|e| e.name != "META-INF/" && e.name != ZIP_METADATA_PATH)
        .collect();
    for e in &entries {
        if e.data.len() as u64 > 0xFFFF_FFFF {
            return Err(StreamingZipConduitError::EntryTooLarge(e.name.clone()));
        }
    }
    let total = entries.iter().map(|e| e.data.len() as i64).sum::<i64>();
    let record_count = (entries.len() + 2) as i64;
    let dir_perms = STANDARD_DIRECTORY_MODE as i64;
    let file_perms = STANDARD_FILE_MODE as i64;
    let metadata = {
        // XML plist, as plistlib.dumps() produces. The device re-parses this, so its exact bytes
        // need not match Python's; only the zip structure around it does.
        let value = crate::plist!({
            "RecordCount": record_count,
            "StandardDirectoryPerms": dir_perms,
            "StandardFilePerms": file_perms,
            "TotalUncompressedBytes": total,
            "Version": 2i64,
        });
        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &value)
            .map_err(|e| StreamingZipConduitError::TransferFailed(e.to_string()))?;
        buf
    };

    let mut all = vec![
        dir_entry("META-INF".into(), STANDARD_DIRECTORY_MODE),
        bytes_entry(ZIP_METADATA_PATH.into(), STANDARD_FILE_MODE, metadata),
    ];
    all.extend(entries);

    let mut out = Vec::new();
    for e in &all {
        out.extend_from_slice(&e.header());
        out.extend_from_slice(&e.data);
    }
    out.extend_from_slice(CENTRAL_DIRECTORY_SIGNATURE);
    Ok(out)
}

/// Client for the streaming_zip_conduit service. Each instance performs a single transfer.
pub struct StreamingZipConduitClient {
    pub idevice: Idevice,
}

impl IdeviceService for StreamingZipConduitClient {
    fn service_name() -> std::borrow::Cow<'static, str> {
        obf!("com.apple.streaming_zip_conduit")
    }

    async fn from_stream(idevice: Idevice) -> Result<Self, IdeviceError> {
        Ok(Self { idevice })
    }
}

impl StreamingZipConduitClient {
    pub fn new(idevice: Idevice) -> Self {
        Self { idevice }
    }

    /// Stream a local unpackaged `.app` directory to the device and install it.
    ///
    /// * `developer` - install as a Developer package rather than a Customer one.
    /// * `progress` - called with the completion percentage as the device reports it.
    ///
    /// ponytail: `.app` directory only; add `.ipa` zip input when a caller actually needs it.
    pub async fn install_app(
        &mut self,
        app: &Path,
        developer: bool,
        mut progress: impl FnMut(u64),
    ) -> Result<(), IdeviceError> {
        let entries = app_directory_entries(app)?;
        let stream = stream_bytes(entries)
            .map_err(|e| IdeviceError::UnexpectedResponse(e.to_string()))?;

        let package_type = if developer { "Developer" } else { "Customer" }.to_string();
        let media_subdir = format!("PublicStaging/idevice-{}.ipa", uuid::Uuid::new_v4());
        let install_options = crate::plist!({
            "PackageType": package_type,
        });
        let setup = crate::plist!({
            "InstallOptionsDictionary": install_options,
            "InstallTransferredDirectory": true,
            "MediaSubdir": media_subdir,
            "UserInitiatedTransfer": false,
        });

        self.idevice.send_plist(setup).await?;
        self.idevice.send_raw(&stream).await?;

        loop {
            let response = self.idevice.read_plist().await?;
            if let Some(e) = response.get("Error").and_then(|v| v.as_string()) {
                return Err(IdeviceError::UnexpectedResponse(format!(
                    "streaming_zip_conduit transfer failed: {e}"
                )));
            }
            if response.get("Status").and_then(|v| v.as_string()) == Some("DataComplete") {
                return Ok(());
            }
            if let Some(p) = response.get("InstallProgressDict").and_then(|v| v.as_dictionary()) {
                if let Some(e) = p.get("Error").and_then(|v| v.as_string()) {
                    let desc = p
                        .get("ErrorDescription")
                        .and_then(|v| v.as_string())
                        .unwrap_or("");
                    return Err(IdeviceError::UnexpectedResponse(format!(
                        "install failed: {e}: {desc}"
                    )));
                }
                if let Some(pct) = p.get("PercentComplete").and_then(|v| v.as_unsigned_integer()) {
                    progress(pct);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_standard_dir_has_no_extra() {
        let e = dir_entry("Payload".into(), STANDARD_DIRECTORY_MODE);
        let h = e.header();
        // extra length field (bytes 28..30) is zero for a standard-mode entry
        assert_eq!(u16::from_le_bytes([h[28], h[29]]), 0);
        assert!(e.name.ends_with('/'));
    }

    #[test]
    fn header_nonstandard_mode_carries_sz_extra() {
        let e = bytes_entry("Payload/x/bin".into(), 0o100_755, b"hi".to_vec());
        let h = e.header();
        let name_len = u16::from_le_bytes([h[26], h[27]]) as usize;
        let extra_len = u16::from_le_bytes([h[28], h[29]]) as usize;
        assert_eq!(extra_len, 6);
        let extra = &h[30 + name_len..30 + name_len + extra_len];
        assert_eq!(&extra[0..2], b"SZ");
        assert_eq!(u16::from_le_bytes([extra[2], extra[3]]), 2);
        assert_eq!(u16::from_le_bytes([extra[4], extra[5]]), 0o100_755u16 & 0xFFFF);
    }

    #[test]
    fn crc32_matches_known_value() {
        // zlib.crc32(b"123456789") == 0xCBF43926
        assert_eq!(crc32_zlib(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn stream_starts_with_metadata_and_ends_with_central_dir_sig() {
        let entries = vec![
            dir_entry("Payload".into(), STANDARD_DIRECTORY_MODE),
            bytes_entry("Payload/a".into(), STANDARD_FILE_MODE, b"x".to_vec()),
        ];
        let s = stream_bytes(entries).unwrap();
        // first record is META-INF/, second is the metadata plist path
        assert_eq!(u32::from_le_bytes([s[0], s[1], s[2], s[3]]), LOCAL_FILE_SIGNATURE);
        assert!(s.ends_with(CENTRAL_DIRECTORY_SIGNATURE));
        let as_text = String::from_utf8_lossy(&s);
        assert!(as_text.contains(ZIP_METADATA_PATH));
        assert!(as_text.contains("RecordCount"));
    }
}
