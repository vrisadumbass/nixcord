//! Discord REST client.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! `Http` is cheap to clone and safe to share across tasks.
//!
//! Internally every endpoint is described by a [`Route`] (method, path, query, JSON body),
//! which is pure data and can be unit-tested without a network. [`Http`] turns a `Route`
//! into a reqwest request, sends it, and maps the response with [`map_error_status`].

pub mod multipart;
pub use multipart::{
    EncodedMultipart, MultipartError, UploadFile, MAX_ATTACHMENTS, MAX_FILE_BYTES, MAX_TOTAL_BYTES,
};

use nixcord_model::components::ComponentInteraction;
use nixcord_model::{Channel, Emoji, Message, RelationshipKind, Snowflake, User};
use reqwest::header::{
    HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT,
};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const API_BASE: &str = "https://discord.com/api/v10";

/// Plain user agent. No browser fingerprinting, no super-properties.
pub const USER_AGENT_VALUE: &str = concat!("nixcord/", env!("CARGO_PKG_VERSION"));

/// 429s asking us to wait at most this long are retried once automatically.
pub const AUTO_RETRY_MAX: Duration = Duration::from_secs(5);

/// Error bodies are truncated to this many characters in [`HttpError::Status`].
pub const ERROR_BODY_MAX_CHARS: usize = 300;

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("unauthorized (bad token)")]
    Unauthorized,
    #[error("forbidden (missing permissions)")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("rate limited, retry after {0:?}")]
    RateLimited(std::time::Duration),
    #[error("http status {0}: {1}")]
    Status(u16, String),
    #[error("request failed: {0}")]
    Transport(String),
    #[error("bad response body: {0}")]
    Decode(String),
    #[error("invalid upload: {0}")]
    InvalidUpload(#[from] MultipartError),
}

pub type Result<T> = std::result::Result<T, HttpError>;

/// HTTP method of a [`Route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

/// Query parameters for guild or channel message search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageSearchQuery {
    pub content: Option<String>,
    pub author_id: Option<Snowflake>,
    /// Used by guild message search; ignored by channel message search.
    pub channel_id: Option<Snowflake>,
    pub offset: Option<u32>,
}

impl MessageSearchQuery {
    fn append_params(&self, query: &mut Vec<(&'static str, String)>, include_channel: bool) {
        if let Some(c) = self.content.as_deref().filter(|s| !s.is_empty()) {
            query.push(("content", c.to_owned()));
        }
        if let Some(a) = self.author_id {
            query.push(("author_id", a.to_string()));
        }
        if include_channel {
            if let Some(ch) = self.channel_id {
                query.push(("channel_id", ch.to_string()));
            }
        }
        if let Some(off) = self.offset.filter(|&o| o > 0) {
            query.push(("offset", off.to_string()));
        }
    }
}

/// Request body for creating a public thread (`POST /channels/{id}/threads`) or a Forum post.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateThreadRequest {
    pub name: String,
    /// Starter message content (required for Forum posts; for text channels sent separately).
    pub content: Option<String>,
    /// Forum tag ids to apply (Forum posts only).
    pub applied_tags: Vec<Snowflake>,
    pub is_forum: bool,
}

/// Response from `GET /guilds/{id}/messages/search` or `GET /channels/{id}/messages/search`.
/// Each inner `Vec<Message>` is a hit together with its surrounding context messages.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct MessageSearchResult {
    #[serde(default)]
    pub total_results: u32,
    #[serde(default)]
    pub messages: Vec<Vec<Message>>,
}

/// A fully described request, independent of any client. Pure data.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub method: Method,
    /// Path relative to the API base, starting with `/`.
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    pub body: Option<serde_json::Value>,
}

impl Route {
    fn new(method: Method, path: String) -> Self {
        Self {
            method,
            path,
            query: Vec::new(),
            body: None,
        }
    }

    pub fn current_user() -> Self {
        Self::new(Method::Get, "/users/@me".into())
    }

    pub fn messages(channel_id: Snowflake, before: Option<Snowflake>, limit: u8) -> Self {
        let mut r = Self::new(Method::Get, format!("/channels/{channel_id}/messages"));
        r.query.push(("limit", limit.clamp(1, 100).to_string()));
        if let Some(b) = before {
            r.query.push(("before", b.to_string()));
        }
        r
    }

    pub fn messages_around(channel_id: Snowflake, around: Snowflake, limit: u8) -> Self {
        let mut r = Self::new(Method::Get, format!("/channels/{channel_id}/messages"));
        r.query.push(("limit", limit.clamp(1, 100).to_string()));
        r.query.push(("around", around.to_string()));
        r
    }

    /// POST /users/@me/channels — open or create a 1-on-1 DM channel with `recipient_id`.
    pub fn create_dm(recipient_id: Snowflake) -> Self {
        let mut r = Self::new(Method::Post, "/users/@me/channels".into());
        r.body = Some(serde_json::json!({ "recipient_id": recipient_id }));
        r
    }

    /// POST /channels/{parent_id}/threads — create a Forum post or public text-channel thread.
    pub fn create_thread(parent_id: Snowflake, req: &CreateThreadRequest) -> Self {
        let mut r = Self::new(Method::Post, format!("/channels/{parent_id}/threads"));
        let mut body = if req.is_forum {
            let content = req.content.as_deref().unwrap_or("");
            serde_json::json!({
                "name": req.name,
                "message": { "content": content },
            })
        } else {
            serde_json::json!({
                "name": req.name,
                "type": 11,
            })
        };
        if req.is_forum && !req.applied_tags.is_empty() {
            body["applied_tags"] = serde_json::json!(req.applied_tags);
        }
        r.body = Some(body);
        r
    }

    /// PATCH /channels/{thread_id} — archive/unarchive or lock/unlock a thread.
    pub fn update_thread(
        thread_id: Snowflake,
        archived: Option<bool>,
        locked: Option<bool>,
    ) -> Self {
        let mut r = Self::new(Method::Patch, format!("/channels/{thread_id}"));
        let mut body = serde_json::Map::new();
        if let Some(a) = archived {
            body.insert("archived".into(), serde_json::Value::Bool(a));
        }
        if let Some(l) = locked {
            body.insert("locked".into(), serde_json::Value::Bool(l));
        }
        r.body = Some(serde_json::Value::Object(body));
        r
    }

    /// `enforce_nonce` makes Discord dedupe retried sends with the same nonce. A reply
    /// adds `message_reference` and pings the replied-to author (`replied_user: true`).
    pub fn send_message(
        channel_id: Snowflake,
        content: &str,
        nonce: &str,
        reply_to: Option<Snowflake>,
    ) -> Self {
        let mut r = Self::new(Method::Post, format!("/channels/{channel_id}/messages"));
        let mut body = serde_json::json!({
            "content": content,
            "nonce": nonce,
            "enforce_nonce": true,
        });
        if let Some(mid) = reply_to {
            body["message_reference"] = serde_json::json!({ "message_id": mid });
            body["allowed_mentions"] = serde_json::json!({ "replied_user": true });
        }
        r.body = Some(body);
        r
    }

    /// POST /channels/{id}/messages with one sticker and no text.
    pub fn send_sticker(
        channel_id: Snowflake,
        sticker_id: Snowflake,
        nonce: &str,
        reply_to: Option<Snowflake>,
    ) -> Self {
        let mut r = Self::send_message(channel_id, "", nonce, reply_to);
        if let Some(body) = r.body.as_mut() {
            body["sticker_ids"] = serde_json::json!([sticker_id]);
        }
        r
    }

    /// GET /gifs/search (Discord's Tenor proxy), MP4 renditions.
    pub fn gifs_search(query: &str, locale: &str) -> Self {
        let mut r = Self::new(Method::Get, "/gifs/search".to_string());
        r.query = vec![
            ("q", query.to_string()),
            ("media_format", "mp4".into()),
            ("provider", "tenor".into()),
            ("locale", locale.to_string()),
        ];
        r
    }

    /// GET /gifs/trending-gifs, MP4 renditions.
    pub fn gifs_trending(locale: &str) -> Self {
        let mut r = Self::new(Method::Get, "/gifs/trending-gifs".to_string());
        r.query = vec![
            ("media_format", "mp4".into()),
            ("provider", "tenor".into()),
            ("locale", locale.to_string()),
        ];
        r
    }

    /// PUT /channels/{c}/messages/{m}/reactions/{emoji}/@me.
    pub fn add_reaction(channel_id: Snowflake, message_id: Snowflake, emoji: &Emoji) -> Self {
        Self::new(
            Method::Put,
            own_reaction_path(channel_id, message_id, emoji),
        )
    }

    /// DELETE /channels/{c}/messages/{m}/reactions/{emoji}/@me.
    pub fn remove_own_reaction(
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &Emoji,
    ) -> Self {
        Self::new(
            Method::Delete,
            own_reaction_path(channel_id, message_id, emoji),
        )
    }

    pub fn edit_message(channel_id: Snowflake, message_id: Snowflake, content: &str) -> Self {
        let mut r = Self::new(
            Method::Patch,
            format!("/channels/{channel_id}/messages/{message_id}"),
        );
        r.body = Some(serde_json::json!({ "content": content }));
        r
    }

    pub fn delete_message(channel_id: Snowflake, message_id: Snowflake) -> Self {
        Self::new(
            Method::Delete,
            format!("/channels/{channel_id}/messages/{message_id}"),
        )
    }

    pub fn trigger_typing(channel_id: Snowflake) -> Self {
        Self::new(Method::Post, format!("/channels/{channel_id}/typing"))
    }

    pub fn guild_channels(guild_id: Snowflake) -> Self {
        Self::new(Method::Get, format!("/guilds/{guild_id}/channels"))
    }

    /// POST /interactions (type 3: MESSAGE_COMPONENT, buttons & select menus).
    pub fn send_component_interaction(ix: &ComponentInteraction, nonce: &str) -> Self {
        let mut r = Self::new(Method::Post, "/interactions".into());
        let mut data = serde_json::json!({
            "component_type": ix.component_type,
            "custom_id": ix.custom_id,
        });
        // Button interactions (component_type == 2) omit `values`; select menus include it.
        if ix.component_type != 2 {
            data["values"] = serde_json::json!(ix.values);
        }
        let mut body = serde_json::json!({
            "type": 3,
            "nonce": nonce,
            "channel_id": ix.channel_id,
            "message_flags": ix.message_flags,
            "message_id": ix.message_id,
            "application_id": ix.application_id,
            "session_id": ix.session_id,
            "data": data,
        });
        if let Some(gid) = ix.guild_id {
            body["guild_id"] = serde_json::json!(gid);
        }
        r.body = Some(body);
        r
    }

    /// POST /channels/{cid}/messages/{mid}/ack.
    pub fn ack_message(channel_id: Snowflake, message_id: Snowflake) -> Self {
        let mut r = Self::new(
            Method::Post,
            format!("/channels/{channel_id}/messages/{message_id}/ack"),
        );
        r.body = Some(serde_json::json!({ "token": serde_json::Value::Null }));
        r
    }

    /// PUT /users/@me/relationships/{user_id}.
    /// `None` sends `{}` (accept incoming friend request / add by id); `Some(kind)` sets `{"type": n}`
    /// (e.g. `RelationshipKind::Blocked` to block).
    pub fn put_relationship(user_id: Snowflake, kind: Option<RelationshipKind>) -> Self {
        let mut r = Self::new(Method::Put, format!("/users/@me/relationships/{user_id}"));
        r.body = Some(match kind {
            Some(k) => serde_json::json!({ "type": u8::from(k) }),
            None => serde_json::json!({}),
        });
        r
    }

    /// POST /users/@me/relationships — send a friend request by username.
    pub fn send_friend_request(username: &str) -> Self {
        let mut r = Self::new(Method::Post, "/users/@me/relationships".into());
        r.body = Some(serde_json::json!({
            "username": username,
            "discriminator": serde_json::Value::Null,
        }));
        r
    }

    /// DELETE /users/@me/relationships/{user_id} — unfriend, cancel/decline request, or unblock.
    pub fn remove_relationship(user_id: Snowflake) -> Self {
        Self::new(
            Method::Delete,
            format!("/users/@me/relationships/{user_id}"),
        )
    }

    /// GET /guilds/{guild_id}/messages/search.
    pub fn search_guild_messages(guild_id: Snowflake, query: &MessageSearchQuery) -> Self {
        let mut r = Self::new(Method::Get, format!("/guilds/{guild_id}/messages/search"));
        query.append_params(&mut r.query, true);
        r
    }

    /// GET /channels/{channel_id}/messages/search (DMs / group DMs).
    pub fn search_channel_messages(channel_id: Snowflake, query: &MessageSearchQuery) -> Self {
        let mut r = Self::new(
            Method::Get,
            format!("/channels/{channel_id}/messages/search"),
        );
        query.append_params(&mut r.query, false);
        r
    }

    /// Full URL including the query string.
    pub fn url(&self, base: &str) -> std::result::Result<reqwest::Url, String> {
        let mut url = reqwest::Url::parse(&format!("{}{}", base.trim_end_matches('/'), self.path))
            .map_err(|e| e.to_string())?;
        if !self.query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in &self.query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }
}

fn own_reaction_path(channel_id: Snowflake, message_id: Snowflake, emoji: &Emoji) -> String {
    format!(
        "/channels/{channel_id}/messages/{message_id}/reactions/{}/@me",
        emoji_path_segment(emoji)
    )
}

/// Emoji as a URL path segment: percent-encoded unicode `name`, or `name:id` for custom
/// emoji (the name part encoded, the colon literal).
pub fn emoji_path_segment(emoji: &Emoji) -> String {
    let name = emoji.name.as_deref().unwrap_or("");
    match emoji.id {
        Some(id) => {
            let name = if name.is_empty() { "_" } else { name };
            format!("{}:{id}", percent_encode(name))
        }
        None => percent_encode(name),
    }
}

/// Percent-encode everything except RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Value for the Authorization header. User tokens are sent raw; a token that already
/// carries a `Bot ` prefix is passed through unchanged.
pub fn auth_header_value(token: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(token.trim()).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// A snowflake-shaped nonce from the current time (plus a process-wide counter in the
/// low bits so two sends in the same millisecond differ). Lets the app dedupe the
/// gateway MESSAGE_CREATE echo against its optimistic local message.
pub fn generate_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(DISCORD_EPOCH_MS);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed) & 0x3F_FFFF;
    ((now_ms.saturating_sub(DISCORD_EPOCH_MS) << 22) | seq).to_string()
}

/// Parse how long a 429 asks us to wait: `retry_after` (float seconds) from the JSON
/// body first, then the `Retry-After` header. Defaults to 1 s if neither is usable.
pub fn parse_retry_after(retry_after_header: Option<&str>, body: &str) -> Duration {
    #[derive(serde::Deserialize)]
    struct Body {
        retry_after: f64,
    }
    let from_body = serde_json::from_str::<Body>(body)
        .ok()
        .map(|b| b.retry_after);
    let from_header = retry_after_header.and_then(|h| h.trim().parse::<f64>().ok());
    from_body
        .or(from_header)
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .unwrap_or(Duration::from_secs(1))
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

/// Map a non-2xx status to an error. Pure; unit-tested.
pub fn map_error_status(status: u16, retry_after_header: Option<&str>, body: &str) -> HttpError {
    match status {
        401 => HttpError::Unauthorized,
        403 => HttpError::Forbidden,
        404 => HttpError::NotFound,
        429 => HttpError::RateLimited(parse_retry_after(retry_after_header, body)),
        code => HttpError::Status(code, truncate_chars(body.trim(), ERROR_BODY_MAX_CHARS)),
    }
}

struct Inner {
    /// Client construction can fail (TLS init); `new` is infallible per the contract,
    /// so the error is reported on each request instead.
    client: std::result::Result<reqwest::Client, String>,
    /// `None` if the token contains characters that are not valid in a header.
    auth: Option<HeaderValue>,
    base: String,
}

#[derive(Clone)]
pub struct Http {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token.
        f.debug_struct("Http")
            .field("base", &self.inner.base)
            .finish_non_exhaustive()
    }
}

impl Http {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, API_BASE)
    }

    /// Like [`Http::new`] but against another base URL (tests, proxies).
    pub fn with_base_url(token: String, base: impl Into<String>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string());
        Self {
            inner: Arc::new(Inner {
                client,
                auth: auth_header_value(&token),
                base: base.into(),
            }),
        }
    }

    /// Send a route and return the raw 2xx body. Handles one automatic retry on short 429s.
    ///
    /// v0.1 rate limiting: we only react to 429s. Per-route bucket tracking
    /// (X-RateLimit-Bucket / Remaining / Reset-After) is out of scope for now.
    async fn execute(&self, route: &Route) -> Result<Vec<u8>> {
        let client = self
            .inner
            .client
            .as_ref()
            .map_err(|e| HttpError::Transport(e.clone()))?;
        let auth = self.inner.auth.clone().ok_or(HttpError::Unauthorized)?;
        let url = route.url(&self.inner.base).map_err(HttpError::Transport)?;
        let body = route
            .body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        let mut retried = false;
        loop {
            tracing::debug!(method = ?route.method, path = %route.path, "http request");
            let mut req = client
                .request(route.method.to_reqwest(), url.clone())
                .header(AUTHORIZATION, auth.clone());
            if let Some(b) = &body {
                req = req.header(CONTENT_TYPE, "application/json").body(b.clone());
            }
            let resp = req
                .send()
                .await
                .map_err(|e| HttpError::Transport(e.without_url().to_string()))?;
            let status = resp.status().as_u16();
            let retry_header = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| HttpError::Transport(e.without_url().to_string()))?;

            if (200..300).contains(&status) {
                return Ok(bytes.to_vec());
            }
            let text = String::from_utf8_lossy(&bytes);
            let err = map_error_status(status, retry_header.as_deref(), &text);
            match err {
                HttpError::RateLimited(wait) if !retried && wait <= AUTO_RETRY_MAX => {
                    tracing::warn!(path = %route.path, ?wait, "rate limited, retrying once");
                    retried = true;
                    tokio::time::sleep(wait).await;
                }
                err => {
                    tracing::debug!(path = %route.path, status, "http error");
                    return Err(err);
                }
            }
        }
    }

    async fn json<T: DeserializeOwned>(&self, route: Route) -> Result<T> {
        let bytes = self.execute(&route).await?;
        serde_json::from_slice(&bytes).map_err(|e| HttpError::Decode(e.to_string()))
    }

    /// For endpoints that answer 204 No Content (or a body we do not need).
    async fn empty(&self, route: Route) -> Result<()> {
        self.execute(&route).await.map(drop)
    }

    /// GET /users/@me — used to validate a token on login.
    pub async fn current_user(&self) -> Result<User> {
        self.json(Route::current_user()).await
    }

    /// GET /channels/{id}/messages. Returned newest-first, exactly as Discord sends them.
    /// `limit` is clamped to 1..=100.
    pub async fn messages(
        &self,
        channel_id: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> Result<Vec<Message>> {
        self.json(Route::messages(channel_id, before, limit)).await
    }

    /// POST /channels/{id}/messages with `{"content", "nonce", "enforce_nonce": true}`.
    /// With `reply_to`, the message is sent as a reply that pings the replied-to author.
    pub async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        self.json(Route::send_message(
            channel_id,
            content,
            &generate_nonce(),
            reply_to,
        ))
        .await
    }

    /// POST /channels/{id}/messages with `sticker_ids: [sticker_id]`.
    pub async fn send_sticker(
        &self,
        channel_id: Snowflake,
        sticker_id: Snowflake,
        reply_to: Option<Snowflake>,
    ) -> Result<Message> {
        self.json(Route::send_sticker(
            channel_id,
            sticker_id,
            &generate_nonce(),
            reply_to,
        ))
        .await
    }

    /// GIF search via Discord (`None` = trending).
    pub async fn gifs(&self, query: Option<&str>) -> Result<Vec<nixcord_model::GifResult>> {
        let route = match query {
            Some(q) => Route::gifs_search(q, "en-US"),
            None => Route::gifs_trending("en-US"),
        };
        self.json(route).await
    }

    /// PUT /channels/{cid}/messages/{mid}/reactions/{emoji}/@me (204).
    pub async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &Emoji,
    ) -> Result<()> {
        self.empty(Route::add_reaction(channel_id, message_id, emoji))
            .await
    }

    /// DELETE /channels/{cid}/messages/{mid}/reactions/{emoji}/@me (204).
    pub async fn remove_own_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &Emoji,
    ) -> Result<()> {
        self.empty(Route::remove_own_reaction(channel_id, message_id, emoji))
            .await
    }

    /// PATCH /channels/{cid}/messages/{mid}.
    pub async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> Result<Message> {
        self.json(Route::edit_message(channel_id, message_id, content))
            .await
    }

    /// DELETE /channels/{cid}/messages/{mid}.
    pub async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.empty(Route::delete_message(channel_id, message_id))
            .await
    }

    /// POST /channels/{id}/typing.
    pub async fn trigger_typing(&self, channel_id: Snowflake) -> Result<()> {
        self.empty(Route::trigger_typing(channel_id)).await
    }

    /// GET /guilds/{id}/channels.
    pub async fn guild_channels(&self, guild_id: Snowflake) -> Result<Vec<Channel>> {
        self.json(Route::guild_channels(guild_id)).await
    }

    /// POST /interactions (type 3: message component). Returns 204 No Content on accept.
    pub async fn send_component_interaction(&self, ix: &ComponentInteraction) -> Result<()> {
        self.empty(Route::send_component_interaction(ix, &generate_nonce()))
            .await
    }

    /// POST /channels/{cid}/messages/{mid}/ack.
    pub async fn ack_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        self.empty(Route::ack_message(channel_id, message_id)).await
    }

    /// PUT /users/@me/relationships/{user_id} (204).
    pub async fn put_relationship(
        &self,
        user_id: Snowflake,
        kind: Option<RelationshipKind>,
    ) -> Result<()> {
        self.empty(Route::put_relationship(user_id, kind)).await
    }

    /// POST /users/@me/relationships (204).
    pub async fn send_friend_request(&self, username: &str) -> Result<()> {
        self.empty(Route::send_friend_request(username)).await
    }

    /// DELETE /users/@me/relationships/{user_id} (204).
    pub async fn remove_relationship(&self, user_id: Snowflake) -> Result<()> {
        self.empty(Route::remove_relationship(user_id)).await
    }

    /// GET /guilds/{guild_id}/messages/search.
    pub async fn search_guild_messages(
        &self,
        guild_id: Snowflake,
        query: &MessageSearchQuery,
    ) -> Result<MessageSearchResult> {
        self.json(Route::search_guild_messages(guild_id, query))
            .await
    }

    /// GET /channels/{channel_id}/messages/search.
    pub async fn search_channel_messages(
        &self,
        channel_id: Snowflake,
        query: &MessageSearchQuery,
    ) -> Result<MessageSearchResult> {
        self.json(Route::search_channel_messages(channel_id, query))
            .await
    }

    /// GET /channels/{id}/messages?around={around}&limit={limit}.
    pub async fn messages_around(
        &self,
        channel_id: Snowflake,
        around: Snowflake,
        limit: u8,
    ) -> Result<Vec<Message>> {
        self.json(Route::messages_around(channel_id, around, limit))
            .await
    }

    /// POST /users/@me/channels — open or create a DM channel with `recipient_id`.
    pub async fn create_dm(&self, recipient_id: Snowflake) -> Result<Channel> {
        self.json(Route::create_dm(recipient_id)).await
    }

    /// POST /channels/{parent_id}/threads — create a Forum post or public text-channel thread.
    pub async fn create_thread(
        &self,
        parent_id: Snowflake,
        req: &CreateThreadRequest,
    ) -> Result<Channel> {
        self.json(Route::create_thread(parent_id, req)).await
    }

    /// PATCH /channels/{thread_id} — archive/unarchive or lock/unlock a thread.
    pub async fn update_thread(
        &self,
        thread_id: Snowflake,
        archived: Option<bool>,
        locked: Option<bool>,
    ) -> Result<Channel> {
        self.json(Route::update_thread(thread_id, archived, locked))
            .await
    }

    /// POST /channels/{id}/messages with optional multipart file attachments.
    /// Falls back to plain JSON [`Http::send_message`] when `files` is empty.
    pub async fn send_message_with_files(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
        files: &[UploadFile],
    ) -> Result<Message> {
        if files.is_empty() {
            return self.send_message(channel_id, content, reply_to).await;
        }
        let route = Route::send_message(channel_id, content, &generate_nonce(), reply_to);
        let payload = route.body.unwrap_or_else(|| serde_json::json!({}));
        let encoded = multipart::encode(&payload, files)?;
        let bytes = self
            .execute_raw(
                Method::Post,
                &route.path,
                &encoded.content_type,
                encoded.body,
                Duration::from_secs(120),
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| HttpError::Decode(e.to_string()))
    }

    /// Execute a request with a pre-encoded body and custom `Content-Type` / timeout.
    async fn execute_raw(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let client = self
            .inner
            .client
            .as_ref()
            .map_err(|e| HttpError::Transport(e.clone()))?;
        let auth = self.inner.auth.clone().ok_or(HttpError::Unauthorized)?;
        let route = Route::new(method, path.to_string());
        let url = route.url(&self.inner.base).map_err(HttpError::Transport)?;
        // Shared buffer: a 429 retry re-sends it without copying up to 25 MiB again.
        let body = bytes::Bytes::from(body);

        let mut retried = false;
        loop {
            tracing::debug!(method = ?method, path, bytes = body.len(), "http multipart request");
            let resp = client
                .request(method.to_reqwest(), url.clone())
                .header(AUTHORIZATION, auth.clone())
                .header(CONTENT_TYPE, content_type)
                .timeout(timeout)
                .body(body.clone())
                .send()
                .await
                .map_err(|e| HttpError::Transport(e.without_url().to_string()))?;
            let status = resp.status().as_u16();
            let retry_header = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| HttpError::Transport(e.without_url().to_string()))?;

            if (200..300).contains(&status) {
                return Ok(bytes.to_vec());
            }
            let text = String::from_utf8_lossy(&bytes);
            let err = map_error_status(status, retry_header.as_deref(), &text);
            match err {
                HttpError::RateLimited(wait) if !retried && wait <= AUTO_RETRY_MAX => {
                    tracing::warn!(path, ?wait, "rate limited, retrying once");
                    retried = true;
                    tokio::time::sleep(wait).await;
                }
                err => {
                    tracing::debug!(path, status, "http error");
                    return Err(err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const C: Snowflake = Snowflake(111);
    const M: Snowflake = Snowflake(222);

    fn assert_send_sync_clone<T: Send + Sync + Clone>() {}

    #[test]
    fn http_is_send_sync_clone() {
        assert_send_sync_clone::<Http>();
    }

    #[test]
    fn route_urls() {
        let u = |r: Route| r.url(API_BASE).unwrap().to_string();
        assert_eq!(
            u(Route::current_user()),
            "https://discord.com/api/v10/users/@me"
        );
        assert_eq!(
            u(Route::guild_channels(Snowflake(5))),
            "https://discord.com/api/v10/guilds/5/channels"
        );
        assert_eq!(
            u(Route::trigger_typing(C)),
            "https://discord.com/api/v10/channels/111/typing"
        );
        assert_eq!(
            u(Route::delete_message(C, M)),
            "https://discord.com/api/v10/channels/111/messages/222"
        );
        assert_eq!(Route::delete_message(C, M).method, Method::Delete);
        assert_eq!(Route::edit_message(C, M, "x").method, Method::Patch);
        assert_eq!(Route::trigger_typing(C).method, Method::Post);
        assert!(Route::trigger_typing(C).body.is_none());
        // trailing slash on base is tolerated
        assert_eq!(
            Route::current_user().url("http://x/api/").unwrap().as_str(),
            "http://x/api/users/@me"
        );
    }

    #[test]
    fn messages_query_and_clamp() {
        let r = Route::messages(C, None, 50);
        assert_eq!(r.method, Method::Get);
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages?limit=50"
        );
        assert_eq!(
            Route::messages(C, None, 0).query,
            vec![("limit", "1".to_string())]
        );
        assert_eq!(
            Route::messages(C, None, 255).query,
            vec![("limit", "100".to_string())]
        );
        let r = Route::messages(C, Some(Snowflake(999)), 100);
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages?limit=100&before=999"
        );
    }

    #[test]
    fn send_and_edit_bodies() {
        let r = Route::send_message(C, "hi \"there\"", "123", None);
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/channels/111/messages");
        assert_eq!(
            r.body,
            Some(serde_json::json!({
                "content": "hi \"there\"",
                "nonce": "123",
                "enforce_nonce": true,
            }))
        );
        let r = Route::send_message(C, "re", "5", Some(M));
        assert_eq!(
            r.body,
            Some(serde_json::json!({
                "content": "re",
                "nonce": "5",
                "enforce_nonce": true,
                "message_reference": {"message_id": "222"},
                "allowed_mentions": {"replied_user": true},
            }))
        );
        let r = Route::edit_message(C, M, "new");
        assert_eq!(r.body, Some(serde_json::json!({"content": "new"})));
    }

    #[test]
    fn reaction_routes() {
        let unicode = Emoji {
            id: None,
            name: Some("\u{1F44D}".into()),
            animated: false,
        };
        let custom = Emoji {
            id: Some(Snowflake(77)),
            name: Some("blob cat".into()),
            animated: true,
        };
        assert_eq!(emoji_path_segment(&unicode), "%F0%9F%91%8D");
        assert_eq!(emoji_path_segment(&custom), "blob%20cat:77");

        let r = Route::add_reaction(C, M, &unicode);
        assert_eq!(r.method, Method::Put);
        assert!(r.body.is_none());
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages/222/reactions/%F0%9F%91%8D/@me"
        );
        let r = Route::remove_own_reaction(C, M, &custom);
        assert_eq!(r.method, Method::Delete);
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages/222/reactions/blob%20cat:77/@me"
        );
        // Reserved characters in names are encoded, so a name cannot alter the path.
        let weird = Emoji {
            id: None,
            name: Some("a/b?c#".into()),
            animated: false,
        };
        assert_eq!(emoji_path_segment(&weird), "a%2Fb%3Fc%23");
    }

    #[test]
    fn nonce_is_snowflake_like_and_unique() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b);
        let n: u64 = a.parse().unwrap();
        let ts = Snowflake(n).timestamp_ms();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(now.abs_diff(ts) < 60_000);
        assert!(a.len() <= 25);
    }

    #[test]
    fn auth_header() {
        let v = auth_header_value("abc.def.ghi").unwrap();
        assert_eq!(v.to_str().unwrap(), "abc.def.ghi");
        assert!(v.is_sensitive());
        assert_eq!(
            auth_header_value("Bot xyz").unwrap().to_str().unwrap(),
            "Bot xyz"
        );
        assert_eq!(
            auth_header_value("  tok\n").unwrap().to_str().unwrap(),
            "tok"
        );
        assert!(auth_header_value("bad\u{0}tok").is_none());
    }

    #[test]
    fn debug_hides_token() {
        let h = Http::with_base_url("supersecret".into(), "http://127.0.0.1:1");
        assert!(!format!("{h:?}").contains("supersecret"));
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(
            map_error_status(401, None, ""),
            HttpError::Unauthorized
        ));
        assert!(matches!(
            map_error_status(403, None, "{}"),
            HttpError::Forbidden
        ));
        assert!(matches!(
            map_error_status(404, None, ""),
            HttpError::NotFound
        ));
        match map_error_status(500, None, " oops ") {
            HttpError::Status(500, b) => assert_eq!(b, "oops"),
            e => panic!("{e:?}"),
        }
        let long = "é".repeat(1000);
        match map_error_status(400, None, &long) {
            HttpError::Status(400, b) => assert_eq!(b.chars().count(), ERROR_BODY_MAX_CHARS + 3),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn retry_after_parsing() {
        let d = |h: Option<&str>, b: &str| match map_error_status(429, h, b) {
            HttpError::RateLimited(d) => d,
            e => panic!("{e:?}"),
        };
        assert_eq!(
            d(None, r#"{"message":"x","retry_after":1.5,"global":false}"#),
            Duration::from_millis(1500)
        );
        assert_eq!(d(Some("3"), ""), Duration::from_secs(3));
        // body wins over header
        assert_eq!(
            d(Some("9"), r#"{"retry_after":0.25}"#),
            Duration::from_millis(250)
        );
        // garbage / negative fall back to 1s
        assert_eq!(d(Some("soon"), "nope"), Duration::from_secs(1));
        assert_eq!(d(None, r#"{"retry_after":-2}"#), Duration::from_secs(1));
    }

    // ---- local mock server (loopback only, never the real API) ----

    /// Serves `responses` in order, one per connection, and returns the raw requests seen.
    async fn mock(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                seen.push(String::from_utf8_lossy(&buf).to_string());
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
            seen
        });
        (base, handle)
    }

    fn resp(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    const USER_JSON: &str = r#"{"id":"42","username":"nix","global_name":null,"extra":1}"#;

    #[tokio::test]
    async fn current_user_sends_headers() {
        let (base, h) = mock(vec![resp(
            "200 OK",
            "Content-Type: application/json\r\n",
            USER_JSON,
        )])
        .await;
        let http = Http::with_base_url("tok123".into(), base);
        let u = http.current_user().await.unwrap();
        assert_eq!(u.id, Snowflake(42));
        let reqs = h.await.unwrap();
        let req = reqs[0].to_ascii_lowercase();
        assert!(req.starts_with("get /users/@me http/1.1"));
        assert!(req.contains("authorization: tok123\r\n"));
        assert!(req.contains(&format!(
            "user-agent: {}",
            USER_AGENT_VALUE.to_ascii_lowercase()
        )));
    }

    #[tokio::test]
    async fn send_message_body_and_204s() {
        let msg = r#"{"id":"9","channel_id":"111","author":{"id":"42","username":"nix"},"content":"hey","timestamp":"2024-01-01T00:00:00+00:00"}"#;
        let (base, h) = mock(vec![
            resp("200 OK", "", msg),
            resp("200 OK", "", msg),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
        ])
        .await;
        let http = Http::with_base_url("t".into(), base);
        let m = http.send_message(C, "hey", None).await.unwrap();
        assert_eq!(m.content, "hey");
        http.send_message(C, "hey", Some(Snowflake(5)))
            .await
            .unwrap();
        http.delete_message(C, M).await.unwrap();
        http.trigger_typing(C).await.unwrap();
        let thumbs = Emoji {
            id: None,
            name: Some("\u{1F44D}".into()),
            animated: false,
        };
        http.add_reaction(C, M, &thumbs).await.unwrap();
        http.remove_own_reaction(C, M, &thumbs).await.unwrap();
        let reqs = h.await.unwrap();
        let body_of = |r: &str| -> serde_json::Value {
            serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap()
        };
        assert!(reqs[0].starts_with("POST /channels/111/messages "));
        let v = body_of(&reqs[0]);
        assert_eq!(v["content"], "hey");
        assert_eq!(v["enforce_nonce"], true);
        assert!(v["nonce"].as_str().unwrap().parse::<u64>().is_ok());
        assert!(v.get("message_reference").is_none());
        let v = body_of(&reqs[1]);
        assert_eq!(v["message_reference"]["message_id"], "5");
        assert_eq!(v["allowed_mentions"]["replied_user"], true);
        assert!(reqs[2].starts_with("DELETE /channels/111/messages/222 "));
        assert!(reqs[3].starts_with("POST /channels/111/typing "));
        let reaction_path = "/channels/111/messages/222/reactions/%F0%9F%91%8D/@me ";
        assert!(reqs[4].starts_with(&format!("PUT {reaction_path}")));
        assert!(reqs[5].starts_with(&format!("DELETE {reaction_path}")));
    }

    #[tokio::test]
    async fn short_429_is_retried_once() {
        let (base, h) = mock(vec![
            resp("429 Too Many Requests", "", r#"{"retry_after":0.05}"#),
            resp("200 OK", "", USER_JSON),
        ])
        .await;
        let http = Http::with_base_url("t".into(), base);
        assert!(http.current_user().await.is_ok());
        assert_eq!(h.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn repeated_or_long_429_is_returned() {
        let r429 = resp("429 Too Many Requests", "", r#"{"retry_after":0.01}"#);
        let (base, _h) = mock(vec![r429.clone(), r429]).await;
        let http = Http::with_base_url("t".into(), base);
        assert!(matches!(
            http.current_user().await,
            Err(HttpError::RateLimited(_))
        ));

        let (base, _h) = mock(vec![resp(
            "429 Too Many Requests",
            "Retry-After: 30\r\n",
            "",
        )])
        .await;
        let http = Http::with_base_url("t".into(), base);
        match http.current_user().await {
            Err(HttpError::RateLimited(d)) => assert_eq!(d, Duration::from_secs(30)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_from_server() {
        let (base, _h) = mock(vec![
            resp(
                "401 Unauthorized",
                "",
                r#"{"message":"401: Unauthorized","code":0}"#,
            ),
            resp("200 OK", "", "not json"),
            resp("502 Bad Gateway", "", "upstream"),
        ])
        .await;
        let http = Http::with_base_url("t".into(), base);
        assert!(matches!(
            http.current_user().await,
            Err(HttpError::Unauthorized)
        ));
        assert!(matches!(
            http.guild_channels(Snowflake(1)).await,
            Err(HttpError::Decode(_))
        ));
        assert!(
            matches!(http.messages(C, None, 10).await, Err(HttpError::Status(502, b)) if b == "upstream")
        );
    }

    #[tokio::test]
    async fn transport_error_and_bad_token() {
        // Port 1 on loopback: connection refused.
        let http = Http::with_base_url("t".into(), "http://127.0.0.1:1");
        assert!(matches!(
            http.current_user().await,
            Err(HttpError::Transport(_))
        ));
        let http = Http::with_base_url("bad\u{0}".into(), "http://127.0.0.1:1");
        assert!(matches!(
            http.current_user().await,
            Err(HttpError::Unauthorized)
        ));
    }

    #[test]
    fn component_interaction_routes() {
        let btn = ComponentInteraction {
            guild_id: Some(Snowflake(10)),
            channel_id: C,
            message_id: M,
            message_flags: 32768,
            application_id: Snowflake(999),
            session_id: "sess_abc".into(),
            component_type: 2,
            custom_id: "vote_yes".into(),
            values: vec![],
        };
        let r = Route::send_component_interaction(&btn, "777");
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/interactions");
        assert_eq!(
            r.body,
            Some(serde_json::json!({
                "type": 3,
                "nonce": "777",
                "guild_id": "10",
                "channel_id": "111",
                "message_flags": 32768,
                "message_id": "222",
                "application_id": "999",
                "session_id": "sess_abc",
                "data": {
                    "component_type": 2,
                    "custom_id": "vote_yes",
                },
            }))
        );

        // DM select menu omits guild_id and includes values.
        let sel = ComponentInteraction {
            guild_id: None,
            channel_id: C,
            message_id: M,
            message_flags: 0,
            application_id: Snowflake(999),
            session_id: "sess_abc".into(),
            component_type: 3,
            custom_id: "pick_theme".into(),
            values: vec!["graphite".into(), "onyx".into()],
        };
        let r = Route::send_component_interaction(&sel, "888");
        let b = r.body.unwrap();
        assert!(b.get("guild_id").is_none());
        assert_eq!(b["data"]["component_type"], 3);
        assert_eq!(b["data"]["custom_id"], "pick_theme");
        assert_eq!(b["data"]["values"], serde_json::json!(["graphite", "onyx"]));
    }

    #[test]
    fn ack_and_relationship_routes() {
        let ack = Route::ack_message(C, M);
        assert_eq!(ack.method, Method::Post);
        assert_eq!(
            ack.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages/222/ack"
        );
        assert_eq!(ack.body, Some(serde_json::json!({ "token": null })));

        let accept = Route::put_relationship(Snowflake(55), None);
        assert_eq!(accept.method, Method::Put);
        assert_eq!(
            accept.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/users/@me/relationships/55"
        );
        assert_eq!(accept.body, Some(serde_json::json!({})));

        let block = Route::put_relationship(Snowflake(55), Some(RelationshipKind::Blocked));
        assert_eq!(block.body, Some(serde_json::json!({ "type": 2 })));

        let req = Route::send_friend_request("alice");
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/users/@me/relationships");
        assert_eq!(
            req.body,
            Some(serde_json::json!({ "username": "alice", "discriminator": null }))
        );

        let rm = Route::remove_relationship(Snowflake(55));
        assert_eq!(rm.method, Method::Delete);
        assert_eq!(rm.path, "/users/@me/relationships/55");
        assert!(rm.body.is_none());
    }

    #[test]
    fn message_search_routes() {
        let q = MessageSearchQuery {
            content: Some("rust lang".into()),
            author_id: Some(Snowflake(42)),
            channel_id: Some(C),
            offset: Some(25),
        };
        let r = Route::search_guild_messages(Snowflake(7), &q);
        assert_eq!(r.method, Method::Get);
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/guilds/7/messages/search?content=rust+lang&author_id=42&channel_id=111&offset=25"
        );

        // Channel search omits channel_id query parameter.
        let r = Route::search_channel_messages(C, &q);
        assert_eq!(
            r.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/111/messages/search?content=rust+lang&author_id=42&offset=25"
        );

        // Empty content and offset 0 are omitted.
        let empty = MessageSearchQuery {
            content: Some(String::new()),
            offset: Some(0),
            ..Default::default()
        };
        assert_eq!(
            Route::search_guild_messages(Snowflake(7), &empty)
                .url(API_BASE)
                .unwrap()
                .as_str(),
            "https://discord.com/api/v10/guilds/7/messages/search"
        );
    }

    #[test]
    fn update_thread_routes() {
        let tid = Snowflake(333);

        let arch = Route::update_thread(tid, Some(true), None);
        assert_eq!(arch.method, Method::Patch);
        assert_eq!(
            arch.url(API_BASE).unwrap().as_str(),
            "https://discord.com/api/v10/channels/333"
        );
        assert_eq!(arch.body, Some(serde_json::json!({ "archived": true })));

        let lock = Route::update_thread(tid, None, Some(true));
        assert_eq!(lock.method, Method::Patch);
        assert_eq!(lock.body, Some(serde_json::json!({ "locked": true })));

        let both = Route::update_thread(tid, Some(false), Some(false));
        assert_eq!(
            both.body,
            Some(serde_json::json!({ "archived": false, "locked": false }))
        );

        let neither = Route::update_thread(tid, None, None);
        assert_eq!(neither.body, Some(serde_json::json!({})));
    }

    #[tokio::test]
    async fn update_thread_mock() {
        let thread_json = r#"{"id":"333","type":11,"guild_id":"10","parent_id":"102","name":"v0.4-thread","thread_metadata":{"archived":true,"locked":true}}"#;
        let (base, h) = mock(vec![resp(
            "200 OK",
            "Content-Type: application/json\r\n",
            thread_json,
        )])
        .await;
        let http = Http::with_base_url("t".into(), base);
        let ch = http
            .update_thread(Snowflake(333), Some(true), Some(true))
            .await
            .unwrap();
        assert_eq!(ch.id, Snowflake(333));
        assert!(ch.is_archived());
        assert!(ch.thread_metadata.as_ref().is_some_and(|m| m.locked));

        let reqs = h.await.unwrap();
        assert!(reqs[0].starts_with("PATCH /channels/333 "));
        let body: serde_json::Value =
            serde_json::from_str(reqs[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["archived"], true);
        assert_eq!(body["locked"], true);
    }

    #[tokio::test]
    async fn interactions_ack_relationships_and_search_mock() {
        let search_json = r#"{"total_results":1,"messages":[[{"id":"9","channel_id":"111","author":{"id":"42","username":"nix"},"content":"found it","timestamp":"2024-01-01T00:00:00+00:00","hit":true}]]}"#;
        let (base, h) = mock(vec![
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("204 No Content", "", ""),
            resp("200 OK", "", search_json),
        ])
        .await;
        let http = Http::with_base_url("t".into(), base);

        let ix = ComponentInteraction {
            guild_id: Some(Snowflake(7)),
            channel_id: C,
            message_id: M,
            message_flags: 0,
            application_id: Snowflake(88),
            session_id: "s1".into(),
            component_type: 2,
            custom_id: "ok".into(),
            values: vec![],
        };
        http.send_component_interaction(&ix).await.unwrap();
        http.ack_message(C, M).await.unwrap();
        http.put_relationship(Snowflake(55), None).await.unwrap();
        http.send_friend_request("bob").await.unwrap();
        http.remove_relationship(Snowflake(55)).await.unwrap();
        let res = http
            .search_guild_messages(
                Snowflake(7),
                &MessageSearchQuery {
                    content: Some("found".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total_results, 1);
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0][0].content, "found it");

        let reqs = h.await.unwrap();
        let body_of = |r: &str| -> serde_json::Value {
            serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap()
        };
        assert!(reqs[0].starts_with("POST /interactions "));
        let v = body_of(&reqs[0]);
        assert_eq!(v["type"], 3);
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["data"]["custom_id"], "ok");
        assert!(v["nonce"].as_str().unwrap().parse::<u64>().is_ok());
        assert!(reqs[1].starts_with("POST /channels/111/messages/222/ack "));
        assert!(reqs[2].starts_with("PUT /users/@me/relationships/55 "));
        assert!(reqs[3].starts_with("POST /users/@me/relationships "));
        assert!(reqs[4].starts_with("DELETE /users/@me/relationships/55 "));
        assert!(reqs[5].starts_with("GET /guilds/7/messages/search?content=found "));
    }

    #[test]
    fn sticker_and_gif_routes() {
        let r = Route::send_sticker(C, Snowflake(77), "n1", Some(M));
        assert_eq!(r.path, "/channels/111/messages");
        let b = r.body.unwrap();
        assert_eq!(b["sticker_ids"], serde_json::json!([Snowflake(77)]));
        assert_eq!(b["content"], "");
        assert_eq!(b["message_reference"]["message_id"], serde_json::json!(M));

        let u = |r: Route| r.url(API_BASE).unwrap().to_string();
        assert_eq!(
            u(Route::gifs_search("happy cat", "en-US")),
            "https://discord.com/api/v10/gifs/search?q=happy+cat&media_format=mp4&provider=tenor&locale=en-US"
        );
        assert_eq!(
            u(Route::gifs_trending("en-US")),
            "https://discord.com/api/v10/gifs/trending-gifs?media_format=mp4&provider=tenor&locale=en-US"
        );
        let gifs: Vec<nixcord_model::GifResult> = serde_json::from_str(
            r#"[{"id":"1","title":"cat","url":"https://tenor.com/view/cat-gif-1",
                "src":"https://media.tenor.com/abcdefghPo/cat.mp4",
                "gif_src":"https://media.tenor.com/abcdefghAC/cat.gif",
                "width":498,"height":280,"preview":"https://media.tenor.com/abcdefghAe/cat.png"}]"#,
        )
        .unwrap();
        assert_eq!(gifs[0].width, 498);
        assert_eq!(gifs[0].url, "https://tenor.com/view/cat-gif-1");
    }
}
