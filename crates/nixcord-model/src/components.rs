//! Message components: legacy interactive rows (buttons, selects) and Components V2 layout
//! (sections, text displays, media galleries, files, separators, containers).
//!
//! Reference: <https://discord.com/developers/docs/components/reference>.
//!
//! [`Component`] is decoded from the `type` integer by hand: an unknown `type`, or an entry
//! whose fields don't decode, becomes [`Component::Unknown`] instead of failing the whole
//! message. Modal-only types (Text Input 4, Label 18, File Upload 19, Radio Group 21,
//! Checkbox Group 22, Checkbox 23) never appear in messages and also decode as `Unknown`.

use crate::{Emoji, Snowflake};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Message flag: the message is laid out with V2 components only (`content` and `embeds`
/// are unused, attachments show only where a component references them).
pub const IS_COMPONENTS_V2: u64 = 1 << 15;

/// One component of any type.
#[derive(Debug, Clone, PartialEq)]
pub enum Component {
    /// Type 1: up to 5 buttons or a single select.
    ActionRow(ActionRow),
    /// Type 2.
    Button(Button),
    /// Types 3 and 5-8.
    Select(SelectMenu),
    /// Type 9: 1-3 text displays plus a thumbnail or button accessory.
    Section(Section),
    /// Type 10: markdown text.
    TextDisplay(TextDisplay),
    /// Type 11: small image, only as a section accessory.
    Thumbnail(Thumbnail),
    /// Type 12: 1-10 images/videos.
    MediaGallery(MediaGallery),
    /// Type 13: an attached file.
    File(FileComponent),
    /// Type 14: vertical padding, optionally with a divider line.
    Separator(Separator),
    /// Type 17: a card with an optional accent bar.
    Container(Container),
    /// Anything else (or a broken entry); `kind` is the raw `type` (0 when missing).
    Unknown { kind: u64 },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionRow {
    #[serde(default, deserialize_with = "de_list")]
    pub components: Vec<Component>,
}

/// Button style (1-6). Anything else is kept as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ButtonStyle {
    #[default]
    Primary,
    Secondary,
    Success,
    Danger,
    /// Opens `url`; sends no interaction.
    Link,
    /// Buys `sku_id`; no label/emoji of its own.
    Premium,
    Other(u8),
}

impl From<u8> for ButtonStyle {
    fn from(n: u8) -> Self {
        match n {
            1 => Self::Primary,
            2 => Self::Secondary,
            3 => Self::Success,
            4 => Self::Danger,
            5 => Self::Link,
            6 => Self::Premium,
            n => Self::Other(n),
        }
    }
}

impl From<ButtonStyle> for u8 {
    fn from(s: ButtonStyle) -> u8 {
        match s {
            ButtonStyle::Primary => 1,
            ButtonStyle::Secondary => 2,
            ButtonStyle::Success => 3,
            ButtonStyle::Danger => 4,
            ButtonStyle::Link => 5,
            ButtonStyle::Premium => 6,
            ButtonStyle::Other(n) => n,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Button {
    #[serde(default)]
    pub style: ButtonStyle,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, deserialize_with = "de_opt")]
    pub emoji: Option<Emoji>,
    #[serde(default)]
    pub custom_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, deserialize_with = "de_opt")]
    pub sku_id: Option<Snowflake>,
    #[serde(default)]
    pub disabled: bool,
}

/// Which select menu (from the component `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectKind {
    #[default]
    String,
    User,
    Role,
    Mentionable,
    Channel,
}

impl SelectKind {
    fn from_type(t: u64) -> Option<Self> {
        Some(match t {
            3 => Self::String,
            5 => Self::User,
            6 => Self::Role,
            7 => Self::Mentionable,
            8 => Self::Channel,
            _ => return None,
        })
    }

    pub fn type_id(self) -> u64 {
        match self {
            Self::String => 3,
            Self::User => 5,
            Self::Role => 6,
            Self::Mentionable => 7,
            Self::Channel => 8,
        }
    }
}

/// Payload data for a Discord type-3 (MESSAGE_COMPONENT) interaction (`POST /interactions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentInteraction {
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub message_flags: u64,
    pub application_id: Snowflake,
    pub session_id: String,
    /// Component `type`: 2 = button, 3 = string select, 5..=8 = entity selects.
    pub component_type: u8,
    pub custom_id: String,
    /// Selected option values (used for select menus; omitted for buttons).
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "de_opt")]
    pub emoji: Option<Emoji>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectMenu {
    /// Set from the component `type`, not a JSON field.
    #[serde(skip)]
    pub kind: SelectKind,
    #[serde(default)]
    pub custom_id: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// String selects only.
    #[serde(default, deserialize_with = "de_lenient")]
    pub options: Vec<SelectOption>,
    #[serde(default)]
    pub min_values: Option<u32>,
    #[serde(default)]
    pub max_values: Option<u32>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    #[serde(default, deserialize_with = "de_list")]
    pub components: Vec<Component>,
    #[serde(default, deserialize_with = "de_opt")]
    pub accessory: Option<Box<Component>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextDisplay {
    /// Required: a text display without text decodes as `Unknown`.
    pub content: String,
}

/// "Unfurled media item": `url` (an https URL or `attachment://name`) plus what Discord
/// fills in once it has resolved it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UnfurledMedia {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl UnfurledMedia {
    /// URL to fetch (the media proxy when known).
    pub fn fetch_url(&self) -> &str {
        self.proxy_url.as_deref().unwrap_or(&self.url)
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height).filter(|&(w, h)| w > 0 && h > 0)
    }

    pub fn is_video(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|c| c.starts_with("video/"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
    #[serde(default)]
    pub media: UnfurledMedia,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaGalleryItem {
    #[serde(default)]
    pub media: UnfurledMedia,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaGallery {
    #[serde(default, deserialize_with = "de_lenient")]
    pub items: Vec<MediaGalleryItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileComponent {
    #[serde(default)]
    pub file: UnfurledMedia,
    #[serde(default)]
    pub spoiler: bool,
    /// Filled in by Discord.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Separator spacing: 1 = small, 2 = large.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Separator {
    #[serde(default = "yes")]
    pub divider: bool,
    #[serde(default = "one")]
    pub spacing: u8,
}

impl Default for Separator {
    fn default() -> Self {
        Self {
            divider: true,
            spacing: 1,
        }
    }
}

impl Separator {
    pub fn is_large(&self) -> bool {
        self.spacing >= 2
    }
}

fn yes() -> bool {
    true
}
fn one() -> u8 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Container {
    #[serde(default, deserialize_with = "de_list")]
    pub components: Vec<Component>,
    /// RGB, `0xRRGGBB`.
    #[serde(default, deserialize_with = "de_opt")]
    pub accent_color: Option<u32>,
    #[serde(default)]
    pub spoiler: bool,
}

// ---- decoding -----------------------------------------------------------------------

impl Component {
    /// Decode one component; never fails (see the module docs).
    pub fn from_value(v: serde_json::Value) -> Self {
        let kind = v
            .get("type")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        fn de<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Option<T> {
            serde_json::from_value(v).ok()
        }
        let decoded = match kind {
            1 => de(v).map(Self::ActionRow),
            2 => de(v).map(Self::Button),
            3 | 5..=8 => de::<SelectMenu>(v).map(|mut s| {
                s.kind = SelectKind::from_type(kind).unwrap_or_default();
                Self::Select(s)
            }),
            9 => de(v).map(Self::Section),
            10 => de(v).map(Self::TextDisplay),
            11 => de(v).map(Self::Thumbnail),
            12 => de(v).map(Self::MediaGallery),
            13 => de(v).map(Self::File),
            14 => de(v).map(Self::Separator),
            17 => de(v).map(Self::Container),
            _ => None,
        };
        decoded.unwrap_or(Self::Unknown { kind })
    }

    /// The component `type` integer.
    pub fn type_id(&self) -> u64 {
        match self {
            Self::ActionRow(_) => 1,
            Self::Button(_) => 2,
            Self::Select(s) => s.kind.type_id(),
            Self::Section(_) => 9,
            Self::TextDisplay(_) => 10,
            Self::Thumbnail(_) => 11,
            Self::MediaGallery(_) => 12,
            Self::File(_) => 13,
            Self::Separator(_) => 14,
            Self::Container(_) => 17,
            Self::Unknown { kind } => *kind,
        }
    }

    /// All text of the text displays in this tree, joined by newlines (reply previews,
    /// search).
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        let kids: &[Component] = match self {
            Self::TextDisplay(t) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&t.content);
                return;
            }
            Self::ActionRow(r) => &r.components,
            Self::Section(s) => &s.components,
            Self::Container(c) => &c.components,
            _ => return,
        };
        for k in kids {
            k.collect_text(out);
        }
    }
}

impl<'de> Deserialize<'de> for Component {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(serde_json::Value::deserialize(d)?))
    }
}

impl Serialize for Component {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let body = match self {
            Self::ActionRow(x) => serde_json::to_value(x),
            Self::Button(x) => serde_json::to_value(x),
            Self::Select(x) => serde_json::to_value(x),
            Self::Section(x) => serde_json::to_value(x),
            Self::TextDisplay(x) => serde_json::to_value(x),
            Self::Thumbnail(x) => serde_json::to_value(x),
            Self::MediaGallery(x) => serde_json::to_value(x),
            Self::File(x) => serde_json::to_value(x),
            Self::Separator(x) => serde_json::to_value(x),
            Self::Container(x) => serde_json::to_value(x),
            Self::Unknown { .. } => Ok(serde_json::Value::Object(Default::default())),
        };
        let mut body = body.map_err(S::Error::custom)?;
        if let serde_json::Value::Object(map) = &mut body {
            map.insert("type".into(), self.type_id().into());
        }
        body.serialize(s)
    }
}

/// A component list; `null` / missing / not-an-array decode as empty.
pub fn de_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Component>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(v) => v.into_iter().map(Component::from_value).collect(),
        _ => Vec::new(),
    })
}

/// A list where undecodable entries are dropped.
fn de_lenient<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(v) => v
            .into_iter()
            .filter_map(|x| serde_json::from_value(x).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// An optional field that becomes `None` when it doesn't decode.
fn de_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, MessageUpdate};

    /// A rules post as an app sends it (shape of a real Components V2 payload).
    const V2_RULES: &str = r##"{
        "id": "1300000000000000001", "channel_id": "102", "type": 0,
        "author": {"id": "1001", "username": "rulesbot", "bot": true},
        "content": "", "timestamp": "2026-11-20T12:00:00.000000+00:00",
        "embeds": [], "attachments": [], "flags": 32768,
        "components": [
          {"type": 17, "id": 1, "accent_color": 10181046, "spoiler": false, "components": [
            {"type": 12, "id": 2, "items": [
              {"media": {"id": "1", "url": "https://cdn.example.com/banner.png",
                         "proxy_url": "https://media.example.com/banner.png",
                         "width": 1200, "height": 400, "content_type": "image/png",
                         "placeholder": "abc", "placeholder_version": 1, "flags": 0},
               "description": null, "spoiler": false}
            ]},
            {"type": 10, "id": 3, "content": "# Server Rules\n**Be nice.** See <#100> :wave:"},
            {"type": 14, "id": 4, "divider": true, "spacing": 2},
            {"type": 9, "id": 5,
             "components": [{"type": 10, "id": 6, "content": "Read the guide"}],
             "accessory": {"type": 11, "id": 7, "media": {"url": "https://x/t.png"}, "spoiler": true}},
            {"type": 13, "id": 8, "file": {"url": "attachment://rules.pdf"}, "name": "rules.pdf", "size": 1234},
            {"type": 14, "id": 9},
            {"type": 99, "id": 10, "whatever": [1, 2, 3]},
            {"type": 10, "id": 11, "content": 42},
            {"type": 1, "id": 12, "components": [
              {"type": 2, "id": 13, "style": 5, "label": "Terms of Service",
               "url": "https://discord.com/terms", "emoji": {"name": "📜"}},
              {"type": 2, "id": 14, "style": 5, "label": "Guidelines",
               "url": "https://discord.com/guidelines",
               "emoji": {"id": "1150000000000000001", "name": "ferris", "animated": false}}
            ]}
          ]}
        ]
    }"##;

    #[test]
    fn v2_rules_post() {
        let m: Message = serde_json::from_str(V2_RULES).unwrap();
        assert_eq!(m.flags & IS_COMPONENTS_V2, IS_COMPONENTS_V2);
        assert_eq!(m.components.len(), 1);
        let Component::Container(c) = &m.components[0] else {
            panic!("container expected: {:?}", m.components[0]);
        };
        assert_eq!(c.accent_color, Some(10181046));
        assert!(!c.spoiler);
        assert_eq!(c.components.len(), 9);
        let Component::MediaGallery(g) = &c.components[0] else {
            panic!()
        };
        assert_eq!(g.items.len(), 1);
        assert_eq!(g.items[0].media.size(), Some((1200, 400)));
        assert_eq!(
            g.items[0].media.fetch_url(),
            "https://media.example.com/banner.png"
        );
        assert!(
            matches!(&c.components[1], Component::TextDisplay(t) if t.content.starts_with("# Server"))
        );
        assert!(matches!(
            &c.components[2],
            Component::Separator(Separator {
                divider: true,
                spacing: 2
            })
        ));
        let Component::Section(s) = &c.components[3] else {
            panic!()
        };
        assert_eq!(s.components.len(), 1);
        assert!(matches!(s.accessory.as_deref(), Some(Component::Thumbnail(t)) if t.spoiler));
        assert!(
            matches!(&c.components[4], Component::File(f) if f.name.as_deref() == Some("rules.pdf") && f.size == Some(1234))
        );
        // Defaults: divider true, small spacing.
        assert_eq!(c.components[5], Component::Separator(Separator::default()));
        // Unknown type and a broken text display don't fail the message.
        assert_eq!(c.components[6], Component::Unknown { kind: 99 });
        assert_eq!(c.components[7], Component::Unknown { kind: 10 });
        let Component::ActionRow(row) = &c.components[8] else {
            panic!()
        };
        let Component::Button(b) = &row.components[1] else {
            panic!()
        };
        assert_eq!(b.style, ButtonStyle::Link);
        assert_eq!(b.url.as_deref(), Some("https://discord.com/guidelines"));
        assert_eq!(
            b.emoji.as_ref().and_then(|e| e.id),
            Some(Snowflake(1150000000000000001))
        );
        assert!(m.components[0].text().contains("Read the guide"));
        assert_eq!(c.components.len(), 9);
    }

    #[test]
    fn legacy_action_row() {
        let json = r#"{
            "id": "5", "channel_id": "102",
            "author": {"id": "1001", "username": "pollbot", "bot": true},
            "content": "Pick one", "timestamp": "2026-11-20T12:00:00+00:00", "flags": 0,
            "components": [
              {"type": 1, "components": [
                {"type": 2, "style": 1, "label": "Yes", "custom_id": "yes"},
                {"type": 2, "style": 4, "label": "No", "custom_id": "no", "disabled": true},
                {"type": 2, "style": 6, "sku_id": "777"},
                {"type": 2, "style": 9, "label": "Future", "custom_id": "f"}
              ]},
              {"type": 1, "components": [
                {"type": 3, "custom_id": "pick", "placeholder": "Choose a color",
                 "options": [{"label": "Red", "value": "r", "default": true}, {"bogus": 1}],
                 "min_values": 1, "max_values": 1}
              ]},
              {"type": 1, "components": [{"type": 8, "custom_id": "ch", "channel_types": [0]}]},
              "not an object"
            ]
        }"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.flags, 0);
        assert_eq!(m.components.len(), 4);
        let Component::ActionRow(r) = &m.components[0] else {
            panic!()
        };
        let styles: Vec<ButtonStyle> = r
            .components
            .iter()
            .filter_map(|c| match c {
                Component::Button(b) => Some(b.style),
                _ => None,
            })
            .collect();
        assert_eq!(
            styles,
            [
                ButtonStyle::Primary,
                ButtonStyle::Danger,
                ButtonStyle::Premium,
                ButtonStyle::Other(9)
            ]
        );
        assert!(matches!(&r.components[1], Component::Button(b) if b.disabled));
        assert!(
            matches!(&r.components[2], Component::Button(b) if b.sku_id == Some(Snowflake(777)))
        );
        let Component::ActionRow(r) = &m.components[1] else {
            panic!()
        };
        let Component::Select(s) = &r.components[0] else {
            panic!()
        };
        assert_eq!(s.kind, SelectKind::String);
        assert_eq!(s.placeholder.as_deref(), Some("Choose a color"));
        // The option missing label/value still decodes (defaults); none are dropped here.
        assert!(s.options[0].default);
        assert!(matches!(&m.components[2], Component::ActionRow(r)
            if matches!(&r.components[0], Component::Select(s) if s.kind == SelectKind::Channel)));
        assert_eq!(m.components[3], Component::Unknown { kind: 0 });
    }

    #[test]
    fn missing_or_null_components() {
        let base =
            r#""id":"1","channel_id":"2","author":{"id":"3","username":"u"},"timestamp":"t""#;
        let m: Message = serde_json::from_str(&format!("{{{base}}}")).unwrap();
        assert!(m.components.is_empty());
        assert_eq!(m.flags, 0);
        let m: Message = serde_json::from_str(&format!(r#"{{{base},"components":null}}"#)).unwrap();
        assert!(m.components.is_empty());

        let u: MessageUpdate = serde_json::from_str(r#"{"id":"1","channel_id":"2"}"#).unwrap();
        assert!(u.components.is_none());
        let u: MessageUpdate = serde_json::from_str(
            r#"{"id":"1","channel_id":"2","components":[{"type":10,"content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(u.components.map(|c| c.len()), Some(1));
    }

    #[test]
    fn roundtrip_keeps_type() {
        let m: Message = serde_json::from_str(V2_RULES).unwrap();
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        // Unknown entries serialize as `{"type": n}` and come back unchanged.
        assert_eq!(back.components, m.components);
    }
}
