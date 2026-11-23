//! Settings window: theme picker, UI prefs, account.

use super::{bold, paint_avatar, prop, Action};
use crate::app::NixApp;
use crate::theme::{self, Theme};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2,
};

pub fn window(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    if !app.settings_open {
        return;
    }
    let t = app.theme;
    let mut open = true;
    egui::Window::new(RichText::new("Settings").font(bold(16.0)).color(t.strong))
        .id(egui::Id::unique("settings"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([600.0, 560.0])
        .frame(
            Frame::window(&ctx.global_style())
                .fill(t.elevated.to_opaque())
                .inner_margin(Margin::same(16)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    section(ui, t, "APPEARANCE");
                    ui.label(RichText::new("Theme").color(t.text));
                    ui.add_space(6.0);
                    theme_grid(ui, app);
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Zoom").color(t.text));
                        ui.add(
                            egui::Slider::new(&mut app.prefs.zoom, 0.75..=1.75)
                                .step_by(0.05)
                                .fixed_decimals(2),
                        );
                    });
                    ui.checkbox(&mut app.prefs.clock_24h, "24-hour clock");
                    let mut auto = app.prefs.show_members.is_none();
                    if ui
                        .checkbox(
                            &mut auto,
                            "Show the member panel automatically on wide windows",
                        )
                        .changed()
                    {
                        app.prefs.show_members = if auto {
                            None
                        } else {
                            Some(app.members_visible(ui.ctx()))
                        };
                    }

                    ui.add_space(18.0);
                    section(ui, t, "ACCOUNT");
                    if let Some(me) = app.store.me().cloned() {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                            paint_avatar(ui, r.center(), 40.0, &me);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(me.display_name())
                                        .font(bold(15.0))
                                        .color(t.strong),
                                );
                                ui.label(RichText::new(&me.username).color(t.muted));
                            });
                        });
                    }
                    ui.add_space(8.0);
                    let logout = egui::Button::new(
                        RichText::new("Log Out")
                            .font(bold(14.0))
                            .color(Color32::WHITE),
                    )
                    .fill(t.danger)
                    .min_size(Vec2::new(100.0, 32.0));
                    if ui
                        .add(logout)
                        .on_hover_text("Also forgets the remembered token")
                        .clicked()
                    {
                        actions.push(Action::Logout);
                    }

                    ui.add_space(18.0);
                    section(ui, t, "ABOUT");
                    ui.label(
                        RichText::new(format!(
                            "nixcord {}{}",
                            env!("CARGO_PKG_VERSION"),
                            if app.demo { " (demo mode)" } else { "" }
                        ))
                        .color(t.muted),
                    );
                    ui.label(
                        RichText::new("No telemetry. Messages are kept in memory only.")
                            .color(t.muted),
                    );
                });
        });
    if !open {
        app.settings_open = false;
    }
}

fn section(ui: &mut Ui, t: &Theme, title: &str) {
    ui.label(RichText::new(title).font(bold(12.0)).color(t.muted));
    ui.add_space(4.0);
}

fn theme_grid(ui: &mut Ui, app: &mut NixApp) {
    let cur = app.theme;
    let card = Vec2::new(124.0, 84.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
        for th in theme::builtin() {
            let (rect, resp) = ui.allocate_exact_size(card, Sense::click());
            paint_preview(ui, rect.shrink2(Vec2::new(0.0, 0.0)), th);
            let selected = std::ptr::eq(th, cur);
            let stroke = if selected {
                Stroke::new(2.5, cur.accent)
            } else if resp.hovered() {
                Stroke::new(1.5, cur.muted)
            } else {
                Stroke::new(1.0, cur.divider)
            };
            ui.painter()
                .rect_stroke(rect, CornerRadius::same(8), stroke, StrokeKind::Outside);
            if resp.on_hover_text(th.name).clicked() {
                app.prefs.theme = th.name.to_string();
            }
        }
    });
}

/// Mini mock of the app in the theme: gradient, rail, sidebar, two chat lines, label.
fn paint_preview(ui: &Ui, rect: Rect, th: &Theme) {
    let p = ui.painter().with_clip_rect(rect);
    let mesh = theme::gradient_mesh(rect, th.bg_top, th.bg_bottom);
    p.add(Shape::mesh(mesh));
    let rail = Rect::from_min_size(rect.min, Vec2::new(16.0, rect.height()));
    p.rect_filled(rail, 0.0, th.rail);
    for i in 0..3 {
        p.circle_filled(
            Pos2::new(rail.center().x, rect.top() + 10.0 + i as f32 * 14.0),
            5.0,
            if i == 0 { th.accent } else { th.elevated },
        );
    }
    let side = Rect::from_min_size(
        Pos2::new(rail.right(), rect.top()),
        Vec2::new(30.0, rect.height()),
    );
    p.rect_filled(side, 0.0, th.sidebar);
    for i in 0..3 {
        let r = Rect::from_min_size(
            Pos2::new(side.left() + 4.0, rect.top() + 8.0 + i as f32 * 9.0),
            Vec2::new(20.0, 4.0),
        );
        p.rect_filled(
            r,
            CornerRadius::same(2),
            if i == 1 {
                th.text
            } else {
                th.muted.gamma_multiply(0.7)
            },
        );
    }
    let chat = Rect::from_min_max(Pos2::new(side.right(), rect.top()), rect.max);
    p.rect_filled(chat, 0.0, th.chat);
    for i in 0..2 {
        let y = rect.top() + 10.0 + i as f32 * 18.0;
        p.circle_filled(
            Pos2::new(chat.left() + 9.0, y + 3.0),
            4.0,
            th.accent.gamma_multiply(0.8),
        );
        p.rect_filled(
            Rect::from_min_size(Pos2::new(chat.left() + 17.0, y), Vec2::new(24.0, 3.0)),
            CornerRadius::same(1),
            th.strong,
        );
        p.rect_filled(
            Rect::from_min_size(Pos2::new(chat.left() + 17.0, y + 6.0), Vec2::new(44.0, 3.0)),
            CornerRadius::same(1),
            th.muted,
        );
    }
    let label = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - 22.0), rect.max);
    p.rect_filled(label, 0.0, Color32::from_black_alpha(140));
    p.text(
        label.center(),
        Align2::CENTER_CENTER,
        th.name,
        prop(12.5),
        Color32::WHITE,
    );
}
