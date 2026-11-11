struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coords: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;

    output.position = vec4(input.position, 1.0);
    output.tex_coords = input.tex_coords;

    return output;
}

@group(0) @binding(0) var texture: texture_2d<f32>;
@group(1) @binding(0) var sampler_: sampler;

fn srgb_to_linear(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = step(srgb, vec3<f32>(0.04045));
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    let lower = srgb / vec3<f32>(12.92);

    return mix(higher, lower, cutoff);
}

// Source holds sRGB values with alpha premultiplied into them, read without
// decoding; destination is an sRGB view, so it takes linear values.
@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(texture, sampler_, input.tex_coords);
    let a = max(color.a, 0.00001);
    let straight = clamp(color.rgb / a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(srgb_to_linear(straight) * color.a, color.a);
}
