//! Settings: a Discord-style modal with a section list on the left (Appearance, Chat,
//! Account, About) and the page on the right. Themes are square swatches in a grid.

use super::{bold, prop, Action};
use crate::app::NixApp;
use crate::theme::{self, Theme};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Mesh, Pos2, Rect,
    RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    Appearance,
    Chat,
    Account,
    About,
}

const PAGES: [(Page, &str); 4] = [
    (Page::Appearance, "Appearance"),
    (Page::Chat, "Chat"),
    (Page::Account, "My Account"),
    (Page::About, "About"),
];

fn page_id() -> Id {
    Id::unique("nixcord-settings-page")
}

pub fn window(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    if !app.settings_open {
        return;
    }
    let t = app.theme;
    let mut page: Page = ctx.data(|d| d.get_temp(page_id())).unwrap_or_default();
    let screen = ctx.content_rect();
    let size = Vec2::new(
        (screen.width() - 80.0).clamp(560.0, 1000.0),
        (screen.height() - 80.0).clamp(420.0, 700.0),
    );
    let mut close = false;
    let resp = egui::Modal::new(Id::unique("settings-modal"))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            Frame::new()
                .fill(t.chat.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10)),
        )
        .show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            let full = ui.max_rect();
            let nav_rect = Rect::from_min_size(full.min, Vec2::new(232.0, full.height()));
            ui.painter().rect_filled(
                nav_rect,
                CornerRadius {
                    nw: 10,
                    sw: 10,
                    ne: 0,
                    se: 0,
                },
                t.sidebar.to_opaque(),
            );
            ui.painter().vline(
                nav_rect.right(),
                nav_rect.y_range(),
                Stroke::new(1.0, t.divider),
            );

            // Left: user header + sections.
            let mut nav = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(nav_rect.shrink2(Vec2::new(12.0, 16.0)))
                    .layout(Layout::top_down(Align::Min)),
            );
            if let Some(me) = app.store.me().cloned() {
                nav.horizontal(|ui| {
                    app.media.borrow_mut().avatar(ui, &me, None, 40.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(
                            RichText::new(me.display_name())
                                .font(bold(15.5))
                                .color(t.strong),
                        );
                        ui.label(RichText::new(&me.username).font(prop(13.0)).color(t.muted));
                    });
                });
                nav.add_space(14.0);
            }
            nav.label(
                RichText::new("User Settings")
                    .font(prop(13.0))
                    .color(t.muted),
            );
            nav.add_space(4.0);
            for (p, label) in PAGES {
                let (r, resp) =
                    nav.allocate_exact_size(Vec2::new(nav.available_width(), 34.0), Sense::click());
                let sel = p == page;
                if sel {
                    nav.painter()
                        .rect_filled(r, CornerRadius::same(6), t.selected);
                } else if resp.hovered() {
                    nav.painter().rect_filled(r, CornerRadius::same(6), t.hover);
                }
                nav.painter().text(
                    Pos2::new(r.left() + 10.0, r.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    prop(15.5),
                    if sel || resp.hovered() {
                        t.strong
                    } else {
                        t.text
                    },
                );
                if resp.clicked() {
                    page = p;
                }
            }
            nav.add_space(10.0);
            let (r, resp) =
                nav.allocate_exact_size(Vec2::new(nav.available_width(), 34.0), Sense::click());
            if resp.hovered() {
                nav.painter().rect_filled(r, CornerRadius::same(6), t.hover);
            }
            nav.painter().text(
                Pos2::new(r.left() + 10.0, r.center().y),
                Align2::LEFT_CENTER,
                "Log Out",
                prop(15.5),
                t.danger,
            );
            if resp
                .on_hover_text("Also forgets the remembered token")
                .clicked()
            {
                actions.push(Action::Logout);
            }

            // Right: header + page.
            let body_rect =
                Rect::from_min_max(Pos2::new(nav_rect.right() + 1.0, full.top()), full.max);
            let header = Rect::from_min_size(body_rect.min, Vec2::new(body_rect.width(), 52.0));
            ui.painter().text(
                Pos2::new(header.left() + 24.0, header.center().y),
                Align2::LEFT_CENTER,
                PAGES
                    .iter()
                    .find(|(p, _)| *p == page)
                    .map_or("", |(_, l)| l),
                bold(17.0),
                t.strong,
            );
            super::hline(ui, header, header.bottom(), t.divider);
            let close_rect = Rect::from_center_size(
                Pos2::new(header.right() - 28.0, header.center().y),
                Vec2::splat(30.0),
            );
            let close_resp = ui.interact(close_rect, Id::unique("settings-close"), Sense::click());
            if close_resp.hovered() {
                ui.painter()
                    .rect_filled(close_rect, CornerRadius::same(6), t.hover);
            }
            let c = close_rect.center();
            let s = 6.0;
            let stroke = Stroke::new(
                1.8,
                if close_resp.hovered() {
                    t.strong
                } else {
                    t.muted
                },
            );
            ui.painter()
                .line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], stroke);
            ui.painter()
                .line_segment([c + Vec2::new(-s, s), c + Vec2::new(s, -s)], stroke);
            if close_resp.on_hover_text("Close (Esc)").clicked() {
                close = true;
            }

            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(Rect::from_min_max(
                        Pos2::new(body_rect.left(), header.bottom() + 1.0),
                        body_rect.max,
                    ))
                    .layout(Layout::top_down(Align::Min)),
            );
            egui::ScrollArea::vertical()
                .id_salt("settings-body")
                .auto_shrink([false, false])
                .show(&mut body, |ui| {
                    Frame::new()
                        .inner_margin(Margin {
                            left: 24,
                            right: 24,
                            top: 20,
                            bottom: 24,
                        })
                        .show(ui, |ui| match page {
                            Page::Appearance => appearance(ui, app),
                            Page::Chat => chat_prefs(ui, app),
                            Page::Account => account(ui, app, actions),
                            Page::About => about(ui, app),
                        });
                });
        });
    ctx.data_mut(|d| d.insert_temp(page_id(), page));
    if close || resp.should_close() {
        app.settings_open = false;
    }
}

fn heading(ui: &mut Ui, t: &Theme, text: &str) {
    ui.label(RichText::new(text).font(bold(16.0)).color(t.strong));
    ui.add_space(8.0);
}

fn appearance(ui: &mut Ui, app: &mut NixApp) {
    let t = app.theme;
    heading(ui, t, "Theme");
    let (flat, gradient): (Vec<&'static Theme>, Vec<&'static Theme>) = theme::builtin()
        .iter()
        .partition(|th| th.bg_top == th.bg_bottom);
    ui.label(
        RichText::new("Default Themes")
            .font(bold(14.0))
            .color(t.text),
    );
    ui.add_space(6.0);
    swatches(ui, app, &flat);
    ui.add_space(18.0);
    ui.label(RichText::new("Color Themes").font(bold(14.0)).color(t.text));
    ui.label(
        RichText::new("Gradient backgrounds behind translucent panels.")
            .font(prop(13.0))
            .color(t.muted),
    );
    ui.add_space(6.0);
    swatches(ui, app, &gradient);
    ui.add_space(10.0);
    ui.label(
        RichText::new(format!("Current theme: {}", app.theme.name))
            .font(prop(13.0))
            .color(t.muted),
    );

    ui.add_space(24.0);
    heading(ui, t, "Zoom");
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 260.0;
        ui.add(
            egui::Slider::new(&mut app.prefs.zoom, 0.75..=1.75)
                .step_by(0.05)
                .fixed_decimals(2),
        );
    });
}

fn swatches(ui: &mut Ui, app: &mut NixApp, themes: &[&'static Theme]) {
    let cur = app.theme;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
        for th in themes {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::click());
            let selected = std::ptr::eq(*th, cur);
            paint_swatch(ui, rect, th);
            let stroke = if selected {
                Stroke::new(2.5, Color32::from_rgb(0x58, 0x65, 0xf2))
            } else if resp.hovered() {
                Stroke::new(1.5, cur.muted)
            } else {
                Stroke::new(1.0, cur.divider)
            };
            ui.painter()
                .rect_stroke(rect, CornerRadius::same(10), stroke, StrokeKind::Outside);
            if selected {
                // Check badge on the top-right corner.
                let c = rect.right_top() + Vec2::new(-2.0, 2.0);
                ui.painter()
                    .circle_filled(c, 9.0, Color32::from_rgb(0x58, 0x65, 0xf2));
                let s = Stroke::new(1.8, Color32::WHITE);
                ui.painter()
                    .line_segment([c + Vec2::new(-4.0, 0.0), c + Vec2::new(-1.0, 3.0)], s);
                ui.painter()
                    .line_segment([c + Vec2::new(-1.0, 3.0), c + Vec2::new(4.5, -3.0)], s);
            }
            if resp.on_hover_text(th.name).clicked() {
                app.prefs.theme = th.name.to_string();
            }
        }
    });
}

/// Rounded square filled with the theme's background (diagonal gradient) and a small
/// accent dot, like Discord's theme swatches.
fn paint_swatch(ui: &Ui, rect: Rect, th: &Theme) {
    let (a, b) = if th.bg_top == th.bg_bottom {
        (th.chat.to_opaque(), th.sidebar.to_opaque())
    } else {
        (th.bg_top, th.bg_bottom)
    };
    let a = if th.bg_top == th.bg_bottom && th.chat.a() == 0 {
        th.bg_top
    } else {
        a
    };
    ui.painter()
        .add(Shape::mesh(rounded_gradient(rect, 10.0, a, b)));
    ui.painter()
        .circle_filled(rect.right_bottom() - Vec2::splat(12.0), 5.0, th.accent);
}

/// Rounded rect as a triangle fan, colored along the top-left -> bottom-right diagonal.
fn rounded_gradient(rect: Rect, r: f32, a: Color32, b: Color32) -> Mesh {
    let color = |p: Pos2| {
        let k = ((p.x - rect.left()) / rect.width() + (p.y - rect.top()) / rect.height()) / 2.0;
        a.lerp_to_gamma(b, k.clamp(0.0, 1.0))
    };
    let mut mesh = Mesh::default();
    let c = rect.center();
    mesh.colored_vertex(c, color(c));
    let corners = [
        (rect.right_bottom() - Vec2::splat(r), 0.0_f32),
        (Pos2::new(rect.left() + r, rect.bottom() - r), 90.0),
        (rect.left_top() + Vec2::splat(r), 180.0),
        (Pos2::new(rect.right() - r, rect.top() + r), 270.0),
    ];
    const SEG: usize = 6;
    for (center, start) in corners {
        for i in 0..=SEG {
            let ang = (start + 90.0 * i as f32 / SEG as f32).to_radians();
            let p = center + Vec2::new(ang.cos(), ang.sin()) * r;
            mesh.colored_vertex(p, color(p));
        }
    }
    let n = mesh.vertices.len() as u32;
    for i in 1..n {
        let next = if i + 1 < n { i + 1 } else { 1 };
        mesh.add_triangle(0, i, next);
    }
    mesh
}

fn chat_prefs(ui: &mut Ui, app: &mut NixApp) {
    let t = app.theme;
    heading(ui, t, "Display");
    ui.checkbox(&mut app.prefs.clock_24h, "Use a 24-hour clock");
    ui.add_space(4.0);
    ui.checkbox(&mut app.prefs.autoplay_gifs, "Play GIFs automatically")
        .on_hover_text("When off, GIFs and animated emoji play while hovered");
    ui.add_space(4.0);
    let mut auto = app.prefs.show_members.is_none();
    if ui
        .checkbox(
            &mut auto,
            "Show the member list automatically on wide windows",
        )
        .changed()
    {
        app.prefs.show_members = if auto {
            None
        } else {
            Some(app.members_visible(ui.ctx()))
        };
    }
}

fn account(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    if let Some(me) = app.store.me().cloned() {
        Frame::new()
            .fill(t.sidebar.to_opaque())
            .stroke(Stroke::new(1.0, t.divider))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                ui.set_width(ui.available_width().min(560.0));
                ui.horizontal(|ui| {
                    app.media.borrow_mut().avatar(ui, &me, None, 64.0);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(me.display_name())
                                .font(bold(20.0))
                                .color(t.strong),
                        );
                        ui.label(RichText::new(&me.username).color(t.muted));
                    });
                });
            });
        ui.add_space(16.0);
    }
    let logout = egui::Button::new(
        RichText::new("Log Out")
            .font(bold(14.0))
            .color(Color32::WHITE),
    )
    .fill(t.danger)
    .min_size(Vec2::new(100.0, 34.0));
    if ui
        .add(logout)
        .on_hover_text("Also forgets the remembered token")
        .clicked()
    {
        actions.push(Action::Logout);
    }
}

fn about(ui: &mut Ui, app: &mut NixApp) {
    let t = app.theme;
    heading(
        ui,
        t,
        &format!(
            "nixcord {}{}",
            env!("CARGO_PKG_VERSION"),
            if app.demo { " (demo mode)" } else { "" }
        ),
    );
    ui.label(RichText::new("No telemetry. Messages are kept in memory only.").color(t.text));
    let (n, bytes) = app.media.borrow().stats();
    let (an, abytes) = app.media.borrow().anim_stats();
    ui.label(
        RichText::new(format!(
            "Images in memory: {n} ({:.1} MB), animations: {an} ({:.1} MB)",
            bytes as f64 / (1024.0 * 1024.0),
            abytes as f64 / (1024.0 * 1024.0)
        ))
        .color(t.muted),
    );
}
