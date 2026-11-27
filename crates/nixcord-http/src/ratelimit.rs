//! Client-side REST rate limiting, per Discord's rate limit docs
//! (docs.discord.com/developers/topics/rate-limits):
//!
//! - Every response may carry `X-RateLimit-Bucket` (an opaque hash shared by routes with
//!   the same limit), `-Remaining` and `-Reset-After` (float seconds). A bucket is limited
//!   separately per *top-level resource* (`channel_id`, `guild_id`, `webhook_id`), so state
//!   is keyed by `(bucket, major)`.
//! - 429s say how long to wait (`retry_after` / `Retry-After`) and whether the limit is
//!   global (`"global": true` / `X-RateLimit-Global`). A global 429 holds every route.
//! - The global limit is 50 requests per second, kept with a one-second window.
//!
//! Before its first response a route has no bucket yet, so requests are queued per route
//! key (FIFO, `tokio::sync::Mutex` is fair): one request per route is in flight at a time
//! and the next waits until the bucket allows it. Two routes that turn out to share a
//! bucket can still race for its last slot; the resulting 429 is handled by the retry.
//!
//! Memory: every map here is capped (expired entries pruned past [`MAX_ENTRIES`]).

use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use crate::{HttpError, Method, AUTO_RETRY_MAX};

/// Discord's global limit for a user or bot.
pub const GLOBAL_PER_SECOND: u32 = 50;

/// Maps are pruned once they grow past this many entries.
pub const MAX_ENTRIES: usize = 256;

/// The rate limit headers of one response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimitHeaders {
    pub bucket: Option<String>,
    pub remaining: Option<u32>,
    pub reset_after: Option<Duration>,
    pub global: bool,
    /// `user`, `global` or `shared` (per-resource limit), only on 429s.
    pub scope: Option<String>,
}

impl RateLimitHeaders {
    pub fn from_headers(h: &HeaderMap) -> Self {
        let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
        Self {
            bucket: get("x-ratelimit-bucket")
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            remaining: get("x-ratelimit-remaining").and_then(|s| s.parse().ok()),
            reset_after: get("x-ratelimit-reset-after")
                .and_then(|s| s.parse::<f64>().ok())
                .and_then(|s| Duration::try_from_secs_f64(s).ok()),
            global: get("x-ratelimit-global").is_some_and(|s| s.eq_ignore_ascii_case("true")),
            scope: get("x-ratelimit-scope").map(str::to_owned),
        }
    }
}

/// Is a 429 body (`{"message","retry_after","global","code"?}`) a global limit?
pub fn body_is_global(body: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        global: bool,
    }
    serde_json::from_str::<Body>(body).is_ok_and(|b| b.global)
}

/// The route a request is limited under: method plus path, with the ids of top-level
/// resources kept (they select separate limits) and other ids and reaction emoji
/// replaced, so `/channels/1/messages/2` and `/channels/1/messages/3` share a queue.
pub fn route_key(method: Method, path: &str) -> String {
    let mut out = format!("{method:?} ");
    let mut prev = "";
    for seg in path.split('?').next().unwrap_or("").split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        let major = matches!(prev, "channels" | "guilds" | "webhooks");
        if prev == "reactions" && seg != "@me" {
            out.push_str(":emoji");
        } else if !major && seg.bytes().all(|b| b.is_ascii_digit()) {
            out.push_str(":id");
        } else {
            out.push_str(seg);
        }
        prev = seg;
    }
    out
}

/// The top-level resource part of a route key (`/channels/123`), or "" when it has none.
fn major_of(key: &str) -> &str {
    let path = key.split_once(' ').map_or(key, |(_, p)| p);
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    match (segs.next(), segs.next()) {
        (Some(kind @ ("channels" | "guilds" | "webhooks")), Some(id)) => {
            let end = kind.len() + id.len() + 2;
            &path[..end.min(path.len())]
        }
        _ => "",
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    remaining: u32,
    reset_at: Instant,
}

#[derive(Debug)]
struct State {
    /// Route key -> bucket hash learned from `X-RateLimit-Bucket`.
    route_bucket: HashMap<String, String>,
    /// (bucket hash or route key, major) -> remaining requests until reset.
    buckets: HashMap<(String, String), Bucket>,
    global_until: Option<Instant>,
    window_start: Instant,
    window_count: u32,
}

/// Shared limiter state; one per [`crate::Http`].
#[derive(Debug)]
pub struct RateLimiter {
    state: Mutex<State>,
    queues: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                route_bucket: HashMap::new(),
                buckets: HashMap::new(),
                global_until: None,
                window_start: Instant::now(),
                window_count: 0,
            }),
            queues: Mutex::new(HashMap::new()),
        }
    }
}

impl RateLimiter {
    /// Take this route's turn: requests on one route run one at a time, in arrival order.
    pub async fn queue(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut q = self.queues.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() > MAX_ENTRIES {
                // Only queues nobody holds or waits on.
                q.retain(|_, m| Arc::strong_count(m) > 1);
            }
            q.entry(key.to_owned()).or_default().clone()
        };
        lock.lock_owned().await
    }

    /// Wait until the global limit and this route's bucket allow one request, and count it.
    ///
    /// Waits of at most [`AUTO_RETRY_MAX`] are slept out. A longer one (a long 429 on this
    /// route, or a long global hold) fails at once with [`HttpError::RateLimited`], so a
    /// caller never sits in its route queue for minutes; that request is not counted.
    pub async fn acquire(&self, key: &str) -> Result<(), HttpError> {
        loop {
            match self.try_acquire(key, Instant::now()) {
                None => return Ok(()),
                Some(wait) if wait > AUTO_RETRY_MAX => {
                    tracing::debug!(
                        route = key,
                        ?wait,
                        "rate limit: wait too long, failing fast"
                    );
                    return Err(HttpError::RateLimited(wait));
                }
                Some(wait) => {
                    tracing::debug!(route = key, ?wait, "rate limit: waiting");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// `None` when a request may go now (and it is counted); else how long to wait.
    fn try_acquire(&self, key: &str, now: Instant) -> Option<Duration> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = s.global_until {
            if until > now {
                return Some(until - now);
            }
            s.global_until = None;
        }
        if now.saturating_duration_since(s.window_start) >= Duration::from_secs(1) {
            s.window_start = now;
            s.window_count = 0;
        }
        if s.window_count >= GLOBAL_PER_SECOND {
            return Some((s.window_start + Duration::from_secs(1)) - now);
        }
        let id = s
            .route_bucket
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_owned());
        let slot = (id, major_of(key).to_owned());
        if let Some(b) = s.buckets.get_mut(&slot) {
            if b.reset_at <= now {
                // Reset passed: the next response tells us the new state.
                s.buckets.remove(&slot);
            } else if b.remaining == 0 {
                return Some(b.reset_at - now);
            } else {
                b.remaining -= 1;
            }
        }
        s.window_count += 1;
        None
    }

    /// Learn from a response. `limited` is `Some((retry_after, global))` for a 429.
    pub fn update(&self, key: &str, h: &RateLimitHeaders, limited: Option<(Duration, bool)>) {
        self.update_at(key, h, limited, Instant::now());
    }

    fn update_at(
        &self,
        key: &str,
        h: &RateLimitHeaders,
        limited: Option<(Duration, bool)>,
        now: Instant,
    ) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bucket) = &h.bucket {
            if s.route_bucket.len() > MAX_ENTRIES * 4 {
                s.route_bucket.clear();
            }
            s.route_bucket.insert(key.to_owned(), bucket.clone());
        }
        let id = h
            .bucket
            .clone()
            .or_else(|| s.route_bucket.get(key).cloned())
            .unwrap_or_else(|| key.to_owned());
        let slot = (id, major_of(key).to_owned());
        let bucket = match limited {
            Some((wait, true)) => {
                tracing::warn!(?wait, "global rate limit hit");
                let until = now + wait;
                s.global_until = Some(s.global_until.map_or(until, |u| u.max(until)));
                return;
            }
            Some((wait, false)) => Bucket {
                remaining: 0,
                reset_at: now + wait.max(h.reset_after.unwrap_or_default()),
            },
            None => match (h.remaining, h.reset_after) {
                (Some(remaining), Some(after)) => Bucket {
                    remaining,
                    reset_at: now + after,
                },
                _ => return,
            },
        };
        if s.buckets.len() > MAX_ENTRIES {
            s.buckets.retain(|_, b| b.reset_at > now);
        }
        s.buckets.insert(slot, bucket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(bucket: &str, remaining: u32, reset_after: f64) -> RateLimitHeaders {
        RateLimitHeaders {
            bucket: Some(bucket.into()),
            remaining: Some(remaining),
            reset_after: Duration::try_from_secs_f64(reset_after).ok(),
            ..Default::default()
        }
    }

    #[test]
    fn route_keys_keep_majors_only() {
        assert_eq!(
            route_key(Method::Get, "/channels/111/messages"),
            "Get /channels/111/messages"
        );
        assert_eq!(
            route_key(Method::Delete, "/channels/111/messages/222"),
            route_key(Method::Delete, "/channels/111/messages/333")
        );
        assert_ne!(
            route_key(Method::Get, "/channels/111/messages"),
            route_key(Method::Get, "/channels/112/messages")
        );
        assert_ne!(
            route_key(Method::Get, "/channels/111/messages/2"),
            route_key(Method::Patch, "/channels/111/messages/2")
        );
        assert_eq!(
            route_key(
                Method::Put,
                "/channels/1/messages/2/reactions/%F0%9F%91%8D/@me"
            ),
            "Put /channels/1/messages/:id/reactions/:emoji/@me"
        );
        assert_eq!(
            route_key(Method::Get, "/guilds/5/messages/search?content=x"),
            "Get /guilds/5/messages/search"
        );
        assert_eq!(
            route_key(Method::Put, "/users/@me/relationships/77"),
            "Put /users/@me/relationships/:id"
        );
        assert_eq!(major_of("Get /channels/111/messages"), "/channels/111");
        assert_eq!(major_of("Get /guilds/5"), "/guilds/5");
        assert_eq!(major_of("Get /users/@me"), "");
    }

    #[test]
    fn parses_headers_and_body() {
        let mut h = HeaderMap::new();
        h.insert("X-RateLimit-Bucket", HeaderValue::from_static("abcd1234"));
        h.insert("X-RateLimit-Remaining", HeaderValue::from_static("4"));
        h.insert("X-RateLimit-Reset-After", HeaderValue::from_static("1.25"));
        h.insert("X-RateLimit-Global", HeaderValue::from_static("true"));
        h.insert("X-RateLimit-Scope", HeaderValue::from_static("shared"));
        let p = RateLimitHeaders::from_headers(&h);
        assert_eq!(p.bucket.as_deref(), Some("abcd1234"));
        assert_eq!(p.remaining, Some(4));
        assert_eq!(p.reset_after, Some(Duration::from_millis(1250)));
        assert!(p.global);
        assert_eq!(p.scope.as_deref(), Some("shared"));
        assert_eq!(
            RateLimitHeaders::from_headers(&HeaderMap::new()),
            RateLimitHeaders::default()
        );
        let mut bad = HeaderMap::new();
        bad.insert("X-RateLimit-Remaining", HeaderValue::from_static("-1"));
        bad.insert("X-RateLimit-Reset-After", HeaderValue::from_static("soon"));
        assert_eq!(
            RateLimitHeaders::from_headers(&bad),
            RateLimitHeaders::default()
        );

        assert!(body_is_global(
            r#"{"message":"You are being rate limited.","retry_after":1.0,"global":true}"#
        ));
        assert!(!body_is_global(r#"{"retry_after":1.0,"global":false}"#));
        assert!(!body_is_global("not json"));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_bucket_waits_for_reset() {
        let rl = RateLimiter::default();
        let key = route_key(Method::Post, "/channels/1/messages");
        rl.acquire(&key).await.unwrap();
        rl.update(&key, &headers("b", 1, 2.0), None);
        let start = Instant::now();
        rl.acquire(&key).await.unwrap(); // the one remaining
        assert!(start.elapsed() < Duration::from_millis(1));
        rl.acquire(&key).await.unwrap(); // waits for the reset
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn buckets_are_shared_by_hash_and_split_by_major() {
        let rl = RateLimiter::default();
        let edit = route_key(Method::Patch, "/channels/1/messages/9");
        let delete = route_key(Method::Delete, "/channels/1/messages/9");
        let other_channel = route_key(Method::Delete, "/channels/2/messages/9");
        rl.update(&edit, &headers("shared", 0, 5.0), None);
        // `delete` learns the same bucket hash from its own response.
        rl.update(&delete, &headers("shared", 0, 5.0), None);
        let now = Instant::now();
        assert!(rl.try_acquire(&edit, now).is_some());
        assert!(rl.try_acquire(&delete, now).is_some());
        // Same hash, other channel: a separate limit with its own remaining count.
        rl.update(&other_channel, &headers("shared", 1, 5.0), None);
        assert_eq!(rl.try_acquire(&other_channel, now), None);
        assert!(rl.try_acquire(&other_channel, now).is_some());
        assert!(rl.try_acquire(&edit, now).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn global_429_holds_every_route() {
        let rl = RateLimiter::default();
        let a = route_key(Method::Get, "/users/@me");
        let b = route_key(Method::Get, "/channels/3/messages");
        rl.update(
            &a,
            &RateLimitHeaders::default(),
            Some((Duration::from_secs(3), true)),
        );
        let start = Instant::now();
        rl.acquire(&b).await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn route_429_holds_only_that_route() {
        let rl = RateLimiter::default();
        let a = route_key(Method::Post, "/channels/3/typing");
        let b = route_key(Method::Get, "/channels/3/messages");
        // No bucket header (as in many 429s): limited under the route key itself.
        rl.update(
            &a,
            &RateLimitHeaders::default(),
            Some((Duration::from_secs(4), false)),
        );
        let now = Instant::now();
        assert_eq!(rl.try_acquire(&b, now), None);
        assert_eq!(rl.try_acquire(&a, now), Some(Duration::from_secs(4)));
    }

    #[tokio::test(start_paused = true)]
    async fn global_window_allows_fifty_per_second() {
        let rl = RateLimiter::default();
        let start = Instant::now();
        for i in 0..GLOBAL_PER_SECOND {
            rl.acquire(&format!("Get /channels/{i}/messages"))
                .await
                .unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(1));
        rl.acquire("Get /users/@me").await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn queue_is_fifo_per_route() {
        let rl = Arc::new(RateLimiter::default());
        let key = route_key(Method::Post, "/channels/1/messages");
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = rl.queue(&key).await;
        let mut tasks = Vec::new();
        for i in 0..3 {
            let (rl, key, order) = (rl.clone(), key.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let _turn = rl.queue(&key).await;
                order.lock().unwrap().push(i);
            }));
            // Let each task reach the queue before the next one.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(order.lock().unwrap().is_empty());
        drop(first);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn long_bucket_wait_fails_fast() {
        let rl = RateLimiter::default();
        let key = route_key(Method::Post, "/channels/1/messages");
        let other = route_key(Method::Get, "/channels/1/messages");
        rl.update(
            &key,
            &RateLimitHeaders::default(),
            Some((Duration::from_secs(600), false)),
        );
        let start = Instant::now();
        let _turn = rl.queue(&key).await;
        match rl.acquire(&key).await {
            Err(HttpError::RateLimited(wait)) => assert_eq!(wait, Duration::from_secs(600)),
            other => panic!("{other:?}"),
        }
        // No sleep (the clock is paused, so any sleep would show), and nothing was counted.
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(rl.state.lock().unwrap().window_count, 0);
        // Other routes are unaffected.
        rl.acquire(&other).await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
        // Once the reset is near enough, the wait is slept out again.
        tokio::time::advance(Duration::from_secs(597)).await;
        rl.acquire(&key).await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(600));
    }

    #[tokio::test(start_paused = true)]
    async fn long_global_hold_fails_fast_on_every_route() {
        let rl = RateLimiter::default();
        let a = route_key(Method::Get, "/users/@me");
        let b = route_key(Method::Get, "/channels/3/messages");
        rl.update(
            &a,
            &RateLimitHeaders::default(),
            Some((Duration::from_secs(600), true)),
        );
        let start = Instant::now();
        for key in [&a, &b] {
            match rl.acquire(key).await {
                Err(HttpError::RateLimited(wait)) => assert_eq!(wait, Duration::from_secs(600)),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(rl.state.lock().unwrap().window_count, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_up_to_the_auto_retry_max_still_sleep() {
        let rl = RateLimiter::default();
        let key = route_key(Method::Post, "/channels/1/typing");
        rl.update(
            &key,
            &RateLimitHeaders::default(),
            Some((AUTO_RETRY_MAX, false)),
        );
        let start = Instant::now();
        rl.acquire(&key).await.unwrap();
        assert_eq!(start.elapsed(), AUTO_RETRY_MAX);
        assert_eq!(rl.state.lock().unwrap().window_count, 1);

        // Just over the limit fails instead.
        rl.update(
            &key,
            &RateLimitHeaders::default(),
            Some((AUTO_RETRY_MAX + Duration::from_millis(1), false)),
        );
        assert!(matches!(
            rl.acquire(&key).await,
            Err(HttpError::RateLimited(_))
        ));
        assert_eq!(start.elapsed(), AUTO_RETRY_MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn fail_fast_releases_the_queue_for_later_callers() {
        let rl = Arc::new(RateLimiter::default());
        let key = route_key(Method::Post, "/channels/1/messages");
        rl.update(
            &key,
            &RateLimitHeaders::default(),
            Some((Duration::from_secs(600), false)),
        );
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let (rl, key) = (rl.clone(), key.clone());
            tasks.push(tokio::spawn(async move {
                let _turn = rl.queue(&key).await;
                rl.acquire(&key).await
            }));
        }
        for t in tasks {
            assert!(matches!(t.await.unwrap(), Err(HttpError::RateLimited(_))));
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn maps_stay_capped() {
        let rl = RateLimiter::default();
        let now = Instant::now();
        for i in 0..(MAX_ENTRIES * 3) {
            let key = format!("Get /channels/{i}/messages");
            rl.update_at(&key, &headers(&format!("b{i}"), 1, 0.0), None, now);
        }
        let s = rl.state.lock().unwrap();
        assert!(s.buckets.len() <= MAX_ENTRIES + 1);
        assert!(s.route_bucket.len() <= MAX_ENTRIES * 4 + 1);
    }
}
