//! Images in the UI: avatars, server icons, custom emoji, attachment/embed previews.
//!
//! # Usage (for the UI integration pass)
//!
//! Own one `Media` in `NixApp` (`media: Media::new(&cc.egui_ctx, demo)`), pass `&mut app.media`
//! down to the screens. Nothing else is needed: the first helper call of each frame drains
//! finished downloads into textures and evicts least-recently-used textures over the byte
//! cap. Requests are fire-and-forget; when an image arrives the UI is repainted. Every
//! helper draws a fallback while loading / on failure, and only requests images whose rect
//! is visible (`ui.is_rect_visible`), so off-screen rows of the chat list cost nothing.
//!
//! - `media.avatar(ui, &user, guild, 40.0)` allocates a 40x40 circle and returns its
//!   `Response`. `guild` is `Some((guild_id, member.avatar.as_deref()))` for guild messages
//!   (a per-guild avatar wins over the global one) or `None` for DMs. Users without an avatar
//!   hash keep the colored initials circle (`ui::paint_avatar`); no default-avatar fetch.
//!   - `media.paint_avatar(ui, center, 40.0, &user, guild)` is the drop-in replacement for
//!     `ui::paint_avatar(ui, center, 40.0, &user)` when the rect is already allocated.
//! - `media.paint_guild_icon(ui, rect, &guild, rounding) -> bool` paints the server icon into
//!   an allocated rect with the given rounding (the rail passes its animated circle ->
//!   squircle radius). It draws NO fallback and returns `false` when there is no image
//!   (yet), so the rail keeps painting its own hover/selected fill + initials.
//! - `media.emoji(ui, emoji_id, animated, 22.0)` for custom emoji (reactions, inline
//!   `<:name:id>`). Animated emoji show their first frame. Fallback: a faint rounded square.
//! - `media.image(ui, &url, Vec2::new(400.0, 300.0), Some((w, h)))` for attachments
//!   (`a.url`, `a.width.zip(a.height)`) and embed images/thumbnails (prefer `proxy_url`,
//!   `media.width.zip(media.height)`). Aspect-correct inside `max_size`, never upscaled,
//!   rounded corners; a click opens the original URL in the browser. Pass `natural` whenever
//!   known so the row height is stable before the image arrives (the chat list caches row
//!   heights). Discord attachment / embed-proxy URLs are fetched pre-scaled by the media
//!   proxy, so we download about as many pixels as we show.
//! - `media.clear()` on logout drops every texture; `media.stats()` -> `(textures, bytes)`.
//!
//! Demo mode (`Media::new(ctx, true)`) never touches the network: avatars, icons, emoji and
//! attachment "screenshots" are generated locally by `nixcord_media::demo`, seeded by the
//! user/guild/emoji id or a hash of the URL.

use eframe::egui::{
    self, epaint::RectShape, Align2, Color32, CornerRadius, Pos2, Rect, Response, Sense,
    TextureHandle, TextureId, TextureOptions, Ui, Vec2,
};
use nixcord_media::{demo, lru::ByteLru, size, url as murl, Fetcher, Image, Key};
use nixcord_model::{cdn, Guild, Snowflake, User};
use std::collections::HashMap;
use std::time::Instant;

/// Total RGBA bytes of textures kept around (visible ones are never evicted).
const TEXTURE_CAP: usize = 24 * 1024 * 1024;
/// Texture uploads per frame from finished downloads (the rest wait a frame).
const UPLOADS_PER_FRAME: usize = 16;
/// Pixels generated per frame in demo mode (at least one image is always generated).
const DEMO_PIXELS_PER_FRAME: u64 = 600_000;
/// Corner radius of attachment previews.
const IMAGE_RADIUS: u8 = 6;

enum Entry {
    Loading,
    Ready { tex: TextureHandle, size: [u32; 2] },
    Failed { retry_at: Instant },
}

/// A texture ready to paint.
#[derive(Clone, Copy)]
struct Tex {
    id: TextureId,
    size: [u32; 2],
}

/// State of one image request this frame.
enum Got {
    Ready(Tex),
    Loading,
    Failed,
}

pub struct Media {
    ctx: egui::Context,
    demo: bool,
    /// Started on the first network request (keeps RAM and threads at zero until needed).
    fetcher: Option<Fetcher>,
    net_broken: bool,
    entries: HashMap<u64, Entry>,
    lru: ByteLru<u64>,
    pass: u64,
    demo_budget: u64,
}

impl Media {
    pub fn new(ctx: &egui::Context, demo: bool) -> Self {
        Self {
            ctx: ctx.clone(),
            demo,
            fetcher: None,
            net_broken: false,
            entries: HashMap::new(),
            lru: ByteLru::new(),
            pass: u64::MAX,
            demo_budget: 0,
        }
    }

    /// `(texture count, RGBA bytes)` currently held.
    pub fn stats(&self) -> (usize, usize) {
        (self.lru.len(), self.lru.total_bytes())
    }

    /// Drop all textures and remembered failures (e.g. on logout).
    pub fn clear(&mut self) {
        self.entries.retain(|_, e| matches!(e, Entry::Loading));
        self.lru = ByteLru::new();
    }

    // ---- helpers ----------------------------------------------------------------------

    /// Circle avatar of `size` points. See the module docs for `guild`.
    pub fn avatar(
        &mut self,
        ui: &mut Ui,
        user: &User,
        guild: Option<(Snowflake, Option<&str>)>,
        size: f32,
    ) -> Response {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        self.paint_avatar(ui, rect.center(), size, user, guild);
        resp
    }

    /// Paint a circle avatar into an already allocated area (drop-in for `ui::paint_avatar`).
    pub fn paint_avatar(
        &mut self,
        ui: &Ui,
        center: Pos2,
        diameter: f32,
        user: &User,
        guild: Option<(Snowflake, Option<&str>)>,
    ) {
        let rect = Rect::from_center_size(center, Vec2::splat(diameter));
        if ui.is_rect_visible(rect) {
            let px = self.px(diameter);
            let got = if self.demo {
                self.local(
                    local_id(1, user.id.0, px, px),
                    px as u64 * px as u64,
                    || demo::avatar(user.id.0, px),
                )
            } else {
                let member = guild.and_then(|(g, h)| h.map(|h| (g, h)));
                let url = match (member, user.avatar.as_deref()) {
                    (Some((g, h)), _) => {
                        Some(cdn::member_avatar(g, user.id, h, size::cdn_size(px)))
                    }
                    (None, Some(h)) => Some(cdn::avatar(user.id, Some(h), size::cdn_size(px))),
                    (None, None) => None,
                };
                match url {
                    Some(url) => self.net(Key::new(url, px, px)),
                    None => Got::Failed,
                }
            };
            if let Got::Ready(t) = got {
                paint_texture(ui, rect, radius(diameter / 2.0), t, true);
                return;
            }
        }
        crate::ui::paint_avatar(ui, center, diameter, user);
    }

    /// Paint the server icon into `rect`. Draws nothing and returns `false` if there is no
    /// icon or it is not loaded (yet), so the caller can draw its own fallback.
    pub fn paint_guild_icon(
        &mut self,
        ui: &Ui,
        rect: Rect,
        guild: &Guild,
        rounding: CornerRadius,
    ) -> bool {
        if !ui.is_rect_visible(rect) {
            return false;
        }
        let px = self.px(rect.width().max(rect.height()));
        let got = if self.demo {
            self.local(
                local_id(2, guild.id.0, px, px),
                px as u64 * px as u64,
                || demo::guild_icon(guild.id.0, px),
            )
        } else {
            match guild.icon.as_deref() {
                Some(h) => self.net(Key::new(
                    cdn::guild_icon(guild.id, h, size::cdn_size(px)),
                    px,
                    px,
                )),
                None => Got::Failed,
            }
        };
        match got {
            Got::Ready(t) => {
                paint_texture(ui, rect, rounding, t, true);
                true
            }
            _ => false,
        }
    }

    /// Custom emoji in a `size` x `size` box (aspect kept, centered).
    pub fn emoji(&mut self, ui: &mut Ui, id: Snowflake, animated: bool, size: f32) -> Response {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
        if ui.is_rect_visible(rect) {
            let px = self.px(size);
            let got = if self.demo {
                self.local(local_id(3, id.0, px, px), px as u64 * px as u64, || {
                    demo::emoji(id.0, px)
                })
            } else {
                self.net(Key::new(
                    cdn::emoji(id, animated, size::cdn_size(px)),
                    px,
                    px,
                ))
            };
            match got {
                Got::Ready(t) => {
                    // Contain: scale the texture (in pixels) to fit the box (in points).
                    let (tw, th) = (t.size[0].max(1) as f32, t.size[1].max(1) as f32);
                    let k = (rect.width() / tw).min(rect.height() / th);
                    let r = Rect::from_center_size(rect.center(), Vec2::new(tw * k, th * k));
                    paint_texture(ui, r, CornerRadius::ZERO, t, false);
                }
                _ => {
                    ui.painter().rect_filled(
                        rect.shrink(size * 0.08),
                        radius(size * 0.2),
                        ui.visuals().faint_bg_color,
                    );
                }
            }
        }
        resp
    }

    /// Texture of a custom emoji drawn `size` points square, if loaded (starts the load on a
    /// miss). Backs the markdown renderer's `EmojiImages` hook.
    pub fn emoji_texture(&mut self, id: u64, animated: bool, size: f32) -> Option<TextureId> {
        let px = self.px(size);
        let got = if self.demo {
            self.local(local_id(3, id, px, px), px as u64 * px as u64, || {
                demo::emoji(id, px)
            })
        } else {
            self.net(Key::new(
                cdn::emoji(Snowflake(id), animated, size::cdn_size(px)),
                px,
                px,
            ))
        };
        match got {
            Got::Ready(t) => Some(t.id),
            _ => None,
        }
    }
    /// Attachment / embed image preview. See the module docs.
    pub fn image(
        &mut self,
        ui: &mut Ui,
        url: &str,
        max_size: Vec2,
        natural: Option<(u32, u32)>,
    ) -> Response {
        let ppp = self.ctx.pixels_per_point();
        let max = (max_size.x.max(1.0), max_size.y.max(1.0));
        // Size we lay out at; with no natural size, a 16:9 box until the image arrives.
        let fallback = (max.0, max.0 * 9.0 / 16.0);
        let (w, h) = size::fit_points(
            natural
                .map(|(w, h)| (w as f32, h as f32))
                .unwrap_or(fallback),
            max,
        );
        let (pw, ph) = (size::to_px(w, ppp), size::to_px(h, ppp));
        let id_and_got = |this: &mut Self| -> Got {
            if this.demo {
                let seed = murl::fnv1a64(url.as_bytes());
                this.local(local_id(4, seed, pw, ph), pw as u64 * ph as u64, || {
                    demo::screenshot(seed, pw, ph)
                })
            } else if natural.is_some() {
                this.net(Key::new(murl::sized_url(url, pw, ph), pw, ph))
            } else {
                // Unknown size: decode into the whole max box, then lay out by its aspect.
                let (mw, mh) = (size::to_px(max.0, ppp), size::to_px(max.1, ppp));
                this.net(Key::new(url, mw, mh))
            }
        };

        // Visible check needs a rect, but the rect for unknown sizes depends on the texture:
        // peek at the would-be rect first.
        let probe = Rect::from_min_size(ui.cursor().min, Vec2::new(w, h));
        let got = if ui.is_rect_visible(probe) {
            id_and_got(self)
        } else {
            Got::Loading
        };
        let shown = match (&got, natural) {
            (Got::Ready(t), None) => {
                let (tw, th) =
                    size::fit_points((t.size[0] as f32 / ppp, t.size[1] as f32 / ppp), max);
                Vec2::new(tw, th)
            }
            _ => Vec2::new(w, h),
        };
        let (rect, resp) = ui.allocate_exact_size(shown, Sense::click());
        let rounding = CornerRadius::same(IMAGE_RADIUS);
        let v = ui.visuals();
        match got {
            Got::Ready(t) => paint_texture(ui, rect, rounding, t, false),
            Got::Loading => {
                ui.painter().rect_filled(rect, rounding, v.faint_bg_color);
            }
            Got::Failed => {
                ui.painter().rect_filled(rect, rounding, v.faint_bg_color);
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "Image unavailable",
                    crate::ui::prop(12.0),
                    v.weak_text_color(),
                );
            }
        }
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    // ---- cache ------------------------------------------------------------------------

    fn px(&self, points: f32) -> u32 {
        size::to_px(points, self.ctx.pixels_per_point())
    }

    /// Once per frame: upload finished downloads, evict textures over the cap.
    fn begin_pass(&mut self) {
        let pass = self.ctx.cumulative_pass_nr();
        if pass == self.pass {
            return;
        }
        self.pass = pass;
        self.demo_budget = DEMO_PIXELS_PER_FRAME;

        if let Some(f) = &self.fetcher {
            let mut loaded = Vec::new();
            while loaded.len() < UPLOADS_PER_FRAME {
                match f.try_recv() {
                    Some(l) => loaded.push(l),
                    None => break,
                }
            }
            if loaded.len() == UPLOADS_PER_FRAME {
                self.ctx.request_repaint(); // more are waiting
            }
            for l in loaded {
                let id = l.key.id();
                match l.result {
                    Ok(img) => {
                        self.upload(id, img);
                    }
                    Err(f) => {
                        self.entries.insert(
                            id,
                            Entry::Failed {
                                retry_at: Instant::now() + f.retry_after(),
                            },
                        );
                    }
                }
            }
        }

        // Anything drawn last frame (or this one) is on screen: keep it.
        for id in self.lru.evict_over(TEXTURE_CAP, pass.saturating_sub(1)) {
            self.entries.remove(&id);
        }
        if self.entries.len() > 4096 {
            let now = Instant::now();
            self.entries
                .retain(|_, e| !matches!(e, Entry::Failed { retry_at } if *retry_at <= now));
        }
    }

    fn upload(&mut self, id: u64, img: Image) -> Tex {
        let size = [img.width, img.height];
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width as usize, img.height as usize],
            &img.rgba,
        );
        drop(img);
        let tex = self
            .ctx
            .load_texture(format!("media-{id:x}"), color, TextureOptions::LINEAR);
        let t = Tex { id: tex.id(), size };
        self.lru
            .insert(id, size::rgba_bytes(size[0], size[1]), self.pass);
        self.entries.insert(id, Entry::Ready { tex, size });
        t
    }

    /// Look up `id`; `Some(got)` if the entry decides the outcome.
    fn cached(&mut self, id: u64) -> Option<Got> {
        match self.entries.get(&id)? {
            Entry::Ready { tex, size } => {
                let t = Tex {
                    id: tex.id(),
                    size: *size,
                };
                self.lru.touch(&id, self.pass);
                Some(Got::Ready(t))
            }
            Entry::Loading => Some(Got::Loading),
            Entry::Failed { retry_at } if Instant::now() < *retry_at => Some(Got::Failed),
            Entry::Failed { .. } => None, // retry
        }
    }

    /// Network image (never used in demo mode).
    fn net(&mut self, key: Key) -> Got {
        self.begin_pass();
        let id = key.id();
        if let Some(g) = self.cached(id) {
            return g;
        }
        if self.demo || self.net_broken {
            return Got::Failed;
        }
        if self.fetcher.is_none() {
            let ctx = self.ctx.clone();
            match Fetcher::spawn(nixcord_media::Config::default(), move || {
                ctx.request_repaint()
            }) {
                Ok(f) => self.fetcher = Some(f),
                Err(e) => {
                    tracing::error!("media fetcher failed to start: {e}");
                    self.net_broken = true;
                    return Got::Failed;
                }
            }
        }
        if let Some(f) = &self.fetcher {
            f.request(key);
        }
        self.entries.insert(id, Entry::Loading);
        Got::Loading
    }

    /// Locally generated image (demo mode), rate-limited by pixels per frame.
    fn local(&mut self, id: u64, pixels: u64, make: impl FnOnce() -> Image) -> Got {
        self.begin_pass();
        if let Some(g) = self.cached(id) {
            return g;
        }
        if self.demo_budget == 0 {
            self.ctx.request_repaint();
            return Got::Loading;
        }
        self.demo_budget = self.demo_budget.saturating_sub(pixels.max(1));
        Got::Ready(self.upload(id, make()))
    }
}

/// Map ids for generated images (kind-tagged so an avatar and an icon never collide).
fn local_id(kind: u8, seed: u64, w: u32, h: u32) -> u64 {
    murl::sized_id(&format!("demo:{kind}:{seed}"), w, h)
}

fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

/// Paint a texture into `rect` with rounded corners. `cover` crops the texture to fill the
/// rect's aspect (avatars, icons); otherwise it is stretched to the rect (already fitted).
fn paint_texture(ui: &Ui, rect: Rect, rounding: CornerRadius, t: Tex, cover: bool) {
    let uv = if cover {
        cover_uv(t.size, rect.size())
    } else {
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
    };
    ui.painter()
        .add(RectShape::filled(rect, rounding, Color32::WHITE).with_texture(t.id, uv));
}

/// UV rect that center-crops a `tex` sized texture to the aspect of `target`.
fn cover_uv(tex: [u32; 2], target: Vec2) -> Rect {
    let (tw, th) = (tex[0].max(1) as f32, tex[1].max(1) as f32);
    let ta = tw / th;
    let ra = (target.x / target.y.max(1e-3)).max(1e-3);
    let (u, v) = if ta > ra {
        (ra / ta, 1.0)
    } else {
        (1.0, ta / ra)
    };
    Rect::from_center_size(Pos2::new(0.5, 0.5), Vec2::new(u, v))
}

/// Custom emoji for the markdown renderer, backed by the shared `Media` cache.
pub struct MediaEmoji<'a>(pub &'a std::cell::RefCell<Media>);

impl crate::ui::markdown::EmojiImages for MediaEmoji<'_> {
    fn texture(&self, _: &egui::Context, id: u64, animated: bool, size: f32) -> Option<TextureId> {
        self.0
            .try_borrow_mut()
            .ok()?
            .emoji_texture(id, animated, size)
    }
}
