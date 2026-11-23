//! Gradient themes: a two-stop background gradient painted as a `Mesh` behind
//! semi-transparent panels, plus panel tints, an accent and text colors.

use eframe::egui::{self, Color32, CornerRadius, Mesh, Pos2, Rect, Shadow, Shape, Stroke};
use std::sync::OnceLock;

pub const DEFAULT_THEME: &str = "Graphite";

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    /// Background gradient, top-left -> bottom-right.
    pub bg_top: Color32,
    pub bg_bottom: Color32,
    /// Panel fills (premultiplied, usually translucent so the gradient shows through).
    pub rail: Color32,
    pub sidebar: Color32,
    pub chat: Color32,
    pub header: Color32,
    /// Inputs, user panel, composer.
    pub elevated: Color32,
    pub hover: Color32,
    pub selected: Color32,
    pub divider: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub strong: Color32,
    pub link: Color32,
    pub mention_bg: Color32,
    pub mention_bar: Color32,
    pub danger: Color32,
    pub online: Color32,
    pub idle: Color32,
}

const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn alpha(c: u32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied((c >> 16) as u8, (c >> 8) as u8, c as u8, a)
}

/// Compact constructor: everything not given derives from the panel tint and text color.
struct Spec {
    name: &'static str,
    bg: (u32, u32),
    tint: u32,
    /// Panel opacity: rail, sidebar, chat.
    opacity: (u8, u8, u8),
    elevated: u32,
    accent: u32,
    text: u32,
    muted: u32,
    strong: u32,
    link: u32,
}

impl Spec {
    fn build(self) -> Theme {
        let (ra, sa, ca) = self.opacity;
        let dark = luminance(hex(self.bg.0)) < 0.5;
        let fg = if dark { 0xffffff } else { 0x000000 };
        Theme {
            name: self.name,
            dark,
            bg_top: hex(self.bg.0),
            bg_bottom: hex(self.bg.1),
            rail: alpha(self.tint, ra),
            sidebar: alpha(self.tint, sa),
            chat: alpha(self.tint, ca),
            header: alpha(self.tint, ca.saturating_add(20)),
            elevated: alpha(self.elevated, 235),
            hover: alpha(fg, if dark { 14 } else { 18 }),
            selected: alpha(fg, if dark { 26 } else { 30 }),
            divider: alpha(fg, if dark { 16 } else { 28 }),
            accent: hex(self.accent),
            on_accent: if luminance(hex(self.accent)) > 0.6 {
                hex(0x111111)
            } else {
                hex(0xffffff)
            },
            text: hex(self.text),
            muted: hex(self.muted),
            strong: hex(self.strong),
            link: hex(self.link),
            mention_bg: alpha(0xf0b232, if dark { 20 } else { 40 }),
            mention_bar: hex(0xf0b232),
            danger: hex(0xf23f43),
            online: hex(0x23a55a),
            idle: hex(0xf0b232),
        }
    }
}

fn luminance(c: Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

pub fn builtin() -> &'static [Theme] {
    static THEMES: OnceLock<Vec<Theme>> = OnceLock::new();
    THEMES.get_or_init(|| {
        let s = |name, bg, tint, opacity, elevated, accent, text, muted, strong, link| {
            Spec {
                name,
                bg,
                tint,
                opacity,
                elevated,
                accent,
                text,
                muted,
                strong,
                link,
            }
            .build()
        };
        vec![
            graphite(s(
                "Graphite",
                (0x1a1a1e, 0x1a1a1e),
                0x121214,
                (255, 255, 0),
                0x222327,
                0x5865f2,
                0xdbdee1,
                0x949ba4,
                0xf2f3f5,
                0x00a8fc,
            )),
            onyx(s(
                "Onyx",
                (0x000000, 0x000000),
                0x000000,
                (255, 255, 255),
                0x111214,
                0x5865f2,
                0xdbdee1,
                0x949ba4,
                0xf2f3f5,
                0x00a8fc,
            )),
            s(
                "Midnight",
                (0x0d1024, 0x1e2347),
                0x080a1a,
                (210, 150, 40),
                0x1a1f3d,
                0x7c8cff,
                0xd6dbf5,
                0x8a91b8,
                0xf2f4ff,
                0x8fb4ff,
            ),
            s(
                "Aurora",
                (0x071a26, 0x1b4d43),
                0x04121a,
                (200, 140, 40),
                0x12303a,
                0x3ddc97,
                0xd5efe8,
                0x86aba3,
                0xf0fffa,
                0x5ee0c8,
            ),
            s(
                "Sunset",
                (0x2a1033, 0x7a2e3b),
                0x1a0a20,
                (200, 140, 40),
                0x3a1b3d,
                0xff8a5b,
                0xf6dfe0,
                0xc29aa6,
                0xfff3f0,
                0xffb38a,
            ),
            s(
                "Ocean",
                (0x061d33, 0x0e5577),
                0x03111f,
                (200, 140, 40),
                0x0f2e48,
                0x38bdf8,
                0xd5e9f7,
                0x86a7bf,
                0xf2fbff,
                0x67d2ff,
            ),
            s(
                "Forest",
                (0x0c1a12, 0x2a4a30),
                0x07110b,
                (205, 145, 40),
                0x173022,
                0x7ccf6b,
                0xdcebd9,
                0x93ab91,
                0xf4fff1,
                0xa8e39a,
            ),
            s(
                "Sakura",
                (0x2a1824, 0x5a2f48),
                0x1a0e16,
                (200, 140, 40),
                0x3a2232,
                0xff9ccf,
                0xf5e1ec,
                0xc6a0b5,
                0xfff4fa,
                0xffb8dc,
            ),
            s(
                "Dracula",
                (0x282a36, 0x1e1f29),
                0x191a21,
                (240, 200, 30),
                0x343746,
                0xbd93f9,
                0xf8f8f2,
                0x9ea3c0,
                0xffffff,
                0x8be9fd,
            ),
            s(
                "Nord",
                (0x2e3440, 0x3b4252),
                0x242933,
                (240, 190, 30),
                0x3b4252,
                0x88c0d0,
                0xe5e9f0,
                0xa3acbd,
                0xeceff4,
                0x81a1c1,
            ),
            s(
                "Mono",
                (0x121212, 0x262626),
                0x0a0a0a,
                (230, 170, 30),
                0x242424,
                0xd0d0d0,
                0xdedede,
                0x8f8f8f,
                0xffffff,
                0xbdbdbd,
            ),
            amoled(s(
                "AMOLED",
                (0x000000, 0x000000),
                0x000000,
                (255, 255, 255),
                0x101013,
                0x5865f2,
                0xdbdee1,
                0x949ba4,
                0xf2f3f5,
                0x00a8fc,
            )),
            s(
                "Synthwave",
                (0x1a0b2e, 0x4a0f52),
                0x10061d,
                (200, 140, 40),
                0x2a1442,
                0xff2fd1,
                0xf3dcff,
                0xb08fc7,
                0xffffff,
                0x2de2e6,
            ),
            s(
                "Coffee",
                (0x1e1612, 0x3e2c22),
                0x140e0b,
                (215, 160, 40),
                0x2e221c,
                0xd4a373,
                0xefe2d6,
                0xab9583,
                0xfff8f0,
                0xe9c194,
            ),
            s(
                "Paper",
                (0xf4f1ea, 0xe6dfd2),
                0xebe6dc,
                (240, 200, 60),
                0xffffff,
                0x4f6bd8,
                0x2b2b2b,
                0x6f6a62,
                0x111111,
                0x2f5fd0,
            ),
        ]
    })
}

pub fn by_name(name: &str) -> &'static Theme {
    let all = builtin();
    all.iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .unwrap_or(&all[0])
}

/// Push the theme into egui's visuals.
pub fn apply(ctx: &egui::Context, t: &Theme) {
    let egui_theme = if t.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(egui_theme);
    let mut v = if t.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let r4 = CornerRadius::same(4);
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = t.elevated.to_opaque();
    v.window_stroke = Stroke::new(1.0, t.divider);
    v.window_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    v.menu_corner_radius = r4;
    v.extreme_bg_color = t.elevated;
    v.text_edit_bg_color = Some(t.elevated);
    v.faint_bg_color = t.hover;
    v.code_bg_color = t.hover;
    v.hyperlink_color = t.link;
    v.weak_text_color = Some(t.muted);
    v.selection.bg_fill = t.accent.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, t.strong);

    let w = &mut v.widgets;
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.divider);
    w.noninteractive.bg_fill = t.elevated;
    w.noninteractive.weak_bg_fill = t.elevated;
    w.noninteractive.corner_radius = r4;
    for (wv, fill) in [
        (&mut w.inactive, t.hover),
        (&mut w.hovered, t.selected),
        (&mut w.active, t.selected),
        (&mut w.open, t.selected),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.corner_radius = r4;
        wv.bg_stroke = Stroke::NONE;
        wv.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0, t.text);
    w.hovered.fg_stroke = Stroke::new(1.5, t.strong);
    w.active.fg_stroke = Stroke::new(1.5, t.strong);
    w.open.fg_stroke = Stroke::new(1.0, t.strong);

    ctx.set_visuals_of(egui_theme, v);
}

/// Paint the gradient across the whole window on the background layer.
pub fn paint_background(ctx: &egui::Context, t: &Theme) {
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::background());
    if t.bg_top == t.bg_bottom {
        painter.rect_filled(rect, 0.0, t.bg_top);
        return;
    }
    painter.add(Shape::mesh(gradient_mesh(rect, t.bg_top, t.bg_bottom)));
}

/// Diagonal two-stop gradient (top-left = `a`, bottom-right = `b`).
pub fn gradient_mesh(rect: Rect, a: Color32, b: Color32) -> Mesh {
    let mid = a.lerp_to_gamma(b, 0.5);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), a);
    mesh.colored_vertex(rect.right_top(), mid);
    mesh.colored_vertex(rect.right_bottom(), b);
    mesh.colored_vertex(Pos2::new(rect.left(), rect.bottom()), mid);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    mesh
}

/// The default theme mirrors the official dark client: flat near-black surfaces.
fn graphite(mut t: Theme) -> Theme {
    t.rail = hex(0x121214);
    t.sidebar = hex(0x19191c);
    t.chat = hex(0x1a1a1e);
    t.header = hex(0x1a1a1e);
    t.elevated = hex(0x222327);
    t
}

/// Pure black with hairline borders, like the official client's darkest theme.
fn amoled(mut t: Theme) -> Theme {
    t.hover = hex(0x121213);
    t.selected = hex(0x242426);
    t.divider = hex(0x1e1e20);
    t
}

/// Pure black everywhere, like the official client's darkest theme (hairline borders,
/// faintly lifted input and selection).
fn onyx(mut t: Theme) -> Theme {
    t.rail = hex(0x000000);
    t.sidebar = hex(0x0a0a0b);
    t.chat = hex(0x000000);
    t.header = hex(0x000000);
    t.elevated = hex(0x111214);
    t.hover = hex(0x121213);
    t.selected = hex(0x242426);
    t.divider = hex(0x1e1e20);
    t
}
