// Private, deliberately fixed schema. Host encodes every scalar explicitly.
struct Rectangle {
    bounds: vec4<f32>,
    palette_range: vec2<f32>,
    padding: vec2<f32>,
}
struct Placement {
    bounds: vec4<f32>,
    mask: vec4<f32>,
    viewport: vec2<f32>,
    opacity: f32,
    scale: f32,
    cell_height: f32,
    cell_gap: f32,
    palette_count: u32,
    premultiplied: u32,
    srgb_target: u32,
    padding0: u32,
    padding1: u32,
    padding2: u32,
}
@group(0) @binding(0) var<uniform> placement: Placement;
@group(1) @binding(0) var<storage, read> rectangles: array<Rectangle>;
@group(1) @binding(1) var<storage, read> palette: array<vec4<f32>>;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) rectangle_index: u32,
}
@vertex
fn rectangle_vertex(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> VertexOutput {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
    let local = corners[vertex];
    let rectangle = rectangles[instance];
    let pixel = placement.bounds.xy + (rectangle.bounds.xy + local * rectangle.bounds.zw) * placement.bounds.zw;
    var output: VertexOutput;
    output.position = vec4<f32>(pixel / placement.viewport * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    output.local = local;
    output.rectangle_index = instance;
    return output;
}
@fragment
fn rectangle_fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    let rectangle = rectangles[input.rectangle_index];
    let bottom_distance = (1.0 - input.local.y) * rectangle.bounds.w * placement.bounds.w;
    // Derivatives must be evaluated before nonuniform clip/segment branches.
    let aa = max(fwidth(bottom_distance), 0.0001);
    if any(input.position.xy < placement.mask.xy) || any(input.position.xy >= placement.mask.xy + placement.mask.zw) {
        discard;
    }
    var coverage = 1.0;
    let height = placement.cell_height * placement.scale;
    let gap = placement.cell_gap * placement.scale;
    if height > 0.0 && gap > 0.0 {
        let period = height + gap;
        let phase = bottom_distance - floor(bottom_distance / period) * period;
        // Signed distance to the closest filled interval, including its previous
        // periodic neighbor. No geometry is expanded beyond the rectangle.
        let distance = select(-min(phase - height, period - phase), min(phase, height - phase), phase <= height);
        coverage = clamp(distance / aa + 0.5, 0.0, 1.0);
    }
    let coordinate = mix(rectangle.palette_range.x, rectangle.palette_range.y, 1.0 - input.local.y);
    let sample_position = clamp(coordinate, 0.0, 1.0) * f32(placement.palette_count - 1u);
    let lower = u32(floor(sample_position));
    var color = mix(palette[lower], palette[min(lower + 1u, placement.palette_count - 1u)], fract(sample_position));
    // Palette interpolation is explicitly in sRGB numerical space. Only a real
    // sRGB attachment needs conversion before its hardware output encoding.
    if placement.srgb_target != 0u {
        color = vec4<f32>(select(pow((color.rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), color.rgb / 12.92, color.rgb <= vec3<f32>(0.04045)), color.a);
    }
    let alpha = color.a * placement.opacity * coverage;
    return vec4<f32>(color.rgb * select(1.0, alpha, placement.premultiplied != 0u), alpha);
}
