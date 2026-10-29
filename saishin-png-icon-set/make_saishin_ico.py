"""Regenerates saishin.ico (plain) and saishin-tile.ico (mark on a rounded
navy tile for small-size legibility) plus a side-by-side comparison strip.

Usage:  python3 make_saishin_ico.py
"""
import io
import os
import struct

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.abspath(__file__))
SIZES = (16, 32, 48, 256)


def load(name):
    return Image.open(os.path.join(ROOT, name)).convert("RGBA")


def write_ico(path, images):
    """images: list of (edge_size, RGBA Image). Entries stored as PNG blobs."""
    blobs = []
    for size, im in images:
        if im.size != (size, size):
            im = im.resize((size, size), Image.LANCZOS)
        buf = io.BytesIO()
        im.save(buf, "PNG")
        blobs.append(buf.getvalue())
    count = len(blobs)
    header = struct.pack("<HHH", 0, 1, count)
    offset = 6 + 16 * count
    entries = b""
    for (size, _), blob in zip(images, blobs):
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    with open(path, "wb") as handle:
        handle.write(header + entries + b"".join(blobs))
    print("wrote", path)


def ico_entry(path, size):
    """Extract one size from an .ico file (Pillow size-selection trick)."""
    im = Image.open(path)
    im.size = (size, size)
    return im.convert("RGBA")


# Plain per-size renders exist in the set but are superseded by the tile.
# (Uncomment to regenerate a plain saishin.ico instead.)
# plain = [(size, load(f"saishin-{size}.png")) for size in SIZES]
# write_ico(os.path.join(ROOT, "saishin.ico"), plain)

# Tiled variant: brand-navy rounded tile, the white 最新 kanji from the
# logo on top and the S-arrow mark below. Downscaled from a 1024 master so
# every size shares the same look.
logo = Image.open(os.path.join(ROOT, "..", "saishin_logo.png")).convert("RGB")
navy = logo.getpixel((4, 4))
print("tile colour:", "#%02x%02x%02x" % navy)
kanji_crop = logo.crop((454, 649, 801, 816))
kanji_alpha = kanji_crop.convert("L").point(
    lambda v: 0 if v < 40 else min(255, (v - 40) * 255 // 160)
)
kanji_crop.save(os.path.join(ROOT, "saishin-kanji-strip.png"))
edge = 1024
tile = Image.new("RGBA", (edge, edge), (0, 0, 0, 0))
ImageDraw.Draw(tile).rounded_rectangle(
    [0, 0, edge - 1, edge - 1], radius=int(edge * 0.22), fill=navy
)
kanji_w = 520
kanji_h = int(kanji_w * kanji_crop.height / kanji_crop.width)
kanji = Image.new("RGBA", (kanji_w, kanji_h), (255, 255, 255, 0))
kanji.putalpha(kanji_alpha.resize((kanji_w, kanji_h), Image.LANCZOS))
content = Image.new("RGBA", (edge, edge), (0, 0, 0, 0))
mark = load("saishin-master-1024.png")
mark_edge = 580
mark_scaled = mark.resize((mark_edge, mark_edge), Image.LANCZOS)
# Mark on top, kanji below (same order as saishin_logo.png).
content.alpha_composite(mark_scaled, ((edge - mark_edge) // 2, 82))
content.alpha_composite(kanji, ((edge - kanji_w) // 2, 82 + mark_edge + 30))
tile = Image.alpha_composite(tile, content)
tile.save(os.path.join(ROOT, "saishin-tile-master.png"))

# Plain twin without kanji: the 16 px entry is too small for two kanji, so it
# uses the mark-only tile while 32/48/256 keep kanji + mark.
plain_edge = 1024
plain = Image.new("RGBA", (plain_edge, plain_edge), (0, 0, 0, 0))
ImageDraw.Draw(plain).rounded_rectangle(
    [0, 0, plain_edge - 1, plain_edge - 1],
    radius=int(plain_edge * 0.22),
    fill=navy,
)
plain_mark = mark.resize(
    (int(plain_edge * 0.78), int(plain_edge * 0.78)), Image.LANCZOS
)
plain_layer = Image.new("RGBA", (plain_edge, plain_edge), (0, 0, 0, 0))
plain_layer.alpha_composite(
    plain_mark,
    (
        (plain_edge - plain_mark.width) // 2,
        (plain_edge - plain_mark.height) // 2,
    ),
)
plain = Image.alpha_composite(plain, plain_layer)

write_ico(
    os.path.join(ROOT, "saishin.ico"),
    [(16, plain)] + [(size, tile) for size in (32, 48, 256)],
)

# Preview strip of the shipped icon at small sizes (4x nearest on gray).
scale = 4
gap = 12
bg = (136, 136, 136, 255)
cells = []
for size in (16, 32, 48):
    cells.append(ico_entry(os.path.join(ROOT, "saishin.ico"), size).resize(
        (size * scale, size * scale), Image.NEAREST))
width = sum(cell.width for cell in cells) + gap * (len(cells) + 1)
height = max(cell.height for cell in cells) + gap * 2
strip = Image.new("RGBA", (width, height), bg)
x = gap
for cell in cells:
    strip.alpha_composite(cell, (x, gap))
    x += cell.width + gap
# Preview of the shipped icon at its small sizes, in the temp directory: it is
# a look-at-it file, not part of the build. Windows has TEMP, macOS TMPDIR;
# the fallback keeps the repository tree clean on a shell with neither.
out = os.path.join(
    os.environ.get("TEMP") or os.environ.get("TMPDIR") or ROOT,
    "saishin-icon-compare.png",
)
strip.save(out)
print("wrote", out)
