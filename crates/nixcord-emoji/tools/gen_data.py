#!/usr/bin/env python3
"""Generate crates/nixcord-emoji/src/data.rs from emojibase-data (MIT).

Usage:
    python -X utf8 tools/gen_data.py [path/to/emojibase-data/package]

Without a path, emojibase-data@VERSION is downloaded from the npm registry into a
temporary directory. Inputs used: en/data.json (labels, tags, groups, order, skins,
presentation) and en/shortcodes/{joypixels,iamcal,emojibase}.json.

Output: one string table, one record per emoji, sorted by Discord category and then by
emojibase order. Fields are tab-separated, lists are space-separated:

    emoji \t flags \t shortcodes \t label \t keywords

flags is three characters: category digit (Discord order 0-7), skin-tone mask (hex: bit i
= the i-th ZWJ component takes the modifier) and presentation ('t' = text by default,
needs U+FE0F to show as emoji; 'e' otherwise).

Skin-tone variants are not separate records: the mask describes where the modifier goes,
and every mask is checked against emojibase's five variants before it is written.
"""

import io
import json
import os
import sys
import tarfile
import tempfile
import urllib.request

VERSION = "18.0.0"
TARBALL = f"https://registry.npmjs.org/emojibase-data/-/emojibase-data-{VERSION}.tgz"

# emojibase group -> Discord category (people, nature, food, activities, travel,
# objects, symbols, flags). Group 2 (components: bare skin tones, hair) is skipped.
GROUP_TO_CAT = {0: 0, 1: 0, 3: 1, 4: 2, 6: 3, 5: 4, 7: 5, 8: 6, 9: 7}

ZWJ = 0x200D
VS16 = 0xFE0F
TONES = [0x1F3FB, 0x1F3FC, 0x1F3FD, 0x1F3FE, 0x1F3FF]
MAX_NAMES = 4
MAX_TAGS = 6

# In emojibase but without an image in Twemoji v17.0.3 (checked against jsDelivr): left
# out so the picker never offers an emoji that can only show as a box.
NO_TWEMOJI = {"1FAEB", "1FAF9", "1FAFA", "1FACC", "1FADD", "1F6D9", "1FA8B", "1FA8C", "1FA8D"}


def cps(hexcode):
    return [int(h, 16) for h in hexcode.split("-")]


def apply_skin(seq, mask, tone):
    """Mirror of nixcord_emoji::with_skin_tone: put `tone` after the first code point of
    every ZWJ component whose bit is set in `mask`, dropping a U+FE0F right after it."""
    comps, cur = [], []
    for c in seq:
        if c == ZWJ:
            comps.append(cur)
            cur = []
        else:
            cur.append(c)
    comps.append(cur)
    out = []
    for i, comp in enumerate(comps):
        if i:
            out.append(ZWJ)
        if mask >> i & 1 and comp:
            rest = comp[1:]
            if rest and rest[0] == VS16:
                rest = rest[1:]
            out.extend([comp[0], tone] + rest)
        else:
            out.extend(comp)
    return out


def skin_mask(e):
    skins = e.get("skins") or []
    # Single-tone variants only (multi-person emoji also list mixed tones).
    by_tone = {}
    for s in skins:
        seq = cps(s["hexcode"])
        tones = {c for c in seq if c in TONES}
        if len(tones) == 1:
            by_tone.setdefault(tones.pop(), seq)
    if len(by_tone) != 5:
        return 0
    base = cps(e["hexcode"])
    sample = by_tone[TONES[0]]
    # Which ZWJ components carry the modifier in the tone-1 variant?
    mask, i, comp_start = 0, 0, True
    for c in sample:
        if c == ZWJ:
            i += 1
        elif c in TONES:
            mask |= 1 << i
    if mask == 0 or mask > 0xF:
        return 0
    for t in TONES:
        if apply_skin(base, mask, t) != by_tone[t]:
            return 0
    return mask


def as_list(v):
    if v is None:
        return []
    return v if isinstance(v, list) else [v]


def load(root):
    with open(os.path.join(root, "en", "data.json"), encoding="utf8") as f:
        data = json.load(f)
    sc = {}
    for preset in ("joypixels", "iamcal", "emojibase"):
        with open(os.path.join(root, "en", "shortcodes", preset + ".json"), encoding="utf8") as f:
            sc[preset] = json.load(f)
    return data, sc


def shortcodes(hexcode, sc):
    # Discord's names follow JoyPixels (emojione), e.g. heart/red_heart, flag_jp,
    # thumbsup. iamcal fills gaps (skipping its dash-style names); emojibase is the
    # last resort so every record has at least one name.
    names = []
    for n in as_list(sc["joypixels"].get(hexcode)):
        names.append(n)
    for n in as_list(sc["iamcal"].get(hexcode)):
        if "-" not in n:
            names.append(n)
    if not names:
        names = as_list(sc["emojibase"].get(hexcode))
    out = []
    for n in names:
        n = n.lower()
        if n not in out and " " not in n and "\t" not in n:
            out.append(n)
    return out[:MAX_NAMES]


def main():
    if len(sys.argv) > 1:
        root = sys.argv[1]
        data, sc = load(root)
    else:
        with tempfile.TemporaryDirectory() as tmp:
            raw = urllib.request.urlopen(TARBALL).read()
            with tarfile.open(fileobj=io.BytesIO(raw)) as tar:
                tar.extractall(tmp, filter="data")
            data, sc = load(os.path.join(tmp, "package"))

    rows = []
    seen_names = set()
    for e in data:
        g = e.get("group")
        if g not in GROUP_TO_CAT:
            continue
        hexcode = e["hexcode"]
        if hexcode in NO_TWEMOJI:
            continue
        names = [n for n in shortcodes(hexcode, sc) if n not in seen_names]
        if not names:
            continue
        seen_names.update(names)
        seq = cps(hexcode)
        # emojibase's `emoji` is the fully-qualified form (with U+FE0F where needed).
        emoji = e["emoji"]
        text_default = e.get("type") == 0 and len([c for c in seq if c != VS16]) == 1
        mask = skin_mask(e)
        label = e["label"].replace("\t", " ")
        tags = []
        lw = set(label.lower().split())
        for t in e.get("tags", []):
            t = t.lower().replace(" ", "_")
            if t in lw or t in names or t in tags:
                continue
            tags.append(t)
        flags = f"{GROUP_TO_CAT[g]}{mask:x}{'t' if text_default else 'e'}"
        rows.append((GROUP_TO_CAT[g], e.get("order", 0), emoji, flags, names, label, tags[:MAX_TAGS]))

    rows.sort(key=lambda r: (r[0], r[1]))
    lines = []
    for _, _, emoji, flags, names, label, tags in rows:
        lines.append("\t".join([emoji, flags, " ".join(names), label, " ".join(tags)]))
    body = "\n".join(lines)
    assert '"#' not in body

    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.join(here, "..", "src", "data.rs")
    with open(out, "w", encoding="utf8", newline="\n") as f:
        f.write("// @generated by tools/gen_data.py from emojibase-data " + VERSION + " (MIT).\n")
        f.write("// Do not edit by hand. Format: see tools/gen_data.py.\n\n")
        f.write(f"pub(crate) const COUNT: usize = {len(rows)};\n\n")
        f.write('pub(crate) const DATA: &str = r#"')
        f.write(body)
        f.write('"#;\n')
    print(f"{len(rows)} emoji, {len(body.encode('utf8'))} bytes -> {os.path.normpath(out)}")


if __name__ == "__main__":
    main()
