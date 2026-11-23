//! Image files for `--demo` mode: avatars, server icons, photos, GIFs and custom emoji under
//! `assets/demo/` (sources and licenses in `assets/demo/LICENSES.md`).
//!
//! They are read from disk at runtime, never compiled in, so the normal exe does not grow.
//! The folder is looked up relative to the current directory, then next to the exe and in
//! its ancestors (so `cargo run` from anywhere in the repo finds it). When it is missing,
//! every lookup here returns `None` and `Media` falls back to the procedural images of
//! `nixcord_media::demo`.
//!
//! Demo users and servers carry fake avatar / icon hashes; the tables below map them to
//! files. Attachment and embed URLs map by their file name (`.../desk-setup.jpg` ->
//! `photos/desk-setup.jpg`), Tenor gifv embeds by their media id.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `(file under avatars/, fake avatar hash)`.
const AVATARS: &[(&str, &str)] = &[
    ("you.webp", "6e1f3a9c0b7d4e25a8c1f09d3b6e7a42"),
    ("hearthbot.png", "b07c4e1a9f2d3c58e6a0b1d47f9c2e35"),
    ("sol.png", "3c9a1e7f5b2d0c84a6e9f1b3d5c7a028"),
    ("quill.png", "9d2e4b6a1c8f3e07b5d9a2c4e6f8b130"),
    ("ferris.png", "f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8"),
    ("mira.png", "5a7c9e1b3d5f7a9c1e3b5d7f9a1c3e57"),
    ("tamsin.webp", "8b6d4f2a0c8e6b4d2f0a8c6e4b2d0f86"),
    ("otto.png", "2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c24"),
    // Pool for the generated member-list users.
    ("aurora.png", "a1b2c3d4e5f60718293a4b5c6d7e8f90"),
    ("basil.png", "0f1e2d3c4b5a69788796a5b4c3d2e1f0"),
    ("cinder.png", "c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6"),
    ("ember.png", "e5d4c3b2a1f0e9d8c7b6a5f4e3d2c1b0"),
    ("fjord.png", "f00dfacec0ffee12345678909abcdef1"),
    ("juniper.png", "1a3c5e7b9d0f2a4c6e8b0d2f4a6c8e0b"),
    ("kestrel.png", "7e9c1a3f5d7b9e1c3a5f7d9b1e3c5a7f"),
    ("sheltie.webp", "4d6f8b0a2c4e6d8f0b2a4c6e8d0f2b4a"),
    ("willow.png", "d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4"),
];

/// Avatars the generated member-list users cycle through (some get none, like on Discord).
const POOL: &[&str] = &[
    "aurora.png",
    "basil.png",
    "cinder.png",
    "ember.png",
    "fjord.png",
    "juniper.png",
    "kestrel.png",
    "sheltie.webp",
    "willow.png",
];

/// `(file under icons/, fake server icon hash)`.
const ICONS: &[(&str, &str)] = &[
    ("hearth.png", "0b1a2c3d4e5f60718293a4b5c6d7e8f9"),
    ("rust-lounge.png", "71c0de5e7a2b4d6f8091a2b3c4d5e6f7"),
    ("pixel-art-club.png", "9a8b7c6d5e4f30211203f4e5d6c7b8a9"),
    ("synthwave.png", "5ca1ab1e0ddba11c0ffee0ddf00dcafe"),
    ("coffee-break.webp", "c0ffeebeefcafe0123456789abcdef01"),
];

/// `(custom emoji id, file under emoji/)`. Ids match `demo::demo_guild_emojis`.
const EMOJI: &[(u64, &str)] = &[
    (1_150_000_000_000_000_001, "ferris.png"),
    (1_150_000_000_000_000_002, "party.gif"),
    (1_150_000_000_000_000_003, "hearth.png"),
    (1_150_000_000_000_000_004, "shipit.png"),
    (1_150_000_000_000_000_005, "heart.png"),
    (1_150_000_000_000_000_006, "think.png"),
    (1_150_000_000_000_000_007, "catjam.gif"),
    (1_150_000_000_000_000_008, "kekw.png"),
    (1_150_000_000_000_000_009, "pog.png"),
    (1_150_000_000_000_000_010, "sadge.png"),
    (1_150_000_000_000_000_011, "locked.png"),
    (1_150_000_000_000_000_022, "rustacean.png"),
    (1_150_000_000_000_000_023, "unsafe.png"),
    (1_150_000_000_000_000_024, "borrowck.png"),
];

/// `(Tenor media id prefix, file under gifs/)` for gifv embeds.
const TENOR: &[(&str, &str)] = &[("nixDemoHorseGallop", "horse.gif")];

/// Folders searched for attachment / embed files, by URL file name.
const URL_DIRS: &[&str] = &["photos", "gifs", "icons"];

/// `assets/demo`, if it can be found.
pub fn root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(find_root).as_deref()
}

fn find_root() -> Option<PathBuf> {
    let sub = Path::new("assets").join("demo");
    let cwd = std::env::current_dir().ok().map(|d| d.join(&sub));
    let exe = std::env::current_exe().ok();
    let beside_exe = exe
        .iter()
        .flat_map(|e| e.ancestors().skip(1))
        .map(|d| d.join(&sub));
    let found = cwd.into_iter().chain(beside_exe).find(|p| p.is_dir());
    if found.is_none() {
        tracing::info!("assets/demo not found; demo images are generated");
    }
    found
}

/// `root/dir/file`, if it exists.
fn file(dir: &str, name: &str) -> Option<PathBuf> {
    let p = root()?.join(dir).join(name);
    p.is_file().then_some(p)
}

/// Fake avatar hash of the asset `name` (e.g. `"sol.png"`).
pub fn avatar_hash(name: &str) -> Option<String> {
    AVATARS
        .iter()
        .find(|(f, _)| *f == name)
        .map(|(_, h)| (*h).to_owned())
}

/// Avatar hash for the `i`-th generated member: cycles through the pool, every fifth
/// member has none (Discord's default avatar).
pub fn pool_avatar(i: usize) -> Option<String> {
    if i % 5 == 4 {
        return None;
    }
    avatar_hash(POOL[(i - i / 5) % POOL.len()])
}

/// Fake icon hash of the server icon asset `name`.
pub fn icon_hash(name: &str) -> Option<String> {
    ICONS
        .iter()
        .find(|(f, _)| *f == name)
        .map(|(_, h)| (*h).to_owned())
}

/// Avatar file for a fake avatar hash.
pub fn avatar_file(hash: &str) -> Option<PathBuf> {
    let (name, _) = AVATARS.iter().find(|(_, h)| *h == hash)?;
    file("avatars", name)
}

/// Server icon file for a fake icon hash.
pub fn icon_file(hash: &str) -> Option<PathBuf> {
    let (name, _) = ICONS.iter().find(|(_, h)| *h == hash)?;
    file("icons", name)
}

/// Custom emoji file for an emoji id.
pub fn emoji_file(id: u64) -> Option<PathBuf> {
    let (_, name) = EMOJI.iter().find(|(i, _)| *i == id)?;
    file("emoji", name)
}

/// File for an attachment / embed image URL (see the module docs).
pub fn url_file(url: &str) -> Option<PathBuf> {
    if let Some(base) = nixcord_media::anim_url::tenor_base(url) {
        if let Some((_, name)) = TENOR.iter().find(|(id, _)| base.starts_with(id)) {
            return file("gifs", name);
        }
    }
    let name = nixcord_media::url::file_name(url);
    if name.is_empty() {
        return None;
    }
    URL_DIRS.iter().find_map(|dir| file(dir, &name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_consistent() {
        let mut hashes: Vec<&str> = AVATARS.iter().chain(ICONS).map(|(_, h)| *h).collect();
        let n = hashes.len();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), n, "hashes are unique");
        assert!(POOL.iter().all(|p| avatar_hash(p).is_some()));
        assert!((0..40).any(|i| pool_avatar(i).is_none()));
        assert!((0..40).filter_map(pool_avatar).count() >= 30);
    }

    /// Table entries with no file yet: they keep the procedural fallback image.
    const NOT_SOURCED: &[&str] = &["you.webp", "tamsin.webp", "coffee-break.webp"];

    /// Every file named in the tables exists in the repo's `assets/demo`.
    #[test]
    fn files_exist() {
        let Some(root) = root() else {
            return; // running outside the repo
        };
        let check = |dir: &str, name: &str| {
            if NOT_SOURCED.contains(&name) {
                return;
            }
            assert!(root.join(dir).join(name).is_file(), "missing {dir}/{name}");
        };
        let photos = [
            "lake-bohinj.jpg",
            "cat-on-keyboard.jpg",
            "desk-setup.jpg",
            "hearth-banner.jpg",
            "nixcord-chat.jpg",
            "nixcord-dms.jpg",
        ];
        photos.iter().for_each(|f| check("photos", f));
        // The photos decode at the natural sizes `demo.rs` declares.
        for (f, size) in [
            ("lake-bohinj.jpg", (1200, 801)),
            ("cat-on-keyboard.jpg", (1200, 675)),
            ("desk-setup.jpg", (1200, 800)),
            ("hearth-banner.jpg", (1200, 400)),
            ("nixcord-chat.jpg", (1200, 675)),
        ] {
            let bytes = std::fs::read(root.join("photos").join(f)).unwrap();
            let img = nixcord_media::decode(&bytes, 4096, 4096).unwrap();
            assert_eq!((img.width, img.height), size, "{f}");
        }
        ["cat-gallop.gif", "loading.gif"]
            .iter()
            .for_each(|f| check("gifs", f));
        check("icons", "mod-badge.png");
        AVATARS.iter().for_each(|(f, _)| check("avatars", f));
        ICONS.iter().for_each(|(f, _)| check("icons", f));
        EMOJI.iter().for_each(|(_, f)| check("emoji", f));
        TENOR.iter().for_each(|(_, f)| check("gifs", f));
        let h = avatar_hash("sol.png").unwrap();
        assert!(avatar_file(&h).is_some());
        assert!(url_file("https://media.tenor.com/nixDemoHorseGallopAx/tenor.webp").is_some());
        assert!(url_file("https://cdn.discordapp.com/attachments/1/2/desk-setup.jpg").is_some());
        assert!(url_file("https://example.com/nothing-here.png").is_none());
    }
}
