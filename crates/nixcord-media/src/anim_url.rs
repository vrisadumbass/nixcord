//! Where to fetch an animated image from.
//!
//! - Attachments (`.gif`, `.webp`, `.apng`): Discord's media proxy pre-scaled, with
//!   `animated=true` so a resized copy keeps its frames, then the original as a fallback.
//! - Custom emoji and `a_` avatars: the CDN's animated WebP (`.webp?animated=true`, per
//!   Discord's image-formatting docs), GIF as the fallback for emoji.
//! - `gifv` embeds (Tenor, Giphy): Discord sends an mp4 plus a still thumbnail, and we have no
//!   video decoder, so we map the media id to the provider's own animated image:
//!   - Tenor media ids end in a two-character format code (`media.tenor.com/<id><code>/x`):
//!     `Ax` = animated WebP (full size), `AC` = GIF; the thumbnail is `Ae`, the mp4 `Po`.
//!     The file name after the id does not matter.
//!   - Giphy: `media.giphy.com/media/<id>/giphy.webp`, then `giphy.gif`.
//!
//!   The images-ext proxy cannot be used for these: its URLs are signed per target, so
//!   only the exact (still / mp4) URLs Discord gave us would pass. Other providers fall
//!   back to the thumbnail when it is itself a GIF, else to a still with a play button.

use crate::url::{file_name, sized_url};

/// Lowercase file extension of the URL's last path segment.
fn ext(url: &str) -> String {
    let name = file_name(url).to_ascii_lowercase();
    name.rsplit_once('.')
        .map(|(_, e)| e.to_owned())
        .unwrap_or_default()
}

/// Could this image URL be animated (worth decoding every frame)? Still WebPs pass too and
/// simply decode as a still.
pub fn maybe_animated(url: &str) -> bool {
    matches!(ext(url).as_str(), "gif" | "webp" | "apng") || url.contains("animated=true")
}

/// Real video, which we cannot play in-app (the viewer offers the browser instead).
pub fn is_video(url: &str) -> bool {
    matches!(ext(url).as_str(), "mp4" | "webm" | "mov" | "m4v" | "gifv")
}

/// Candidate URLs, best first, for an attachment / embed image shown at `w` x `h` pixels.
pub fn image_candidates(url: &str, w: u32, h: u32) -> Vec<String> {
    let sized = sized_url(url, w, h);
    if sized == url {
        return vec![url.to_owned()];
    }
    vec![format!("{sized}&animated=true"), url.to_owned()]
}

/// The target URL of an `images-ext-N.discordapp.net/external/<sig>/[<query>/]https/host/path`
/// proxy link (query dropped), or the URL itself.
pub fn unproxy(url: &str) -> String {
    let Some(rest) = url.strip_prefix("https://") else {
        return url.to_owned();
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !(host.starts_with("images-ext-") && host.ends_with(".discordapp.net")) {
        return url.to_owned();
    }
    let Some(path) = path.strip_prefix("external/") else {
        return url.to_owned();
    };
    for scheme in ["https", "http"] {
        let marker = format!("/{scheme}/");
        if let Some(i) = path.find(&marker) {
            let target = &path[i + marker.len()..];
            let target = target.split(['?', '#']).next().unwrap_or(target);
            return format!("{scheme}://{target}");
        }
    }
    url.to_owned()
}

fn host_path(url: &str) -> Option<(&str, &str)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    Some(rest.split_once('/').unwrap_or((rest, "")))
}

/// Tenor media id without its two-character format code, from a media URL
/// (`media.tenor.com/<id>/..`, `media1.tenor.com/m/<id>/..`, `c.tenor.com/<id>/..`).
pub fn tenor_base(url: &str) -> Option<String> {
    let url = unproxy(url);
    let (host, path) = host_path(&url)?;
    if !host.ends_with("tenor.com") || host == "tenor.com" || host == "www.tenor.com" {
        return None;
    }
    let path = path.strip_prefix("m/").unwrap_or(path);
    let id = path.split('/').next()?;
    let ok = id.len() >= 8
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| id[..id.len() - 2].to_owned())
}

/// Giphy media id from a media URL (`media*.giphy.com/media/[v1.../]<id>/<file>`,
/// `i.giphy.com/<id>.gif`) or a page URL (`giphy.com/gifs/<slug>-<id>`, `giphy.com/embed/<id>`).
pub fn giphy_id(url: &str) -> Option<String> {
    let url = unproxy(url);
    let (host, path) = host_path(&url)?;
    let valid =
        |s: &str| (6..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric());
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let id = if host == "giphy.com" || host == "www.giphy.com" {
        match segs.as_slice() {
            ["gifs" | "stickers", slug, ..] => slug.rsplit('-').next()?,
            ["embed", id, ..] => id,
            _ => return None,
        }
    } else if host == "i.giphy.com" {
        segs.last()?.split('.').next()?
    } else if host.ends_with(".giphy.com") {
        // media/<id>/<file> or media/v1.<token>/<id>/<file>
        match segs.as_slice() {
            ["media", .., id, _file] if segs.len() >= 3 => id,
            _ => return None,
        }
    } else {
        return None;
    };
    valid(id).then(|| id.to_owned())
}

/// Animated-image URLs, best first, for a `gifv` embed. `page` is the embed URL, `thumb` and
/// `video` the thumbnail / video URLs (proxied or not). Empty when nothing animated is known.
pub fn gifv_candidates(
    page: Option<&str>,
    thumb: Option<&str>,
    video: Option<&str>,
) -> Vec<String> {
    let all = [video, thumb, page];
    if let Some(base) = all.iter().flatten().find_map(|u| tenor_base(u)) {
        return vec![
            format!("https://media.tenor.com/{base}Ax/tenor.webp"),
            format!("https://media.tenor.com/{base}AC/tenor.gif"),
        ];
    }
    if let Some(id) = all.iter().flatten().find_map(|u| giphy_id(u)) {
        return vec![
            format!("https://media.giphy.com/media/{id}/giphy.webp"),
            format!("https://media.giphy.com/media/{id}/giphy.gif"),
        ];
    }
    match thumb {
        Some(t) if ext(&unproxy(t)) == "gif" || ext(t) == "gif" => vec![t.to_owned()],
        _ => Vec::new(),
    }
}

/// Animated custom emoji, best first.
pub fn emoji_candidates(id: u64, size: u32) -> Vec<String> {
    vec![
        format!("https://cdn.discordapp.com/emojis/{id}.webp?size={size}&animated=true"),
        format!("https://cdn.discordapp.com/emojis/{id}.gif?size={size}"),
    ]
}

/// Animated version of a CDN avatar URL (`.../<a_hash>.webp?size=N`).
pub fn animated_avatar(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&animated=true")
    } else {
        format!("{url}?animated=true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert!(maybe_animated(
            "https://cdn.discordapp.com/attachments/1/2/cat.GIF?ex=1&hm=2"
        ));
        assert!(maybe_animated("https://media.tenor.com/abcAx/tenor.webp"));
        assert!(maybe_animated(
            "https://cdn.discordapp.com/emojis/1.webp?size=48&animated=true"
        ));
        assert!(!maybe_animated("https://example.com/a.png"));
        assert!(is_video(
            "https://cdn.discordapp.com/attachments/1/2/clip.mp4?ex=a"
        ));
        assert!(is_video("https://i.imgur.com/abc.gifv"));
        assert!(!is_video("https://example.com/a.gif"));
    }

    #[test]
    fn attachment_candidates_keep_animation() {
        let c = image_candidates(
            "https://cdn.discordapp.com/attachments/1/2/cat.gif?ex=a&is=b&hm=c&",
            400,
            300,
        );
        assert_eq!(
            c,
            vec![
                "https://media.discordapp.net/attachments/1/2/cat.gif?ex=a&is=b&hm=c&width=400&height=300&animated=true".to_owned(),
                "https://cdn.discordapp.com/attachments/1/2/cat.gif?ex=a&is=b&hm=c&".to_owned(),
            ]
        );
        assert_eq!(
            image_candidates("https://example.com/x.gif", 10, 10),
            vec!["https://example.com/x.gif".to_owned()]
        );
    }

    #[test]
    fn unproxies_images_ext() {
        assert_eq!(
            unproxy("https://images-ext-1.discordapp.net/external/AbC_d-1/https/media.tenor.com/DEBMsntBV9gAAAAe/cat.png"),
            "https://media.tenor.com/DEBMsntBV9gAAAAe/cat.png"
        );
        assert_eq!(
            unproxy("https://images-ext-2.discordapp.net/external/sig/%3Fcid%3D1/https/media0.giphy.com/media/xT9IgG50Fb7Mi0prBC/giphy.gif?width=10"),
            "https://media0.giphy.com/media/xT9IgG50Fb7Mi0prBC/giphy.gif"
        );
        assert_eq!(unproxy("https://example.com/a"), "https://example.com/a");
    }

    #[test]
    fn tenor_mapping() {
        // Discord's gifv embed: tenor page, png thumbnail (Ae), mp4 video (Po), both proxied.
        let page = "https://tenor.com/view/cat-dance-gif-12345678";
        let thumb = "https://images-ext-1.discordapp.net/external/x/https/media.tenor.com/DEBMsntBV9gAAAAe/cat-dance.png";
        let video = "https://images-ext-2.discordapp.net/external/y/https/media.tenor.com/DEBMsntBV9gAAAPo/cat-dance.mp4";
        assert_eq!(
            gifv_candidates(Some(page), Some(thumb), Some(video)),
            vec![
                "https://media.tenor.com/DEBMsntBV9gAAAAx/tenor.webp".to_owned(),
                "https://media.tenor.com/DEBMsntBV9gAAAAC/tenor.gif".to_owned(),
            ]
        );
        assert_eq!(
            tenor_base("https://media1.tenor.com/m/DEBMsntBV9gAAAAC/x.gif").as_deref(),
            Some("DEBMsntBV9gAAA")
        );
        assert_eq!(
            tenor_base("https://c.tenor.com/DEBMsntBV9gAAAAC/tenor.gif").as_deref(),
            Some("DEBMsntBV9gAAA")
        );
        // The page URL alone has no media id.
        assert_eq!(tenor_base(page), None);
        assert!(gifv_candidates(Some(page), None, None).is_empty());
    }

    #[test]
    fn giphy_mapping() {
        let want = vec![
            "https://media.giphy.com/media/3o7TKSjRrfIPjeiVyM/giphy.webp".to_owned(),
            "https://media.giphy.com/media/3o7TKSjRrfIPjeiVyM/giphy.gif".to_owned(),
        ];
        for u in [
            "https://giphy.com/gifs/funny-cat-3o7TKSjRrfIPjeiVyM",
            "https://giphy.com/embed/3o7TKSjRrfIPjeiVyM",
            "https://media.giphy.com/media/3o7TKSjRrfIPjeiVyM/giphy.mp4",
            "https://media4.giphy.com/media/v1.Y2lkPTc5MGI3NjEx/3o7TKSjRrfIPjeiVyM/giphy_s.gif?cid=1",
            "https://i.giphy.com/3o7TKSjRrfIPjeiVyM.gif",
            "https://images-ext-1.discordapp.net/external/z/%3Fcid%3D2/https/media2.giphy.com/media/3o7TKSjRrfIPjeiVyM/giphy.gif",
        ] {
            assert_eq!(gifv_candidates(Some(u), None, None), want, "{u}");
        }
        assert_eq!(giphy_id("https://giphy.com/explore/cats"), None);
    }

    #[test]
    fn other_gifv_providers() {
        // A thumbnail that is a GIF plays as-is; an imgur mp4 with a jpg poster does not.
        let g = "https://images-ext-1.discordapp.net/external/q/https/example.com/a.gif";
        assert_eq!(gifv_candidates(None, Some(g), None), vec![g.to_owned()]);
        assert!(gifv_candidates(
            Some("https://i.imgur.com/abc.gifv"),
            Some("https://i.imgur.com/abc.jpg"),
            Some("https://i.imgur.com/abc.mp4")
        )
        .is_empty());
    }

    #[test]
    fn emoji_and_avatar_urls() {
        assert_eq!(
            emoji_candidates(42, 48)[0],
            "https://cdn.discordapp.com/emojis/42.webp?size=48&animated=true"
        );
        assert_eq!(
            animated_avatar("https://cdn.discordapp.com/avatars/1/a_h.webp?size=64"),
            "https://cdn.discordapp.com/avatars/1/a_h.webp?size=64&animated=true"
        );
    }
}
