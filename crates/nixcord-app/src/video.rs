//! Inline video player for attachments, video embeds, `gifv` embeds and media-gallery
//! videos (cargo feature `video`, on by default; see `nixcord-video` for the pipeline).
//!
//! # Behaviour (matching Discord)
//!
//! - Before playing: the poster (Discord's media proxy still, or the first frame decoded from
//!   a small ranged download when there is none), a big play button, and a pill with the
//!   duration (from the same ranged probe) and the file size.
//! - Play: the file is streamed to the disk cache (capped at [`MAX_BYTES`]); playback starts
//!   once about a second is buffered, with a spinner while waiting.
//! - On hover (or while paused): play/pause, a seek bar with the buffered range, time,
//!   volume / mute, fullscreen. A click on the picture toggles pause; Space too while the
//!   player has keyboard focus (it takes focus when clicked).
//! - One video plays at a time: starting one pauses the others. A video that leaves the
//!   screen or a minimized window pauses; players not drawn for a few seconds are dropped,
//!   which frees the decoder, the frame queue, the texture and the audio stream.
//! - `gifv` (Tenor / Giphy mp4): muted, looping, no controls, plays while visible.
//!   [`gifv_prefers_video`] decides between this and the animated-image path in `media`.
//! - HEVC, VP8/VP9/AV1 (WebM) and other formats show "Can't play this format" with a link to
//!   the browser instead.
//!
//! # Cost
//!
//! One texture per player (`TextureHandle::set` per frame), at the size it is shown; the
//! player thread keeps three decoded frames at most. Repaints are scheduled with
//! `request_repaint_after(next frame due)`; nothing repaints continuously.
//!
//! The state lives in an egui [`Plugin`](egui::Plugin) (`on_end_pass` pauses and drops what
//! was not drawn), so the app needs no field for it. With the feature off every entry point
//! returns `false` and the callers keep their poster / "open in browser" fallbacks.
//!
//! Demo mode never touches the network: `NIXCORD_DEMO_VIDEO=<path to an mp4>` adds a demo
//! message playing that file, and `NIXCORD_DEMO_NET=1` adds one with a small public test mp4
//! (MDN's CC0 `flower.mp4`) fetched over the network.

// Without the feature only the entry points (returning `false`) are used.
#![cfg_attr(not(feature = "video"), allow(dead_code))]

use crate::app::NixApp;
use eframe::egui::{self, Rect, Ui, Vec2};
use nixcord_model::components::UnfurledMedia;
use nixcord_model::{Attachment, Embed};

/// Largest video downloaded for inline playback.
pub const MAX_BYTES: u64 = 100 * 1024 * 1024;
/// URL prefix of the demo message's local video (see the module docs).
pub const DEMO_VIDEO_URL: &str = "https://demo.nixcord.invalid/video/";
/// Public, CC0 test clip (H.264 + AAC, 960x540, 5 s, 1.1 MB) for `NIXCORD_DEMO_NET=1`.
pub const DEMO_NET_VIDEO: &str =
    "https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4";

/// Should a `gifv` embed play its mp4 through the video player (true) or through the
/// animated GIF / WebP path in `media` (false)? The video path keeps three small frames
/// instead of every decoded GIF frame, so it wins whenever it can play the file: the
/// feature is on and the embed's video is an MP4 (Tenor and Giphy mp4s are H.264). If the
/// player later fails, [`embed`] returns `false` and the caller falls back to the
/// animated-image path.
pub fn gifv_prefers_video(e: &Embed) -> bool {
    cfg!(feature = "video") && gifv_mp4(e).is_some()
}

/// The mp4 of a `gifv` embed, if it has one.
fn gifv_mp4(e: &Embed) -> Option<&str> {
    if e.kind.as_deref() != Some("gifv") {
        return None;
    }
    let v = e.video.as_ref()?;
    // The original URL tells the format; the proxy URL is what we fetch.
    let is_mp4 = |u: &str| path_ext(u).is_some_and(|x| x == "mp4");
    is_mp4(&v.url)
        .then(|| v.proxy_url.as_deref().unwrap_or(&v.url))
        .filter(|u| u.starts_with("https://"))
}

/// Lower-case extension of the URL's last path segment.
fn path_ext(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let seg = path.rsplit('/').next()?;
    let (_, ext) = seg.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// What a video URL / content type is, before downloading anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// MP4 / MOV / M4V (or unknown): try it; the demuxer tells HEVC apart.
    Mp4,
    /// WebM / MKV: VP8, VP9 or AV1, which we don't decode.
    WebM,
}

fn format_of(url: &str, content_type: Option<&str>) -> Format {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let ext = path_ext(url).unwrap_or_default();
    if ct.contains("webm") || ct.contains("matroska") || ext == "webm" || ext == "mkv" {
        Format::WebM
    } else {
        Format::Mp4
    }
}

fn is_video_attachment(a: &Attachment) -> bool {
    match a.content_type.as_deref() {
        Some(ct) => ct.starts_with("video/"),
        None => matches!(
            path_ext(&a.filename).as_deref(),
            Some("mp4" | "mov" | "m4v" | "webm" | "mkv")
        ),
    }
}

/// `m:ss`, or `h:mm:ss` from an hour.
fn fmt_time(secs: f64) -> String {
    let s = if secs.is_finite() { secs.max(0.0) } else { 0.0 } as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---- entry points (feature-independent signatures) ---------------------------------------

/// A video attachment as an inline player. `false`: not a video (or the feature is off), the
/// caller draws its usual preview.
pub fn attachment(ui: &mut Ui, app: &NixApp, a: &Attachment, max: Vec2) -> bool {
    if !is_video_attachment(a) {
        return false;
    }
    #[cfg(feature = "video")]
    {
        imp::attachment(ui, app, a, max)
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (ui, app, max);
        false
    }
}

/// A bare embed (no title / description): `gifv` when [`gifv_prefers_video`], or a direct
/// video file link. `false`: the caller draws it as before.
pub fn embed(ui: &mut Ui, app: &NixApp, e: &Embed, max: Vec2) -> bool {
    #[cfg(feature = "video")]
    {
        imp::embed(ui, app, e, max, false)
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (ui, app, e, max);
        false
    }
}

/// The video of a `video` embed card (e.g. a tweet with a video) when it is a playable
/// file; YouTube and other iframe players return `false` (the caller keeps its poster).
pub fn embed_in_card(ui: &mut Ui, app: &NixApp, e: &Embed, max: Vec2) -> bool {
    #[cfg(feature = "video")]
    {
        imp::embed(ui, app, e, max, true)
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (ui, app, e, max);
        false
    }
}

/// A video item of a media gallery, filling `rect` (letterboxed).
pub fn gallery_cell(ui: &mut Ui, app: &NixApp, rect: Rect, m: &UnfurledMedia) -> bool {
    #[cfg(feature = "video")]
    {
        imp::gallery_cell(ui, app, rect, m)
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (ui, app, rect, m);
        false
    }
}

/// The fullscreen player, if one is open. Call once per frame after the main UI (from the
/// image viewer's `show`, whose overlay this mirrors).
pub fn fullscreen(ctx: &egui::Context, app: &NixApp) {
    #[cfg(feature = "video")]
    imp::fullscreen(ctx, app);
    #[cfg(not(feature = "video"))]
    let _ = (ctx, app);
}

// ---- implementation ------------------------------------------------------------------------

#[cfg(feature = "video")]
mod imp {
    use super::*;
    use crate::media::{paint_texture, Tex};
    use crate::ui::prop;
    use eframe::egui::{
        Align2, Color32, ColorImage, Context, CornerRadius, CursorIcon, Id, Key, Modifiers, Order,
        Pos2, Sense, Shape, Stroke, TextureHandle, TextureOptions,
    };
    use nixcord_media::{size, url as murl};
    use nixcord_video::{Input, Options, Player, Prober, Problem, State, Target};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    /// Players not drawn for this long are dropped (decoder, frames, audio, texture).
    const KEEP: Duration = Duration::from_secs(5);
    const KEEP_GIFV: Duration = Duration::from_secs(2);
    /// Probe results kept (they are tiny; posters are dropped sooner).
    const MAX_PROBES: usize = 512;
    const POSTER_KEEP: Duration = Duration::from_secs(30);
    const BAR_H: f32 = 34.0;
    const RADIUS: u8 = 6;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Kind {
        Video,
        Gifv,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Play {
        Url(String),
        File(PathBuf),
    }

    impl Play {
        fn input(&self) -> Input {
            match self {
                Play::Url(u) => Input::Url(u.clone()),
                Play::File(p) => Input::File(p.clone()),
            }
        }
        fn target(&self) -> Target {
            match self {
                Play::Url(u) => Target::Url(u.clone()),
                Play::File(p) => Target::File(p.clone()),
            }
        }
    }

    /// Everything needed to draw one video.
    #[derive(Clone, Debug)]
    struct Src {
        key: u64,
        kind: Kind,
        /// `None`: can't be played here (see `unsupported`).
        play: Option<Play>,
        unsupported: Option<String>,
        /// Opened by "Open in browser".
        open: String,
        /// Discord media-proxy poster (unsized URL).
        poster: Option<String>,
        natural: Option<(u32, u32)>,
        bytes: Option<u64>,
    }

    struct Slot {
        player: Player,
        src: Src,
        tex: Option<TextureHandle>,
        tex_size: [u32; 2],
        gen: Option<u32>,
        /// The user (or autoplay) wants it playing.
        want_play: bool,
        /// Paused by us (minimized / off-screen gifv): resume when visible again.
        auto_paused: bool,
        drawn_pass: u64,
        last_drawn: Instant,
        target: (u32, u32),
        /// Seek bar drag position (fraction) while dragging.
        seek_drag: Option<f32>,
        /// Play was sent to an ended player; its status says "ended" until it restarts.
        replay_sent: bool,
    }

    enum Probe {
        Pending {
            asked_poster: bool,
        },
        Done {
            duration: f64,
            size: (u32, u32),
            playable: Result<(), Problem>,
            poster: Option<TextureHandle>,
            asked_poster: bool,
            last_drawn: Instant,
        },
        Failed,
    }

    struct Manager {
        slots: HashMap<u64, Slot>,
        probes: HashMap<u64, Probe>,
        prober: Option<Prober>,
        prober_broken: bool,
        volume: f32,
        muted: bool,
        pass: u64,
        fullscreen: Option<u64>,
        minimized: bool,
        /// The player whose volume slider is open (key, fullscreen copy).
        vol_open: Option<(u64, bool)>,
    }

    impl Default for Manager {
        fn default() -> Self {
            Self {
                slots: HashMap::new(),
                probes: HashMap::new(),
                prober: None,
                prober_broken: false,
                volume: 1.0,
                muted: false,
                pass: u64::MAX,
                fullscreen: None,
                minimized: false,
                vol_open: None,
            }
        }
    }

    /// The plugin only holds the manager: widgets lock the manager, never the plugin, so
    /// egui can call plugin hooks while a player is being drawn.
    #[derive(Default)]
    struct VideoPlugin {
        m: Arc<Mutex<Manager>>,
    }

    impl egui::Plugin for VideoPlugin {
        fn debug_name(&self) -> &'static str {
            "nixcord-video"
        }

        fn on_begin_pass(&mut self, ui: &mut Ui) {
            lock(&self.m).fullscreen_keys(ui.ctx());
        }

        fn on_end_pass(&mut self, ui: &mut Ui) {
            lock(&self.m).end_pass(ui.ctx());
        }

        fn on_exit(&mut self, _ctx: &Context) {
            // Players hold a repaint callback with the context: drop them before it goes.
            let mut m = lock(&self.m);
            m.slots.clear();
            m.probes.clear();
            m.prober = None;
        }
    }

    fn lock(m: &Mutex<Manager>) -> MutexGuard<'_, Manager> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn manager(ctx: &Context) -> Arc<Mutex<Manager>> {
        if let Some(m) = ctx.with_plugin::<VideoPlugin, _>(|p| p.m.clone()) {
            return m;
        }
        ctx.add_plugin(VideoPlugin::default());
        ctx.with_plugin::<VideoPlugin, _>(|p| p.m.clone())
            .expect("video plugin registered")
    }

    fn with<R>(ctx: &Context, f: impl FnOnce(&mut Manager) -> R) -> R {
        let m = manager(ctx);
        let mut g = lock(&m);
        f(&mut g)
    }

    fn demo_net() -> bool {
        std::env::var_os("NIXCORD_DEMO_NET").is_some_and(|v| v == "1")
    }

    /// What to play for `url`. Demo mode plays only the local demo file and, with
    /// `NIXCORD_DEMO_NET=1`, real https URLs.
    fn play_target(app: &NixApp, url: &str) -> Option<Play> {
        if let Some(rest) = url.strip_prefix(DEMO_VIDEO_URL) {
            let _ = rest;
            return app
                .demo
                .then(|| std::env::var_os("NIXCORD_DEMO_VIDEO"))
                .flatten()
                .map(|p| Play::File(PathBuf::from(p)));
        }
        if !url.starts_with("https://") || (app.demo && !demo_net()) {
            return None;
        }
        Some(Play::Url(url.to_owned()))
    }

    fn poster_url(url: &str) -> Option<String> {
        // Discord's media proxy renders a still of a video attachment.
        let proxied = murl::sized_url(url, 1, 1) != url;
        proxied.then(|| {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}format=webp")
        })
    }

    pub(super) fn attachment(ui: &mut Ui, app: &NixApp, a: &Attachment, max: Vec2) -> bool {
        let fmt = format_of(&a.url, a.content_type.as_deref());
        let src = Src {
            key: murl::cache_key(&a.url),
            kind: Kind::Video,
            play: (fmt == Format::Mp4)
                .then(|| play_target(app, &a.url))
                .flatten(),
            unsupported: (fmt == Format::WebM).then(|| "WebM".to_owned()),
            open: a.url.clone(),
            poster: poster_url(&a.url),
            natural: a.width.zip(a.height).filter(|&(w, h)| w > 0 && h > 0),
            bytes: (a.size > 0).then_some(a.size),
        };
        if src.play.is_none() && src.unsupported.is_none() {
            return false; // demo mode without a file: the caller's poster
        }
        ui.add_space(2.0);
        let ctx = ui.ctx().clone();
        with(&ctx, |m| m.widget(ui, app, &src, max));
        true
    }

    pub(super) fn embed(ui: &mut Ui, app: &NixApp, e: &Embed, max: Vec2, in_card: bool) -> bool {
        let bare = e.title.is_none() && e.description.is_none();
        let (kind, url) = match e.kind.as_deref() {
            Some("gifv") if !in_card && gifv_prefers_video(e) => match gifv_mp4(e) {
                Some(u) => (Kind::Gifv, u),
                None => return false,
            },
            Some("video") if in_card != bare => {
                // A direct file: Discord proxies it (YouTube & co. have no proxy URL).
                let Some(v) = e.video.as_ref() else {
                    return false;
                };
                let Some(p) = v.proxy_url.as_deref() else {
                    return false;
                };
                if !matches!(path_ext(&v.url).as_deref(), Some("mp4" | "mov" | "m4v")) {
                    return false;
                }
                (Kind::Video, p)
            }
            _ => return false,
        };
        let Some(play) = play_target(app, url) else {
            return false;
        };
        let v = e.video.as_ref();
        let thumb = e.thumbnail.as_ref();
        let src = Src {
            // A gifv player (muted, looping) is never shared with a regular one.
            key: match kind {
                Kind::Gifv => murl::cache_key(url) ^ 0x6769_6676_0000_0000,
                Kind::Video => murl::cache_key(url),
            },
            kind,
            play: Some(play),
            unsupported: None,
            open: e
                .url
                .clone()
                .or_else(|| v.map(|v| v.url.clone()))
                .unwrap_or_else(|| url.to_owned()),
            poster: thumb.map(|t| t.proxy_url.clone().unwrap_or_else(|| t.url.clone())),
            natural: v
                .and_then(|v| v.width.zip(v.height))
                .or_else(|| thumb.and_then(|t| t.width.zip(t.height)))
                .filter(|&(w, h)| w > 0 && h > 0),
            bytes: None,
        };
        let ctx = ui.ctx().clone();
        if !in_card {
            ui.add_space(2.0);
        }
        // A gifv that can't play (yet) hands back to the animated-image path.
        with(&ctx, |m| {
            if kind == Kind::Gifv && m.gifv_failed(src.key) {
                return false;
            }
            m.widget(ui, app, &src, max);
            true
        })
    }

    pub(super) fn gallery_cell(ui: &mut Ui, app: &NixApp, rect: Rect, m: &UnfurledMedia) -> bool {
        let url = if m.url.starts_with("https://") {
            m.url.as_str()
        } else {
            m.fetch_url()
        };
        let fmt = format_of(url, m.content_type.as_deref());
        let src = Src {
            key: murl::cache_key(url),
            kind: Kind::Video,
            play: (fmt == Format::Mp4)
                .then(|| play_target(app, url))
                .flatten(),
            unsupported: (fmt == Format::WebM).then(|| "WebM".to_owned()),
            open: url.to_owned(),
            poster: m.proxy_url.as_deref().and_then(poster_url),
            natural: m.size(),
            bytes: None,
        };
        if src.play.is_none() && src.unsupported.is_none() {
            return false;
        }
        let ctx = ui.ctx().clone();
        with(&ctx, |mg| {
            mg.draw(ui, app, &src, rect, CornerRadius::same(RADIUS), false)
        });
        true
    }

    pub(super) fn fullscreen_open(ctx: &Context) -> bool {
        ctx.with_plugin::<VideoPlugin, _>(|p| p.m.clone())
            .is_some_and(|m| lock(&m).fullscreen.is_some())
    }

    pub(super) fn fullscreen(ctx: &Context, app: &NixApp) {
        if !fullscreen_open(ctx) {
            return;
        }
        with(ctx, |m| m.fullscreen_ui(ctx, app));
    }

    impl Manager {
        /// Once per pass: probe results in.
        fn begin(&mut self, ctx: &Context) {
            let pass = ctx.cumulative_pass_nr();
            if pass == self.pass {
                return;
            }
            self.pass = pass;
            let Some(p) = &self.prober else { return };
            while let Some((key, r)) = p.try_recv() {
                let entry = match r {
                    Ok(info) => Probe::Done {
                        duration: info.duration,
                        size: info.size,
                        playable: info.playable,
                        asked_poster: info.poster.is_some()
                            || matches!(
                                self.probes.get(&key),
                                Some(Probe::Pending { asked_poster: true })
                            ),
                        poster: info.poster.map(|f| {
                            ctx.load_texture(
                                format!("video-poster-{key:x}"),
                                ColorImage::from_rgba_unmultiplied(
                                    [f.width as usize, f.height as usize],
                                    &f.rgba,
                                ),
                                TextureOptions::LINEAR,
                            )
                        }),
                        last_drawn: Instant::now(),
                    },
                    Err(_) => Probe::Failed,
                };
                self.probes.insert(key, entry);
            }
        }

        fn end_pass(&mut self, ctx: &Context) {
            let now = Instant::now();
            let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
            let restored = self.minimized && !minimized;
            self.minimized = minimized;
            let pass = ctx.cumulative_pass_nr();
            for s in self.slots.values_mut() {
                let drawn = s.drawn_pass == pass;
                if (minimized || !drawn) && s.want_play {
                    // Off-screen or minimized: pause. Gifs and minimized videos resume
                    // when they are visible again; a video scrolled away stays paused.
                    s.player.pause();
                    s.want_play = false;
                    s.auto_paused = minimized || s.src.kind == Kind::Gifv;
                } else if restored && s.auto_paused && drawn {
                    s.player.play();
                    s.want_play = true;
                    s.auto_paused = false;
                }
            }
            let fs = self.fullscreen;
            self.slots.retain(|k, s| {
                let keep = match s.src.kind {
                    Kind::Video => KEEP,
                    Kind::Gifv => KEEP_GIFV,
                };
                Some(*k) == fs || now.duration_since(s.last_drawn) < keep
            });
            if fs.is_some_and(|k| !self.slots.contains_key(&k)) {
                self.fullscreen = None;
            }
            for p in self.probes.values_mut() {
                if let Probe::Done {
                    poster, last_drawn, ..
                } = p
                {
                    if poster.is_some() && now.duration_since(*last_drawn) > POSTER_KEEP {
                        *poster = None;
                    }
                }
            }
            if self.probes.len() > MAX_PROBES {
                self.probes.clear();
            }
        }

        fn gifv_failed(&self, key: u64) -> bool {
            self.slots
                .get(&key)
                .is_some_and(|s| matches!(s.player.status().state, State::Failed(_)))
        }

        /// Allocate the player's box (aspect from the natural size, a probe, or 16:9) and
        /// draw it.
        fn widget(&mut self, ui: &mut Ui, app: &NixApp, src: &Src, max: Vec2) {
            self.begin(ui.ctx());
            let natural = src.natural.or_else(|| match self.probes.get(&src.key) {
                Some(Probe::Done { size, .. }) if size.0 > 0 && size.1 > 0 => Some(*size),
                _ => None,
            });
            let (w, h) = size::fit_points(
                natural
                    .map(|(w, h)| (w as f32, h as f32))
                    .unwrap_or((max.x, max.x * 9.0 / 16.0)),
                (max.x.max(1.0), max.y.max(1.0)),
            );
            // Too small to use: give the controls room.
            let (w, h) = if w < 160.0 && natural.is_some() {
                let k = (160.0 / w).min(max.x / w.max(1.0));
                (w * k, h * k)
            } else {
                (w, h)
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
            self.draw(ui, app, src, rect, CornerRadius::same(RADIUS), false);
        }

        fn start(&mut self, ctx: &Context, src: &Src, target: (u32, u32)) {
            let Some(play) = &src.play else { return };
            if src.kind == Kind::Video {
                self.pause_others(src.key);
            }
            let gifv = src.kind == Kind::Gifv;
            let opts = Options {
                audio: !gifv,
                looping: gifv,
                autoplay: true,
                volume: self.volume,
                muted: gifv || self.muted,
                max_bytes: MAX_BYTES,
                target,
            };
            let c = ctx.clone();
            let player = Player::spawn(play.input(), opts, Arc::new(move || c.request_repaint()));
            self.slots.insert(
                src.key,
                Slot {
                    player,
                    src: src.clone(),
                    tex: None,
                    tex_size: [0, 0],
                    gen: None,
                    want_play: true,
                    auto_paused: false,
                    drawn_pass: self.pass,
                    last_drawn: Instant::now(),
                    target,
                    seek_drag: None,
                    replay_sent: false,
                },
            );
        }

        fn pause_others(&mut self, key: u64) {
            for (k, s) in self.slots.iter_mut() {
                if *k != key && s.src.kind == Kind::Video && s.want_play {
                    s.player.pause();
                    s.want_play = false;
                    s.auto_paused = false;
                }
            }
        }

        fn toggle(&mut self, key: u64) {
            let Some(s) = self.slots.get_mut(&key) else {
                return;
            };
            if s.want_play {
                s.player.pause();
                s.want_play = false;
            } else {
                s.player.play();
                s.want_play = true;
                s.auto_paused = false;
                s.replay_sent = true;
                self.pause_others(key);
            }
        }

        fn set_volume(&mut self, volume: f32, muted: bool) {
            self.volume = volume.clamp(0.0, 1.0);
            self.muted = muted;
            for s in self.slots.values() {
                if s.src.kind == Kind::Video {
                    s.player.set_volume(self.volume, muted);
                }
            }
        }

        /// Draw `src` into `rect`. `full`: the fullscreen copy.
        fn draw(
            &mut self,
            ui: &mut Ui,
            app: &NixApp,
            src: &Src,
            rect: Rect,
            rounding: CornerRadius,
            full: bool,
        ) {
            self.begin(ui.ctx());
            if !ui.is_rect_visible(rect) {
                return;
            }
            let ctx = ui.ctx().clone();
            let ppp = ctx.pixels_per_point();
            let target = (
                size::to_px(rect.width(), ppp),
                size::to_px(rect.height(), ppp),
            );
            let id = ui.scope_id().with(("nixcord-video", src.key, full));
            let resp = ui.interact(rect, id, Sense::click());
            let painter = ui.painter().clone();
            painter.rect_filled(rect, rounding, Color32::BLACK);

            // gifv: play while visible.
            if src.kind == Kind::Gifv && !self.minimized {
                match self.slots.get_mut(&src.key) {
                    None => self.start(&ctx, src, target),
                    Some(s) if !s.want_play && s.auto_paused => {
                        s.player.play();
                        s.want_play = true;
                        s.auto_paused = false;
                    }
                    _ => {}
                }
            }

            let pass = self.pass;
            let fs_other = self.fullscreen == Some(src.key) && !full;
            let mut picture: Option<Tex> = None;
            let mut status = None;
            if let Some(s) = self.slots.get_mut(&src.key) {
                s.drawn_pass = pass;
                s.last_drawn = Instant::now();
                if !fs_other && s.target != target {
                    s.target = target;
                    s.player.set_target(target.0, target.1);
                }
                let (pick, gen) = s.player.pick(s.gen);
                if let Some(f) = pick.frame {
                    let img = ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize],
                        &f.rgba,
                    );
                    match &mut s.tex {
                        Some(t) => t.set(img, TextureOptions::LINEAR),
                        None => {
                            s.tex = Some(ctx.load_texture(
                                format!("video-{:x}", src.key),
                                img,
                                TextureOptions::LINEAR,
                            ))
                        }
                    }
                    s.tex_size = [f.width, f.height];
                    s.gen = Some(gen);
                    s.player.recycle(f.rgba);
                }
                let st = s.player.status();
                if st.state != State::Ended {
                    s.replay_sent = false;
                } else if !s.replay_sent {
                    s.want_play = false; // shows the replay button
                }
                if st.state == State::Playing {
                    if let Some(d) = pick.next_in {
                        ctx.request_repaint_after(Duration::from_secs_f64(d.max(0.001)));
                    }
                }
                picture = s.tex.as_ref().map(|t| Tex {
                    id: t.id(),
                    size: s.tex_size,
                });
                status = Some((st, s.want_play, s.player.position(), s.seek_drag));
            }

            // Poster until the first frame.
            if picture.is_none() {
                picture = self.poster(ui, app, src, target);
            }
            if let Some(t) = picture {
                let fit = fit_rect(rect, t.size);
                let r = if fit.width() >= rect.width() - 1.0 && fit.height() >= rect.height() - 1.0
                {
                    rounding
                } else {
                    CornerRadius::ZERO
                };
                paint_texture(ui, fit, r, t, false);
            }

            // Duration / probe info.
            let probed = match self.probes.get_mut(&src.key) {
                Some(Probe::Done {
                    duration,
                    playable,
                    last_drawn,
                    ..
                }) => {
                    *last_drawn = Instant::now();
                    Some((*duration, playable.clone()))
                }
                _ => None,
            };
            let problem = match (&status, &probed) {
                (Some((st, ..)), _) => match &st.state {
                    State::Failed(p) => Some(p.clone()),
                    _ => None,
                },
                // Before playing, only a known-unsupported codec is worth saying.
                (None, Some((_, Err(p @ Problem::Unsupported(_))))) => Some(p.clone()),
                _ => None,
            }
            .or_else(|| {
                src.unsupported
                    .as_ref()
                    .map(|n| Problem::Unsupported(n.clone()))
            });

            if src.kind == Kind::Gifv {
                if status
                    .as_ref()
                    .is_some_and(|(st, ..)| matches!(st.state, State::Loading | State::Buffering))
                    && picture.is_none()
                {
                    spinner(ui, rect.center(), 14.0);
                }
                return;
            }

            if let Some(p) = problem {
                self.problem_overlay(ui, &resp, rect, src, &p);
                return;
            }

            let hovered = ui.rect_contains_pointer(rect);
            match status {
                None => {
                    // Not started: play button + duration / size pill.
                    big_button(ui, rect.center(), Glyph::Play, hovered);
                    let mut pill = Vec::new();
                    if let Some((d, _)) = probed {
                        pill.push(fmt_time(d));
                    }
                    if let Some(b) = src.bytes {
                        pill.push(crate::ui::fmt_size(b));
                    }
                    if !pill.is_empty() {
                        info_pill(ui, rect, &pill.join("  \u{00B7}  "));
                    }
                    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                    if resp.clicked() {
                        resp.request_focus();
                        self.start(&ctx, src, target);
                        ctx.request_repaint();
                    }
                }
                Some((st, want, pos, drag)) => {
                    let loading = matches!(st.state, State::Loading | State::Buffering) && want;
                    if loading {
                        spinner(ui, rect.center(), 18.0);
                    } else if !want {
                        let g = if st.state == State::Ended {
                            Glyph::Replay
                        } else {
                            Glyph::Play
                        };
                        big_button(ui, rect.center(), g, hovered);
                    }
                    let show_bar = hovered || !want || drag.is_some();
                    let mut bar_hit = false;
                    if show_bar && rect.height() >= BAR_H + 8.0 && rect.width() >= 120.0 {
                        bar_hit = self.controls(ui, src.key, rect, &st, pos, full);
                    }
                    let space = resp.has_focus()
                        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space));
                    if (resp.clicked() && !bar_hit) || space {
                        resp.request_focus();
                        self.toggle(src.key);
                        ctx.request_repaint();
                    }
                }
            }
        }

        /// Poster texture: the media-proxy still, else the probe's first frame. Starts the
        /// probe (duration, codec, maybe a poster) the first time a video is drawn.
        fn poster(&mut self, ui: &Ui, app: &NixApp, src: &Src, target: (u32, u32)) -> Option<Tex> {
            let proxy = match &src.poster {
                Some(p) if !app.demo => {
                    let url = murl::sized_url(p, target.0, target.1);
                    app.media
                        .try_borrow_mut()
                        .ok()
                        .map(|mut m| m.still(&url, target))
                }
                _ => Some(Err(())),
            };
            let proxy_failed = matches!(proxy, Some(Err(())));
            if let Some(Ok(Some(t))) = proxy {
                self.ensure_probe(ui.ctx(), src, false, target);
                return Some(t);
            }
            let want_frame = proxy_failed && src.kind == Kind::Video;
            self.ensure_probe(ui.ctx(), src, want_frame, target);
            match self.probes.get(&src.key) {
                Some(Probe::Done {
                    poster: Some(t), ..
                }) => Some(Tex {
                    id: t.id(),
                    size: [t.size()[0] as u32, t.size()[1] as u32],
                }),
                _ => None,
            }
        }

        fn ensure_probe(&mut self, ctx: &Context, src: &Src, want_frame: bool, bx: (u32, u32)) {
            if src.kind != Kind::Video {
                return;
            }
            let Some(play) = &src.play else { return };
            let again = match self.probes.get(&src.key) {
                None => true,
                // The proxy still failed after a probe without a poster: once more with one.
                Some(Probe::Done { asked_poster, .. }) => want_frame && !*asked_poster,
                _ => false,
            };
            if !again {
                return;
            }
            if self.prober.is_none() && !self.prober_broken {
                let c = ctx.clone();
                match Prober::spawn(Arc::new(move || c.request_repaint())) {
                    Ok(p) => self.prober = Some(p),
                    Err(e) => {
                        tracing::warn!("video prober failed to start: {e}");
                        self.prober_broken = true;
                    }
                }
            }
            if let Some(p) = &self.prober {
                p.request(src.key, play.target(), want_frame.then_some(bx));
                self.probes.insert(
                    src.key,
                    Probe::Pending {
                        asked_poster: want_frame,
                    },
                );
            }
        }

        fn problem_overlay(
            &mut self,
            ui: &Ui,
            resp: &egui::Response,
            rect: Rect,
            src: &Src,
            p: &Problem,
        ) {
            let msg = match p {
                Problem::Unsupported(name) => format!("Can't play this format ({name})"),
                Problem::TooLarge => "Too large to play here".to_owned(),
                Problem::NotFound => "This video is unavailable".to_owned(),
                Problem::Network | Problem::Corrupt => "Couldn't play this video".to_owned(),
            };
            let painter = ui.painter();
            painter.rect_filled(
                rect,
                CornerRadius::same(RADIUS),
                Color32::from_black_alpha(150),
            );
            let c = rect.center();
            painter.text(
                c - Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                msg,
                prop(14.0),
                Color32::from_gray(235),
            );
            let hovered = resp.hovered();
            let link_color = if hovered {
                Color32::WHITE
            } else {
                Color32::from_rgb(0, 168, 252)
            };
            let g = painter.text(
                c + Vec2::new(0.0, 12.0),
                Align2::CENTER_CENTER,
                "Open in browser",
                prop(14.0),
                link_color,
            );
            if hovered {
                painter.line_segment(
                    [g.left_bottom(), g.right_bottom()],
                    Stroke::new(1.0, link_color),
                );
            }
            if resp
                .clone()
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&src.open));
            }
            // A failed player is dropped soon; keep it from retrying in a loop.
            if let Some(s) = self.slots.get_mut(&src.key) {
                s.want_play = false;
            }
        }

        /// The control bar. Returns whether the pointer is over it (clicks there don't
        /// toggle pause).
        fn controls(
            &mut self,
            ui: &mut Ui,
            key: u64,
            rect: Rect,
            st: &nixcord_video::Status,
            pos: f64,
            full: bool,
        ) -> bool {
            let bar = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - BAR_H), rect.max);
            let painter = ui.painter().clone();
            // Darkening gradient (two bands) under the controls.
            painter.rect_filled(
                bar.with_min_y(bar.top() - 14.0).with_max_y(bar.top()),
                0.0,
                Color32::from_black_alpha(40),
            );
            painter.rect_filled(
                bar,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: if full { 0 } else { RADIUS },
                    se: if full { 0 } else { RADIUS },
                },
                Color32::from_black_alpha(150),
            );
            let hit = ui.rect_contains_pointer(bar);
            let cy = bar.center().y;
            let duration = st.duration.unwrap_or(0.0).max(0.0);
            let want = self.slots.get(&key).is_some_and(|s| s.want_play);

            // Play / pause.
            let mut x = bar.left() + 8.0;
            let b = Rect::from_center_size(Pos2::new(x + 12.0, cy), Vec2::splat(24.0));
            let glyph = if want {
                Glyph::Pause
            } else if st.state == State::Ended {
                Glyph::Replay
            } else {
                Glyph::Play
            };
            if icon_button(
                ui,
                Id::unique(("vid-play", key, full)),
                b,
                glyph,
                "Play / pause (Space)",
            ) {
                self.toggle(key);
            }
            x = b.right() + 8.0;

            // Time.
            let drag = self.slots.get(&key).and_then(|s| s.seek_drag);
            let shown_pos = drag.map_or(pos, |f| f as f64 * duration);
            let time = format!(
                "{} / {}",
                fmt_time(shown_pos.min(duration.max(shown_pos))),
                fmt_time(duration)
            );
            let g = painter.text(
                Pos2::new(x, cy),
                Align2::LEFT_CENTER,
                time,
                prop(12.0),
                Color32::from_gray(235),
            );
            x = g.right() + 12.0;

            // Right side: fullscreen, volume.
            let mut rx = bar.right() - 8.0;
            let fs = Rect::from_center_size(Pos2::new(rx - 12.0, cy), Vec2::splat(24.0));
            let fs_glyph = if full { Glyph::ExitFull } else { Glyph::Full };
            if icon_button(
                ui,
                Id::unique(("vid-fs", key, full)),
                fs,
                fs_glyph,
                "Fullscreen",
            ) {
                self.fullscreen = if full { None } else { Some(key) };
            }
            rx = fs.left() - 6.0;
            if st.has_audio {
                let vb = Rect::from_center_size(Pos2::new(rx - 12.0, cy), Vec2::splat(24.0));
                let vg = if self.muted || self.volume <= 0.0 {
                    Glyph::Muted
                } else {
                    Glyph::Volume
                };
                if icon_button(ui, Id::unique(("vid-mute", key, full)), vb, vg, "Mute") {
                    let muted = !self.muted;
                    let v = if !muted && self.volume <= 0.0 {
                        1.0
                    } else {
                        self.volume
                    };
                    self.set_volume(v, muted);
                }
                rx = vb.left() - 4.0;
                // The slider opens from the volume button and stays open while the
                // pointer is on it (coming from the seek bar doesn't open it).
                let area = Rect::from_min_max(
                    Pos2::new(rx - 64.0, bar.top()),
                    Pos2::new(vb.right(), bar.bottom()),
                );
                let sid = Id::unique(("vid-vol", key, full));
                let dragging = ui.ctx().is_being_dragged(sid);
                let was_open = self.vol_open == Some((key, full));
                let open = ui.rect_contains_pointer(vb.expand(2.0))
                    || dragging
                    || (was_open && ui.rect_contains_pointer(area));
                self.vol_open = if open {
                    Some((key, full))
                } else if was_open {
                    None
                } else {
                    self.vol_open
                };
                if open {
                    let track = Rect::from_min_max(
                        Pos2::new(rx - 60.0, cy - 2.0),
                        Pos2::new(rx - 4.0, cy + 2.0),
                    );
                    let r = ui.interact(
                        track.expand2(Vec2::new(4.0, 8.0)),
                        sid,
                        Sense::click_and_drag(),
                    );
                    if let Some(p) = r.interact_pointer_pos() {
                        if r.dragged() || r.clicked() {
                            let v = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
                            self.set_volume(v, v <= 0.0);
                        }
                    }
                    let v = if self.muted { 0.0 } else { self.volume };
                    painter.rect_filled(
                        track,
                        CornerRadius::same(2),
                        Color32::from_white_alpha(70),
                    );
                    let filled = track.with_max_x(track.left() + track.width() * v);
                    painter.rect_filled(filled, CornerRadius::same(2), Color32::WHITE);
                    painter.circle_filled(Pos2::new(filled.right(), cy), 5.0, Color32::WHITE);
                    rx = track.left() - 8.0;
                }
            }

            // Seek bar with the buffered range.
            let track = Rect::from_min_max(
                Pos2::new(x, cy - 2.0),
                Pos2::new(rx.max(x + 20.0), cy + 2.0),
            );
            let sid = Id::unique(("vid-seek", key, full));
            let r = ui.interact(
                track.expand2(Vec2::new(0.0, 9.0)),
                sid,
                Sense::click_and_drag(),
            );
            let frac_at = |p: Pos2| ((p.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
            if duration > 0.0 {
                if let Some(p) = r.interact_pointer_pos() {
                    if r.dragged() {
                        if let Some(s) = self.slots.get_mut(&key) {
                            s.seek_drag = Some(frac_at(p));
                        }
                    }
                    if r.drag_stopped() || r.clicked() {
                        if let Some(s) = self.slots.get_mut(&key) {
                            s.seek_drag = None;
                            s.player.seek(frac_at(p) as f64 * duration);
                        }
                    }
                }
            }
            let hover_seek = r.hovered() || r.dragged();
            let tr = if hover_seek {
                track.expand2(Vec2::new(0.0, 1.0))
            } else {
                track
            };
            let frac = |t: f64| {
                if duration > 0.0 {
                    (t / duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                }
            };
            painter.rect_filled(tr, CornerRadius::same(3), Color32::from_white_alpha(50));
            let buf = tr.with_max_x(tr.left() + tr.width() * frac(st.buffered));
            painter.rect_filled(buf, CornerRadius::same(3), Color32::from_white_alpha(110));
            let played = tr.with_max_x(tr.left() + tr.width() * frac(shown_pos));
            painter.rect_filled(
                played,
                CornerRadius::same(3),
                Color32::from_rgb(88, 101, 242),
            );
            if hover_seek {
                painter.circle_filled(Pos2::new(played.right(), cy), 6.0, Color32::WHITE);
                if let (Some(p), true) = (ui.ctx().pointer_hover_pos(), duration > 0.0) {
                    let t = frac_at(p) as f64 * duration;
                    let label = Pos2::new(p.x.clamp(track.left(), track.right()), bar.top() - 12.0);
                    let g = painter.layout_no_wrap(fmt_time(t), prop(12.0), Color32::WHITE);
                    let bg = Rect::from_center_size(label, g.size() + Vec2::new(10.0, 4.0));
                    painter.rect_filled(bg, CornerRadius::same(4), Color32::from_black_alpha(200));
                    painter.galley(bg.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
                }
            }
            hit
        }

        /// Before the rest of the UI sees them: Esc closes the fullscreen player, Space
        /// toggles it, and nothing underneath keeps keyboard focus.
        fn fullscreen_keys(&mut self, ctx: &Context) {
            let Some(key) = self.fullscreen else { return };
            ctx.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
            let (esc, space) = ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::Escape),
                    i.consume_key(Modifiers::NONE, Key::Space),
                )
            });
            if esc {
                self.fullscreen = None;
            } else if space {
                self.toggle(key);
            }
        }

        fn fullscreen_ui(&mut self, ctx: &Context, app: &NixApp) {
            let Some(key) = self.fullscreen else { return };
            let Some(src) = self.slots.get(&key).map(|s| s.src.clone()) else {
                self.fullscreen = None;
                return;
            };
            let screen = ctx.content_rect();
            let natural = self
                .slots
                .get(&key)
                .and_then(|s| s.player.status().video_size)
                .or(src.natural)
                .unwrap_or((16, 9));
            let stage = screen.shrink2(Vec2::new(24.0, 56.0));
            let k = (stage.width() / natural.0.max(1) as f32)
                .min(stage.height() / natural.1.max(1) as f32);
            let vr = Rect::from_center_size(
                stage.center(),
                Vec2::new(natural.0 as f32 * k, natural.1 as f32 * k),
            );
            let mut close = false;
            egui::Area::new(Id::unique("nixcord-video-fullscreen"))
                .order(Order::Foreground)
                .fixed_pos(screen.min)
                .constrain(false)
                .movable(false)
                .show(ctx, |ui| {
                    ui.set_clip_rect(screen);
                    let scrim = ui.allocate_rect(screen, Sense::click());
                    ui.painter()
                        .rect_filled(screen, 0.0, Color32::from_black_alpha(240));
                    self.draw(ui, app, &src, vr, CornerRadius::ZERO, true);
                    let b = Rect::from_min_size(
                        Pos2::new(screen.right() - 52.0, screen.top() + 14.0),
                        Vec2::splat(36.0),
                    );
                    if icon_button(
                        ui,
                        Id::unique("vid-fs-close"),
                        b,
                        Glyph::Close,
                        "Close (Esc)",
                    ) {
                        close = true;
                    }
                    if scrim.clicked() {
                        close = true;
                    }
                });
            if close {
                self.fullscreen = None;
            }
        }
    }

    /// `tex` fitted (letterboxed) into `rect`.
    fn fit_rect(rect: Rect, tex: [u32; 2]) -> Rect {
        let (tw, th) = (tex[0].max(1) as f32, tex[1].max(1) as f32);
        let k = (rect.width() / tw).min(rect.height() / th);
        Rect::from_center_size(rect.center(), Vec2::new(tw * k, th * k))
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Glyph {
        Play,
        Pause,
        Replay,
        Volume,
        Muted,
        Full,
        ExitFull,
        Close,
    }

    fn paint_glyph(ui: &Ui, c: Pos2, s: f32, g: Glyph, color: Color32) {
        let p = ui.painter();
        let st = Stroke::new((s * 0.12).max(1.5), color);
        match g {
            Glyph::Play => {
                p.add(Shape::convex_polygon(
                    vec![
                        c + Vec2::new(-0.3 * s, -0.42 * s),
                        c + Vec2::new(0.45 * s, 0.0),
                        c + Vec2::new(-0.3 * s, 0.42 * s),
                    ],
                    color,
                    Stroke::NONE,
                ));
            }
            Glyph::Pause => {
                for dx in [-0.22, 0.22] {
                    p.rect_filled(
                        Rect::from_center_size(
                            c + Vec2::new(dx * s, 0.0),
                            Vec2::new(0.2 * s, 0.8 * s),
                        ),
                        CornerRadius::same(1),
                        color,
                    );
                }
            }
            Glyph::Replay => {
                let r = 0.36 * s;
                let pts: Vec<Pos2> = (0..=20)
                    .map(|i| {
                        let a = -0.6 + i as f32 / 20.0 * 5.0;
                        c + Vec2::new(a.cos() * r, a.sin() * r)
                    })
                    .collect();
                p.add(Shape::line(pts.clone(), st));
                let tip = pts[0];
                p.add(Shape::convex_polygon(
                    vec![
                        tip + Vec2::new(-0.18 * s, -0.12 * s),
                        tip + Vec2::new(0.16 * s, -0.2 * s),
                        tip + Vec2::new(0.02 * s, 0.16 * s),
                    ],
                    color,
                    Stroke::NONE,
                ));
            }
            Glyph::Volume | Glyph::Muted => {
                let body = vec![
                    c + Vec2::new(-0.42 * s, -0.14 * s),
                    c + Vec2::new(-0.2 * s, -0.14 * s),
                    c + Vec2::new(0.05 * s, -0.38 * s),
                    c + Vec2::new(0.05 * s, 0.38 * s),
                    c + Vec2::new(-0.2 * s, 0.14 * s),
                    c + Vec2::new(-0.42 * s, 0.14 * s),
                ];
                p.add(Shape::convex_polygon(body, color, Stroke::NONE));
                if g == Glyph::Volume {
                    for r in [0.22, 0.38] {
                        let pts: Vec<Pos2> = (0..=10)
                            .map(|i| {
                                let a = -0.8 + i as f32 / 10.0 * 1.6;
                                c + Vec2::new(0.08 * s + a.cos() * r * s, a.sin() * r * s)
                            })
                            .collect();
                        p.add(Shape::line(pts, st));
                    }
                } else {
                    let o = c + Vec2::new(0.3 * s, 0.0);
                    let d = 0.14 * s;
                    p.line_segment([o + Vec2::new(-d, -d), o + Vec2::new(d, d)], st);
                    p.line_segment([o + Vec2::new(-d, d), o + Vec2::new(d, -d)], st);
                }
            }
            Glyph::Full | Glyph::ExitFull => {
                let d = 0.36 * s;
                let l = 0.18 * s;
                for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                    let corner = c + Vec2::new(sx * d, sy * d);
                    let (hx, vy) = if g == Glyph::Full {
                        (-sx * l, -sy * l)
                    } else {
                        (sx * l, sy * l)
                    };
                    let base = if g == Glyph::Full {
                        corner
                    } else {
                        c + Vec2::new(sx * (d - l), sy * (d - l))
                    };
                    p.line_segment([base, base + Vec2::new(hx, 0.0)], st);
                    p.line_segment([base, base + Vec2::new(0.0, vy)], st);
                }
            }
            Glyph::Close => {
                let d = 0.24 * s;
                p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], st);
                p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], st);
            }
        }
    }

    /// Small glyph button; `true` when clicked.
    fn icon_button(ui: &mut Ui, id: Id, rect: Rect, g: Glyph, tip: &str) -> bool {
        let r = ui.interact(rect, id, Sense::click());
        let hovered = r.hovered();
        if hovered {
            ui.painter().circle_filled(
                rect.center(),
                rect.width() / 2.0,
                Color32::from_white_alpha(30),
            );
        }
        let color = if hovered {
            Color32::WHITE
        } else {
            Color32::from_gray(225)
        };
        paint_glyph(ui, rect.center(), rect.width() * 0.62, g, color);
        r.on_hover_text(tip)
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
    }

    fn big_button(ui: &Ui, c: Pos2, g: Glyph, hovered: bool) {
        let fill = Color32::from_black_alpha(if hovered { 200 } else { 150 });
        ui.painter().circle_filled(c, 26.0, fill);
        paint_glyph(
            ui,
            c + Vec2::new(if g == Glyph::Play { 2.0 } else { 0.0 }, 0.0),
            26.0,
            g,
            Color32::WHITE,
        );
    }

    fn info_pill(ui: &Ui, rect: Rect, text: &str) {
        let g = ui
            .painter()
            .layout_no_wrap(text.to_owned(), prop(12.0), Color32::WHITE);
        let r = Rect::from_min_size(
            Pos2::new(rect.left() + 8.0, rect.bottom() - 8.0 - g.size().y - 4.0),
            g.size() + Vec2::new(12.0, 4.0),
        );
        if r.right() > rect.right() - 4.0 {
            return;
        }
        ui.painter()
            .rect_filled(r, CornerRadius::same(4), Color32::from_black_alpha(170));
        ui.painter()
            .galley(r.min + Vec2::new(6.0, 2.0), g, Color32::WHITE);
    }

    /// A rotating arc, repainted at 30 fps while shown (not continuously).
    fn spinner(ui: &Ui, c: Pos2, r: f32) {
        let t = ui.input(|i| i.time) as f32;
        let start = t * 5.0;
        let pts: Vec<Pos2> = (0..=24)
            .map(|i| {
                let a = start + i as f32 / 24.0 * 4.4;
                c + Vec2::new(a.cos() * r, a.sin() * r)
            })
            .collect();
        ui.painter()
            .circle_filled(c, r + 8.0, Color32::from_black_alpha(120));
        ui.painter()
            .add(Shape::line(pts, Stroke::new(3.0, Color32::from_gray(235))));
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn poster_comes_from_the_media_proxy() {
            let u = "https://cdn.discordapp.com/attachments/1/2/clip.mp4?ex=1&is=2&hm=3";
            let p = poster_url(u).unwrap();
            assert!(p.ends_with("&format=webp"), "{p}");
            assert!(poster_url("https://example.com/clip.mp4").is_none());
        }

        #[test]
        fn fit_rect_letterboxes() {
            let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 400.0));
            let f = fit_rect(r, [1280, 720]);
            assert_eq!(f.width(), 400.0);
            assert!((f.height() - 225.0).abs() < 1e-3);
            assert_eq!(f.center(), r.center());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcord_model::EmbedMedia;

    fn gifv(video: &str) -> Embed {
        Embed {
            kind: Some("gifv".into()),
            video: Some(EmbedMedia {
                url: video.into(),
                proxy_url: Some(format!(
                    "https://images-ext-1.discordapp.net/external/x/{video}"
                )),
                width: Some(300),
                height: Some(200),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn gifv_decision() {
        let tenor = gifv("https://media.tenor.com/abcAAAPo/dance.mp4");
        assert_eq!(gifv_prefers_video(&tenor), cfg!(feature = "video"));
        assert!(gifv_mp4(&tenor)
            .unwrap()
            .starts_with("https://images-ext-1"));
        // WebM or no video at all: the animated-image path.
        assert!(!gifv_prefers_video(&gifv(
            "https://media.tenor.com/abc/dance.webm"
        )));
        let mut no_video = tenor.clone();
        no_video.video = None;
        assert!(!gifv_prefers_video(&no_video));
        let mut not_gifv = tenor;
        not_gifv.kind = Some("video".into());
        assert!(!gifv_prefers_video(&not_gifv));
    }

    #[test]
    fn formats_and_attachment_detection() {
        assert_eq!(format_of("https://x/a.MP4?x=1", None), Format::Mp4);
        assert_eq!(
            format_of("https://x/a.mov", Some("video/quicktime")),
            Format::Mp4
        );
        assert_eq!(format_of("https://x/a", Some("video/webm")), Format::WebM);
        assert_eq!(format_of("https://x/a.mkv", None), Format::WebM);
        let mut a = Attachment {
            filename: "clip.MOV".into(),
            ..Default::default()
        };
        assert!(is_video_attachment(&a));
        a.content_type = Some("image/png".into());
        assert!(!is_video_attachment(&a));
        a.content_type = Some("video/mp4".into());
        assert!(is_video_attachment(&a));
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(65.9), "1:05");
        assert_eq!(fmt_time(3725.0), "1:02:05");
        assert_eq!(fmt_time(f64::NAN), "0:00");
    }
}
