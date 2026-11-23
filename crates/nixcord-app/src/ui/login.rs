//! Login screen: paste a token (masked), optionally remember it in the OS keyring.

use super::{bold, prop, Action};
use crate::app::NixApp;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Key, Margin, RichText, Sense, Stroke, Vec2,
};

pub fn show(ui: &mut egui::Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    let card_w = 440.0_f32.min(ui.available_width() - 32.0);

    ui.vertical_centered(|ui| {
        let top = ((ui.available_height() - 420.0) / 2.0).max(16.0);
        ui.add_space(top);
        Frame::new()
            .fill(t.elevated.to_opaque())
            .stroke(Stroke::new(1.0, t.divider))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(32))
            .shadow(egui::Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: Color32::from_black_alpha(90),
            })
            .show(ui, |ui| {
                ui.set_width(card_w - 64.0);
                ui.vertical_centered(|ui| {
                    // Monogram.
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(18), t.accent);
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "N", bold(30.0), t.on_accent);
                    ui.add_space(12.0);
                    ui.label(RichText::new("Welcome to nixcord").font(bold(24.0)).color(t.strong));
                    ui.add_space(2.0);
                    ui.label(RichText::new("A lightweight native Discord client").color(t.muted));
                });
                ui.add_space(24.0);

                ui.label(RichText::new("TOKEN").font(bold(12.0)).color(t.muted));
                ui.add_space(4.0);
                let busy = app.login.busy;
                let resp = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut app.login.token)
                        .password(true)
                        .hint_text("Paste your token")
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 9))
                        .font(prop(15.0)),
                );
                let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(10.0);
                ui.add_enabled(
                    !busy,
                    egui::Checkbox::new(&mut app.login.remember, "Remember me on this device"),
                )
                .on_hover_text("Stored in the operating system's credential manager, never on disk in plain text.");
                ui.add_space(18.0);

                let label = if busy { "Signing in…" } else { "Log In" };
                let btn = egui::Button::new(RichText::new(label).font(bold(15.0)).color(t.on_accent))
                    .fill(t.accent)
                    .corner_radius(CornerRadius::same(4))
                    .min_size(Vec2::new(ui.available_width(), 40.0));
                let clicked = ui.add_enabled(!busy, btn).clicked();
                if (clicked || submit) && !busy {
                    actions.push(Action::Login {
                        token: app.login.token.clone(),
                        remember: app.login.remember,
                    });
                }
                if busy {
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| ui.add(egui::Spinner::new().color(t.muted)));
                }
                if let Some(err) = &app.login.error {
                    ui.add_space(12.0);
                    ui.label(RichText::new(err.as_str()).color(t.danger));
                }
                ui.add_space(18.0);
                ui.label(
                    RichText::new(
                        "The token can also come from the NIXCORD_TOKEN environment variable. \
                         Run with --demo to try the client without an account.",
                    )
                    .small()
                    .color(t.muted),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Third-party clients are not supported by Discord. Use at your own risk.")
                        .small()
                        .color(t.muted),
                );
            });
    });
}
