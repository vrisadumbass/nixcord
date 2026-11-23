//! URL helpers: stable cache keys and Discord media-proxy resizing.

/// 64-bit FNV-1a. Stable across runs and Rust versions (used for disk file names).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Id for "this URL decoded into a `w` x `h` box".
pub fn sized_id(url: &str, w: u32, h: u32) -> u64 {
    let mut x = fnv1a64(url.as_bytes()) ^ ((w as u64) << 32 | h as u64);
    // splitmix64 finalizer so nearby sizes don't collide in low bits.
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn split_host(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let slash = rest.find('/').unwrap_or(rest.len());
    Some((&rest[..slash], &rest[slash..]))
}

fn is_discord_media_host(host: &str) -> bool {
    host == "cdn.discordapp.com"
        || host == "media.discordapp.net"
        || (host.starts_with("images-ext-") && host.ends_with(".discordapp.net"))
}

/// Query parameters that change between fetches of the same Discord file (signed URLs).
const VOLATILE: [&str; 3] = ["ex", "is", "hm"];

fn param_name(p: &str) -> &str {
    p.split('=').next().unwrap_or(p)
}

/// Disk-cache key for a URL. For Discord hosts the expiring signature (`ex`, `is`, `hm`) is
/// ignored so a refreshed link still hits the cache.
pub fn cache_key(url: &str) -> u64 {
    match (split_host(url), url.split_once('?')) {
        (Some((host, _)), Some((base, query))) if is_discord_media_host(host) => {
            let mut s = String::with_capacity(url.len());
            s.push_str(base);
            let mut first = true;
            for p in query.split('&') {
                if p.is_empty() || VOLATILE.contains(&param_name(p)) {
                    continue;
                }
                s.push(if first { '?' } else { '&' });
                first = false;
                s.push_str(p);
            }
            fnv1a64(s.as_bytes())
        }
        _ => fnv1a64(url.as_bytes()),
    }
}

/// Ask Discord's media proxy for a pre-scaled copy, so we download (and decode) only about
/// as many pixels as we will show. Applies to attachment URLs (`cdn.discordapp.com` /
/// `media.discordapp.net` under `/attachments/` or `/ephemeral-attachments/`) and to embed
/// proxy URLs (`images-ext-N.discordapp.net/external/...`). Other URLs are returned as-is.
///
/// Existing `width`/`height` parameters are replaced; the signature parameters are kept.
pub fn sized_url(url: &str, w: u32, h: u32) -> String {
    let Some((host, path)) = split_host(url) else {
        return url.to_owned();
    };
    let attachment = (host == "cdn.discordapp.com" || host == "media.discordapp.net")
        && (path.starts_with("/attachments/") || path.starts_with("/ephemeral-attachments/"));
    let external = host.starts_with("images-ext-")
        && host.ends_with(".discordapp.net")
        && path.starts_with("/external/");
    if !attachment && !external {
        return url.to_owned();
    }
    let host = if attachment {
        "media.discordapp.net"
    } else {
        host
    };
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let mut out = format!("https://{host}{path}?");
    for p in query.split('&') {
        if p.is_empty() || matches!(param_name(p), "width" | "height") {
            continue;
        }
        out.push_str(p);
        out.push('&');
    }
    out.push_str(&format!("width={}&height={}", w.max(1), h.max(1)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn sized_ids_differ() {
        let u = "https://cdn.discordapp.com/avatars/1/a.webp?size=64";
        assert_ne!(sized_id(u, 40, 40), sized_id(u, 50, 50));
        assert_ne!(sized_id(u, 40, 41), sized_id(u, 41, 40));
        assert_eq!(sized_id(u, 40, 40), sized_id(u, 40, 40));
    }

    #[test]
    fn cache_key_ignores_signature() {
        let a = "https://cdn.discordapp.com/attachments/1/2/x.png?ex=aa&is=bb&hm=cc&";
        let b = "https://cdn.discordapp.com/attachments/1/2/x.png?ex=dd&is=ee&hm=ff";
        let c = "https://cdn.discordapp.com/attachments/1/2/x.png";
        assert_eq!(cache_key(a), cache_key(b));
        assert_eq!(cache_key(a), cache_key(c));
        // Non-volatile params still matter.
        let d = "https://cdn.discordapp.com/avatars/1/h.webp?size=64";
        let e = "https://cdn.discordapp.com/avatars/1/h.webp?size=128";
        assert_ne!(cache_key(d), cache_key(e));
        // Other hosts are keyed verbatim.
        assert_ne!(
            cache_key("https://example.com/a.png?ex=1"),
            cache_key("https://example.com/a.png?ex=2")
        );
    }

    #[test]
    fn sized_attachment_urls() {
        assert_eq!(
            sized_url(
                "https://cdn.discordapp.com/attachments/1/2/x.png?ex=a&is=b&hm=c&",
                400,
                300
            ),
            "https://media.discordapp.net/attachments/1/2/x.png?ex=a&is=b&hm=c&width=400&height=300"
        );
        assert_eq!(
            sized_url(
                "https://media.discordapp.net/attachments/1/2/x.png?width=10&height=20",
                400,
                300
            ),
            "https://media.discordapp.net/attachments/1/2/x.png?width=400&height=300"
        );
        assert_eq!(
            sized_url(
                "https://images-ext-1.discordapp.net/external/abc/https/example.com/a.jpg",
                64,
                32
            ),
            "https://images-ext-1.discordapp.net/external/abc/https/example.com/a.jpg?width=64&height=32"
        );
    }

    #[test]
    fn sized_leaves_other_urls() {
        for u in [
            "https://example.com/a.png",
            "https://cdn.discordapp.com/avatars/1/a.webp?size=64",
            "http://cdn.discordapp.com/attachments/1/2/x.png",
            "not a url",
        ] {
            assert_eq!(sized_url(u, 10, 10), u);
        }
    }
}
