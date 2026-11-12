"""A guided matte's reference encoder and decoder, and the conformance vector `guided_matte_tests` checks.

The encoder follows Miroir's (`guided_matte::split`). The decoder is the format's, at full precision:
`rebuild_alpha.wgsl` follows it step for step, its intermediates at 8 bits.
`python reference.py` rewrites `guided_matte.{picture,matte,expected}.bin` beside it (numpy only).
"""
from pathlib import Path

import numpy as np

KR, KB = 0.2126, 0.0722
BACKDROPS = (0.0, 0.5, 1.0)
BLOCK, BINS, CONTENT, OUTLIERS = 4, 32, 13, 0.05
LIKENESS, RIDGE, TIE = 0.1, 1e-4, 1e-6


def limited_luma(v):
    return np.clip(np.round(16 + 219 * v), 0, 255).astype(np.uint8)


def limited_chroma(v):
    return np.clip(np.round(128 + 224 * v), 0, 255).astype(np.uint8)


def ycbcr(rgb):
    y = KR * rgb[..., 0] + (1 - KR - KB) * rgb[..., 1] + KB * rgb[..., 2]
    return y, (rgb[..., 2] - y) / (2 * (1 - KB)), (rgb[..., 0] - y) / (2 * (1 - KR))


def mean2(m):
    return 0.25 * (m[0::2, 0::2] + m[0::2, 1::2] + m[1::2, 0::2] + m[1::2, 1::2])


def backdrops(luma, alpha8):
    """Each 4×4 block's backdrop: farthest from its 3×3 blocks' content luma but for the nearest 5%."""
    h, w = alpha8.shape
    rows, columns = h // BLOCK, w // BLOCK
    histograms = np.zeros((rows, columns, BINS), np.int32)
    seen = np.zeros((rows, columns), bool)
    for y in range(h):
        for x in range(w):
            a = alpha8[y, x]
            seen[y // BLOCK, x // BLOCK] |= a < 255
            if a >= CONTENT:
                histograms[y // BLOCK, x // BLOCK, min(int(luma[y, x] / (a / 255) * BINS), BINS - 1)] += 1
    centres = (np.arange(BINS) + 0.5) / BINS
    keys = np.zeros((rows, columns))
    for r in range(rows):
        for c in range(columns):
            near = (slice(max(r - 1, 0), r + 2), slice(max(c - 1, 0), c + 2))
            if not seen[near].any():
                continue
            histogram = histograms[near].sum((0, 1))
            outliers = int(histogram.sum() * OUTLIERS)
            best = None
            for level in BACKDROPS:
                order = sorted(range(BINS), key=lambda b: abs(centres[b] - level))
                distance, total = 0.0, 0
                for b in order:
                    total += histogram[b]
                    if total > outliers:
                        distance = abs(centres[b] - level)
                        break
                if best is None or distance > best[1]:
                    best = (level, distance)
            keys[r, c] = best[0]
    return keys


def encode(premultiplied8):
    """Premultiplied RGBA8, sides multiples of 4 -> picture (Y, U, V) and matte (Y, U, V) planes."""
    rgb = premultiplied8[..., :3] / 255.0
    alpha = premultiplied8[..., 3] / 255.0
    luma, cb, cr = ycbcr(rgb)
    keys = backdrops(luma, premultiplied8[..., 3])
    key = np.kron(keys, np.ones((BLOCK, BLOCK)))
    opacity = mean2(alpha)
    safe = np.maximum(opacity, 1e-12)
    picture = (limited_luma(luma + (1 - alpha) * key),
               limited_chroma(np.where(opacity > 0, mean2(cb) / safe, 0)),
               limited_chroma(np.where(opacity > 0, mean2(cr) / safe, 0)))
    matte = (limited_luma(opacity), limited_chroma(keys - 0.5), np.full(keys.shape, 128, np.uint8))
    return picture, matte


def decode(picture, matte):
    """The planes -> straight sRGB-encoded colour and alpha."""
    y8, u8, v8 = picture
    m8, mu8 = matte[0], matte[1]
    H, W = y8.shape
    h, w = m8.shape
    luma = (y8 - 16.0) / 219.0
    clamp = lambda a, n: np.clip(a, 0, n - 1)  # noqa: E731
    backdrop = lambda ty, tx: np.clip(np.round((mu8[clamp(ty // 2, h // 2), clamp(tx // 2, w // 2)] - 16.0) / 112.0), 0, 2) * 0.5  # noqa: E731
    mean_alpha = lambda ty, tx: np.clip((m8[clamp(ty, h), clamp(tx, w)] - 16.0) / 219.0, 0, 1)  # noqa: E731

    def quad(ty, tx):
        py, px = 2 * ty, 2 * tx
        return np.stack([luma[py, px], luma[py, px + 1], luma[py + 1, px], luma[py + 1, px + 1]], -1)

    ty, tx = np.mgrid[0:h, 0:w]
    sums = np.zeros((8, h, w))  # luma: guide, ·p, ²; distance: guide, ·p, ²; p, p²
    for dy in (-1, 0, 1):
        for dx in (-1, 0, 1):
            ny, nx = clamp(ty + dy, h), clamp(tx + dx, w)
            q, p = quad(ny, nx), mean_alpha(ny, nx) - 0.5
            l, d = q.mean(-1) - 0.5, np.abs(q - backdrop(ny, nx)[..., None]).mean(-1) - 0.5
            sums += (2 - abs(dx)) * (2 - abs(dy)) / 16.0 * np.stack([l, l * p, l * l, d, d * p, d * d, p, p * p])

    def fit(g, gp, gg, guide):
        covariance = gp - g * sums[6]
        slope = covariance / (gg - g * g + RIDGE)
        residual = sums[7] - sums[6] ** 2 - slope * covariance
        values = np.clip(slope[..., None] * (guide - 0.5 - g[..., None]) + sums[6][..., None] + 0.5, 0, 1)
        return values, residual

    lumas = quad(ty, tx)
    by_luma, luma_residual = fit(*sums[0:3], lumas)
    by_distance, distance_residual = fit(*sums[3:6], np.abs(lumas - backdrop(ty, tx)[..., None]))
    chosen = np.where((distance_residual <= luma_residual + TIE)[..., None], by_distance, by_luma)
    share = (chosen + 1e-3) / (chosen + 1e-3).sum(-1, keepdims=True)
    alphas = 4 * mean_alpha(ty, tx)[..., None] * share
    for _ in range(2):
        excess = np.maximum(alphas - 1, 0).sum(-1, keepdims=True)
        alphas = np.minimum(alphas, 1)
        open_ = np.where(alphas < 1, share, 0)
        alphas += excess * open_ / np.maximum(open_.sum(-1, keepdims=True), 1e-6)
    alphas = np.clip(alphas, 0, 1)
    key = backdrop(ty, tx)[..., None]
    unkeyed = np.clip((lumas - key * (1 - alphas)) / np.maximum(alphas, 1e-3), 0, 1)
    mass = mean_alpha(ty, tx)
    cell_luma = np.clip((lumas.mean(-1) - key[..., 0] * (1 - mass)) / np.maximum(mass, 1e-3), 0, 1)

    py, px = np.mgrid[0:H, 0:W]
    own = (py & 1) * 2 + (px & 1)
    alpha = alphas[py // 2, px // 2, own]
    straight = unkeyed[py // 2, px // 2, own]
    at_y, at_x = (py + 0.5) / 2 - 0.5, (px + 0.5) / 2 - 0.5
    first_y, first_x = np.floor(at_y).astype(int), np.floor(at_x).astype(int)
    fy, fx = at_y - first_y, at_x - first_x
    weighted, total = np.zeros((H, W, 2)), np.zeros((H, W))
    for corner in range(4):
        sx, sy = corner & 1, corner >> 1
        cy, cx = clamp(first_y + sy, h), clamp(first_x + sx, w)
        distance = (cell_luma[cy, cx] - straight) / LIKENESS
        weight = (1 - abs(sx - fx)) * (1 - abs(sy - fy)) * mass[cy, cx] * (np.exp(-distance * distance) + 1e-4)
        weighted += weight[..., None] * (np.stack([u8[cy, cx], v8[cy, cx]], -1) - 128.0) / 224.0
        total += weight
    cb, cr = np.moveaxis(weighted / np.maximum(total, 1e-12)[..., None], -1, 0)
    rgb = np.clip(np.stack([straight + 1.5748 * cr, straight - 0.1873 * cb - 0.4681 * cr, straight + 1.8556 * cb], -1), 0, 1)
    return rgb, alpha


def vector(width=32, height=16):
    """Straight RGBA8: white text outlined in black against the left edge, a white box inside a
    half-transparent red panel, a soft round edge across a colour edge."""
    rgba = np.zeros((height, width, 4))
    rgba[2:14, 1:10] = (0.05, 0.05, 0.08, 1.0)
    rgba[2:14, 3:9] = (0.95, 0.95, 0.95, 1.0)
    rgba[1:15, 11:21] = (0.8, 0.1, 0.1, 0.44)
    rgba[5:9, 14:17] = (1.0, 1.0, 1.0, 1.0)
    yy, xx = np.mgrid[0:height, 0:width]
    disc = np.clip(5.5 - np.hypot(yy - 8, xx - 26), 0, 1)
    colour = np.where((xx < 26)[..., None], (0.1, 0.4, 0.9), (0.9, 0.8, 0.1))
    rgba[..., :3] = np.where((disc > 0)[..., None], colour, rgba[..., :3])
    rgba[..., 3] = np.where(disc > 0, disc, rgba[..., 3])
    return np.round(rgba * 255).astype(np.uint8)


def linear(c):
    return np.where(c <= 0.04045, c / 12.92, ((c + 0.055) / 1.055) ** 2.4)


def srgb(c):
    c = np.clip(c, 0, 1)
    return np.where(c <= 0.0031308, c * 12.92, 1.055 * c ** (1 / 2.4) - 0.055)


if __name__ == "__main__":
    here = Path(__file__).parent
    straight = vector()
    premultiplied = straight.copy()
    premultiplied[..., :3] = np.round(straight[..., :3] * (straight[..., 3:] / 255.0)).astype(np.uint8)
    picture, matte = encode(premultiplied)
    rgb, alpha = decode(picture, matte)
    background = np.array([51, 128, 77]) / 255  # the test's view
    composite = srgb(linear(rgb) * alpha[..., None] + linear(background) * (1 - alpha[..., None]))
    expected = np.concatenate([composite, np.ones(alpha.shape + (1,))], -1)
    (here / "guided_matte.picture.bin").write_bytes(b"".join(p.tobytes() for p in picture))
    (here / "guided_matte.matte.bin").write_bytes(b"".join(p.tobytes() for p in matte))
    (here / "guided_matte.expected.bin").write_bytes(np.round(expected * 255).astype(np.uint8).tobytes())
