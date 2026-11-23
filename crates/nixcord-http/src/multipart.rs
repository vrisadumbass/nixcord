//! Pure RFC 7578 `multipart/form-data` encoder for Discord message attachments.
//!
//! Zero external crates beyond `serde_json` and `thiserror`: builds Discord v10
//! `POST /channels/{id}/messages` multipart bodies with a `payload_json` part
//! (including the `attachments` metadata array) followed by `files[0]`..`files[n-1]`.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum number of attachments allowed per message (Discord limit).
pub const MAX_ATTACHMENTS: usize = 10;
/// Maximum bytes per single file attachment (10 MiB safe default cap).
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// Maximum combined bytes across all attachments in one message (25 MiB cap).
pub const MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
/// RFC 2046 §5.1.1 limits MIME boundary strings to at most 70 ASCII characters.
const MAX_BOUNDARY_LEN: usize = 70;

/// A single file staged for multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFile {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl UploadFile {
    /// Construct an [`UploadFile`] from raw bytes, guessing `content_type` from the extension.
    pub fn from_bytes(filename: impl Into<String>, bytes: Vec<u8>) -> Self {
        let filename = sanitize_filename(&filename.into());
        let content_type = guess_content_type(&filename).to_string();
        Self {
            filename,
            content_type,
            bytes,
        }
    }

    /// True when the MIME type is an image (`image/*`).
    pub fn is_image(&self) -> bool {
        self.content_type
            .trim()
            .to_ascii_lowercase()
            .starts_with("image/")
    }
}

/// Output of [`encode`]: ready to send as an HTTP request body with `content_type`.
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

/// Guess a MIME type from a filename's extension (case-insensitive).
/// Falls back to `"application/octet-stream"` when unknown or missing.
pub fn guess_content_type(filename: &str) -> &'static str {
    let base = basename(filename);
    let Some((stem, ext)) = base.rsplit_once('.') else {
        return DEFAULT_CONTENT_TYPE;
    };
    if stem.is_empty() || ext.is_empty() {
        return DEFAULT_CONTENT_TYPE;
    }
    match ext.to_ascii_lowercase().as_str() {
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        // Video
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        // Audio
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        // Text & source code
        "txt" | "log" | "md" | "markdown" | "rs" | "py" | "toml" | "yaml" | "yml" | "ini"
        | "cfg" | "sh" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "ts" | "tsx" | "jsx" => {
            "text/plain; charset=utf-8"
        }
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "json" => "application/json",
        // Documents & archives
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "7z" => "application/x-7z-compressed",
        "wasm" => "application/wasm",
        _ => DEFAULT_CONTENT_TYPE,
    }
}

/// Extract the final path component, stripping directories on both Unix and Windows.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or("")
}

/// Sanitize a filename for safe inclusion in MIME headers and Discord's `attachments` metadata:
/// keeps only the basename, removes control characters (including CR/LF/NUL), and falls back
/// to `"attachment"` if nothing usable remains.
pub fn sanitize_filename(name: &str) -> String {
    let clean: String = basename(name).chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        "attachment".to_string()
    } else {
        clean.to_string()
    }
}

/// Sanitize a MIME header value (e.g. `Content-Type`) against CRLF/control injection.
pub fn sanitize_content_type(ct: &str) -> String {
    let clean: String = ct.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        DEFAULT_CONTENT_TYPE.to_string()
    } else {
        clean.to_string()
    }
}

/// Escape `\` and `"` inside a MIME quoted-string parameter (`filename="..."`).
fn escape_quoted_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_control() {
            continue;
        }
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Return true if `needle` appears anywhere inside `haystack`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Some(&first) = needle.first() else {
        return true;
    };
    let nlen = needle.len();
    if haystack.len() < nlen {
        return false;
    }
    let last = haystack.len() - nlen;
    let mut i = 0;
    while i <= last {
        if haystack[i] == first && &haystack[i..i + nlen] == needle {
            return true;
        }
        i += 1;
    }
    false
}

/// Generate a collision-free RFC 2046 boundary token guaranteed not to occur in `payload_bytes`
/// or any file's bytes.
pub fn generate_boundary(payload_bytes: &[u8], files: &[UploadFile]) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = format!("nixcord-{nanos:016x}-{seq:08x}");
    ensure_boundary_unique(&base, payload_bytes, files)
}

fn ensure_boundary_unique(base: &str, payload_bytes: &[u8], files: &[UploadFile]) -> String {
    let base: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(MAX_BOUNDARY_LEN - 12)
        .collect();
    let base = if base.is_empty() {
        "nixcord-boundary".to_string()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut attempt: u64 = 0;
    loop {
        let b = candidate.as_bytes();
        let collides = contains_subslice(payload_bytes, b)
            || files.iter().any(|f| contains_subslice(&f.bytes, b));
        if !collides {
            return candidate;
        }
        attempt += 1;
        candidate.clear();
        let _ = write!(candidate, "{base}-{attempt:x}");
    }
}

/// Validate attachment count, per-file byte limit, and combined byte cap.
pub fn validate_files(files: &[UploadFile]) -> Result<usize, MultipartError> {
    if files.len() > MAX_ATTACHMENTS {
        return Err(MultipartError::TooManyFiles);
    }
    let mut total: usize = 0;
    for file in files {
        if file.bytes.len() > MAX_FILE_BYTES {
            return Err(MultipartError::FileTooLarge(sanitize_filename(
                &file.filename,
            )));
        }
        total = total.saturating_add(file.bytes.len());
        if total > MAX_TOTAL_BYTES {
            return Err(MultipartError::TotalTooLarge);
        }
    }
    Ok(total)
}

/// Build the serialized `payload_json` bytes, injecting Discord's `attachments` metadata array
/// (`[{"id": i, "filename": clean_name}, ...]`) if `payload` is a JSON object without one.
fn serialize_payload(
    payload: &serde_json::Value,
    clean_names: &[String],
) -> Result<Vec<u8>, MultipartError> {
    if !clean_names.is_empty() {
        if let serde_json::Value::Object(map) = payload {
            if !map.contains_key("attachments") {
                let mut cloned = map.clone();
                let meta: Vec<serde_json::Value> = clean_names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| serde_json::json!({ "id": i, "filename": name }))
                    .collect();
                cloned.insert("attachments".into(), serde_json::Value::Array(meta));
                return serde_json::to_vec(&serde_json::Value::Object(cloned))
                    .map_err(|e| MultipartError::SerializePayload(e.to_string()));
            }
        }
    }
    serde_json::to_vec(payload).map_err(|e| MultipartError::SerializePayload(e.to_string()))
}

/// Encode `payload` (`payload_json`) and `files` (`files[0]`..`files[n-1]`) as an RFC 7578
/// `multipart/form-data` body with a fresh collision-free boundary.
pub fn encode(
    payload: &serde_json::Value,
    files: &[UploadFile],
) -> Result<EncodedMultipart, MultipartError> {
    let total_file_bytes = validate_files(files)?;
    let clean_names: Vec<String> = files
        .iter()
        .map(|f| sanitize_filename(&f.filename))
        .collect();
    let json = serialize_payload(payload, &clean_names)?;
    let boundary = generate_boundary(&json, files);
    Ok(write_multipart(
        &boundary,
        &json,
        files,
        &clean_names,
        total_file_bytes,
    ))
}

/// Deterministic variant of [`encode`] using a caller-provided boundary seed (validated to be
/// safe ASCII and collision-free against the payload and file contents).
pub fn encode_with_boundary(
    payload: &serde_json::Value,
    files: &[UploadFile],
    boundary_seed: &str,
) -> Result<EncodedMultipart, MultipartError> {
    let total_file_bytes = validate_files(files)?;
    let clean_names: Vec<String> = files
        .iter()
        .map(|f| sanitize_filename(&f.filename))
        .collect();
    let json = serialize_payload(payload, &clean_names)?;
    let boundary = ensure_boundary_unique(boundary_seed, &json, files);
    Ok(write_multipart(
        &boundary,
        &json,
        files,
        &clean_names,
        total_file_bytes,
    ))
}

fn write_multipart(
    boundary: &str,
    json: &[u8],
    files: &[UploadFile],
    clean_names: &[String],
    total_file_bytes: usize,
) -> EncodedMultipart {
    let header_overhead = (files.len() + 2) * (boundary.len() + 128);
    let mut body = Vec::with_capacity(json.len() + total_file_bytes + header_overhead);

    // Part 0: payload_json
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(
        b"\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n",
    );
    body.extend_from_slice(json);
    body.extend_from_slice(b"\r\n");

    // Parts 1..=N: files[i]
    for ((i, file), clean_name) in files.iter().enumerate().zip(clean_names.iter()) {
        let escaped_name = escape_quoted_param(clean_name);
        let content_type = sanitize_content_type(&file.content_type);
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(
            format!(
                "\r\nContent-Disposition: form-data; name=\"files[{i}]\"; filename=\"{escaped_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&file.bytes);
        body.extend_from_slice(b"\r\n");
    }

    // Closing delimiter
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");

    EncodedMultipart {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        boundary: boundary.to_string(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_content_type_covers_extensions_and_edge_cases() {
        assert_eq!(guess_content_type("photo.png"), "image/png");
        assert_eq!(guess_content_type("PHOTO.JPG"), "image/jpeg");
        assert_eq!(guess_content_type("anim.WebP"), "image/webp");
        assert_eq!(guess_content_type("clip.mov"), "video/quicktime");
        assert_eq!(guess_content_type("song.opus"), "audio/opus");
        assert_eq!(guess_content_type("notes.md"), "text/plain; charset=utf-8");
        assert_eq!(guess_content_type("data.json"), "application/json");
        assert_eq!(guess_content_type("archive.tar.gz"), "application/gzip");
        assert_eq!(guess_content_type("report.pdf"), "application/pdf");
        // Directory with dot must not be treated as file extension.
        assert_eq!(
            guess_content_type("/tmp/v1.2/README"),
            "application/octet-stream"
        );
        assert_eq!(
            guess_content_type(r"C:\folder.d\Makefile"),
            "application/octet-stream"
        );
        // Dotfiles have no extension.
        assert_eq!(guess_content_type(".gitignore"), "application/octet-stream");
        assert_eq!(guess_content_type("file.unknownext"), DEFAULT_CONTENT_TYPE);
    }

    #[test]
    fn sanitize_filename_strips_paths_and_controls() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Users\demo\pic.png"), "pic.png");
        assert_eq!(sanitize_filename("hello\r\nworld\0.txt"), "helloworld.txt");
        assert_eq!(sanitize_filename("   "), "attachment");
        assert_eq!(sanitize_filename(".."), "attachment");
        assert_eq!(sanitize_filename("."), "attachment");
    }

    #[test]
    fn upload_file_from_bytes_and_is_image() {
        let img = UploadFile::from_bytes("dir/shot.PNG", vec![1, 2, 3]);
        assert_eq!(img.filename, "shot.PNG");
        assert_eq!(img.content_type, "image/png");
        assert!(img.is_image());

        let doc = UploadFile::from_bytes("spec.pdf", vec![4, 5]);
        assert!(!doc.is_image());
    }

    #[test]
    fn encode_with_boundary_matches_rfc7578_and_injects_attachments_metadata() {
        let payload = serde_json::json!({
            "content": "hello",
            "nonce": "123",
            "enforce_nonce": true,
        });
        let files = vec![
            UploadFile::from_bytes("note.txt", b"line1\r\nline2".to_vec()),
            UploadFile {
                filename: "bin.dat".into(),
                content_type: "application/octet-stream".into(),
                bytes: vec![0x00, 0xFF, 0x0D, 0x0A, 0x80, 0x7F],
            },
        ];

        let enc = encode_with_boundary(&payload, &files, "test-bound-1").unwrap();
        assert_eq!(enc.boundary, "test-bound-1");
        assert_eq!(
            enc.content_type,
            "multipart/form-data; boundary=test-bound-1"
        );

        let delim = b"--test-bound-1\r\n";
        let closing = b"--test-bound-1--\r\n";
        assert!(enc.body.starts_with(delim));
        assert!(enc.body.ends_with(closing));

        // Split parts manually by boundary delimiter.
        let inner = &enc.body[delim.len()..enc.body.len() - closing.len()];
        let sep = b"\r\n--test-bound-1\r\n";
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut rest = inner;
        while let Some(pos) = rest.windows(sep.len()).position(|w| w == sep) {
            parts.push(&rest[..pos]);
            rest = &rest[pos + sep.len()..];
        }
        // Strip final CRLF before closing boundary.
        assert!(rest.ends_with(b"\r\n"));
        parts.push(&rest[..rest.len() - 2]);

        assert_eq!(parts.len(), 3);

        // Part 0: payload_json with injected attachments metadata.
        let (h0, b0) = split_part(parts[0]);
        assert_eq!(
            h0,
            "Content-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json"
        );
        let parsed: serde_json::Value = serde_json::from_slice(b0).unwrap();
        assert_eq!(parsed["content"], "hello");
        assert_eq!(
            parsed["attachments"],
            serde_json::json!([
                { "id": 0, "filename": "note.txt" },
                { "id": 1, "filename": "bin.dat" }
            ])
        );

        // Part 1: note.txt
        let (h1, b1) = split_part(parts[1]);
        assert_eq!(
            h1,
            "Content-Disposition: form-data; name=\"files[0]\"; filename=\"note.txt\"\r\nContent-Type: text/plain; charset=utf-8"
        );
        assert_eq!(b1, b"line1\r\nline2");

        // Part 2: arbitrary binary bytes preserved losslessly.
        let (h2, b2) = split_part(parts[2]);
        assert_eq!(
            h2,
            "Content-Disposition: form-data; name=\"files[1]\"; filename=\"bin.dat\"\r\nContent-Type: application/octet-stream"
        );
        assert_eq!(b2, &[0x00, 0xFF, 0x0D, 0x0A, 0x80, 0x7F]);
    }

    #[test]
    fn preserves_caller_provided_attachments_in_payload() {
        let payload = serde_json::json!({
            "content": "",
            "attachments": [{ "id": 0, "filename": "custom.png", "description": "alt text" }]
        });
        let files = vec![UploadFile::from_bytes("a.png", vec![1, 2])];
        let enc = encode_with_boundary(&payload, &files, "b-keep").unwrap();
        let text = String::from_utf8_lossy(&enc.body);
        assert!(text.contains("\"description\":\"alt text\""));
        assert!(text.contains("\"filename\":\"custom.png\""));
    }

    #[test]
    fn escapes_quotes_and_blocks_header_injection() {
        let payload = serde_json::json!({ "content": "" });
        let files = vec![UploadFile {
            filename: "evil\"\r\nX-Injected: yes\\file.txt".into(),
            content_type: "text/plain\r\nX-Bad: 1".into(),
            bytes: b"ok".to_vec(),
        }];
        let enc = encode_with_boundary(&payload, &files, "safe-b").unwrap();
        let body_str = String::from_utf8(enc.body).unwrap();
        // Path separator `\` splits basename to `file.txt`; test quote-only injection directly too.
        assert!(!body_str.contains("\r\nX-Injected:"));
        assert!(!body_str.contains("\r\nX-Bad:"));
        assert!(body_str.contains("Content-Type: text/plainX-Bad: 1\r\n"));

        let quoted = vec![UploadFile {
            filename: "my \"quoted\" file.txt".into(),
            content_type: "text/plain".into(),
            bytes: b"hi".to_vec(),
        }];
        let enc2 = encode_with_boundary(&payload, &quoted, "safe-b2").unwrap();
        let body2 = String::from_utf8(enc2.body).unwrap();
        assert!(body2.contains("filename=\"my \\\"quoted\\\" file.txt\""));
    }

    #[test]
    fn boundary_avoids_collision_with_file_and_payload_bytes() {
        let seed = "collide-seed";
        let payload = serde_json::json!({ "content": "mentions collide-seed inside json" });
        let files = vec![UploadFile::from_bytes(
            "a.bin",
            b"prefix collide-seed-1 suffix".to_vec(),
        )];
        let enc = encode_with_boundary(&payload, &files, seed).unwrap();
        // `collide-seed` collides with payload, `collide-seed-1` collides with file -> picks `-2`.
        assert_eq!(enc.boundary, "collide-seed-2");
        assert!(enc.boundary.len() <= MAX_BOUNDARY_LEN);
        assert!(!contains_subslice(&files[0].bytes, enc.boundary.as_bytes()));
    }

    #[test]
    fn enforces_count_per_file_and_total_byte_limits() {
        let payload = serde_json::json!({ "content": "" });

        // 11 files -> TooManyFiles
        let too_many: Vec<UploadFile> = (0..=MAX_ATTACHMENTS)
            .map(|i| UploadFile::from_bytes(format!("f{i}.txt"), vec![b'a']))
            .collect();
        assert_eq!(
            encode(&payload, &too_many),
            Err(MultipartError::TooManyFiles)
        );

        // 1 file over 10 MiB -> FileTooLarge
        let big = vec![UploadFile::from_bytes(
            "huge.bin",
            vec![0u8; MAX_FILE_BYTES + 1],
        )];
        assert_eq!(
            encode(&payload, &big),
            Err(MultipartError::FileTooLarge("huge.bin".into()))
        );

        // 3 files of 9 MiB each = 27 MiB > 25 MiB -> TotalTooLarge
        let nine_mb = 9 * 1024 * 1024;
        let combined_big = vec![
            UploadFile::from_bytes("a.bin", vec![0u8; nine_mb]),
            UploadFile::from_bytes("b.bin", vec![0u8; nine_mb]),
            UploadFile::from_bytes("c.bin", vec![0u8; nine_mb]),
        ];
        assert_eq!(
            encode(&payload, &combined_big),
            Err(MultipartError::TotalTooLarge)
        );
    }

    #[test]
    fn encode_generates_unique_valid_boundaries() {
        let payload = serde_json::json!({ "content": "hi" });
        let files = vec![UploadFile::from_bytes("x.txt", b"hello".to_vec())];
        let a = encode(&payload, &files).unwrap();
        let b = encode(&payload, &files).unwrap();
        assert_ne!(a.boundary, b.boundary);
        assert!(a.boundary.len() <= MAX_BOUNDARY_LEN);
        assert!(a
            .boundary
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    }

    fn split_part(part: &[u8]) -> (String, &[u8]) {
        let sep = b"\r\n\r\n";
        let idx = part.windows(sep.len()).position(|w| w == sep).unwrap();
        (
            String::from_utf8(part[..idx].to_vec()).unwrap(),
            &part[idx + sep.len()..],
        )
    }
}
