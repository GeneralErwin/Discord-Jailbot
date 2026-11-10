"""Create the static Windows XP-inspired banner used by the prisoner roster."""

from pathlib import Path
import struct
import zlib


WIDTH, HEIGHT = 900, 270
pixels = bytearray(WIDTH * HEIGHT * 3)


def color(hex_color):
    value = hex_color.lstrip("#")
    return tuple(int(value[index:index + 2], 16) for index in (0, 2, 4))


def rect(x, y, width, height, fill):
    rgb = color(fill) if isinstance(fill, str) else fill
    left, top = max(0, x), max(0, y)
    right, bottom = min(WIDTH, x + width), min(HEIGHT, y + height)
    for row in range(top, bottom):
        start = (row * WIDTH + left) * 3
        pixels[start:start + (right - left) * 3] = bytes(rgb) * (right - left)


def frame(x, y, width, height, fill, light="#FFFFFF", dark="#315477"):
    rect(x, y, width, height, fill)
    rect(x, y, width, 2, light)
    rect(x, y, 2, height, light)
    rect(x, y + height - 2, width, 2, dark)
    rect(x + width - 2, y, 2, height, dark)


FONT = {
    "A": ("01110", "10001", "10001", "11111", "10001", "10001", "10001"),
    "B": ("11110", "10001", "10001", "11110", "10001", "10001", "11110"),
    "C": ("01111", "10000", "10000", "10000", "10000", "10000", "01111"),
    "D": ("11110", "10001", "10001", "10001", "10001", "10001", "11110"),
    "E": ("11111", "10000", "10000", "11110", "10000", "10000", "11111"),
    "F": ("11111", "10000", "10000", "11110", "10000", "10000", "10000"),
    "G": ("01111", "10000", "10000", "10111", "10001", "10001", "01111"),
    "H": ("10001", "10001", "10001", "11111", "10001", "10001", "10001"),
    "I": ("11111", "00100", "00100", "00100", "00100", "00100", "11111"),
    "J": ("00111", "00010", "00010", "00010", "10010", "10010", "01100"),
    "K": ("10001", "10010", "10100", "11000", "10100", "10010", "10001"),
    "L": ("10000", "10000", "10000", "10000", "10000", "10000", "11111"),
    "M": ("10001", "11011", "10101", "10101", "10001", "10001", "10001"),
    "N": ("10001", "11001", "10101", "10011", "10001", "10001", "10001"),
    "O": ("01110", "10001", "10001", "10001", "10001", "10001", "01110"),
    "P": ("11110", "10001", "10001", "11110", "10000", "10000", "10000"),
    "Q": ("01110", "10001", "10001", "10001", "10101", "10010", "01101"),
    "R": ("11110", "10001", "10001", "11110", "10100", "10010", "10001"),
    "S": ("01111", "10000", "10000", "01110", "00001", "00001", "11110"),
    "T": ("11111", "00100", "00100", "00100", "00100", "00100", "00100"),
    "U": ("10001", "10001", "10001", "10001", "10001", "10001", "01110"),
    "V": ("10001", "10001", "10001", "10001", "10001", "01010", "00100"),
    "W": ("10001", "10001", "10001", "10101", "10101", "10101", "01010"),
    "X": ("10001", "10001", "01010", "00100", "01010", "10001", "10001"),
    "Y": ("10001", "10001", "01010", "00100", "00100", "00100", "00100"),
    "Z": ("11111", "00001", "00010", "00100", "01000", "10000", "11111"),
    "0": ("01110", "10001", "10011", "10101", "11001", "10001", "01110"),
    "1": ("00100", "01100", "00100", "00100", "00100", "00100", "01110"),
    "2": ("01110", "10001", "00001", "00010", "00100", "01000", "11111"),
    "3": ("11110", "00001", "00001", "01110", "00001", "00001", "11110"),
    "4": ("00010", "00110", "01010", "10010", "11111", "00010", "00010"),
    "5": ("11111", "10000", "10000", "11110", "00001", "00001", "11110"),
    "6": ("01110", "10000", "10000", "11110", "10001", "10001", "01110"),
    "7": ("11111", "00001", "00010", "00100", "01000", "01000", "01000"),
    "8": ("01110", "10001", "10001", "01110", "10001", "10001", "01110"),
    "9": ("01110", "10001", "10001", "01111", "00001", "00001", "01110"),
    ":": ("00000", "00100", "00100", "00000", "00100", "00100", "00000"),
    "/": ("00001", "00010", "00010", "00100", "01000", "01000", "10000"),
    "-": ("00000", "00000", "00000", "11111", "00000", "00000", "00000"),
    ".": ("00000", "00000", "00000", "00000", "00000", "00110", "00110"),
    "|": ("00100", "00100", "00100", "00100", "00100", "00100", "00100"),
    "[": ("01110", "01000", "01000", "01000", "01000", "01000", "01110"),
    "]": ("01110", "00010", "00010", "00010", "00010", "00010", "01110"),
    "_": ("00000", "00000", "00000", "00000", "00000", "00000", "11111"),
    "#": ("01010", "11111", "01010", "01010", "11111", "01010", "00000"),
    "?": ("01110", "10001", "00001", "00010", "00100", "00000", "00100"),
}


def text(x, y, value, fill="#FFFFFF", scale=2):
    cursor = x
    for character in value.upper():
        if character == " ":
            cursor += 4 * scale
            continue
        glyph = FONT.get(character, FONT["?"])
        for row, bits in enumerate(glyph):
            for column, bit in enumerate(bits):
                if bit == "1":
                    rect(cursor + column * scale, y + row * scale, scale, scale, fill)
        cursor += 6 * scale


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


# XP blue title bar, silver menu strip, and inset CRT-style custody panel.
rect(0, 0, WIDTH, HEIGHT, "#17375E")
frame(8, 6, 884, 258, "#D6D3CE", "#FFFFFF", "#3B5268")
for y in range(10, 40):
    ratio = (y - 10) / 30
    blue = (int(0 + 34 * ratio), int(72 + 116 * ratio), int(190 + 61 * ratio))
    rect(12, y, 876, 1, blue)

# Small monitor glyph in the title bar.
frame(21, 16, 20, 18, "#C6E6F5", "#FFFFFF", "#1B3857")
rect(24, 19, 14, 10, "#123B60")
rect(28, 31, 6, 2, "#4D6178")
text(49, 20, "PRISONER CONTROL CENTER", "#FFFFFF", 2)
for index, fill in enumerate(("#7FF080", "#FFE47A", "#FF7777")):
    rect(823 + index * 20, 17, 15, 14, fill)
    rect(825 + index * 20, 19, 11, 2, "#FFFFFF")

rect(12, 40, 876, 23, "#ECE9D8")
text(24, 48, "FILE   VIEW   CUSTODY   HELP", "#25364A", 1)
rect(12, 63, 876, 1, "#FFFFFF")

frame(22, 72, 183, 155, "#E8E7E2", "#FFFFFF", "#77766F")
rect(25, 75, 177, 20, "#245EDB")
text(33, 82, "SYSTEM STATUS", "#FFFFFF", 1)
frame(35, 106, 155, 55, "#101A20", "#888888", "#FFFFFF")
text(46, 115, "SECURE", "#64F58C", 2)
text(47, 143, "ACCESS: STAFF", "#B0F3C1", 1)
rect(36, 173, 12, 12, "#43C867")
text(55, 176, "DATABASE LINK", "#25364A", 1)
rect(36, 196, 12, 12, "#44A9F2")
text(55, 199, "RECORDS READY", "#25364A", 1)

frame(215, 72, 661, 155, "#101B20", "#FFFFFF", "#77766F")
rect(220, 77, 651, 23, "#183544")
text(233, 85, "ACTIVE DETENTION ROSTER", "#88FFAA", 1)
text(232, 111, "MEMBER RECORDS", "#88FFAA", 1)
text(417, 111, "OFFENSE", "#88FFAA", 1)
text(585, 111, "ARRESTED", "#88FFAA", 1)
text(746, 111, "EXPIRY", "#88FFAA", 1)
rect(229, 123, 633, 1, "#3A6652")
for y in (135, 157, 179):
    text(232, y, "SELECT A PRISONER BELOW TO INSPECT RECORD", "#66C989", 1)
    rect(228, y + 15, 636, 1, "#263D36")

rect(12, 232, 876, 26, "#ECE9D8")
rect(12, 232, 876, 1, "#FFFFFF")
text(23, 242, "ACTIVE CUSTODY  |  PAGE NAVIGATION READY", "#26384D", 1)
text(758, 242, "CONNECTED", "#167A3B", 1)

raw_rows = b"".join(b"\x00" + pixels[row * WIDTH * 3:(row + 1) * WIDTH * 3] for row in range(HEIGHT))
png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">2I5B", WIDTH, HEIGHT, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(raw_rows, 9))
png += chunk(b"IEND", b"")

destination = Path(__file__).with_name("prisoner_terminal.png")
destination.write_bytes(png)
print(f"Wrote {destination} ({len(png)} bytes)")
