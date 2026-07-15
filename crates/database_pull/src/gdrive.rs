//! Google Drive upload using service-account credentials: mint an access
//! token from a signed JWT, resolve the destination folder by name, and
//! stream the dump up in resumable chunks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request, Response};
use serde::{Deserialize, Serialize};
use smol::channel::Sender;

const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
const DRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const DRIVE_UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
// Drive requires chunk sizes to be a multiple of 256 KiB.
const UPLOAD_CHUNK_SIZE: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

#[derive(Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
}

#[derive(Deserialize)]
struct DriveFile {
    id: String,
}

/// Checks that `bytes` parse as a usable service-account key. Used by the
/// credentials import action before storing them in the keychain.
pub(crate) fn validate_service_account_json(bytes: &[u8]) -> Result<()> {
    let key: ServiceAccountKey = serde_json::from_slice(bytes).context(
        "not a Google service-account JSON — expected client_email and private_key fields",
    )?;
    anyhow::ensure!(
        !key.client_email.is_empty() && !key.private_key.is_empty(),
        "the service-account JSON has an empty client_email or private_key"
    );
    Ok(())
}

pub(crate) async fn upload(
    client: Arc<dyn HttpClient>,
    auth_json: Vec<u8>,
    folder: String,
    file_path: PathBuf,
    file_name: String,
    progress: Sender<u64>,
) -> Result<()> {
    let key: ServiceAccountKey = serde_json::from_slice(&auth_json).context(
        "parsing Google credentials — expected a service-account JSON \
         with client_email and private_key fields",
    )?;
    let token = fetch_access_token(client.as_ref(), &key).await?;
    let folder_id = resolve_folder(client.as_ref(), &token, &folder).await?;
    upload_file(
        client.as_ref(),
        &token,
        &folder_id,
        &file_path,
        &file_name,
        progress,
    )
    .await
}

async fn fetch_access_token(client: &dyn HttpClient, key: &ServiceAccountKey) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs();
    let claims = JwtClaims {
        iss: key.client_email.clone(),
        scope: DRIVE_SCOPE.to_string(),
        aud: key.token_uri.clone(),
        iat: now,
        exp: now + 3600,
    };
    let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes())
        .context("reading the service account's private key")?;
    let assertion = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &encoding_key,
    )
    .context("signing the token assertion")?;

    // The assertion is base64url so it needs no form encoding.
    let body = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri(&key.token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(body))?;
    let mut response = client.send(request).await.context("requesting access token")?;
    let body = read_body(&mut response).await?;
    if !response.status().is_success() {
        bail!(
            "Google token request failed ({}): {}",
            response.status(),
            body
        );
    }
    let token: TokenResponse =
        serde_json::from_str(&body).context("parsing Google token response")?;
    Ok(token.access_token)
}

/// Resolves a folder path like "Backups/SBL" to a folder id, creating
/// missing intermediate folders. The first segment must already exist (a
/// folder shared with the service account) — creating it would place it in
/// the service account's own Drive where the user can't see it.
async fn resolve_folder(client: &dyn HttpClient, token: &str, folder: &str) -> Result<String> {
    let mut parent: Option<String> = None;
    for segment in folder.split('/').filter(|segment| !segment.trim().is_empty()) {
        let existing = find_folder(client, token, segment, parent.as_deref()).await?;
        parent = Some(match existing {
            Some(id) => id,
            None => {
                let Some(parent) = parent.as_deref() else {
                    bail!(
                        "Drive folder {segment:?} not found — create it in your Drive and \
                         share it with the service account's email address"
                    );
                };
                create_folder(client, token, segment, parent).await?
            }
        });
    }
    parent.context("the Drive folder path is empty")
}

async fn find_folder(
    client: &dyn HttpClient,
    token: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<Option<String>> {
    let mut query = format!(
        "name='{}' and mimeType='{FOLDER_MIME_TYPE}' and trashed=false",
        escape_drive_query(name)
    );
    if let Some(parent) = parent {
        query.push_str(&format!(" and '{}' in parents", escape_drive_query(parent)));
    }
    let uri = format!(
        "{DRIVE_FILES_URL}?q={}&fields=files(id)&pageSize=1\
         &supportsAllDrives=true&includeItemsFromAllDrives=true",
        urlencode(&query)
    );
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(AsyncBody::empty())?;
    let mut response = client.send(request).await.context("listing Drive folders")?;
    let body = read_body(&mut response).await?;
    if !response.status().is_success() {
        bail!("Drive folder lookup failed ({}): {}", response.status(), body);
    }
    let list: FileList = serde_json::from_str(&body).context("parsing Drive folder list")?;
    Ok(list.files.into_iter().next().map(|file| file.id))
}

async fn create_folder(
    client: &dyn HttpClient,
    token: &str,
    name: &str,
    parent: &str,
) -> Result<String> {
    let metadata = serde_json::json!({
        "name": name,
        "mimeType": FOLDER_MIME_TYPE,
        "parents": [parent],
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("{DRIVE_FILES_URL}?supportsAllDrives=true&fields=id"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json; charset=UTF-8")
        .body(AsyncBody::from(metadata.to_string()))?;
    let mut response = client.send(request).await.context("creating Drive folder")?;
    let body = read_body(&mut response).await?;
    if !response.status().is_success() {
        bail!(
            "creating Drive folder {name:?} failed ({}): {}",
            response.status(),
            body
        );
    }
    let file: DriveFile = serde_json::from_str(&body).context("parsing created Drive folder")?;
    Ok(file.id)
}

async fn upload_file(
    client: &dyn HttpClient,
    token: &str,
    folder_id: &str,
    file_path: &PathBuf,
    file_name: &str,
    progress: Sender<u64>,
) -> Result<()> {
    let total = smol::fs::metadata(file_path)
        .await
        .with_context(|| format!("reading metadata of {file_path:?}"))?
        .len();
    anyhow::ensure!(total > 0, "the dump file is empty");

    let metadata = serde_json::json!({ "name": file_name, "parents": [folder_id] });
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "{DRIVE_UPLOAD_URL}?uploadType=resumable&supportsAllDrives=true"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json; charset=UTF-8")
        .header("X-Upload-Content-Type", "application/gzip")
        .header("X-Upload-Content-Length", total.to_string())
        .body(AsyncBody::from(metadata.to_string()))?;
    let mut response = client
        .send(request)
        .await
        .context("starting Drive upload session")?;
    if !response.status().is_success() {
        let body = read_body(&mut response).await.unwrap_or_default();
        bail!(
            "starting Drive upload failed ({}): {}",
            response.status(),
            body
        );
    }
    let session_uri = response
        .headers()
        .get("location")
        .context("Drive did not return an upload session URL")?
        .to_str()
        .context("invalid upload session URL")?
        .to_string();

    let mut file = smol::fs::File::open(file_path)
        .await
        .with_context(|| format!("opening {file_path:?}"))?;
    let mut buffer = vec![0u8; UPLOAD_CHUNK_SIZE];
    let mut offset: u64 = 0;
    while offset < total {
        let chunk_len = usize::try_from((total - offset).min(UPLOAD_CHUNK_SIZE as u64))
            .context("chunk length overflow")?;
        let mut filled = 0;
        while filled < chunk_len {
            let read = file
                .read(&mut buffer[filled..chunk_len])
                .await
                .context("reading dump chunk")?;
            if read == 0 {
                bail!("the dump file was truncated while uploading");
            }
            filled += read;
        }

        let request = Request::builder()
            .method(Method::PUT)
            .uri(&session_uri)
            .header("Content-Length", chunk_len.to_string())
            .header(
                "Content-Range",
                content_range(offset, chunk_len as u64, total),
            )
            .body(AsyncBody::from(buffer[..chunk_len].to_vec()))?;
        let mut response = client.send(request).await.context("uploading chunk")?;
        match response.status().as_u16() {
            // 308 = "Resume Incomplete": the expected response for every
            // chunk except the last.
            200 | 201 | 308 => {}
            status => {
                let body = read_body(&mut response).await.unwrap_or_default();
                bail!("Drive chunk upload failed ({status}): {body}");
            }
        }
        offset += chunk_len as u64;
        // A closed receiver only means progress reporting stopped.
        progress.try_send(offset).ok();
    }
    Ok(())
}

fn content_range(offset: u64, len: u64, total: u64) -> String {
    format!("bytes {}-{}/{}", offset, offset + len - 1, total)
}

fn escape_drive_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

async fn read_body(response: &mut Response<AsyncBody>) -> Result<String> {
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("reading response body")?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_content_range() {
        assert_eq!(content_range(0, 8, 20), "bytes 0-7/20");
        assert_eq!(content_range(16, 4, 20), "bytes 16-19/20");
    }

    #[test]
    fn test_escape_drive_query() {
        assert_eq!(escape_drive_query("it's"), r"it\'s");
        assert_eq!(escape_drive_query(r"a\b"), r"a\\b");
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("a b'c"), "a%20b%27c");
        assert_eq!(urlencode("safe-chars_1.2~"), "safe-chars_1.2~");
    }
}
