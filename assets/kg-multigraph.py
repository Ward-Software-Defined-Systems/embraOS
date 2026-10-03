#!/usr/bin/env python3
"""Draw assets/kg-multigraph.png and .svg: the knowledge graph as it is in Phase 1.

The figure is drawn with pycairo (Ubuntu: python3-cairo), so it renders on a
build host without a browser. Run from the repository root:

    python3 assets/kg-multigraph.py

The node labels are illustrative. The numbers in the right-hand panel are
measured: the structure counts come from the 2026-10-02 backup of the
production instance (see docs/KNOWLEDGE-GRAPH.md), the timings from the
measurements recorded there.
"""
import math
import os

import cairo

W, H = 1200, 630            # logical size; the PNG is rendered at 2x
SCALE = 2
OUT = os.path.dirname(os.path.abspath(__file__))

# ---------------------------------------------------------------- palette --
BG = (0.039, 0.039, 0.059)
LANE = (1, 1, 1, 0.025)
LANE_EDGE = (1, 1, 1, 0.05)
LANE_LABEL = (0.33, 0.33, 0.41)
TEXT = (0.86, 0.86, 0.88)
DIM = (0.42, 0.37, 0.31)
ORANGE = (0.961, 0.620, 0.043)
RED = (0.937, 0.267, 0.267)
BURNT = (0.761, 0.255, 0.047)
BLUE = (0.33, 0.69, 0.85)
TEAL = (0.30, 0.75, 0.62)
LAVENDER = (0.62, 0.56, 0.86)
GREY = (0.45, 0.45, 0.52)
PANEL = (1, 1, 1, 0.03)

AUTO = (0.62, 0.52, 0.30)    # same_session / temporal / tag_overlap
DERIVED = BURNT              # derived_from
BRAIN = BLUE                 # enables / refines / depends_on / related_to
CONTRA = RED                 # contradicts
IDENT = LAVENDER             # identity relations

CATEGORY = {
    "PREFERENCE": ORANGE, "FACT": ORANGE, "DECISION": ORANGE, "PATTERN": ORANGE,
    "OBSERVATION": ORANGE, "PROCEDURAL": BURNT, "SEED": TEAL,
    "SELF": LAVENDER, "SOUL_LINE": LAVENDER, "VALUE": LAVENDER, "TRAIT": LAVENDER,
}


# ---------------------------------------------------------------- helpers --
def rgba(ctx, c, a=None):
    if len(c) == 4 and a is None:
        ctx.set_source_rgba(*c)
    else:
        ctx.set_source_rgba(c[0], c[1], c[2], 1.0 if a is None else a)


def rounded_rect(ctx, x, y, w, h, r):
    ctx.new_sub_path()
    ctx.arc(x + w - r, y + r, r, -math.pi / 2, 0)
    ctx.arc(x + w - r, y + h - r, r, 0, math.pi / 2)
    ctx.arc(x + r, y + h - r, r, math.pi / 2, math.pi)
    ctx.arc(x + r, y + r, r, math.pi, 3 * math.pi / 2)
    ctx.close_path()


def font(ctx, size, bold=False):
    ctx.select_font_face("DejaVu Sans", cairo.FONT_SLANT_NORMAL,
                         cairo.FONT_WEIGHT_BOLD if bold else cairo.FONT_WEIGHT_NORMAL)
    ctx.set_font_size(size)


def text(ctx, x, y, s, size, color, bold=False, spacing=0.0, align="left"):
    """Draw `s` with its baseline at y. `spacing` adds letter spacing."""
    font(ctx, size, bold)
    rgba(ctx, color)
    if spacing:
        width = sum(ctx.text_extents(ch).x_advance + spacing for ch in s) - spacing
    else:
        width = ctx.text_extents(s).x_advance
    if align == "center":
        x -= width / 2
    elif align == "right":
        x -= width
    if not spacing:
        ctx.move_to(x, y)
        ctx.show_text(s)
        return width
    for ch in s:
        ctx.move_to(x, y)
        ctx.show_text(ch)
        x += ctx.text_extents(ch).x_advance + spacing
    return width


def text_width(ctx, s, size, bold=False, spacing=0.0):
    font(ctx, size, bold)
    if spacing:
        return sum(ctx.text_extents(ch).x_advance + spacing for ch in s) - spacing
    return ctx.text_extents(s).x_advance


def gradient_text(ctx, x, y, s, size, spacing=0.0):
    font(ctx, size, True)
    width = text_width(ctx, s, size, True, spacing)
    grad = cairo.LinearGradient(x, y - size, x + width, y)
    grad.add_color_stop_rgb(0.0, *ORANGE)
    grad.add_color_stop_rgb(0.4, *RED)
    grad.add_color_stop_rgb(0.7, *BURNT)
    grad.add_color_stop_rgb(1.0, *ORANGE)
    ctx.set_source(grad)
    cx = x
    for ch in s:
        ctx.move_to(cx, y)
        ctx.text_path(ch)
        cx += ctx.text_extents(ch).x_advance + spacing
    ctx.fill()


def arrow_head(ctx, tip, direction, size, color):
    dx, dy = direction
    n = math.hypot(dx, dy) or 1.0
    dx, dy = dx / n, dy / n
    px, py = -dy, dx
    bx, by = tip[0] - dx * size, tip[1] - dy * size
    ctx.move_to(*tip)
    ctx.line_to(bx + px * size * 0.45, by + py * size * 0.45)
    ctx.line_to(bx - px * size * 0.45, by - py * size * 0.45)
    ctx.close_path()
    rgba(ctx, color)
    ctx.fill()


def edge(ctx, a, b, color, width=1.4, dash=None, arrow=True, bend=0.0, alpha=1.0, head=8):
    """A curved edge from a to b; `bend` offsets the control point sideways."""
    (x1, y1), (x2, y2) = a, b
    mx, my = (x1 + x2) / 2, (y1 + y2) / 2
    dx, dy = x2 - x1, y2 - y1
    n = math.hypot(dx, dy) or 1.0
    cx, cy = mx - dy / n * bend, my + dx / n * bend
    # Stop short of the tip so the head is not covered by the line.
    if arrow:
        tx, ty = x2 - cx, y2 - cy
        tn = math.hypot(tx, ty) or 1.0
        ex, ey = x2 - tx / tn * head * 0.8, y2 - ty / tn * head * 0.8
    else:
        ex, ey = x2, y2
    ctx.new_path()
    ctx.move_to(x1, y1)
    ctx.curve_to(cx, cy, cx, cy, ex, ey)
    rgba(ctx, color, alpha)
    ctx.set_line_width(width)
    ctx.set_dash(dash or [])
    ctx.stroke()
    ctx.set_dash([])
    if arrow:
        arrow_head(ctx, (x2, y2), (x2 - cx, y2 - cy), head, (*color[:3], alpha))


def node(ctx, x, y, w, h, tag, label, color, glyph=None):
    """A node box with a category tag; returns its anchors."""
    ctx.set_line_width(1.2)
    rounded_rect(ctx, x, y, w, h, 8)
    ctx.set_source_rgba(color[0], color[1], color[2], 0.10)
    ctx.fill_preserve()
    ctx.set_source_rgba(color[0], color[1], color[2], 0.75)
    ctx.stroke()
    if tag:
        tw = text_width(ctx, tag, 8, True, 0.6) + 10
        rounded_rect(ctx, x + 10, y + 8, tw, 14, 4)
        ctx.set_source_rgba(color[0], color[1], color[2], 0.22)
        ctx.fill()
        text(ctx, x + 15, y + 18.5, tag, 8, color, True, 0.6)
        text(ctx, x + 10, y + h - 10, label, 12.5, TEXT)
    else:
        text(ctx, x + 10, y + h / 2 + 4.5, label, 12.5, TEXT)
    if glyph:
        text(ctx, x + w - 8, y + 19, glyph, 8, TEAL, True, 0.3, align="right")
    return {
        "top": (x + w / 2, y), "bottom": (x + w / 2, y + h),
        "left": (x, y + h / 2), "right": (x + w, y + h / 2),
        "tl": (x + w * 0.25, y), "tr": (x + w * 0.75, y),
        "bl": (x + w * 0.25, y + h), "br": (x + w * 0.75, y + h),
    }


def lane(ctx, x, y, w, h, label):
    rounded_rect(ctx, x, y, w, h, 12)
    rgba(ctx, LANE)
    ctx.fill_preserve()
    rgba(ctx, LANE_EDGE)
    ctx.set_line_width(1)
    ctx.stroke()
    text(ctx, x + 16, y + 20, label, 9.5, LANE_LABEL, True, 1.6)


# ------------------------------------------------------------------ figure --
def draw(ctx):
    # Background with the warm glow of the original.
    rgba(ctx, BG)
    ctx.rectangle(0, 0, W, H)
    ctx.fill()
    glow = cairo.RadialGradient(600, 300, 20, 600, 300, 520)
    glow.add_color_stop_rgba(0, 0.78, 0.39, 0.08, 0.08)
    glow.add_color_stop_rgba(1, 0.78, 0.39, 0.08, 0.0)
    ctx.set_source(glow)
    ctx.rectangle(0, 0, W, H)
    ctx.fill()

    # Title bar.
    gradient_text(ctx, 40, 52, "embraOS Knowledge Graph", 28, 1.4)
    text(ctx, 40, 73,
         "One multigraph in WardSONDB: the sealed identity graph, episodic entries and promoted knowledge "
         "— retrieval by tags, text, in-OS vectors and structure",
         11.5, DIM)
    badge = "PHASE 1 · STABLE (CODE REVIEW IN PROGRESS)"
    bw = text_width(ctx, badge, 9.5, True, 1.0) + 24
    rounded_rect(ctx, W - 40 - bw, 30, bw, 22, 11)
    ctx.set_source_rgba(*ORANGE, 0.10)
    ctx.fill_preserve()
    ctx.set_source_rgba(*ORANGE, 0.25)
    ctx.set_line_width(1)
    ctx.stroke()
    text(ctx, W - 40 - bw + 12, 45.5, badge, 9.5, ORANGE, True, 1.0)

    # Lanes: the four node collections.
    LX, LW = 30, 830
    lanes = {
        "identity": (90, 86),
        "procedural": (184, 78),
        "semantic": (270, 112),
        "entries": (390, 118),
    }
    lane(ctx, LX, lanes["identity"][0], LW, lanes["identity"][1], "IDENTITY.GRAPH  ·  the sealed IDENTITY+SOUL graph, projected")
    lane(ctx, LX, lanes["procedural"][0], LW, lanes["procedural"][1], "MEMORY.PROCEDURAL")
    lane(ctx, LX, lanes["semantic"][0], LW, lanes["semantic"][1], "MEMORY.SEMANTIC")
    lane(ctx, LX, lanes["entries"][0], LW, lanes["entries"][1], "MEMORY.ENTRIES")

    # Sealed marker on the identity lane.
    seal = "sealed once · SHA-256 · verified by embra-trustd at every boot"
    text(ctx, LX + LW - 16, lanes["identity"][0] + 20, seal, 8.5, LAVENDER, False, 0.3, align="right")

    # Identity nodes.
    iy = lanes["identity"][0] + 32
    i_self = node(ctx, 70, iy, 118, 42, "SELF", "Embra", LAVENDER)
    i_val = node(ctx, 258, iy, 150, 42, "VALUE", "continuity", LAVENDER)
    i_soul = node(ctx, 478, iy, 232, 42, "SOUL_LINE", "never fabricates a source", LAVENDER)
    i_trait = node(ctx, 760, iy, 90, 42, "TRAIT", "precise", LAVENDER)
    edge(ctx, i_self["right"], i_val["left"], IDENT, 1.2, arrow=True, bend=-8, head=6)
    edge(ctx, i_val["right"], i_soul["left"], IDENT, 1.2, arrow=True, bend=-8, head=6)
    edge(ctx, i_self["br"], i_trait["left"], IDENT, 1.2, arrow=True, bend=24, head=6, alpha=0.75)
    text(ctx, 194, iy + 36, "holds_value", 7.0, IDENT, False, 0.1)
    text(ctx, 414, iy + 36, "bounded_by", 7.0, IDENT, False, 0.1)
    text(ctx, 714, iy + 36, "has_trait", 7.0, IDENT, False, 0.1)

    # Procedural nodes.
    py = lanes["procedural"][0] + 28
    p_qemu = node(ctx, 440, py, 230, 42, "PROCEDURAL", "QEMU boot debugging", BURNT, "▮ 384-d")
    p_img = node(ctx, 700, py, 155, 42, "PROCEDURAL", "Rebuilding the image", BURNT, "▮ 384-d")

    # Semantic nodes.
    sy = lanes["semantic"][0] + 44
    s_pref = node(ctx, 46, sy, 172, 42, "PREFERENCE", "User prefers dark mode", ORANGE, "▮ 384-d")
    s_fact = node(ctx, 243, sy, 183, 42, "FACT", "Rust async requires tokio", ORANGE, "▮ 384-d")
    s_dec = node(ctx, 451, sy, 196, 42, "DECISION", "SquashFS immutable rootfs", ORANGE, "▮ 384-d")
    s_seed = node(ctx, 690, sy, 165, 42, "SEED", "how my memory works", TEAL, "▮ 384-d")
    text(ctx, 855, sy + 54, "seed pack · ensured at every boot", 7.5, TEAL, False, 0.1, align="right")

    # Entries: each one under the node `remember` wrote with it, carrying the
    # same text. A seed node has no entry.
    ey = lanes["entries"][0] + 30
    e_pref = node(ctx, 46, ey, 172, 38, None, "User prefers dark mode", GREY)
    e_fact = node(ctx, 243, ey, 183, 38, None, "Rust async requires tokio", GREY)
    e_dec = node(ctx, 451, ey, 196, 38, None, "SquashFS immutable rootfs", GREY)
    e_qemu = node(ctx, 670, ey, 185, 38, None, "QEMU boot debugging", GREY)
    text(ctx, 46, ey + 54, "one entry per memory, per session — remember writes the entry and its node in one call",
         7.5, GREY, False, 0.1)
    text(ctx, 46, ey + 66, "same_session · temporal · tag_overlap: derived at write time, double-written —", 7.5, AUTO, False, 0.1)
    text(ctx, 46, ey + 78, "99.3 % of the edges; retrieval reads none of them per turn, a traversal reads 500 per node", 7.5, AUTO, False, 0.1)

    # Auto-derived edges among entries (thin, dashed).
    dash = [3, 3]
    edge(ctx, e_pref["right"], e_fact["left"], AUTO, 1.0, dash, False, bend=-10, alpha=0.8)
    edge(ctx, e_fact["right"], e_dec["left"], AUTO, 1.0, dash, False, bend=-10, alpha=0.8)
    edge(ctx, e_dec["right"], e_qemu["left"], AUTO, 1.0, dash, False, bend=-10, alpha=0.8)
    # Auto-derived edges among semantic nodes too.
    edge(ctx, s_pref["right"], s_fact["left"], AUTO, 1.0, dash, False, bend=-14, alpha=0.7)
    edge(ctx, s_fact["right"], s_dec["left"], AUTO, 1.0, dash, False, bend=14, alpha=0.7)

    # Promotion: derived_from runs node -> entry; the entry's promoted_to
    # pointer runs back up. The procedure's edge comes down through the gap
    # between the decision and the seed node.
    # The first one runs right of the lane title.
    edge(ctx, s_pref["br"], e_pref["tr"], DERIVED, 1.8, None, True)
    edge(ctx, s_fact["bottom"], e_fact["top"], DERIVED, 1.8, None, True)
    edge(ctx, s_dec["bottom"], e_dec["top"], DERIVED, 1.8, None, True)
    edge(ctx, (658, py + 42), (684, ey), DERIVED, 1.8, None, True)
    text(ctx, 183, sy + 58, "promoted_to ↑ (the entry's pointer)", 7.5, DERIVED, False, 0.2)
    text(ctx, 342, sy + 58, "derived_from", 7.5, DERIVED, False, 0.2)

    # Brain-created edges among knowledge nodes.
    edge(ctx, s_fact["top"], p_qemu["left"], BRAIN, 1.5, None, True, bend=-22)
    text(ctx, 392, sy - 22, "enables", 7.5, BRAIN, False, 0.2)
    text(ctx, 392, sy - 12, "from remember's candidates", 7.5, BRAIN, False, 0.2)
    edge(ctx, s_dec["top"], p_img["bottom"], BRAIN, 1.5, None, True, bend=-12)
    text(ctx, 706, sy - 18, "depends_on", 7.5, BRAIN, False, 0.2)
    edge(ctx, p_qemu["right"], p_img["left"], BRAIN, 1.5, None, True, bend=-8)
    text(ctx, 672, py - 4, "refines", 7.5, BRAIN, False, 0.2)
    edge(ctx, s_seed["left"], s_dec["right"], CONTRA, 1.3, [5, 4], True, bend=-14)
    text(ctx, 646, sy + 54, "contradicts", 7.5, CONTRA, False, 0.2, align="right")
    # A memory linked into the identity graph.
    # It leaves the node right of the lane title, which it used to cross.
    edge(ctx, (200, sy), i_soul["bl"], BRAIN, 1.3, None, True, bend=-40, alpha=0.8)
    text(ctx, 150, py + 44, "related_to → identity", 7.5, BRAIN, False, 0.2)

    # Right panel: measured numbers.
    PX, PY, PW, PH = 880, 90, 290, 418
    rounded_rect(ctx, PX, PY, PW, PH, 12)
    rgba(ctx, PANEL)
    ctx.fill_preserve()
    rgba(ctx, LANE_EDGE)
    ctx.set_line_width(1)
    ctx.stroke()
    text(ctx, PX + 16, PY + 22, "PRODUCTION INSTANCE", 9.5, ORANGE, True, 1.6)
    text(ctx, PX + 16, PY + 36, "backup of 2026-10-02 · measured on copies", 8, DIM)

    def row(y, big, small, lines):
        text(ctx, PX + 16, y, big, 19, TEXT, True)
        text(ctx, PX + 16 + text_width(ctx, big, 19, True) + 8, y, small, 9.5, DIM)
        for i, ln in enumerate(lines):
            text(ctx, PX + 16, y + 15 + i * 12, ln, 8.5, GREY)

    row(PY + 70, "2,641", "nodes", [
        "identity 107 · procedural 33",
        "semantic 1,213 · entries 1,288 (69 sessions)",
    ])
    row(PY + 128, "433,325", "edges", [
        "auto-derived 430,271 (99.3 %)",
        "brain-created 1,767 · derived_from 1,261",
        "identity relations 354 · seed 73",
    ])
    row(PY + 198, "1,246", "vectors · 384-d · ~1.9 MB", [
        "bge-small-en-v1.5 in the OS (tract, CLS)",
        "brute-force cosine over all of them: < 1 ms",
    ])
    row(PY + 256, "212 MB", "on disk (fjall)", [
        "5 indexes on memory.edges",
    ])
    text(ctx, PX + 16, PY + 304, "PER TURN", 9.5, ORANGE, True, 1.6)
    per = [
        ("enrichment, end to end", "0.10 – 0.16 s"),
        ("query embedding", "~14 ms"),
        ("traversal hop (indexed)", "0.3 – 1.5 ms"),
        ("node embedding, in the OS", "~55 ms"),
        ("boot reconcile edge reads", "index-served"),
    ]
    for i, (k, v) in enumerate(per):
        y = PY + 322 + i * 14
        text(ctx, PX + 16, y, k, 8.5, GREY)
        text(ctx, PX + PW - 16, y, v, 8.5, TEXT, True, align="right")

    # Legend.
    ly = 526
    items = [
        (AUTO, [3, 3], False, "same_session · temporal · tag_overlap"),
        (DERIVED, None, True, "derived_from"),
        (BRAIN, None, True, "enables · refines · depends_on · related_to"),
        (CONTRA, [5, 4], True, "contradicts"),
        (IDENT, None, True, "identity relation"),
    ]
    x = 40
    for color, d, arr, label in items:
        edge(ctx, (x, ly), (x + 26, ly), color, 1.6, d, arr, head=6)
        x += 32
        x += text(ctx, x, ly + 3.5, label, 8.5, GREY) + 18
    text(ctx, x, ly + 3.5, "▮ 384-d", 8, TEAL, True, 0.3)
    x += text_width(ctx, "▮ 384-d", 8, True, 0.3) + 6
    text(ctx, x, ly + 3.5, "vector on the node", 8.5, GREY)
    text(ctx, 40, ly + 19,
         "Writing: remember saves the entry and its node in one call, embeds the node, and returns its nearest nodes "
         "(cosine ≥ 0.75) for the intelligence to link.",
         8.5, DIM)
    text(ctx, 40, ly + 32,
         "Retrieval on every turn: tag match, IDF-weighted text match, cosine over the vector index, session adjacency — "
         "then relevance·0.6 + recency·0.2 + access·0.2, a relevance floor, and at most five nodes injected.",
         8.5, DIM)

    # Bottom bar.
    ctx.set_source_rgba(1, 1, 1, 0.04)
    ctx.set_line_width(1)
    ctx.move_to(0, 584)
    ctx.line_to(W, 584)
    ctx.stroke()
    text(ctx, 40, 610, "Ward Software Defined Systems LLC · wsds.io", 12, (0.23, 0.23, 0.29))
    text(ctx, W - 40, 610, "Phase 1 · WardSONDB · Rust · tract + bge-small-en-v1.5", 11,
         (0.29, 0.23, 0.16), spacing=0.5, align="right")


def main():
    png = cairo.ImageSurface(cairo.FORMAT_ARGB32, W * SCALE, H * SCALE)
    ctx = cairo.Context(png)
    ctx.scale(SCALE, SCALE)
    draw(ctx)
    png.write_to_png(os.path.join(OUT, "kg-multigraph.png"))

    svg = cairo.SVGSurface(os.path.join(OUT, "kg-multigraph.svg"), W, H)
    draw(cairo.Context(svg))
    svg.finish()


if __name__ == "__main__":
    main()
