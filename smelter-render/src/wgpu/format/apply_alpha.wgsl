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

@group(0) @binding(0) var luma_texture: texture_2d<f32>;
@group(1) @binding(0) var sampler_: sampler;

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let luma = textureSample(luma_texture, sampler_, input.tex_coords).x;
    // (235 - 16) / 255 ~= .859
    let alpha = clamp((luma - (16.0 / 255.0)) / 0.85882352941, 0.0, 1.0);
    return vec4(alpha);
}
