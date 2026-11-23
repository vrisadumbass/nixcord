//! Pure RFC 7578 `multipart/form-data` encoder for Discord message attachments.
//! (Contract stub — full implementation & tests owned by `upload-eng`.)

/// Maximum number of attachments allowed per message.
pub const MAX_ATTACHMENTS: usize = 10;
/// Maximum bytes per single file attachment (10 MiB).
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// Maximum combined bytes across all attachments in one message (25 MiB).
pub const MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFile {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl UploadFile {
    pub fn from_bytes(filename: impl Into<String>, bytes: Vec<u8>) -> Self {
        let filename = filename.into();
        let content_type = guess_content_type(&filename).to_string();
        Self {
            filename,
            content_type,
            bytes,
        }
    }

    pub fn is_image(&self) -> bool {
        self.content_type.starts_with("image/")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedMultipart {
    pub boundary: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MultipartError {
    #[error("too many attachments (max {MAX_ATTACHMENTS})")]
    TooManyFiles,
    #[error("file `{0}` exceeds size limit")]
    FileTooLarge(String),
    #[error("combined attachments exceed size limit")]
    TotalTooLarge,
    #[error("failed to serialize payload_json: {0}")]
    SerializePayload(String),
}

pub fn guess_content_type(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "txt" | "log" | "md" | "rs" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

pub fn encode(
    payload: &serde_json::Value,
    files: &[UploadFile],
) -> Result<EncodedMultipart, MultipartError> {
    let boundary = "nixcord-stub-boundary".to_string();
    let json = serde_json::to_vec(payload)
        .map_err(|e| MultipartError::SerializePayload(e.to_string()))?;
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n").as_bytes());
    body.extend_from_slice(&json);
    body.extend_from_slice(b"\r\n");
    for (i, f) in files.iter().enumerate() {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files[{i}]\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n", f.filename, f.content_type).as_bytes());
        body.extend_from_slice(&f.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(EncodedMultipart {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        boundary,
        body,
    })
}
