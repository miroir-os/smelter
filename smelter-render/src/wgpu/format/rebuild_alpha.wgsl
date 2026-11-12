// A guided matte's decoder. The picture's luma is its colour composited over a backdrop (black,
// grey or white, the matte's U per 4×4 block), its chroma the colour's own, averaged per 2×2 cell
// weighted by alpha. The matte, at half the picture's size, codes each 2×2 texel's mean alpha.
//
// `texels` fits the matte, over each texel's 3×3 neighbourhood, along the picture's luma and along
// its distance to the backdrop, which tells apart content lighter and darker than it, keeps the fit
// with less residual (the distance's on a tie, as only it knows the backdrop is transparent) and
// spreads the texel's mean over its four pixels by it. It writes the texel's alphas, its pixels'
// lumas unkeyed and, as a chroma cell, its straight luma, mean alpha and chroma. `pixels`
// interpolates the chroma from the four nearest cells, weighted by alpha and by luma likeness.

struct VertexInput {
    @location(0) position: vec3<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> @builtin(position) vec4<f32> {
    return vec4(input.position, 1.0);
}

// `texels` reads the picture and the matte.
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var matte: texture_2d<f32>;
@group(0) @binding(2) var backdrops: texture_2d<f32>;
@group(0) @binding(3) var chroma_u: texture_2d<f32>;
@group(0) @binding(4) var chroma_v: texture_2d<f32>;
// `pixels` reads what `texels` wrote.
@group(0) @binding(0) var alphas: texture_2d<f32>;
@group(0) @binding(1) var straight_lumas: texture_2d<f32>;
@group(0) @binding(2) var cells: texture_2d<f32>;

override SRGB_TARGET: bool = true;
// NV12 binds its interleaved chroma twice, V in green; planar YUV each plane, V in red.
override V_CHANNEL: u32 = 1u;

// How alike a cell's luma must be to a pixel's for its colour to count.
const LIKENESS: f32 = 0.1;
const RIDGE: f32 = 1e-4;
const TIE: f32 = 1e-6;

fn limited(value: f32) -> f32 {
    return (value * 255.0 - 16.0) / 219.0;
}

fn clamped(texture: texture_2d<f32>, at: vec2<i32>) -> vec2<i32> {
    return clamp(at, vec2(0), vec2<i32>(textureDimensions(texture)) - 1);
}

/// The backdrop's luma under `texel`: its block's U, 16, 128 or 240.
fn backdrop(texel: vec2<i32>) -> f32 {
    let u = textureLoad(backdrops, texel / 2, 0).r * 255.0;
    return clamp(round((u - 16.0) / 112.0), 0.0, 2.0) * 0.5;
}

fn mean_alpha(texel: vec2<i32>) -> f32 {
    return clamp(limited(textureLoad(matte, texel, 0).r), 0.0, 1.0);
}

/// The lumas of `texel`'s pixels: top left, top right, bottom left, bottom right.
fn quad(texel: vec2<i32>) -> vec4<f32> {
    let pixel = texel * 2;
    return vec4(
        limited(textureLoad(luma, pixel, 0).r),
        limited(textureLoad(luma, pixel + vec2(1, 0), 0).r),
        limited(textureLoad(luma, pixel + vec2(0, 1), 0).r),
        limited(textureLoad(luma, pixel + vec2(1, 1), 0).r),
    );
}

/// `4·mass` shared among the four pixels in proportion to `fit`, none above 1: every pixel
/// takes a little, so a texel's mass is kept even where its fit is nil.
fn spread(fit: vec4<f32>, mass: f32) -> vec4<f32> {
    let share = (fit + 1e-3) / dot(fit + 1e-3, vec4(1.0));
    var alpha = 4.0 * mass * share;
    for (var pass_ = 0; pass_ < 2; pass_++) {
        let excess = dot(max(alpha - 1.0, vec4(0.0)), vec4(1.0));
        alpha = min(alpha, vec4(1.0));
        let open = select(vec4(0.0), share, alpha < vec4(1.0));
        alpha += excess * open / max(dot(open, vec4(1.0)), 1e-6);
    }
    return clamp(alpha, vec4(0.0), vec4(1.0));
}

struct Texel {
    @location(0) alphas: vec4<f32>,
    @location(1) straight_lumas: vec4<f32>,
    /// Its straight luma, its mean alpha, and its chroma as coded.
    @location(2) cell: vec4<f32>,
}

@fragment
fn texels(@builtin(position) position: vec4<f32>) -> Texel {
    let texel = vec2<i32>(position.xy);
    // Both guides' moments, luma then distance, centred on ½, over the 1-2-1 neighbourhood.
    var guide = vec2(0.0);
    var product = vec2(0.0);
    var guide_squared = vec2(0.0);
    var alpha = 0.0;
    var alpha_squared = 0.0;
    for (var near_index = 0; near_index < 9; near_index++) {
        let offset = vec2(near_index % 3 - 1, near_index / 3 - 1);
        let near = clamped(matte, texel + offset);
        let weight = f32((2 - abs(offset.x)) * (2 - abs(offset.y))) / 16.0;
        let lumas = quad(near);
        let p = mean_alpha(near) - 0.5;
        let g = vec2(dot(lumas, vec4(0.25)), dot(abs(lumas - backdrop(near)), vec4(0.25))) - 0.5;
        guide += weight * g;
        product += weight * g * p;
        guide_squared += weight * g * g;
        alpha += weight * p;
        alpha_squared += weight * p * p;
    }
    let covariance = product - guide * alpha;
    let slope = covariance / (guide_squared - guide * guide + RIDGE);
    let residual = alpha_squared - alpha * alpha - slope * covariance;
    let lumas = quad(texel);
    let key = backdrop(texel);
    let by_distance = residual.y <= residual.x + TIE;
    let along = select(lumas, abs(lumas - key), by_distance);
    let fit = select(vec2(slope.x, guide.x), vec2(slope.y, guide.y), by_distance);
    let mass = mean_alpha(texel);
    let alphas = spread(clamp(fit.x * (along - 0.5 - fit.y) + alpha + 0.5, vec4(0.0), vec4(1.0)), mass);
    let unkeyed = clamp((lumas - key * (1.0 - alphas)) / max(alphas, vec4(1e-3)), vec4(0.0), vec4(1.0));
    let cell_luma = clamp((dot(lumas, vec4(0.25)) - key * (1.0 - mass)) / max(mass, 1e-3), 0.0, 1.0);
    let u = textureLoad(chroma_u, texel, 0).r;
    let v = textureLoad(chroma_v, texel, 0)[V_CHANNEL];
    return Texel(alphas, unkeyed, vec4(cell_luma, mass, u, v));
}

fn decoded(srgb: vec3<f32>) -> vec3<f32> {
    return select(pow((srgb + 0.055) / 1.055, vec3(2.4)), srgb / 12.92, srgb <= vec3(0.04045));
}

/// The pixel's colour premultiplied by its alpha: in linear space when the target is sRGB, as the
/// renderer blends.
@fragment
fn pixels(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    let own = (pixel.y & 1) * 2 + (pixel.x & 1);
    let alpha = textureLoad(alphas, pixel / 2, 0)[own];
    let straight_luma = textureLoad(straight_lumas, pixel / 2, 0)[own];
    // The four nearest cells, weighted bilinearly, by their alpha and by how close their luma is
    // to the pixel's: barely, where none is close.
    let at = (vec2<f32>(pixel) + 0.5) / 2.0 - 0.5;
    var weighted = vec2(0.0);
    var total = 0.0;
    for (var corner = 0; corner < 4; corner++) {
        let step = vec2(corner & 1, corner >> 1);
        let cell = textureLoad(cells, clamped(cells, vec2<i32>(floor(at)) + step), 0);
        let bilinear = 1.0 - abs(vec2<f32>(step) - fract(at));
        let distance = (cell.x - straight_luma) / LIKENESS;
        let weight = bilinear.x * bilinear.y * cell.y * (exp(-distance * distance) + 1e-4);
        weighted += weight * (cell.zw * 255.0 - 128.0) / 224.0;
        total += weight;
    }
    let difference = weighted / max(total, 1e-12);
    let rgb = clamp(
        vec3(
            straight_luma + 1.5748 * difference.y,
            straight_luma - 0.1873 * difference.x - 0.4681 * difference.y,
            straight_luma + 1.8556 * difference.x,
        ),
        vec3(0.0),
        vec3(1.0),
    );
    return vec4(select(rgb, decoded(rgb), SRGB_TARGET) * alpha, alpha);
}
