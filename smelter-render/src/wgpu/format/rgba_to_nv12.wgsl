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

const PI: f32 = 3.14159265359;

fn sinc(x: f32) -> f32 {
    if abs(x) < 1e-6 {
        return 1.0;
    }
    let px = PI * x;
    return sin(px) / px;
}



fn rgba_to_y(color: vec4<f32>) -> f32 {
    let y = color.r * 0.2126 + color.g * 0.7152 + color.b * 0.0722;
    return clamp((y * 0.85882352941) + (16.0/255.0), 0.0, 1.0);
}

fn rgba_to_uv(color: vec4<f32>) -> vec2<f32> {
    let u = color.r * -0.1146 + color.g * -0.3854 + color.b * 0.5;
    let v = color.r * 0.5 + color.g * -0.4542 + color.b * -0.0458;
    return clamp(vec2(
        ((u + 0.5) * 0.87843137254) + (16.0/255.0),
        ((v + 0.5) * 0.87843137254) + (16.0/255.0),
    ), vec2(0.0, 0.0), vec2(1.0, 1.0));
}

@fragment
fn fs_main_y(input: VertexOutput) -> @location(0) f32 {
    let color = textureSample(texture, sampler_, input.tex_coords);

    // YUV conversion from: https://en.wikipedia.org/w/index.php?title=YCbCr&section=8#ITU-R_BT.709_conversion
    // YUV values footroom needs to be added
    // Y plane
    return rgba_to_y(color);
}

@fragment
fn fs_main_uv(input: VertexOutput) -> @location(0) vec2<f32> {
    let color = textureSample(texture, sampler_, input.tex_coords);

    // YUV conversion from: https://en.wikipedia.org/w/index.php?title=YCbCr&section=8#ITU-R_BT.709_conversion
    // YUV values footroom needs to be added
    // UV planes are returned in range (-0.5, 0.5) and need to be moved to (0, 1)
    return rgba_to_uv(color);
}


