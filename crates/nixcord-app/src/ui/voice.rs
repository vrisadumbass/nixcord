//! Voice calls UI: connected status bar, voice channel participants list, and Voice & Audio
//! settings section.

use super::{bold, emoji_text, member_name, paint_default_avatar, paint_icon, prop, Action, Icon};
use crate::app::NixApp;
use crate::backend::AppVoiceEvent;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Layout, Pos2, Rect, RichText, Sense,
    Stroke, Ui, Vec2,
};
use nixcord_model::{Snowflake, VoiceState};
use std::collections::HashSet;

/// Default VAD activation threshold in dBFS.
pub const DEFAULT_VAD_THRESHOLD_DB: f32 = -45.0;

/// Connection lifecycle of the local user's voice session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

/// UI and audio state for the active voice session and settings.
#[derive(Debug, Clone)]
pub struct VoiceViewState {
    pub guild_id: Option<Snowflake>,
    pub channel_id: Option<Snowflake>,
    pub state: VoiceConnectionState,
    pub ping_ms: Option<u32>,
    pub endpoint: Option<String>,
    pub speaking_users: HashSet<Snowflake>,
    pub self_speaking: bool,
    pub error: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    pub vad_threshold_db: f32,
    /// Cached OS audio device names (populated when Settings opens, never per-frame).
    pub cached_input_devices: Vec<String>,
    pub cached_output_devices: Vec<String>,
    pub devices_loaded: bool,
}

impl Default for VoiceViewState {
    fn default() -> Self {
        Self {
            guild_id: None,
            channel_id: None,
            state: VoiceConnectionState::Disconnected,
            ping_ms: None,
            endpoint: None,
            speaking_users: HashSet::new(),
            self_speaking: false,
            error: None,
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            vad_threshold_db: DEFAULT_VAD_THRESHOLD_DB,
            cached_input_devices: Vec::new(),
            cached_output_devices: Vec::new(),
            devices_loaded: false,
        }
    }
}

impl VoiceViewState {
    /// True while connecting or connected to a voice channel.
    pub fn is_active(&self) -> bool {
        self.state != VoiceConnectionState::Disconnected
    }

    /// True if `user_id` should currently display a green speaking ring.
    pub fn is_user_speaking(&self, user_id: Snowflake, me_id: Option<Snowflake>) -> bool {
        self.speaking_users.contains(&user_id) || (Some(user_id) == me_id && self.self_speaking)
    }

    /// Begin connecting to `(guild_id, channel_id)`.
    pub fn start_connecting(&mut self, guild_id: Option<Snowflake>, channel_id: Snowflake) {
        self.guild_id = guild_id;
        self.channel_id = Some(channel_id);
        self.state = VoiceConnectionState::Connecting;
        self.ping_ms = None;
        self.endpoint = None;
        self.self_speaking = false;
        self.error = None;
    }

    /// Transition to `Disconnected` and clear active session fields.
    pub fn disconnect(&mut self, reason: Option<String>) {
        self.guild_id = None;
        self.channel_id = None;
        self.state = VoiceConnectionState::Disconnected;
        self.ping_ms = None;
        self.endpoint = None;
        self.speaking_users.clear();
        self.self_speaking = false;
        self.error = reason;
    }

    /// Apply a backend voice event to the view state.
    pub fn apply_event(&mut self, ev: &AppVoiceEvent) {
        match ev {
            AppVoiceEvent::Connecting {
                guild_id,
                channel_id,
            } => {
                self.guild_id = *guild_id;
                self.channel_id = Some(*channel_id);
                self.state = VoiceConnectionState::Connecting;
                self.error = None;
            }
            AppVoiceEvent::Connected {
                guild_id,
                channel_id,
                endpoint,
            } => {
                self.guild_id = *guild_id;
                self.channel_id = Some(*channel_id);
                self.state = VoiceConnectionState::Connected;
                self.endpoint = Some(endpoint.clone());
                self.error = None;
            }
            AppVoiceEvent::Speaking { user_id, speaking } => {
                if *speaking {
                    self.speaking_users.insert(*user_id);
                } else {
                    self.speaking_users.remove(user_id);
                }
            }
            AppVoiceEvent::Ping(ms) => {
                self.ping_ms = Some(*ms);
            }
            AppVoiceEvent::SelfSpeaking(speaking) => {
                self.self_speaking = *speaking;
            }
            AppVoiceEvent::Disconnected { reason } => {
                self.disconnect(reason.clone());
            }
        }
    }

    /// Enumerate OS audio input/output devices once and cache them for the Settings modal.
    pub fn refresh_devices(&mut self) {
        #[cfg(feature = "voice")]
        {
            self.cached_input_devices = nixcord_voice::audio::list_input_devices();
            self.cached_output_devices = nixcord_voice::audio::list_output_devices();
        }
        #[cfg(not(feature = "voice"))]
        {
            self.cached_input_devices.clear();
            self.cached_output_devices.clear();
        }
        self.devices_loaded = true;
    }
}

// ---- connected bar (above user panel) -----------------------------------------------

/// Compact Discord-style voice connection status bar rendered directly above the bottom
/// user panel while connecting or connected.
pub fn connected_bar(ui: &mut Ui, app: &NixApp, actions: &mut Vec<Action>) {
    if !app.view.voice.is_active() {
        return;
    }
    let t = app.theme;
    let voice = &app.view.voice;

    let channel_name = voice
        .channel_id
        .and_then(|cid| app.store.channel(cid))
        .map(|c| c.label())
        .unwrap_or_else(|| "Voice".into());
    let guild_name = voice
        .guild_id
        .and_then(|gid| app.store.guild(gid))
        .map(|g| g.name.as_str());
    let subtitle = match guild_name {
        Some(g) => format!("{channel_name} / {g}"),
        None => channel_name,
    };

    connected_bar_view(ui, t, voice, &subtitle, actions);
}

fn connected_bar_view(
    ui: &mut Ui,
    t: &Theme,
    voice: &VoiceViewState,
    subtitle: &str,
    actions: &mut Vec<Action>,
) {
    let panel = ui.max_rect();
    ui.painter().rect_filled(
        panel,
        CornerRadius::same(8),
        Color32::from_black_alpha(if t.dark { 70 } else { 20 }),
    );

    let (status_text, status_color, bars_lit) = match voice.state {
        VoiceConnectionState::Connected => {
            let (col, bars) = match voice.ping_ms {
                Some(ms) if ms > 300 => (t.danger, 1),
                Some(ms) if ms > 150 => (t.idle, 2),
                _ => (t.online, 3),
            };
            ("Voice Connected", col, bars)
        }
        VoiceConnectionState::Connecting => ("Voice Connecting…", t.idle, 1),
        VoiceConnectionState::Disconnected => return,
    };

    let ping_tooltip = match (voice.state, voice.ping_ms, voice.endpoint.as_deref()) {
        (VoiceConnectionState::Connected, Some(ms), Some(ep)) => format!("{ms} ms • {ep}"),
        (VoiceConnectionState::Connected, Some(ms), None) => format!("{ms} ms"),
        (VoiceConnectionState::Connected, None, Some(ep)) => format!("Connected • {ep}"),
        (VoiceConnectionState::Connected, None, None) => "Voice Connected".into(),
        _ => "Connecting to voice server…".into(),
    };

    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.add_space(4.0);
        let disc = disconnect_button(ui, t);
        if disc.clicked() {
            actions.push(Action::LeaveVoice);
        }

        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(8.0);
            let info_w = (ui.available_width() - 4.0).max(0.0);
            let (info_rect, info_resp) =
                ui.allocate_exact_size(Vec2::new(info_w, panel.height().min(44.0)), Sense::hover());

            // Top line: signal bars + status text.
            let top_y = info_rect.top() + 13.0;
            paint_signal_bars(
                ui,
                Pos2::new(info_rect.left(), top_y),
                bars_lit,
                status_color,
                t.muted.gamma_multiply(0.35),
            );
            let text_left = info_rect.left() + 18.0;
            let clip = Rect::from_min_max(
                Pos2::new(text_left, info_rect.top()),
                info_rect.right_bottom(),
            )
            .intersect(ui.clip_rect());
            let p = ui.painter().with_clip_rect(clip);
            p.text(
                Pos2::new(text_left, top_y),
                Align2::LEFT_CENTER,
                status_text,
                bold(13.0),
                status_color,
            );
            // Bottom line: channel / server subtitle.
            let sub_clip = info_rect.intersect(ui.clip_rect());
            ui.painter().with_clip_rect(sub_clip).text(
                Pos2::new(info_rect.left(), info_rect.top() + 31.0),
                Align2::LEFT_CENTER,
                subtitle,
                prop(12.0),
                t.muted,
            );

            info_resp.on_hover_text(ping_tooltip);
        });
    });
}

fn paint_signal_bars(
    ui: &Ui,
    left_center: Pos2,
    lit: u8,
    active_color: Color32,
    dim_color: Color32,
) {
    let p = ui.painter();
    let heights = [5.0_f32, 8.5, 12.0];
    let baseline = left_center.y + 6.0;
    for (i, h) in heights.iter().enumerate() {
        let x = left_center.x + i as f32 * 4.5;
        let rect = Rect::from_min_size(Pos2::new(x, baseline - *h), Vec2::new(3.0, *h));
        let col = if (i as u8) < lit {
            active_color
        } else {
            dim_color
        };
        p.rect_filled(rect, CornerRadius::same(1), col);
    }
}

fn disconnect_button(ui: &mut Ui, t: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.danger.gamma_multiply(0.22));
    }
    let col = if hovered { t.danger } else { t.muted };
    // Crisp phone-hangup / disconnect cross icon.
    let c = rect.center();
    let s = 5.5;
    let st = Stroke::new(1.8, col);
    ui.painter()
        .line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], st);
    ui.painter()
        .line_segment([c + Vec2::new(-s, s), c + Vec2::new(s, -s)], st);
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Disconnect")
}

// ---- voice channel participants (sidebar) -------------------------------------------

/// Render connected voice participants beneath a voice channel row in the sidebar.
pub fn participants(ui: &mut Ui, app: &NixApp, channel_id: Snowflake, actions: &mut Vec<Action>) {
    let states = app.store.voice_states_in(channel_id);
    if states.is_empty() {
        return;
    }
    let t = app.theme;
    let me_id = app.store.me().map(|u| u.id);

    for vs in states {
        participant_row(ui, app, t, vs, me_id, actions);
    }
}

fn participant_row(
    ui: &mut Ui,
    app: &NixApp,
    t: &Theme,
    vs: &VoiceState,
    me_id: Option<Snowflake>,
    actions: &mut Vec<Action>,
) {
    let row_w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(row_w, 28.0), Sense::click());
    let rect = Rect::from_min_max(
        Pos2::new(outer.left() + 26.0, outer.top() + 1.0),
        Pos2::new(outer.right() - 8.0, outer.bottom() - 1.0),
    );
    let hovered = resp.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }

    let speaking = app.view.voice.is_user_speaking(vs.user_id, me_id);
    let avatar_d = 24.0;
    let avatar_center = Pos2::new(rect.left() + 6.0 + avatar_d / 2.0, rect.center().y);

    let user = app.store.user(vs.user_id);
    if let Some(u) = user {
        let guild_av = vs.guild_id.map(|g| (g, app.store.member_avatar(g, u.id)));
        app.media
            .borrow_mut()
            .paint_avatar(ui, avatar_center, avatar_d, u, guild_av);
    } else {
        paint_default_avatar(ui, avatar_center, avatar_d, vs.user_id);
    }

    if speaking {
        ui.painter().circle_stroke(
            avatar_center,
            avatar_d / 2.0 + 1.5,
            Stroke::new(2.0, t.online),
        );
    }

    // Reserve space on the right for mute / deaf badges.
    let muted = vs.self_mute || vs.mute;
    let deafened = vs.self_deaf || vs.deaf;
    let badge_count = usize::from(muted) + usize::from(deafened);
    let badges_w = if badge_count > 0 {
        badge_count as f32 * 18.0 + 4.0
    } else {
        0.0
    };

    let name_left = avatar_center.x + avatar_d / 2.0 + 8.0;
    let text_rect = Rect::from_min_max(
        Pos2::new(name_left, rect.top()),
        Pos2::new(
            (rect.right() - 6.0 - badges_w).max(name_left),
            rect.bottom(),
        ),
    );
    let display_name = match user {
        Some(u) => member_name(&app.store, vs.guild_id, u).to_string(),
        None => format!("User {}", vs.user_id),
    };
    let name_color = if speaking || hovered {
        t.strong
    } else {
        t.muted
    };
    let p = ui
        .painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()));
    let galley = emoji_text::layout(&p, &display_name, prop(13.5), name_color, f32::INFINITY, 0);
    galley.paint(
        &p,
        Pos2::new(
            text_rect.left(),
            text_rect.center().y - galley.size().y / 2.0,
        ),
        &app.media,
    );

    // Paint small mute / deaf badges on the right.
    let mut right_x = rect.right() - 6.0;
    if deafened {
        let r =
            Rect::from_center_size(Pos2::new(right_x - 8.0, rect.center().y), Vec2::splat(16.0));
        let col = if vs.deaf { t.danger } else { t.muted };
        paint_icon(ui, Icon::Headphones, r, col);
        paint_small_strike(ui, r, t.danger);
        right_x -= 18.0;
    }
    if muted {
        let r =
            Rect::from_center_size(Pos2::new(right_x - 8.0, rect.center().y), Vec2::splat(16.0));
        let col = if vs.mute { t.danger } else { t.muted };
        paint_icon(ui, Icon::Mic, r, col);
        paint_small_strike(ui, r, t.danger);
    }

    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        actions.push(Action::OpenProfile {
            user_id: vs.user_id,
            guild_id: vs.guild_id,
            anchor: rect,
        });
    }
}

fn paint_small_strike(ui: &Ui, rect: Rect, color: Color32) {
    let r = rect.shrink(3.0);
    ui.painter()
        .line_segment([r.left_top(), r.right_bottom()], Stroke::new(1.5, color));
}

// ---- settings section ---------------------------------------------------------------

/// Voice & Audio settings page rendered inside the Settings modal.
///
/// Audio device lists are read from `app.view.voice.cached_input_devices` /
/// `cached_output_devices` (populated once on modal open, never enumerated per-frame).
pub fn settings_section(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    if !app.view.voice.devices_loaded {
        app.view.voice.refresh_devices();
    }
    let t = app.theme;
    settings_section_view(ui, t, &mut app.view.voice, actions);
}

fn settings_section_view(
    ui: &mut Ui,
    t: &Theme,
    voice: &mut VoiceViewState,
    actions: &mut Vec<Action>,
) {
    #[cfg(not(feature = "voice"))]
    {
        ui.label(
            RichText::new("This build was compiled without the voice feature.")
                .font(prop(13.0))
                .color(t.idle),
        );
        ui.add_space(10.0);
    }

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Audio Devices")
                .font(bold(16.0))
                .color(t.strong),
        );
        ui.add_space(8.0);
        if ui
            .small_button("Refresh")
            .on_hover_text("Re-scan connected microphones and speakers")
            .clicked()
        {
            voice.refresh_devices();
        }
    });
    ui.add_space(8.0);

    // Input device selector.
    ui.label(RichText::new("Input Device").font(bold(13.5)).color(t.text));
    ui.add_space(4.0);
    let selected_in = voice
        .input_device
        .as_deref()
        .unwrap_or("Default")
        .to_string();
    let mut next_in = voice.input_device.clone();
    egui::ComboBox::from_id_salt("voice_input_device")
        .width(280.0)
        .selected_text(&selected_in)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut next_in, None, "Default");
            for dev in &voice.cached_input_devices {
                ui.selectable_value(&mut next_in, Some(dev.clone()), dev);
            }
        });
    if next_in != voice.input_device {
        voice.input_device = next_in.clone();
        actions.push(Action::SetVoiceInputDevice(next_in));
    }

    ui.add_space(12.0);

    // Output device selector.
    ui.label(
        RichText::new("Output Device")
            .font(bold(13.5))
            .color(t.text),
    );
    ui.add_space(4.0);
    let selected_out = voice
        .output_device
        .as_deref()
        .unwrap_or("Default")
        .to_string();
    let mut next_out = voice.output_device.clone();
    egui::ComboBox::from_id_salt("voice_output_device")
        .width(280.0)
        .selected_text(&selected_out)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut next_out, None, "Default");
            for dev in &voice.cached_output_devices {
                ui.selectable_value(&mut next_out, Some(dev.clone()), dev);
            }
        });
    if next_out != voice.output_device {
        voice.output_device = next_out.clone();
        actions.push(Action::SetVoiceOutputDevice(next_out));
    }

    ui.add_space(20.0);
    ui.label(RichText::new("Volume").font(bold(16.0)).color(t.strong));
    ui.add_space(8.0);

    // Input gain slider (0%..200%).
    ui.label(RichText::new("Input Gain").font(bold(13.5)).color(t.text));
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 260.0;
        let resp = ui.add(
            egui::Slider::new(&mut voice.input_gain, 0.0..=2.0)
                .step_by(0.05)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        );
        if resp.changed() {
            actions.push(Action::SetVoiceInputGain(voice.input_gain));
        }
    });

    ui.add_space(10.0);

    // Output volume slider (0%..200%).
    ui.label(
        RichText::new("Output Volume")
            .font(bold(13.5))
            .color(t.text),
    );
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 260.0;
        let resp = ui.add(
            egui::Slider::new(&mut voice.output_volume, 0.0..=2.0)
                .step_by(0.05)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
        );
        if resp.changed() {
            actions.push(Action::SetVoiceOutputVolume(voice.output_volume));
        }
    });

    ui.add_space(20.0);
    ui.label(
        RichText::new("Voice Activity Detection")
            .font(bold(16.0))
            .color(t.strong),
    );
    ui.add_space(8.0);

    ui.label(
        RichText::new("Input Sensitivity (VAD Threshold)")
            .font(bold(13.5))
            .color(t.text),
    );
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 260.0;
        let resp = ui.add(
            egui::Slider::new(&mut voice.vad_threshold_db, -80.0..=-10.0)
                .step_by(1.0)
                .custom_formatter(|v, _| format!("{v:.0} dB")),
        );
        if resp.changed() {
            actions.push(Action::SetVoiceVadThreshold(voice.vad_threshold_db));
        }
    });
    ui.add_space(4.0);
    ui.label(
        RichText::new("Sounds louder than this threshold transmit to the voice channel.")
            .font(prop(12.0))
            .color(t.muted),
    );

    if let Some(err) = &voice.error {
        ui.add_space(12.0);
        ui.label(RichText::new(err).font(prop(12.5)).color(t.danger));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    fn run_headless(mut draw: impl FnMut(&mut Ui)) {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                draw(ui);
            });
        });
        out.textures_delta.clear();
    }

    #[test]
    fn voice_view_state_transitions() {
        let mut st = VoiceViewState::default();
        assert!(!st.is_active());
        assert_eq!(st.state, VoiceConnectionState::Disconnected);
        assert_eq!(st.input_gain, 1.0);
        assert_eq!(st.output_volume, 1.0);
        assert_eq!(st.vad_threshold_db, DEFAULT_VAD_THRESHOLD_DB);

        let g = Snowflake(10);
        let c = Snowflake(108);
        let u = Snowflake(1002);
        let me = Snowflake(1000);

        st.start_connecting(Some(g), c);
        assert!(st.is_active());
        assert_eq!(st.state, VoiceConnectionState::Connecting);
        assert_eq!(st.guild_id, Some(g));
        assert_eq!(st.channel_id, Some(c));

        st.apply_event(&AppVoiceEvent::Connected {
            guild_id: Some(g),
            channel_id: c,
            endpoint: "us-east1.discord.media:443".into(),
        });
        assert_eq!(st.state, VoiceConnectionState::Connected);
        assert_eq!(st.endpoint.as_deref(), Some("us-east1.discord.media:443"));

        st.apply_event(&AppVoiceEvent::Ping(24));
        assert_eq!(st.ping_ms, Some(24));

        st.apply_event(&AppVoiceEvent::Speaking {
            user_id: u,
            speaking: true,
        });
        assert!(st.is_user_speaking(u, Some(me)));
        assert!(!st.is_user_speaking(me, Some(me)));

        st.apply_event(&AppVoiceEvent::SelfSpeaking(true));
        assert!(st.is_user_speaking(me, Some(me)));

        st.apply_event(&AppVoiceEvent::Speaking {
            user_id: u,
            speaking: false,
        });
        assert!(!st.is_user_speaking(u, Some(me)));

        st.apply_event(&AppVoiceEvent::Disconnected {
            reason: Some("kicked".into()),
        });
        assert!(!st.is_active());
        assert_eq!(st.state, VoiceConnectionState::Disconnected);
        assert!(st.channel_id.is_none());
        assert!(st.speaking_users.is_empty());
        assert!(!st.self_speaking);
        assert_eq!(st.error.as_deref(), Some("kicked"));
    }

    #[test]
    fn headless_connected_bar_renders_connecting_and_connected() {
        let t = theme::by_name(theme::DEFAULT_THEME);
        let mut st = VoiceViewState::default();
        st.start_connecting(Some(Snowflake(10)), Snowflake(108));
        let mut actions = Vec::new();
        run_headless(|ui| {
            connected_bar_view(ui, t, &st, "Lounge / Hearth", &mut actions);
        });
        assert!(actions.is_empty());

        st.apply_event(&AppVoiceEvent::Connected {
            guild_id: Some(Snowflake(10)),
            channel_id: Snowflake(108),
            endpoint: "us-east1.discord.media:443".into(),
        });
        st.apply_event(&AppVoiceEvent::Ping(24));
        run_headless(|ui| {
            connected_bar_view(ui, t, &st, "Lounge / Hearth", &mut actions);
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn headless_settings_section_renders_without_enumerating_when_cached() {
        let t = theme::by_name(theme::DEFAULT_THEME);
        let mut st = VoiceViewState {
            cached_input_devices: vec!["Studio Mic".into(), "USB Headset".into()],
            cached_output_devices: vec!["Desk Speakers".into(), "USB Headset".into()],
            devices_loaded: true,
            ..VoiceViewState::default()
        };
        let mut actions = Vec::new();
        run_headless(|ui| {
            settings_section_view(ui, t, &mut st, &mut actions);
        });
        assert!(st.devices_loaded);
        assert_eq!(st.cached_input_devices.len(), 2);
        assert_eq!(st.cached_output_devices.len(), 2);
    }

    #[test]
    fn headless_participants_renders_speaking_and_muted_users() {
        use crate::media::Media;
        use nixcord_model::{Channel, ChannelKind, GatewayEvent, Guild, Ready, User};
        use nixcord_store::Store;
        use std::cell::RefCell;

        let ctx = egui::Context::default();
        let mut store = Store::new();
        let u1 = User {
            id: Snowflake(1002),
            username: "sol".into(),
            global_name: Some("Sol".into()),
            ..Default::default()
        };
        let u2 = User {
            id: Snowflake(1003),
            username: "quill".into(),
            global_name: Some("Quill".into()),
            ..Default::default()
        };
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: User {
                id: Snowflake(1000),
                username: "you".into(),
                global_name: Some("You".into()),
                ..Default::default()
            },
            guilds: vec![Guild {
                id: Snowflake(10),
                name: "Hearth".into(),
                channels: vec![Channel {
                    id: Snowflake(108),
                    kind: ChannelKind::Voice,
                    guild_id: Some(Snowflake(10)),
                    name: Some("Lounge".into()),
                    ..Default::default()
                }],
                voice_states: vec![
                    VoiceState {
                        guild_id: Some(Snowflake(10)),
                        channel_id: Some(Snowflake(108)),
                        user_id: u1.id,
                        session_id: "s1".into(),
                        ..Default::default()
                    },
                    VoiceState {
                        guild_id: Some(Snowflake(10)),
                        channel_id: Some(Snowflake(108)),
                        user_id: u2.id,
                        session_id: "s2".into(),
                        self_mute: true,
                        self_deaf: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        })));

        let mut voice = VoiceViewState::default();
        voice.speaking_users.insert(u1.id);
        let t = theme::by_name(theme::DEFAULT_THEME);
        let media = RefCell::new(Media::new(&ctx, true));

        let states = store.voice_states_in(Snowflake(108));
        assert_eq!(states.len(), 2);

        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                for vs in &states {
                    let row_w = ui.available_width();
                    let (outer, _resp) =
                        ui.allocate_exact_size(Vec2::new(row_w, 28.0), Sense::click());
                    let rect = Rect::from_min_max(
                        Pos2::new(outer.left() + 26.0, outer.top() + 1.0),
                        Pos2::new(outer.right() - 8.0, outer.bottom() - 1.0),
                    );
                    let avatar_center = Pos2::new(rect.left() + 18.0, rect.center().y);
                    paint_default_avatar(ui, avatar_center, 24.0, vs.user_id);
                    if voice.is_user_speaking(vs.user_id, Some(Snowflake(1000))) {
                        ui.painter()
                            .circle_stroke(avatar_center, 13.5, Stroke::new(2.0, t.online));
                    }
                    if vs.self_deaf {
                        let r = Rect::from_center_size(
                            Pos2::new(rect.right() - 14.0, rect.center().y),
                            Vec2::splat(16.0),
                        );
                        paint_icon(ui, Icon::Headphones, r, t.muted);
                        paint_small_strike(ui, r, t.danger);
                    }
                    if vs.self_mute {
                        let r = Rect::from_center_size(
                            Pos2::new(rect.right() - 32.0, rect.center().y),
                            Vec2::splat(16.0),
                        );
                        paint_icon(ui, Icon::Mic, r, t.muted);
                        paint_small_strike(ui, r, t.danger);
                    }
                }
            });
        });
        out.textures_delta.clear();
        drop(media);
    }
}
