//! Steam's content servers (SteamPipe): depot manifests and chunks.
//!
//! A depot version is described by a manifest: every file, and the chunks
//! (up to 1 MiB each, identified by the SHA-1 of their plain bytes) it's
//! built from. Manifests and chunks are plain HTTP(S) downloads; what makes
//! them private is the depot key (filenames and chunks are AES-256
//! encrypted with it) and the manifest request code, which Steam only gives
//! to accounts that own the depot.

use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aes::Aes256;
use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit, generic_array::GenericArray};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use sha1::{Digest, Sha1};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::content_manifest::{
    ContentManifestMetadata, ContentManifestPayload, ContentManifestSignature,
};
use steam_vent_proto::protobuf::Message;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::{
    CContentServerDirectory_GetManifestRequestCode_Request,
    CContentServerDirectory_GetServersForSteamPipe_Request,
};

pub type DepotKey = [u8; 32];

/// `EDepotFileFlag` bits that matter when writing files.
pub mod flags {
    pub const EXECUTABLE: u32 = 32;
    pub const DIRECTORY: u32 = 64;
    pub const SYMLINK: u32 = 512;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub depot: u32,
    pub gid: u64,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Relative path with `/` separators, checked to stay inside the game.
    pub name: String,
    pub size: u64,
    pub flags: u32,
    pub sha: Vec<u8>,
    pub chunks: Vec<Chunk>,
    pub link_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// SHA-1 of the plain bytes; also the chunk's name on the server.
    pub id: Vec<u8>,
    pub checksum: u32,
    pub offset: u64,
    pub size: u32,
}

/// The key that decrypts a depot's chunks and filenames. Steam only hands
/// it to accounts that own the depot.
pub async fn depot_key(connection: &Connection, appid: u32, depot: u32) -> Result<DepotKey> {
    let response: CMsgClientGetDepotDecryptionKeyResponse = connection
        .job(CMsgClientGetDepotDecryptionKey {
            depot_id: Some(depot),
            app_id: Some(appid),
            ..Default::default()
        })
        .await
        .with_context(|| format!("depot key request for {depot} failed"))?;
    if response.eresult() != 1 {
        bail!(
            "Steam refused the key for depot {depot} (result {})",
            response.eresult()
        );
    }
    response
        .depot_encryption_key()
        .try_into()
        .map_err(|_| anyhow!("depot {depot}'s key has the wrong length"))
}

/// A short-lived code that must accompany manifest downloads.
pub async fn manifest_request_code(
    connection: &Connection,
    appid: u32,
    depot: u32,
    manifest: u64,
) -> Result<u64> {
    let response = connection
        .service_method(CContentServerDirectory_GetManifestRequestCode_Request {
            app_id: Some(appid),
            depot_id: Some(depot),
            manifest_id: Some(manifest),
            app_branch: Some("public".into()),
            ..Default::default()
        })
        .await
        .with_context(|| format!("manifest request code for depot {depot} failed"))?;
    match response.manifest_request_code() {
        0 => bail!("Steam refused a manifest request code for depot {depot}"),
        code => Ok(code),
    }
}

pub struct Cdn {
    http: reqwest::Client,
    /// `scheme://host`, best first.
    servers: Vec<String>,
    next: AtomicUsize,
}

impl Cdn {
    /// Ask Steam which content servers to use from here.
    pub async fn discover(connection: &Connection, appid: u32) -> Result<Cdn> {
        let response = connection
            .service_method(CContentServerDirectory_GetServersForSteamPipe_Request {
                cell_id: Some(connection.cell_id()),
                max_servers: Some(20),
                ..Default::default()
            })
            .await
            .context("content server list request failed")?;
        let mut servers: Vec<_> = response
            .servers
            .iter()
            .filter(|s| matches!(s.type_(), "SteamCache" | "CDN"))
            .filter(|s| !s.use_as_proxy() && !s.steam_china_only())
            .filter(|s| s.allowed_app_ids.is_empty() || s.allowed_app_ids.contains(&appid))
            .collect();
        servers.sort_by(|a, b| a.weighted_load().total_cmp(&b.weighted_load()));
        let servers: Vec<String> = servers
            .iter()
            .map(|s| {
                let host = if s.vhost().is_empty() {
                    s.host()
                } else {
                    s.vhost()
                };
                // Like Steam itself: HTTPS only where the server demands
                // it. Many caches' certificates don't match the name they're
                // listed under, and the content doesn't rely on transport
                // security: chunks and file names are encrypted with the
                // depot key and checked against the manifest's hashes.
                let scheme = if s.https_support() == "mandatory" {
                    "https"
                } else {
                    "http"
                };
                format!("{scheme}://{host}")
            })
            .collect();
        if servers.is_empty() {
            bail!("Steam listed no usable content servers");
        }
        Ok(Cdn {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .user_agent("Valve/Steam HTTP Client 1.0")
                .build()?,
            servers,
            next: AtomicUsize::new(0),
        })
    }

    /// GET with retries, moving to the next server on each failure so one
    /// bad server doesn't stall the download.
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        const ATTEMPTS: usize = 6;
        let mut last_error = None;
        for attempt in 0..ATTEMPTS {
            let index = self.next.fetch_add(1, Ordering::Relaxed) % self.servers.len();
            let url = format!("{}{path}", self.servers[index]);
            match self.try_get(&url).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    tracing::debug!(%url, error = %e, "content server request failed");
                    last_error = Some(e);
                    tokio::time::sleep(Duration::from_millis(250 << attempt.min(4))).await;
                }
            }
        }
        Err(last_error.unwrap().context(format!("downloading {path}")))
    }

    async fn try_get(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn manifest(
        &self,
        depot: u32,
        gid: u64,
        request_code: u64,
        key: &DepotKey,
    ) -> Result<Manifest> {
        let bytes = self
            .get(&format!("/depot/{depot}/manifest/{gid}/5/{request_code}"))
            .await?;
        parse_manifest(&bytes, Some(key))
            .with_context(|| format!("manifest {gid} of depot {depot} is malformed"))
    }

    /// One chunk, decrypted, decompressed and checked.
    pub async fn chunk(&self, depot: u32, chunk: &Chunk, key: &DepotKey) -> Result<Vec<u8>> {
        let raw = self
            .get(&format!("/depot/{depot}/chunk/{}", hex::encode(&chunk.id)))
            .await?;
        let (chunk, key) = (chunk.clone(), *key);
        tokio::task::spawn_blocking(move || decode_chunk(&raw, &key, &chunk)).await?
    }
}

const PAYLOAD_MAGIC: u32 = 0x71F617D0;
const METADATA_MAGIC: u32 = 0x1F4812BE;
const SIGNATURE_MAGIC: u32 = 0x1B81B817;
const END_MAGIC: u32 = 0x32C415AB;

/// A v5 manifest: a zip holding length-prefixed protobuf sections.
pub fn parse_manifest(bytes: &[u8], key: Option<&DepotKey>) -> Result<Manifest> {
    let unzipped;
    let mut data = bytes;
    if bytes.starts_with(b"PK") {
        unzipped = unzip_single(bytes)?;
        data = &unzipped;
    }

    let mut payload = None;
    let mut metadata = None;
    loop {
        ensure!(data.len() >= 4, "manifest ends early");
        let magic = u32::from_le_bytes(data[..4].try_into()?);
        if magic == END_MAGIC {
            break;
        }
        ensure!(data.len() >= 8, "manifest ends early");
        let len = u32::from_le_bytes(data[4..8].try_into()?) as usize;
        ensure!(data.len() >= 8 + len, "manifest section overruns the file");
        let section = &data[8..8 + len];
        match magic {
            PAYLOAD_MAGIC => payload = Some(ContentManifestPayload::parse_from_bytes(section)?),
            METADATA_MAGIC => metadata = Some(ContentManifestMetadata::parse_from_bytes(section)?),
            SIGNATURE_MAGIC => {
                ContentManifestSignature::parse_from_bytes(section)?;
            }
            other => bail!("unknown manifest section {other:#x}"),
        }
        data = &data[8 + len..];
        if data.is_empty() {
            break;
        }
    }
    let payload = payload.context("manifest has no file list")?;
    let metadata = metadata.context("manifest has no metadata")?;

    let encrypted = metadata.filenames_encrypted();
    let decode_name = |raw: &str| -> Result<String> {
        if !encrypted {
            return Ok(raw.to_owned());
        }
        let key = key.context("manifest filenames are encrypted and no key was given")?;
        let cipher = base64::engine::general_purpose::STANDARD
            .decode(raw.split_whitespace().collect::<String>())
            .context("encrypted filename isn't base64")?;
        let plain = symmetric_decrypt(key, &cipher)?;
        Ok(String::from_utf8(plain)?.trim_end_matches('\0').to_owned())
    };

    let mut files = Vec::with_capacity(payload.mappings.len());
    for mapping in &payload.mappings {
        let name = clean_path(&decode_name(mapping.filename())?)?;
        let link_target = match mapping.linktarget() {
            "" => None,
            target => Some(decode_name(target)?),
        };
        let mut chunks: Vec<Chunk> = mapping
            .chunks
            .iter()
            .map(|c| Chunk {
                id: c.sha().to_vec(),
                checksum: c.crc(),
                offset: c.offset(),
                size: c.cb_original(),
            })
            .collect();
        chunks.sort_by_key(|c| c.offset);
        files.push(FileEntry {
            name,
            size: mapping.size(),
            flags: mapping.flags(),
            sha: mapping.sha_content().to_vec(),
            chunks,
            link_target,
        });
    }
    Ok(Manifest {
        depot: metadata.depot_id(),
        gid: metadata.gid_manifest(),
        files,
    })
}

/// Normalise a manifest path to `a/b/c` and refuse anything that would
/// escape the install folder.
pub fn clean_path(raw: &str) -> Result<String> {
    let parts: Vec<&str> = raw
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    ensure!(!parts.is_empty(), "empty path in manifest");
    for part in &parts {
        ensure!(
            *part != ".." && !part.contains(':'),
            "unsafe path in manifest: {raw:?}"
        );
    }
    Ok(parts.join("/"))
}

/// Steam's symmetric scheme: the first block is the IV, itself encrypted
/// with AES-256-ECB; the rest is AES-256-CBC with PKCS#7 padding.
pub fn symmetric_decrypt(key: &DepotKey, data: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        data.len() >= 32 && data.len().is_multiple_of(16),
        "encrypted data has a bad length ({})",
        data.len()
    );
    let mut iv = GenericArray::clone_from_slice(&data[..16]);
    Aes256::new(key.into()).decrypt_block(&mut iv);
    let mut buf = data[16..].to_vec();
    let len = cbc::Decryptor::<Aes256>::new(key.into(), &iv)
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("decryption failed (wrong key?)"))?
        .len();
    buf.truncate(len);
    Ok(buf)
}

pub fn decode_chunk(raw: &[u8], key: &DepotKey, chunk: &Chunk) -> Result<Vec<u8>> {
    let plain = symmetric_decrypt(key, raw)?;
    let data = decompress(&plain, chunk.size as usize)?;
    ensure!(
        data.len() == chunk.size as usize,
        "chunk {} is {} bytes, expected {}",
        hex::encode(&chunk.id),
        data.len(),
        chunk.size
    );
    ensure!(
        steam_adler32(&data) == chunk.checksum,
        "chunk {} failed its checksum",
        hex::encode(&chunk.id)
    );
    Ok(data)
}

/// Chunks come in three wrappers: VZip (LZMA), VZstd, and plain zip for
/// old content.
fn decompress(data: &[u8], size: usize) -> Result<Vec<u8>> {
    if data.starts_with(b"VSZa") {
        // "VSZa", crc32 | zstd frame | crc32, size, 4 unknown bytes, "zsv"
        ensure!(
            data.len() >= 8 + 15 && data.ends_with(b"zsv"),
            "bad VZstd chunk"
        );
        let frame = &data[8..data.len() - 15];
        let mut out = Vec::with_capacity(size);
        ruzstd::decoding::StreamingDecoder::new(frame)
            .map_err(|e| anyhow!("bad zstd frame: {e}"))?
            .read_to_end(&mut out)?;
        Ok(out)
    } else if data.starts_with(b"VZa") {
        // "VZa", u32 | 5 bytes of LZMA properties, stream | crc32, size, "zv"
        ensure!(
            data.len() >= 7 + 5 + 10 && data.ends_with(b"zv"),
            "bad VZip chunk"
        );
        let footer = &data[data.len() - 10..];
        let crc = u32::from_le_bytes(footer[..4].try_into()?);
        let mut out = Vec::with_capacity(size);
        lzma_rs::lzma_decompress_with_options(
            &mut &data[7..data.len() - 10],
            &mut out,
            &lzma_rs::decompress::Options {
                unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(size as u64)),
                ..Default::default()
            },
        )
        .map_err(|e| anyhow!("bad LZMA stream: {e:?}"))?;
        ensure!(crc32fast::hash(&out) == crc, "VZip chunk failed its CRC");
        Ok(out)
    } else if data.starts_with(b"PK") {
        unzip_single(data)
    } else {
        bail!("unknown chunk compression")
    }
}

fn unzip_single(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    ensure!(!archive.is_empty(), "empty zip");
    let mut entry = archive.by_index(0)?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out)?;
    Ok(out)
}

/// Adler-32 as Steam computes it: both sums start at 0, not 1.
pub fn steam_adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (0u32, 0u32);
    // 5552 is the most bytes that can be summed before b can overflow.
    for block in data.chunks(5552) {
        for &byte in block {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    a | (b << 16)
}

/// SHA-1 of a byte range, used to check what's already on disk.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated outside Rust so the decoder isn't only checked against
    // itself: `openssl enc` for the AES layers, Python's lzma/zipfile for
    // the compression. See `tests/fixtures/README.md`.
    const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn key() -> DepotKey {
        hex::decode(KEY_HEX).unwrap().try_into().unwrap()
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn plain() -> Vec<u8> {
        fixture("chunk.plain")
    }

    fn chunk_for(data: &[u8]) -> Chunk {
        Chunk {
            id: sha1(data).to_vec(),
            checksum: steam_adler32(data),
            offset: 0,
            size: data.len() as u32,
        }
    }

    #[test]
    fn adler_starts_from_zero() {
        // Standard Adler-32 would give 1 and 0x00620062 here.
        assert_eq!(steam_adler32(b""), 0);
        assert_eq!(steam_adler32(b"a"), 0x0061_0061);
        let long = vec![0xFFu8; 100_000];
        let (mut a, mut b) = (0u64, 0u64);
        for &x in &long {
            a = (a + x as u64) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(steam_adler32(&long), (a | (b << 16)) as u32);
    }

    #[test]
    fn decodes_vzip_lzma_chunk() {
        let data = plain();
        let out = decode_chunk(&fixture("chunk.vzip.enc"), &key(), &chunk_for(&data)).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn decodes_zip_chunk() {
        let data = plain();
        let out = decode_chunk(&fixture("chunk.zip.enc"), &key(), &chunk_for(&data)).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn decodes_vzstd_chunk() {
        let data = plain();
        let out = decode_chunk(&fixture("chunk.vzstd.enc"), &key(), &chunk_for(&data)).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn rejects_wrong_key_and_bad_checksum() {
        let data = plain();
        let mut wrong = key();
        wrong[0] ^= 1;
        assert!(decode_chunk(&fixture("chunk.vzip.enc"), &wrong, &chunk_for(&data)).is_err());
        let mut chunk = chunk_for(&data);
        chunk.checksum ^= 1;
        let err = decode_chunk(&fixture("chunk.vzip.enc"), &key(), &chunk).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn parses_manifest_with_encrypted_names() {
        let manifest = parse_manifest(&fixture("manifest.zip"), Some(&key())).unwrap();
        assert_eq!(manifest.depot, 4001);
        assert_eq!(manifest.gid, 1234567890123456789);
        let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["bin/win64/game.exe", "data", "data/pak0.pak", "link"]
        );
        let exe = &manifest.files[0];
        assert_eq!(exe.size, 2_500_000);
        assert_eq!(exe.flags, flags::EXECUTABLE);
        assert_eq!(exe.chunks.len(), 3);
        assert!(exe.chunks.windows(2).all(|w| w[0].offset < w[1].offset));
        assert_eq!(manifest.files[1].flags, flags::DIRECTORY);
        assert_eq!(
            manifest.files[3].link_target.as_deref(),
            Some("bin/win64/game.exe")
        );
        // Without the key the names can't be read.
        assert!(parse_manifest(&fixture("manifest.zip"), None).is_err());
    }

    #[test]
    fn refuses_paths_outside_the_game() {
        assert_eq!(
            clean_path(r"bin\win64\.\game.exe").unwrap(),
            "bin/win64/game.exe"
        );
        assert!(clean_path(r"..\..\.bashrc").is_err());
        assert!(clean_path("a/../../b").is_err());
        assert!(clean_path(r"C:\Windows\x.dll").is_err());
        assert!(clean_path("/").is_err());
    }
}
