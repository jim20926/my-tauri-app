use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use keyring::{Entry, Error as KeyringError};
use log::{debug, error, info};
use zeroize::Zeroizing;

/*
    加密金鑰階層：

    macOS Keychain
        └── KEK：裝置層級金鑰，共用於所有離線影片
                └── wrapped DEK：寫在每部 .enc 影片的檔頭
                    └── DEK：影片專屬金鑰，用來加密影片 chunks

    KEK 不直接存入影片檔案；DEK 不直接存入 Keychain。
    影片檔案只保存被 KEK 包裝過的 DEK。
*/

pub const CHUNK_SIZE: u64 = 1024 * 1024;

const MAGIC: &[u8; 8] = b"MYENCMP4";
const FORMAT_VERSION: u8 = 1;
const NONCE_SIZE: usize = 12;
const TAG_SIZE: u64 = 16;
const PREFIX_SIZE: usize = 41;
const KEYRING_SERVICE: &str = "com.jim.mytauriapp.content-keys";
const KEYRING_USER: &str = "device-kek-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoErrorKind {
    Internal,
    KeyUnavailable,
    NotFound,
    InvalidFormat,
    Corrupt,
}

#[derive(Debug)]
pub struct CryptoError {
    pub kind: CryptoErrorKind,
    pub message: String,
}

impl CryptoError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            kind: CryptoErrorKind::Internal,
            message: message.into(),
        }
    }

    fn with_kind(kind: CryptoErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CryptoError {}

fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| CryptoError::new(error.to_string()))?;
    Ok(bytes)
}

/// 建立 Keychain 項目的參照。
/// 這一步只指定 service/account，不會讀取或建立實際的 Keychain 資料。
fn keyring_entry() -> Result<Entry, CryptoError> {
    Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|error| {
        CryptoError::with_kind(
            CryptoErrorKind::KeyUnavailable,
            format!("無法初始化作業系統金鑰儲存區：{error}"),
        )
    })
}

fn load_or_create_kek() -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let entry = keyring_entry()?;
    // 先嘗試讀取裝置層級的 KEK。
    // 同一組 KEK 會被不同的離線影片共用。
    match entry.get_secret() {
        Ok(secret) => {
            debug!(target: "crypto", "operation=keychain_read result=success");
            let secret = Zeroizing::new(secret);
            let bytes: [u8; 32] = secret
                .as_slice()
                .try_into()
                .map_err(|_| CryptoError::new("作業系統金鑰儲存區中的 KEK 長度不正確"))?;
            Ok(Zeroizing::new(bytes))
        }
        // Keychain 中尚未有 KEK 時，產生一組隨機的 256-bit KEK
        // KEK 會以「應用程式密碼」項目寫入 macOS Keychain。
        // 首次寫入時，macOS 可能會顯示 Keychain 授權視窗。
        Err(KeyringError::NoEntry) => {
            info!(target: "crypto", "operation=keychain_create result=start");
            let kek = random_bytes::<32>()?;
            let secret = Zeroizing::new(kek.to_vec());
            // 程式寫入 Keychain
            entry.set_secret(&secret).map_err(|error| {
                error!(target: "crypto", "operation=keychain_create result=error error={error}");
                CryptoError::with_kind(
                    CryptoErrorKind::KeyUnavailable,
                    format!("無法保存作業系統 KEK：{error}"),
                )
            })?;
            info!(target: "crypto", "operation=keychain_create result=success");
            Ok(Zeroizing::new(kek))
        }
        Err(error) => {
            error!(target: "crypto", "operation=keychain_read result=error error={error}");
            Err(CryptoError::with_kind(
                CryptoErrorKind::KeyUnavailable,
                format!("無法讀取作業系統 KEK：{error}"),
            ))
        }
    }
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("AES-256 key must always be 32 bytes")
}

fn wrap_aad(asset_id: &str, key_id: &str) -> Vec<u8> {
    format!("my-tauri-app:wrap:v{FORMAT_VERSION}:{asset_id}:{key_id}").into_bytes()
}

fn chunk_aad(asset_id: &str, chunk_index: u64) -> Vec<u8> {
    format!("my-tauri-app:chunk:v{FORMAT_VERSION}:{asset_id}:{chunk_index}").into_bytes()
}

fn encrypt_with_key(
    key: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    cipher(key)
        .encrypt(
            &Nonce::try_from(nonce.as_slice()).expect("AES-GCM nonce must always be 12 bytes"),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::new("AES-GCM 加密失敗"))
}

fn decrypt_with_key(
    key: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    cipher(key)
        .decrypt(
            &Nonce::try_from(nonce.as_slice()).expect("AES-GCM nonce must always be 12 bytes"),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| {
            CryptoError::with_kind(
                CryptoErrorKind::Corrupt,
                "影片資料驗證失敗，檔案可能已損毀或被竄改",
            )
        })
}

fn wrap_dek_with_key(
    kek: &[u8; 32],
    dek: &[u8; 32],
    asset_id: &str,
    key_id: &str,
) -> Result<([u8; NONCE_SIZE], Vec<u8>), CryptoError> {
    let nonce = random_bytes::<NONCE_SIZE>()?;
    let wrapped = encrypt_with_key(kek, &nonce, dek, &wrap_aad(asset_id, key_id))?;
    Ok((nonce, wrapped))
}

fn unwrap_dek_with_key(
    kek: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    wrapped: &[u8],
    asset_id: &str,
    key_id: &str,
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let dek = decrypt_with_key(kek, nonce, wrapped, &wrap_aad(asset_id, key_id))?;
    let bytes: [u8; 32] = dek.as_slice().try_into().map_err(|_| {
        CryptoError::with_kind(CryptoErrorKind::InvalidFormat, "wrapped DEK 長度不正確")
    })?;
    Ok(Zeroizing::new(bytes))
}

#[derive(Debug, Clone)]
struct Header {
    asset_id: String,               // 影片識別碼
    key_id: String,                 // 用來識別包裝此 DEK 的 KEK 版本
    chunk_size: u64,                // 每個加密區塊的大小
    original_size: u64,             // 原始影片大小
    header_size: u64,               // 檔頭總大小，用來定位第一個 chunk
    wrap_nonce: [u8; NONCE_SIZE],   // 加密 wrapped_dek 時使用的 nonce
    wrapped_dek: Vec<u8>,           // 使用 KEK 加密後的 DEK，包含 AES-GCM authentication tag
}

/*
    .enc 檔案的檔頭。
    檔頭包含影片識別資訊，以及被 KEK 加密過的 DEK。
    因此檔頭可以公開存在檔案中，但沒有 KEK 就無法還原 DEK。
*/
impl Header {
    fn encode(&self) -> Result<Vec<u8>, CryptoError> {
        let asset_id = self.asset_id.as_bytes();
        let key_id = self.key_id.as_bytes();
        if asset_id.len() > u16::MAX as usize || key_id.len() > u16::MAX as usize {
            return Err(CryptoError::new("asset_id 或 key_id 太長"));
        }
        let wrapped_len = u32::try_from(self.wrapped_dek.len())
            .map_err(|_| CryptoError::new("wrapped DEK 太長"))?;

        let mut bytes = Vec::with_capacity(
            PREFIX_SIZE + asset_id.len() + key_id.len() + self.wrapped_dek.len(),
        );
        bytes.extend_from_slice(MAGIC);
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&(self.chunk_size as u32).to_le_bytes());
        bytes.extend_from_slice(&self.original_size.to_le_bytes());
        bytes.extend_from_slice(&(asset_id.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(key_id.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&self.wrap_nonce);
        bytes.extend_from_slice(&wrapped_len.to_le_bytes());
        bytes.extend_from_slice(asset_id);
        bytes.extend_from_slice(key_id);
        bytes.extend_from_slice(&self.wrapped_dek);
        Ok(bytes)
    }

    fn read_from(file: &mut File, expected_asset_id: &str) -> Result<Self, CryptoError> {
        let mut prefix = [0_u8; PREFIX_SIZE];
        file.seek(SeekFrom::Start(0))
            .map_err(|error| CryptoError::new(format!("無法讀取加密檔案：{error}")))?;
        file.read_exact(&mut prefix)
            .map_err(|error| CryptoError::new(format!("加密檔案檔頭不完整：{error}")))?;
        if &prefix[..8] != MAGIC {
            return Err(CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "不是支援的加密影片格式",
            ));
        }
        if prefix[8] != FORMAT_VERSION {
            return Err(CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "不支援的加密影片版本",
            ));
        }

        let chunk_size = u32::from_le_bytes(prefix[9..13].try_into().unwrap()) as u64;
        let original_size = u64::from_le_bytes(prefix[13..21].try_into().unwrap());
        let asset_len = u16::from_le_bytes(prefix[21..23].try_into().unwrap()) as usize;
        let key_len = u16::from_le_bytes(prefix[23..25].try_into().unwrap()) as usize;
        let wrap_nonce: [u8; NONCE_SIZE] = prefix[25..37].try_into().unwrap();
        let wrapped_len = u32::from_le_bytes(prefix[37..41].try_into().unwrap()) as usize;

        if chunk_size != CHUNK_SIZE || asset_len > 1024 || key_len > 256 || wrapped_len > 4096 {
            return Err(CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "加密影片檔頭欄位不合法",
            ));
        }
        let mut asset_bytes = vec![0_u8; asset_len];
        let mut key_bytes = vec![0_u8; key_len];
        let mut wrapped_dek = vec![0_u8; wrapped_len];
        file.read_exact(&mut asset_bytes)
            .and_then(|_| file.read_exact(&mut key_bytes))
            .and_then(|_| file.read_exact(&mut wrapped_dek))
            .map_err(|error| CryptoError::new(format!("加密影片檔頭不完整：{error}")))?;

        let asset_id = String::from_utf8(asset_bytes).map_err(|_| {
            CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "加密影片 asset_id 不是有效 UTF-8",
            )
        })?;
        let key_id = String::from_utf8(key_bytes).map_err(|_| {
            CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "加密影片 key_id 不是有效 UTF-8",
            )
        })?;
        if asset_id != expected_asset_id {
            return Err(CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "影片識別碼與加密檔案不一致",
            ));
        }
        if wrapped_dek.len() != 32 + TAG_SIZE as usize {
            return Err(CryptoError::with_kind(
                CryptoErrorKind::InvalidFormat,
                "wrapped DEK 長度不正確",
            ));
        }

        Ok(Self {
            asset_id,
            key_id,
            chunk_size,
            original_size,
            header_size: (PREFIX_SIZE + asset_len + key_len + wrapped_len) as u64,
            wrap_nonce,
            wrapped_dek,
        })
    }
}

pub struct EncryptedFileWriter {
    file: File,
    header: Header,
    dek: Zeroizing<[u8; 32]>,
    buffer: Zeroizing<Vec<u8>>,
    written: u64,
    encrypted_chunks: u64,
}

impl EncryptedFileWriter {
    pub fn create(path: &Path, asset_id: &str) -> Result<Self, CryptoError> {
        info!(target: "crypto", "operation=encrypt_start asset_id={asset_id}");
        let kek = load_or_create_kek()?;
        Self::create_with_key(path, asset_id, &kek)
    }

    pub(crate) fn create_with_key(
        path: &Path,
        asset_id: &str,
        kek: &[u8; 32],
    ) -> Result<Self, CryptoError> {
        // 每部影片各自產生一組隨機的 256-bit DEK。
        // DEK 只用來加密這部影片，不直接存入 Keychain。
        let dek = random_bytes::<32>()?;
        let key_id = KEYRING_USER.to_string();
        // 使用裝置 KEK 加密這部影片專屬的 DEK。
        // 回傳的 wrapped_dek 和 wrap_nonce 會存入 .enc 檔案檔頭；原始 DEK 不會寫入磁碟。
        let (wrap_nonce, wrapped_dek) = wrap_dek_with_key(kek, &dek, asset_id, &key_id)?;
        let header = Header {
            asset_id: asset_id.to_string(),
            key_id,
            chunk_size: CHUNK_SIZE,
            original_size: 0,
            header_size: 0,
            wrap_nonce,
            wrapped_dek,
        };
        let encoded = header.encode()?;
        let header = Header {
            header_size: encoded.len() as u64,
            ..header
        };
        let mut file = File::create(path)
            .map_err(|error| CryptoError::new(format!("無法建立加密暫存檔：{error}")))?;
        file.write_all(&encoded)
            .map_err(|error| CryptoError::new(format!("無法寫入加密檔頭：{error}")))?;
        Ok(Self {
            file,
            header,
            dek: Zeroizing::new(dek),
            buffer: Zeroizing::new(Vec::with_capacity(CHUNK_SIZE as usize)),
            written: 0,
            encrypted_chunks: 0,
        })
    }

    pub fn write_chunk(&mut self, plaintext: &[u8]) -> Result<(), CryptoError> {
        if plaintext.is_empty() {
            return Ok(());
        }
        self.buffer.extend_from_slice(plaintext);
        self.written += plaintext.len() as u64;
        while self.buffer.len() >= CHUNK_SIZE as usize {
            let chunk =
                Zeroizing::new(self.buffer.drain(..CHUNK_SIZE as usize).collect::<Vec<_>>());
            self.write_encrypted_chunk(&chunk)?;
        }
        Ok(())
    }

    // 使用同一部影片的 DEK 加密每個 chunk。
    // 每個 chunk 都產生新的 nonce，避免相同 nonce 重複使用。
    // AES-GCM 同時提供機密性與完整性驗證。
    fn write_encrypted_chunk(&mut self, plaintext: &[u8]) -> Result<(), CryptoError> {
        debug!(
            target: "crypto",
            "operation=encrypt_chunk asset_id={} chunk_index={} plaintext_bytes={}",
            self.header.asset_id,
            self.encrypted_chunks,
            plaintext.len()
        );
        let nonce = random_bytes::<NONCE_SIZE>()?;
        let ciphertext = encrypt_with_key(
            &self.dek,
            &nonce,
            plaintext,
            &chunk_aad(&self.header.asset_id, self.encrypted_chunks),
        )?;
        self.file
            .write_all(&nonce)
            .and_then(|_| self.file.write_all(&ciphertext))
            .map_err(|error| CryptoError::new(format!("無法寫入加密影片資料：{error}")))?;
        self.encrypted_chunks += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64, CryptoError> {
        if !self.buffer.is_empty() {
            let chunk = std::mem::take(&mut self.buffer);
            self.write_encrypted_chunk(&chunk)?;
        }
        self.file
            .seek(SeekFrom::Start(13))
            .and_then(|_| self.file.write_all(&self.written.to_le_bytes()))
            .and_then(|_| self.file.sync_all())
            .map_err(|error| CryptoError::new(format!("無法完成加密影片檔案：{error}")))?;
        info!(
            target: "crypto",
            "operation=encrypt_complete asset_id={} plaintext_bytes={} chunks={}",
            self.header.asset_id,
            self.written,
            self.encrypted_chunks
        );
        Ok(self.written)
    }
}

/*
    開啟加密影片的流程：
    1. 從 macOS Keychain 取得裝置 KEK。
    2. 從 .enc 檔案檔頭讀取 wrapped_dek 與 wrap_nonce。
    3. 使用 KEK 解開 wrapped_dek，還原影片專屬 DEK。
    4. 將 DEK 暫存在記憶體中，並用它解密影片 chunks。
    5. DEK 使用 Zeroizing 保護，離開作用範圍時清除記憶體內容。
*/
pub struct EncryptedFileReader {
    file: File,
    header: Header,
    dek: Zeroizing<[u8; 32]>,
}

impl EncryptedFileReader {
    pub fn open(path: &Path, expected_asset_id: &str) -> Result<Self, CryptoError> {
        debug!(
            target: "crypto",
            "operation=decrypt_reader_open asset_id={expected_asset_id}"
        );
        let kek = load_or_create_kek()?;
        Self::open_with_key(path, expected_asset_id, &kek)
    }

    pub(crate) fn open_with_dek(
        path: &Path,
        expected_asset_id: &str,
        dek: &[u8; 32],
    ) -> Result<Self, CryptoError> {
        let mut file = File::open(path).map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                CryptoErrorKind::NotFound
            } else {
                CryptoErrorKind::Internal
            };
            CryptoError::with_kind(kind, format!("無法開啟加密影片：{error}"))
        })?;
        let header = Header::read_from(&mut file, expected_asset_id)?;
        Ok(Self {
            file,
            header,
            dek: Zeroizing::new(*dek),
        })
    }

    pub(crate) fn clone_dek(&self) -> Zeroizing<[u8; 32]> {
        self.dek.clone()
    }

    pub(crate) fn open_with_key(
        path: &Path,
        expected_asset_id: &str,
        kek: &[u8; 32],
    ) -> Result<Self, CryptoError> {
        let mut file = File::open(path).map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                CryptoErrorKind::NotFound
            } else {
                CryptoErrorKind::Internal
            };
            CryptoError::with_kind(kind, format!("無法開啟加密影片：{error}"))
        })?;
        let header = Header::read_from(&mut file, expected_asset_id)?;
        let dek = unwrap_dek_with_key(
            kek,
            &header.wrap_nonce,
            &header.wrapped_dek,
            &header.asset_id,
            &header.key_id,
        )?;
        Ok(Self { file, header, dek })
    }

    pub fn original_size(&self) -> u64 {
        self.header.original_size
    }

    pub(crate) fn read_chunk(
        &mut self,
        chunk_index: u64,
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        debug!(
            target: "crypto",
            "operation=decrypt_chunk asset_id={} chunk_index={}",
            self.header.asset_id,
            chunk_index
        );
        let chunk_start = chunk_index
            .checked_mul(self.header.chunk_size)
            .ok_or_else(|| CryptoError::new("影片 chunk 位置超出範圍"))?;
        if chunk_start >= self.header.original_size {
            return Err(CryptoError::new("要求的影片 chunk 不存在"));
        }
        let plaintext_len = (self.header.original_size - chunk_start).min(self.header.chunk_size);
        let encrypted_len = plaintext_len + TAG_SIZE;
        let offset = self.header.header_size
            + chunk_index * (NONCE_SIZE as u64 + self.header.chunk_size + TAG_SIZE);

        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| CryptoError::new(format!("無法定位加密影片 chunk：{error}")))?;
        let mut nonce = [0_u8; NONCE_SIZE];
        let mut ciphertext = Zeroizing::new(vec![0_u8; encrypted_len as usize]);
        self.file
            .read_exact(&mut nonce)
            .and_then(|_| self.file.read_exact(ciphertext.as_mut_slice()))
            .map_err(|error| CryptoError::new(format!("加密影片 chunk 不完整：{error}")))?;
        decrypt_with_key(
            &self.dek,
            &nonce,
            &ciphertext,
            &chunk_aad(&self.header.asset_id, chunk_index),
        )
    }

    #[cfg(test)]
    pub fn read_range(&mut self, start: u64, end: u64) -> Result<Vec<u8>, CryptoError> {
        debug!(
            target: "crypto",
            "operation=decrypt_range asset_id={} start={} end={} bytes={}",
            self.header.asset_id,
            start,
            end,
            end.saturating_sub(start)
        );
        if start >= end || end > self.header.original_size {
            return Err(CryptoError::new("要求的影片範圍不合法"));
        }
        let first_chunk = start / self.header.chunk_size;
        let last_chunk = (end - 1) / self.header.chunk_size;
        let mut output = Vec::with_capacity((end - start) as usize);

        for chunk_index in first_chunk..=last_chunk {
            let chunk_start = chunk_index * self.header.chunk_size;
            let plaintext_len =
                (self.header.original_size - chunk_start).min(self.header.chunk_size);
            let plaintext = self.read_chunk(chunk_index)?;

            let from = start.max(chunk_start) - chunk_start;
            let to = end.min(chunk_start + plaintext_len) - chunk_start;
            output.extend_from_slice(&plaintext[from as usize..to as usize]);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_round_trip_rejects_wrong_aad() {
        let key = [7_u8; 32];
        let nonce = [9_u8; NONCE_SIZE];
        let plaintext = b"encrypted test payload";
        let ciphertext = encrypt_with_key(&key, &nonce, plaintext, b"asset:video:0").unwrap();

        let decoded = decrypt_with_key(&key, &nonce, &ciphertext, b"asset:video:0").unwrap();
        assert_eq!(decoded.as_slice(), plaintext);
        assert!(decrypt_with_key(&key, &nonce, &ciphertext, b"asset:video:1").is_err());
    }

    #[test]
    fn header_round_trip_preserves_fields() {
        let header = Header {
            asset_id: "video-1".to_string(),
            key_id: "device-kek-v1".to_string(),
            chunk_size: CHUNK_SIZE,
            original_size: 123,
            header_size: 0,
            wrap_nonce: [1_u8; NONCE_SIZE],
            wrapped_dek: vec![2_u8; 48],
        };
        let encoded = header.encode().unwrap();
        let path =
            std::env::temp_dir().join(format!("my-tauri-header-test-{}", std::process::id()));
        std::fs::write(&path, encoded).unwrap();
        let mut file = File::open(&path).unwrap();
        let decoded = Header::read_from(&mut file, "video-1").unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(decoded.asset_id, "video-1");
        assert_eq!(decoded.original_size, 123);
        assert_eq!(decoded.header_size, header.encode().unwrap().len() as u64);
    }

    #[test]
    fn encrypted_file_supports_random_access_across_chunks() {
        let path = std::env::temp_dir().join(format!(
            "my-tauri-encrypted-video-test-{}",
            std::process::id()
        ));
        let data: Vec<u8> = (0..(CHUNK_SIZE as usize * 2 + 123))
            .map(|index| (index % 251) as u8)
            .collect();
        let kek = [3_u8; 32];

        let mut writer = EncryptedFileWriter::create_with_key(&path, "video-test", &kek).unwrap();
        for part in data.chunks(7_777) {
            writer.write_chunk(part).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), data.len() as u64);

        let mut reader = EncryptedFileReader::open_with_key(&path, "video-test", &kek).unwrap();
        assert_eq!(reader.original_size(), data.len() as u64);
        assert_eq!(reader.read_range(0, data.len() as u64).unwrap(), data);
        let start = CHUNK_SIZE - 10;
        let end = CHUNK_SIZE + 20;
        assert_eq!(
            reader.read_range(start, end).unwrap(),
            data[start as usize..end as usize]
        );
        assert!(EncryptedFileReader::open_with_key(&path, "wrong-video", &kek).is_err());
        let _ = std::fs::remove_file(path);
    }
}
