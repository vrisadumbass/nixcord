//! Animated images (GIF, animated WebP, APNG) for `Media`: attachments, `gifv` embeds,
//! animated custom emoji and `a_` avatars (on hover), plus playback in the viewer.
//!
//! # Memory
//!
//! - Decoding happens on the fetcher (`nixcord_media::anim`): every frame is downscaled to the
//!   display size, and one animation's frames are capped at 16 MB (smaller frames, then
//!   skipped frames, then a still with a "GIF" badge).
//! - Decoded animations live in their own byte-capped LRU ([`ANIM_CAP`]), separate from the
//!   still-texture LRU. Only animations drawn recently keep their frames: anything not drawn
//!   for [`GRACE`] (scrolled off, channel switched, viewer closed) is dropped and reloads
//!   from the disk cache when it comes back.
//! - GPU: one texture per animation, re-uploaded in place (`set_partial`) when the frame
//!   changes; the frames themselves stay in RAM as `Arc<ColorImage>`, so an upload is a
//!   reference-count bump, not a copy. `NIXCORD_ANIM_TEX=frames` instead uploads one texture
//!   per frame and drops the CPU copies (kept for measuring the trade-off; see the report in
//!   `media.rs`).
//!
//! # Playback
//!
//! No continuous repainting: each drawn animation asks for a repaint when its next frame is
//! due (`request_repaint_after`), so egui sleeps in between. Everything pauses while the
//! window is unfocused or minimized. With "Play GIFs automatically" off, animations show
//! their current frame with a "GIF" badge and play while hovered.

use super::{local_id, Got, Media, Tex};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, Rect, Shape, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2,
};
use nixcord_media::anim::{Budget, Decoded};
use nixcord_media::lru::ByteLru;
use nixcord_media::{anim_url, demo_anim, url as murl, Key};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Total bytes (RAM frames + GPU texture) of decoded animations kept.
pub(super) const ANIM_CAP: usize = 48 * 1024 * 1024;
/// How long an animation that is no longer drawn keeps its frames.
pub(super) const GRACE: Duration = Duration::from_millis(1500);
/// Demo animations: frames and delay.
const DEMO_FRAMES: u32 = 20;
const DEMO_DELAY_MS: u32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TexMode {
    /// One texture per animation, updated on frame change; CPU frames kept.
    Set,
    /// One texture per frame; CPU frames dropped after upload.
    PerFrame,
}

/// Animation for demo mode: the asset file if there is one (see `crate::demo_assets`),
/// else generated from the seed.
#[derive(Clone, Debug)]
pub(super) enum DemoAnim {
    Party(u64, Option<PathBuf>),
    Emoji(u64, Option<PathBuf>),
}

struct Play {
    size: [u32; 2],
    /// `TexMode::Set` only.
    frames: Vec<Arc<ColorImage>>,
    /// One texture (`Set`) or one per frame (`PerFrame`).
    texs: Vec<TextureHandle>,
    delays: Vec<u32>,
    cur: usize,
    /// Frame currently in `texs[0]` (`Set` mode).
    shown: usize,
    /// When the next frame is due; `None` while paused.
    due: Option<Instant>,
}

enum State {
    Loading {
        urls: Vec<String>,
        cand: usize,
        px: (u32, u32),
    },
    Ready(Box<Play>),
    /// Too large to animate: first frame, shown with a badge.
    Fallback {
        tex: TextureHandle,
        size: [u32; 2],
    },
    /// Decoded as a single frame: the still lives in the regular cache under the same id.
    Still,
    /// The viewer asked and it is not animated: use the still path.
    NotAnimated,
    Failed {
        retry_at: Instant,
    },
}

struct A {
    state: State,
    seen: Instant,
    /// Pass it was last drawn in.
    seen_pass: u64,
    /// Viewer request: a single-frame result is not uploaded (the viewer's still path loads
    /// it).
    viewer: bool,
}

pub(super) struct Anims {
    map: HashMap<u64, A>,
    lru: ByteLru<u64>,
    /// Fetch key id -> animation id.
    pending: HashMap<u64, u64>,
    demo_tx: mpsc::Sender<(u64, Result<Decoded, ()>)>,
    demo_rx: mpsc::Receiver<(u64, Result<Decoded, ()>)>,
    mode: TexMode,
    /// "Play GIFs automatically".
    pub(super) autoplay: bool,
    /// Window focused and not minimized (this pass).
    active: bool,
}

impl Default for Anims {
    fn default() -> Self {
        let (demo_tx, demo_rx) = mpsc::channel();
        let mode = match std::env::var("NIXCORD_ANIM_TEX").as_deref() {
            Ok("frames") => TexMode::PerFrame,
            _ => TexMode::Set,
        };
        Self {
            map: HashMap::new(),
            lru: ByteLru::new(),
            pending: HashMap::new(),
            demo_tx,
            demo_rx,
            mode,
            autoplay: true,
            active: true,
        }
    }
}

/// One animation request.
pub(super) struct Req<'a> {
    /// Cache id (source + size).
    pub id: u64,
    /// Candidate URLs, best first (ignored in demo mode).
    pub urls: &'a [String],
    /// Pixel box to decode into.
    pub px: (u32, u32),
    pub demo: Option<DemoAnim>,
    /// Hovered (plays even with autoplay off).
    pub hover: bool,
    pub viewer: bool,
}

/// What an animation request produced this frame.
pub(super) struct Shown {
    pub got: Got,
    /// Draw a "GIF" badge (paused by the setting, or too large to animate).
    pub badge: bool,
}

impl Media {
    /// "Play GIFs automatically" (call every frame from the app with the preference).
    pub fn set_autoplay(&mut self, on: bool) {
        self.anim.autoplay = on;
    }

    /// `(animations, bytes)` held by the animation cache.
    pub fn anim_stats(&self) -> (usize, usize) {
        (self.anim.lru.len(), self.anim.lru.total_bytes())
    }

    /// Once per pass (from `begin_pass`): take finished decodes, drop animations that are
    /// over the cap or were not drawn for [`GRACE`].
    pub(super) fn pump_anims(&mut self) {
        self.anim.active = self.ctx.input(|i| {
            let v = i.viewport();
            v.focused.unwrap_or(true) && !v.minimized.unwrap_or(false)
        });
        let mut done: Vec<(u64, Result<Decoded, nixcord_media::Failure>)> = Vec::new();
        while let Ok((id, r)) = self.anim.demo_rx.try_recv() {
            done.push((id, r.map_err(|_| nixcord_media::Failure::Decode)));
        }
        if let Some(f) = &self.fetcher {
            while let Some(l) = f.try_recv_anim() {
                let Some(id) = self.anim.pending.remove(&l.key.id()) else {
                    continue;
                };
                done.push((id, l.result));
            }
        }
        let now = Instant::now();
        for (id, result) in done {
            self.anim_arrived(id, result, now);
        }

        let pass = self.pass;
        for id in self.anim.lru.evict_over(ANIM_CAP, pass.saturating_sub(1)) {
            self.anim.map.remove(&id);
        }
        let mut wake: Option<Duration> = None;
        let lru = &mut self.anim.lru;
        self.anim.map.retain(|id, a| {
            // Drawn in the previous pass = still on screen, however long ago that was.
            let off_screen = a.seen_pass.saturating_add(1) < pass;
            let idle = now.duration_since(a.seen);
            let keep = match a.state {
                State::Failed { retry_at } => retry_at > now,
                State::Still | State::NotAnimated => !off_screen || idle < GRACE * 20,
                _ => !off_screen || idle < GRACE,
            };
            if !keep {
                lru.remove(id);
            } else if off_screen && matches!(a.state, State::Ready(_) | State::Fallback { .. }) {
                let left = GRACE.saturating_sub(idle) + Duration::from_millis(10);
                wake = Some(wake.map_or(left, |w| w.min(left)));
            }
            keep
        });
        if let Some(left) = wake {
            // Run a pass once the grace period is over even if nothing else repaints, so the
            // frames of animations that left the screen are freed.
            self.ctx.request_repaint_after(left);
        }
    }

    fn anim_arrived(
        &mut self,
        id: u64,
        result: Result<Decoded, nixcord_media::Failure>,
        now: Instant,
    ) {
        let Some(a) = self.anim.map.get_mut(&id) else {
            return; // evicted while loading
        };
        let off_screen = a.seen_pass.saturating_add(1) < self.pass;
        if !matches!(a.state, State::Loading { .. })
            || (off_screen && now.duration_since(a.seen) >= GRACE)
        {
            self.anim.map.remove(&id);
            return;
        }
        let viewer = a.viewer;
        let state = match result {
            Ok(Decoded::Anim(anim)) => {
                let size = [anim.width, anim.height];
                let dims = [anim.width as usize, anim.height as usize];
                let frame_bytes = nixcord_media::size::rgba_bytes(size[0], size[1]);
                let n = anim.frames.len();
                let mut frames = Vec::new();
                let mut texs = Vec::new();
                match self.anim.mode {
                    TexMode::Set => {
                        frames.reserve_exact(n);
                        for f in anim.frames {
                            frames.push(Arc::new(ColorImage::from_rgba_unmultiplied(dims, &f)));
                        }
                        texs.push(self.ctx.load_texture(
                            format!("anim-{id:x}"),
                            frames[0].clone(),
                            TextureOptions::LINEAR,
                        ));
                        self.anim.lru.insert(id, frame_bytes * (n + 1), self.pass);
                    }
                    TexMode::PerFrame => {
                        for (i, f) in anim.frames.into_iter().enumerate() {
                            let img = ColorImage::from_rgba_unmultiplied(dims, &f);
                            drop(f);
                            texs.push(self.ctx.load_texture(
                                format!("anim-{id:x}-{i}"),
                                img,
                                TextureOptions::LINEAR,
                            ));
                        }
                        self.anim.lru.insert(id, frame_bytes * n, self.pass);
                    }
                }
                State::Ready(Box::new(Play {
                    size,
                    frames,
                    texs,
                    delays: anim.delays_ms,
                    cur: 0,
                    shown: 0,
                    due: None,
                }))
            }
            Ok(Decoded::Fallback(img)) => {
                let size = [img.width, img.height];
                let color = ColorImage::from_rgba_unmultiplied(
                    [img.width as usize, img.height as usize],
                    &img.rgba,
                );
                let tex =
                    self.ctx
                        .load_texture(format!("anim-{id:x}"), color, TextureOptions::LINEAR);
                self.anim.lru.insert(id, img.byte_size(), self.pass);
                State::Fallback { tex, size }
            }
            Ok(Decoded::Still(_)) if viewer => State::NotAnimated,
            Ok(Decoded::Still(img)) => {
                self.upload(id, img);
                State::Still
            }
            Err(f) => {
                // Next candidate URL, if any.
                if let Some(a) = self.anim.map.get_mut(&id) {
                    if let State::Loading { urls, cand, px } = &mut a.state {
                        if *cand + 1 < urls.len() {
                            *cand += 1;
                            let key = Key::new(urls[*cand].clone(), px.0, px.1);
                            self.anim.pending.insert(key.id(), id);
                            if let Some(fetcher) = &self.fetcher {
                                fetcher.request_anim(key);
                            }
                            return;
                        }
                    }
                }
                State::Failed {
                    retry_at: now + f.retry_after(),
                }
            }
        };
        if let Some(a) = self.anim.map.get_mut(&id) {
            a.state = state;
        }
    }

    /// The current frame of an animation (starting the load on a miss), advancing playback.
    pub(super) fn anim_get(&mut self, req: Req<'_>) -> Shown {
        self.begin_pass();
        let now = Instant::now();
        let none = Shown {
            got: Got::Loading,
            badge: false,
        };
        let id = req.id;
        if let Some(a) = self.anim.map.get_mut(&id) {
            a.seen = now;
            a.seen_pass = self.pass;
            self.anim.lru.touch(&id, self.pass);
        }
        let playing = (self.anim.autoplay || req.hover || req.viewer) && self.anim.active;
        let badge_paused = !(self.anim.autoplay || req.hover || req.viewer);
        let ctx = self.ctx.clone();
        let state = self.anim.map.get_mut(&id).map(|a| &mut a.state);
        match state {
            Some(State::Ready(p)) => {
                let n = p.delays.len();
                if playing && n > 1 {
                    let delay = |p: &Play, i: usize| Duration::from_millis(p.delays[i] as u64);
                    let mut due = p.due.unwrap_or_else(|| now + delay(p, p.cur));
                    let mut steps = 0;
                    while now >= due && steps < n {
                        p.cur = (p.cur + 1) % n;
                        due += delay(p, p.cur);
                        steps += 1;
                    }
                    if now >= due {
                        due = now + delay(p, p.cur); // far behind (e.g. after a stall)
                    }
                    p.due = Some(due);
                    ctx.request_repaint_after(due - now);
                } else {
                    p.due = None;
                }
                let tex = if p.texs.len() == 1 && !p.frames.is_empty() {
                    if p.shown != p.cur {
                        p.texs[0].set_partial(
                            [0, 0],
                            p.frames[p.cur].clone(),
                            TextureOptions::LINEAR,
                        );
                        p.shown = p.cur;
                    }
                    &p.texs[0]
                } else {
                    &p.texs[p.cur.min(p.texs.len() - 1)]
                };
                Shown {
                    got: Got::Ready(Tex {
                        id: tex.id(),
                        size: p.size,
                    }),
                    badge: badge_paused,
                }
            }
            Some(State::Fallback { tex, size }) => Shown {
                got: Got::Ready(Tex {
                    id: tex.id(),
                    size: *size,
                }),
                badge: true,
            },
            Some(State::Loading { .. }) => none,
            Some(State::Failed { .. }) => Shown {
                got: Got::Failed,
                badge: false,
            },
            Some(State::NotAnimated) => Shown {
                got: Got::Failed,
                badge: false,
            },
            Some(State::Still) => match self.cached(id) {
                Some(g) => Shown {
                    got: g,
                    badge: false,
                },
                None => {
                    self.anim.map.remove(&id);
                    self.anim_start(req, now);
                    none
                }
            },
            None => {
                self.anim_start(req, now);
                none
            }
        }
    }

    fn anim_start(&mut self, req: Req<'_>, now: Instant) {
        let id = req.id;
        let (w, h) = req.px;
        let state = if self.demo {
            match req.demo {
                Some(kind) => {
                    let tx = self.anim.demo_tx.clone();
                    let ctx = self.ctx.clone();
                    let spawned = std::thread::Builder::new()
                        .name("nixcord-demo-anim".into())
                        .spawn(move || {
                            let (file, seed) = match &kind {
                                DemoAnim::Party(s, f) | DemoAnim::Emoji(s, f) => (f, *s),
                            };
                            let bytes = match file.as_ref().and_then(|f| std::fs::read(f).ok()) {
                                Some(b) => b,
                                None => match kind {
                                    DemoAnim::Party(..) => {
                                        demo_anim::party_gif(seed, w, h, DEMO_FRAMES, DEMO_DELAY_MS)
                                    }
                                    DemoAnim::Emoji(..) => demo_anim::emoji_gif(seed, w.min(h)),
                                },
                            };
                            let r =
                                nixcord_media::anim::decode_anim(&bytes, w, h, Budget::default())
                                    .map_err(|_| ());
                            let _ = tx.send((id, r));
                            ctx.request_repaint();
                        });
                    match spawned {
                        Ok(_) => State::Loading {
                            urls: Vec::new(),
                            cand: 0,
                            px: req.px,
                        },
                        Err(_) => State::Failed {
                            retry_at: now + Duration::from_secs(3600),
                        },
                    }
                }
                None => State::Failed {
                    retry_at: now + Duration::from_secs(3600),
                },
            }
        } else if req.urls.is_empty() || !self.ensure_fetcher() {
            State::Failed {
                retry_at: now + Duration::from_secs(3600),
            }
        } else {
            let key = Key::new(req.urls[0].clone(), w, h);
            self.anim.pending.insert(key.id(), id);
            if let Some(f) = &self.fetcher {
                f.request_anim(key);
            }
            State::Loading {
                urls: req.urls.to_vec(),
                cand: 0,
                px: req.px,
            }
        };
        self.anim.map.insert(
            id,
            A {
                state,
                seen: now,
                seen_pass: self.pass,
                viewer: req.viewer,
            },
        );
    }

    /// Current frame of an animated custom emoji `px` pixels square, if it is decoded
    /// (starting the load on a miss). `None` while loading: the caller shows the still.
    pub(super) fn anim_emoji(&mut self, id: u64, px: u32, hover: bool) -> Option<Tex> {
        let cdn = nixcord_media::size::cdn_size(px);
        let urls = anim_url::emoji_candidates(id, cdn);
        let shown = self.anim_get(Req {
            id: local_id(7, id, px, px),
            urls: &urls,
            px: (px, px),
            demo: Some(DemoAnim::Emoji(
                id,
                self.demo
                    .then(|| crate::demo_assets::emoji_file(id))
                    .flatten(),
            )),
            hover,
            viewer: false,
        });
        match shown.got {
            Got::Ready(t) => Some(t),
            _ => None,
        }
    }

    /// Animated avatar frame while hovered (`url` is the still CDN URL of an `a_` hash).
    pub(super) fn anim_avatar(&mut self, url: &str, px: u32) -> Option<Tex> {
        if self.demo {
            return None;
        }
        let anim = anim_url::animated_avatar(url);
        let urls = [anim];
        let shown = self.anim_get(Req {
            id: Key::new(urls[0].as_str(), px, px).id(),
            urls: &urls,
            px: (px, px),
            demo: None,
            hover: true,
            viewer: false,
        });
        match shown.got {
            Got::Ready(t) => Some(t),
            _ => None,
        }
    }

    /// Viewer textures for an animated image, or `None` to use the still path.
    pub(super) fn viewer_anim(
        &mut self,
        img: &super::ViewImage,
        bw: u32,
        bh: u32,
    ) -> Option<super::ViewTex> {
        if img.alt.is_empty() && !anim_url::maybe_animated(&img.url) {
            return None;
        }
        // The chat preview is known to be a still: nothing to animate.
        if matches!(
            self.anim.map.get(&img.preview_id).map(|a| &a.state),
            Some(State::Still)
        ) {
            return None;
        }
        let seed = murl::fnv1a64(img.url.as_bytes());
        let urls = if img.alt.is_empty() {
            anim_url::image_candidates(&img.url, bw, bh)
        } else {
            img.alt.clone()
        };
        let id = local_id(6, seed, bw, bh);
        let shown = self.anim_get(Req {
            id,
            urls: &urls,
            px: (bw, bh),
            demo: Some(DemoAnim::Party(seed, self.demo_url_file(&img.url))),
            hover: true,
            viewer: true,
        });
        if matches!(
            self.anim.map.get(&id).map(|a| &a.state),
            Some(State::NotAnimated)
        ) {
            return None;
        }
        let preview =
            self.anim_peek(img.preview_id)
                .or_else(|| match self.cached(img.preview_id) {
                    Some(Got::Ready(t)) => Some(t),
                    _ => None,
                });
        Some(match shown.got {
            Got::Ready(t) => super::ViewTex {
                big: Some(t),
                preview,
                failed: false,
            },
            Got::Loading => super::ViewTex {
                big: None,
                preview,
                failed: false,
            },
            Got::Failed => super::ViewTex {
                big: None,
                preview,
                failed: true,
            },
        })
    }

    /// Texture showing an animation's current frame, without advancing or loading it.
    fn anim_peek(&self, id: u64) -> Option<Tex> {
        match &self.anim.map.get(&id)?.state {
            State::Ready(p) => {
                let i = if p.texs.len() == 1 {
                    0
                } else {
                    p.cur.min(p.texs.len() - 1)
                };
                Some(Tex {
                    id: p.texs[i].id(),
                    size: p.size,
                })
            }
            State::Fallback { tex, size } => Some(Tex {
                id: tex.id(),
                size: *size,
            }),
            _ => None,
        }
    }

    pub(super) fn clear_anims(&mut self) {
        self.anim
            .map
            .retain(|_, a| matches!(a.state, State::Loading { .. }));
        self.anim.lru = ByteLru::new();
    }

    /// Video poster: `poster` drawn like an image (no viewer), a play button on top; a click
    /// opens `open` in the browser. For mp4/webm attachments and video embeds.
    pub fn video_poster(
        &mut self,
        ui: &mut Ui,
        poster: Option<&str>,
        open: &str,
        max_size: Vec2,
        natural: Option<(u32, u32)>,
    ) -> egui::Response {
        let (w, h) = nixcord_media::size::fit_points(
            natural
                .map(|(w, h)| (w as f32, h as f32))
                .unwrap_or((max_size.x, max_size.x * 9.0 / 16.0)),
            (max_size.x.max(1.0), max_size.y.max(1.0)),
        );
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::click());
        let rounding = CornerRadius::same(super::IMAGE_RADIUS);
        let faint = ui.visuals().faint_bg_color;
        if ui.is_rect_visible(rect) {
            let ppp = self.ctx.pixels_per_point();
            let (pw, ph) = (
                nixcord_media::size::to_px(w, ppp),
                nixcord_media::size::to_px(h, ppp),
            );
            let got = match poster {
                Some(p) if !self.demo => self.net(Key::new(murl::sized_url(p, pw, ph), pw, ph)),
                _ => Got::Failed,
            };
            match got {
                Got::Ready(t) => super::paint_texture(ui, rect, rounding, t, true),
                _ => {
                    ui.painter().rect_filled(rect, rounding, faint);
                }
            }
            let c = rect.center();
            ui.painter()
                .circle_filled(c, 22.0, Color32::from_black_alpha(160));
            ui.painter().add(Shape::convex_polygon(
                vec![
                    c + Vec2::new(-7.0, -10.0),
                    c + Vec2::new(11.0, 0.0),
                    c + Vec2::new(-7.0, 10.0),
                ],
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(open));
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Open video in browser")
    }
}

/// Discord's small "GIF" tag in the top-left corner of a paused / too-large animation.
pub(super) fn paint_badge(ui: &Ui, rect: Rect) {
    if rect.width() < 36.0 || rect.height() < 22.0 {
        return;
    }
    let font = crate::ui::bold(11.0);
    let r = Rect::from_min_size(rect.min + Vec2::new(6.0, 6.0), Vec2::new(30.0, 17.0));
    ui.painter()
        .rect_filled(r, CornerRadius::same(4), Color32::from_black_alpha(170));
    ui.painter().text(
        r.center(),
        Align2::CENTER_CENTER,
        "GIF",
        font,
        Color32::WHITE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ctx: &egui::Context, f: impl FnMut(&mut Ui)) {
        ctx.run_ui(egui::RawInput::default(), f)
            .textures_delta
            .clear();
    }

    /// Draw one demo GIF until it is decoded; returns the media.
    fn loaded(ctx: &egui::Context) -> Media {
        let mut media = Media::new(ctx, true);
        for _ in 0..500 {
            let mut ready = false;
            run(ctx, |ui| {
                let r = media.image(
                    ui,
                    "https://example.com/party.gif",
                    Vec2::new(64.0, 64.0),
                    Some((64, 36)),
                );
                ready = media.anim_stats().0 == 1;
                let _ = r;
            });
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(media.anim_stats().0, 1, "demo gif never decoded");
        media
    }

    #[test]
    fn demo_gif_plays_and_is_freed_off_screen() {
        let ctx = egui::Context::default();
        let mut media = loaded(&ctx);
        let (_, bytes) = media.anim_stats();
        // 20 frames + 1 texture of 64x36 RGBA.
        assert_eq!(bytes, 21 * 64 * 36 * 4);
        // Not in the still LRU.
        assert_eq!(media.stats().1, 0);
        // Frames advance with time.
        let frame = |m: &Media| match &m.anim.map.values().next().unwrap().state {
            State::Ready(p) => p.cur,
            _ => usize::MAX,
        };
        let mut seen = std::collections::HashSet::new();
        for _ in 0..12 {
            run(&ctx, |ui| {
                media.image(
                    ui,
                    "https://example.com/party.gif",
                    Vec2::new(64.0, 64.0),
                    Some((64, 36)),
                );
            });
            seen.insert(frame(&media));
            std::thread::sleep(Duration::from_millis(DEMO_DELAY_MS as u64 / 2));
        }
        assert!(seen.len() >= 3, "frames seen: {seen:?}");
        let draw = |media: &mut Media| {
            run(&ctx, |ui| {
                media.image(
                    ui,
                    "https://example.com/party.gif",
                    Vec2::new(64.0, 64.0),
                    Some((64, 36)),
                );
            })
        };
        // Still on screen but the app was idle (no passes) for longer than the grace
        // period: kept.
        std::thread::sleep(GRACE + Duration::from_millis(50));
        draw(&mut media);
        assert_eq!(media.anim_stats().0, 1);
        // Off screen (passes without it) for longer than the grace period: dropped.
        run(&ctx, |_| {});
        std::thread::sleep(GRACE + Duration::from_millis(50));
        run(&ctx, |_| {
            media.begin_pass();
        });
        assert_eq!(media.anim_stats(), (0, 0));
    }

    #[test]
    fn autoplay_off_pauses_with_badge() {
        let ctx = egui::Context::default();
        let mut media = loaded(&ctx);
        media.set_autoplay(false);
        let id = *media.anim.map.keys().next().unwrap();
        let mut badges = Vec::new();
        for _ in 0..6 {
            run(&ctx, |_| {
                let s = media.anim_get(Req {
                    id,
                    urls: &[],
                    px: (64, 36),
                    demo: None,
                    hover: false,
                    viewer: false,
                });
                badges.push(s.badge);
            });
            std::thread::sleep(Duration::from_millis(DEMO_DELAY_MS as u64));
        }
        assert!(badges.iter().all(|b| *b));
        let State::Ready(p) = &media.anim.map[&id].state else {
            panic!()
        };
        assert_eq!(p.cur, 0, "paused animation advanced");
        assert!(p.due.is_none());
    }
}
