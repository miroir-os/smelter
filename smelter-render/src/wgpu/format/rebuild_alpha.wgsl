// The picture is composited over black or white per 4×4 block, which the coded alpha's chroma
// says, and its alpha is coded at half its size: each texel of the reduced grid the mean of its 2×2
// pixels. A linear fit of alpha on luma over each texel's 3×3 neighbourhood says how the texel's
// alpha spreads over its four pixels, and the picture's colour is then unkeyed from its block's.

struct VertexInput {
    @location(0) position: vec3<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> @builtin(position) vec4<f32> {
    return vec4(input.position, 1.0);
}

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var matte: texture_2d<f32>;
@group(0) @binding(1) var moments: texture_2d<f32>;
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var alphas: texture_2d<f32>;
@group(0) @binding(2) var keys: texture_2d<f32>;
@group(1) @binding(0) var bilinear: sampler;

override SRGB_TARGET: bool = true;

fn from_limited_range(luma: vec4<f32>) -> vec4<f32> {
    return (luma - 16.0 / 255.0) * (255.0 / 219.0);
}

/// The lumas of the texel's pixels: top left, top right, bottom left, bottom right.
fn quad(texel: vec2<i32>) -> vec4<f32> {
    let pixel = texel * 2;
    return from_limited_range(vec4(
        textureLoad(luma, pixel, 0).r,
        textureLoad(luma, pixel + vec2(1, 0), 0).r,
        textureLoad(luma, pixel + vec2(0, 1), 0).r,
        textureLoad(luma, pixel + vec2(1, 1), 0).r,
    ));
}

/// The moments the neighbourhoods' fits average, centred on ½ so half floats keep the variance's
/// precision.
@fragment
fn texel_moments(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(position.xy);
    let i = dot(quad(texel), vec4(0.25)) - 0.5;
    let p = clamp(from_limited_range(textureLoad(matte, texel, 0)).r, 0.0, 1.0) - 0.5;
    return vec4(i, p, i * p, i * i);
}

/// The texel's four alphas, `4·mass` spread over them in proportion to `fit`, none above 1.
fn spread(fit: vec4<f32>, mass: f32) -> vec4<f32> {
    let share = fit / max(dot(fit, vec4(1.0)), 1e-3);
    var alpha = 4.0 * mass * share;
    for (var iteration = 0; iteration < 3; iteration++) {
        let excess = dot(max(alpha - 1.0, vec4(0.0)), vec4(1.0));
        alpha = min(alpha, vec4(1.0));
        let open = select(vec4(0.0), share, alpha < vec4(1.0));
        alpha += excess * open / max(dot(open, vec4(1.0)), 1e-6);
    }
    return clamp(alpha, vec4(0.0), vec4(1.0));
}

/// The texel's fit from its neighbourhood's means, which bilinear fetches at its four corners
/// weigh 1-2-1 per axis.
@fragment
fn texel_alphas(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(position.xy);
    let step = 1.0 / vec2<f32>(textureDimensions(moments));
    let corner = vec2<f32>(texel) * step;
    let mean = 0.25 * (textureSampleLevel(moments, bilinear, corner, 0.0)
        + textureSampleLevel(moments, bilinear, corner + vec2(step.x, 0.0), 0.0)
        + textureSampleLevel(moments, bilinear, corner + vec2(0.0, step.y), 0.0)
        + textureSampleLevel(moments, bilinear, corner + step, 0.0));
    let slope = (mean.z - mean.x * mean.y) / (mean.w - mean.x * mean.x + 1e-4);
    let fit = clamp(slope * (quad(texel) - 0.5 - mean.x) + mean.y + 0.5, vec4(0.0), vec4(1.0));
    return spread(fit, textureLoad(moments, texel, 0).y + 0.5);
}

fn decoded(srgb: vec3<f32>) -> vec3<f32> {
    return select(pow((srgb + 0.055) / 1.055, vec3(2.4)), srgb / 12.92, srgb <= vec3(0.04045));
}

/// The pixel's straight colour, unkeyed, premultiplied by its alpha: in linear space when the
/// target is sRGB, as the renderer blends.
@fragment
fn pixels(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    let own = pixel & vec2(1);
    let alpha = textureLoad(alphas, pixel / 2, 0)[own.y * 2 + own.x];
    let keyed = textureLoad(picture, pixel, 0).rgb;
    let key = step(0.5, textureLoad(keys, pixel / 4, 0).r);
    let straight = clamp((keyed - key * (1.0 - alpha)) / max(alpha, 1e-3), vec3(0.0), vec3(1.0));
    return vec4(select(straight, decoded(straight), SRGB_TARGET) * alpha, alpha);
}
