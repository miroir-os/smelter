struct VertexInput {
    // position in clip space [-1, -1] (bottom-left) X [1, 1] (top-right)
    @location(0) position: vec3<f32>,
    // texture coordinates in texture coordiantes [0, 0] (top-left) X [1, 1] (bottom-right)
    @location(1) tex_coords: vec2<f32>,
}

struct VertexOutput {
    // position in output in pixel coordinates [0, 0] (top-left) X [output_resolution.x, output_resolution.y] (bottom-right)
    @builtin(position) position: vec4<f32>,
    // texture coordinates in texture coordiantes [0, 0] (top-left) X [1, 1] (bottom-right)
    @location(0) tex_coords: vec2<f32>,
    // Position relative to center of the rectangle in [-rect_width/2, rect_width/2] X [-rect_height/2, height/2]
    @location(2) center_position: vec2<f32>,
}

struct BoxShadowParams {
    border_radius: vec4<f32>,
    color: vec4<f32>,
    top: f32,
    left: f32,
    width: f32,
    height: f32,
    rotation_degrees: f32,
    blur_radius: f32,
    opacity: f32,
    spread_radius: f32,
    offset_x: f32,
    offset_y: f32,
}

struct TextureParams {
    border_radius: vec4<f32>,
    border_color: vec4<f32>,
    // position
    top: f32,
    left: f32,
    width: f32,
    height: f32,
    // texture crop
    crop_top: f32,
    crop_left: f32,
    crop_width: f32,
    crop_height: f32,

    rotation_degrees: f32,
    // border size in pixels
    border_width: f32,
    opacity: f32,
}

struct ColorParams {
    border_radius: vec4<f32>,
    border_color: vec4<f32>,
    color: vec4<f32>,

    top: f32,
    left: f32,
    width: f32,
    height: f32,

    rotation_degrees: f32,
    border_width: f32,
    opacity: f32,
}

struct ParentMask {
    radius: vec4<f32>,
    top: f32,
    left: f32,
    width: f32,
    height: f32,
}

struct LayoutInfo {
    // 0 -> Texture, 1 -> Color, 2 -> BoxShadow
    layout_type: u32,
    index: u32,
    masks_len: u32
}


// Replaced at runtime based on the configured layouts limit (see shader.rs)
const MAX_LAYOUTS_COUNT: u32 = 100;

const MAX_MASKS_COUNT: u32 = 20;

@group(0) @binding(0) var texture: texture_2d<f32>;

@group(1) @binding(0) var<uniform> output_resolution: vec4<f32>;
@group(1) @binding(1) var<uniform> texture_params: array<TextureParams, MAX_LAYOUTS_COUNT>;
@group(1) @binding(2) var<uniform> color_params: array<ColorParams, MAX_LAYOUTS_COUNT>;
@group(1) @binding(3) var<uniform> box_shadow_params: array<BoxShadowParams, MAX_LAYOUTS_COUNT>;

@group(2) @binding(0) var<uniform> masks: array<ParentMask, MAX_MASKS_COUNT>;

@group(3) @binding(0) var sampler_: sampler;

var<immediate> layout_info: LayoutInfo;

fn rotation_matrix(rotation: f32) -> mat4x4<f32> {
    // wgsl is column-major
    let angle = radians(rotation);
    let c = cos(angle);
    let s = sin(angle);
    return mat4x4<f32>(
        vec4<f32>(c, s, 0.0, 0.0),
        vec4<f32>(-s, c, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, 1.0, 0.0),
        vec4<f32>(0.0, 0.0, 0.0, 1.0)
    );
}

fn scale_matrix(scale: vec2<f32>) -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4<f32>(scale.x, 0.0, 0.0, 0.0),
        vec4<f32>(0.0, scale.y, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, 1.0, 0.0),
        vec4<f32>(0.0, 0.0, 0.0, 1.0)
    );
}


fn translation_matrix(translation: vec2<f32>) -> mat4x4<f32> {
    return mat4x4<f32>(
        vec4<f32>(1.0, 0.0, 0.0, 0.0),
        vec4<f32>(0.0, 1.0, 0.0, 0.0),
        vec4<f32>(0.0, 0.0, 1.0, 0.0),
        vec4<f32>(translation, 0.0, 1.0)
    );
}

fn vertices_transformation_matrix(left: f32, top: f32, width: f32, height: f32, rotation: f32) -> mat4x4<f32> {    
    let scale_to_size = vec2<f32>(
        width / output_resolution.x,
        height / output_resolution.y
    );
    let scale_to_pixels = vec2<f32>(
        output_resolution.x / 2.0,
        output_resolution.y / 2.0
    );
    let scale_to_clip_space = vec2<f32>(
        1.0 / scale_to_pixels.x,
        1.0 / scale_to_pixels.y
    );

    let scale_to_pixels_mat = scale_matrix(scale_to_pixels * scale_to_size);
    let scale_to_clip_space_mat = scale_matrix(scale_to_clip_space);

    let left_border_x = -(output_resolution.x / 2.0);
    let distance_left_to_middle = left + width / 2.0;
    let top_border_y = output_resolution.y / 2.0;
    let distance_top_to_middle = top + height / 2.0;
    let translation = vec2<f32>(
        left_border_x + distance_left_to_middle,
        top_border_y - distance_top_to_middle
    );

    let translation_mat = translation_matrix(translation);
    let rotation_mat = rotation_matrix(rotation);

    return scale_to_clip_space_mat * translation_mat * rotation_mat * scale_to_pixels_mat;
}

fn texture_coord_transformation_matrix(crop_left: f32, crop_top: f32, crop_width: f32, crop_height: f32) -> mat4x4<f32> {
    let dim = textureDimensions(texture);
    let scale = vec2<f32>(
        crop_width / f32(dim.x),
        crop_height / f32(dim.y),
    );

    let translation = vec2<f32>(
        crop_left / f32(dim.x),
        crop_top / f32(dim.y),
    );

    return translation_matrix(translation) * scale_matrix(scale);
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;

    switch (layout_info.layout_type) {
        // texture
        case 0u: {
            let vertices_transformation = vertices_transformation_matrix(
                texture_params[layout_info.index].left,
                texture_params[layout_info.index].top,
                texture_params[layout_info.index].width,
                texture_params[layout_info.index].height,
                texture_params[layout_info.index].rotation_degrees
            );
            let texture_transformation = texture_coord_transformation_matrix(
                texture_params[layout_info.index].crop_left,
                texture_params[layout_info.index].crop_top,
                texture_params[layout_info.index].crop_width,
                texture_params[layout_info.index].crop_height
            );

            output.position = vertices_transformation * vec4(input.position, 1.0);
            output.tex_coords = (texture_transformation * vec4<f32>(input.tex_coords, 0.0, 1.0)).xy;
            let rect_size = vec2<f32>(texture_params[layout_info.index].width, texture_params[layout_info.index].height);
            output.center_position = input.position.xy / 2.0 * rect_size;
        }
        // color
        case 1u: {
            let vertices_transformation = vertices_transformation_matrix(
                color_params[layout_info.index].left,
                color_params[layout_info.index].top,
                color_params[layout_info.index].width,
                color_params[layout_info.index].height,
                color_params[layout_info.index].rotation_degrees
            );
            output.position = vertices_transformation * vec4(input.position, 1.0);
            output.tex_coords = input.tex_coords;
            let rect_size = vec2<f32>(color_params[layout_info.index].width, color_params[layout_info.index].height);
            output.center_position = input.position.xy / 2.0 * rect_size;
        }
        // box shadow
        case 2u:  {
            let reach = 1.5 * box_shadow_params[layout_info.index].blur_radius;
            let width = box_shadow_params[layout_info.index].width + 2.0 * reach;
            let height = box_shadow_params[layout_info.index].height + 2.0 * reach;

            let vertices_transformation = vertices_transformation_matrix(
                box_shadow_params[layout_info.index].left - reach,
                box_shadow_params[layout_info.index].top - reach,
                width,
                height,
                box_shadow_params[layout_info.index].rotation_degrees
            );
            output.position = vertices_transformation * vec4(input.position, 1.0);
            output.tex_coords = input.tex_coords;
            let rect_size = vec2<f32>(width, height);
            output.center_position = input.position.xy / 2.0 * rect_size;
        }
        default {}
    }

    return output;
}

// Signed distance function for rounded rectangle https://iquilezles.org/articles/distfunctions
// adapted from https://www.shadertoy.com/view/4llXD7
// Distance from outside is positive and inside it is negative
//
// dist - signed distance from the center of the rectangle in pixels
// size - size of the rectangle in pixels
// radius - radius of the corners in pixels [top-left, top-right, bottom-right, bottom-left]
// rotation - rotation of the rectangle in degrees
// WARNING - it doesn't work when border radius is > min(size.x, size.y) / 2
fn roundedRectSDF(dist: vec2<f32>, size: vec2<f32>, radius: vec4<f32>, rotation: f32) -> f32 {
    let half_size = size / 2.0;
    
    // wierd hack to get the radius of the nearest corner stored in r.x
    var r: vec2<f32> = vec2<f32>(0.0, 0.0);
    r = select(radius.yz, radius.xw, dist.x < 0.0);
    r.x = select(r.x, r.y, dist.y < 0.0);

    let q = abs(dist) - half_size + r.x;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r.x;
}

fn nearest_corner_radius(dist: vec2<f32>, radius: vec4<f32>) -> f32 {
    var r: vec2<f32> = select(radius.yz, radius.xw, dist.x < 0.0);
    return select(r.x, r.y, dist.y < 0.0);
}

// Abramowitz and Stegun's approximation of erf.
fn erf(x: vec2<f32>) -> vec2<f32> {
    let s = sign(x);
    let a = abs(x);
    var y = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    y = y * y;
    return s - s / (y * y);
}

fn gaussian(x: f32, sigma: f32) -> f32 {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * 3.141592653589793) * sigma);
}

// The blurred box along x at height y, its corner rounded by `corner`.
fn rounded_box_shadow_x(x: f32, y: f32, sigma: f32, corner: f32, half_size: vec2<f32>) -> f32 {
    let delta = min(half_size.y - corner - abs(y), 0.0);
    let curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    let integral = 0.5 + 0.5 * erf((x + vec2<f32>(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

// Coverage of a rounded box blurred by a Gaussian of `sigma`, at `point` from its center:
// exact along x, sampled along y (https://madebyevan.com/shaders/fast-rounded-rectangle-shadows/).
fn rounded_box_shadow(point: vec2<f32>, half_size: vec2<f32>, sigma: f32, corner: f32) -> f32 {
    let low = point.y - half_size.y;
    let high = point.y + half_size.y;
    let start = clamp(-3.0 * sigma, low, high);
    let end = clamp(3.0 * sigma, low, high);
    let step = (end - start) / 4.0;
    var y = start + step * 0.5;
    var value = 0.0;
    for (var i = 0; i < 4; i++) {
        value += rounded_box_shadow_x(point.x, point.y - y, sigma, corner, half_size) * gaussian(y, sigma) * step;
        y += step;
    }
    return value;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let transparent = vec4<f32>(0.0, 0.0, 0.0, 0.0);

    // A layout's coverage is the least of its own edge's and its parents' masks':
    // nested shapes share edges, as a child filling its parent does, and the
    // product would fade a shared edge once per shape drawing it.
    var mask_alpha = 1.0;

    for (var i = 0; i < i32(layout_info.masks_len); i++) {
        let radius = masks[i].radius;
        let top = masks[i].top;
        let left = masks[i].left;
        let width = masks[i].width;
        let height = masks[i].height;
        let size = vec2<f32>(width, height);

        let from_center = input.position.xy - (vec2<f32>(left, top) + size / 2.0);
        let distance = roundedRectSDF(
            vec2<f32>(from_center.x, -from_center.y),
            size,
            radius,
            0.0,
        );
        mask_alpha = min(mask_alpha, smoothstep(-0.5, 0.5 , -distance));
    }

    switch layout_info.layout_type {
        case 0u: {
            let sample = textureSample(texture, sampler_, input.tex_coords);

            let width = texture_params[layout_info.index].width;
            let height = texture_params[layout_info.index].height;
            let border_radius = texture_params[layout_info.index].border_radius;
            let rotation_degrees = texture_params[layout_info.index].rotation_degrees;
            let border_width = texture_params[layout_info.index].border_width;
            let border_color = texture_params[layout_info.index].border_color;
            let opacity = texture_params[layout_info.index].opacity;

            let size = vec2<f32>(width, height);
            let edge_distance = -roundedRectSDF(
                input.center_position,
                size, 
                border_radius, 
                rotation_degrees
            );

            if (border_width < 1.0) {
                let content_alpha = smoothstep(-0.5, 0.5, edge_distance);
                return sample * min(content_alpha, mask_alpha) * opacity;
            } else if (mask_alpha < 0.01) {
                return vec4<f32>(0, 0, 0, 0);
            } else {
                if (edge_distance > border_width / 2.0) {
                    // border <-> content
                    let border_alpha = smoothstep(border_width - 0.5, border_width + 0.5, edge_distance);
                    let border_or_content = mix(border_color, sample, border_alpha);
                    return border_or_content * mask_alpha * opacity;
                } else {
                    // border <-> outside
                    let content_alpha = smoothstep(-0.5, 0.5, edge_distance);
                    return border_color * min(content_alpha, mask_alpha) * opacity;
                }
            }
        }
        case 1u: {
            let color = color_params[layout_info.index].color;

            let width = color_params[layout_info.index].width;
            let height = color_params[layout_info.index].height;
            let border_radius = color_params[layout_info.index].border_radius;
            let rotation_degrees = color_params[layout_info.index].rotation_degrees;
            let border_width = color_params[layout_info.index].border_width;
            let border_color = color_params[layout_info.index].border_color;
            let opacity = color_params[layout_info.index].opacity;

            let size = vec2<f32>(width, height);
            let edge_distance = -roundedRectSDF(
                input.center_position,
                size, 
                border_radius, 
                rotation_degrees
            );

            if (border_width < 1.0) {
              let content_alpha = smoothstep(-0.5, 0.5, edge_distance);
              return color * min(content_alpha, mask_alpha) * opacity;
            } else {
                if (edge_distance > border_width / 2.0) {
                    // border <-> content
                    let border_alpha = smoothstep(border_width, border_width + 1.0, edge_distance);
                    let border_or_content = mix(border_color, color, border_alpha);
                    return border_or_content * mask_alpha * opacity;
                } else {
                    // border <-> outside
                    let content_alpha = smoothstep(-0.5, 0.5, edge_distance);
                    return border_color * min(content_alpha, mask_alpha) * opacity;
                }
            }
        }
        case 2u: {
            let params = box_shadow_params[layout_info.index];
            let size = vec2<f32>(params.width, params.height);
            let point = input.center_position;

            // The spread grows rounded corners with the box; square ones stay square.
            let radius = select(vec4<f32>(0.0), max(params.border_radius + params.spread_radius, vec4<f32>(0.0)), params.border_radius > vec4<f32>(0.0));
            var shadow_alpha: f32;
            if (params.blur_radius < 0.5) {
                shadow_alpha = smoothstep(-0.5, 0.5, -roundedRectSDF(point, size, radius, 0.0));
            } else {
                shadow_alpha = rounded_box_shadow(point, size / 2.0, params.blur_radius / 2.0, nearest_corner_radius(point, radius));
            }

            // Never under its own box, as in CSS, so a transparent picture shows what is behind
            // it. The cut starts inside the box's anti-aliased edge, leaving no seam.
            let angle = radians(params.rotation_degrees);
            let offset = vec2<f32>(-params.offset_x, params.offset_y);
            let own_center = vec2<f32>(
                cos(angle) * offset.x + sin(angle) * offset.y,
                -sin(angle) * offset.x + cos(angle) * offset.y,
            );
            let own_distance = -roundedRectSDF(point - own_center, size - 2.0 * params.spread_radius, params.border_radius, 0.0);
            let outside_own_box = 1.0 - smoothstep(0.5, 1.5, own_distance);

            return params.color * shadow_alpha * outside_own_box * mask_alpha * params.opacity;
        }
        default {
            return vec4(0.0, 0.0, 0.0, 0.0);
        }
    }
}
