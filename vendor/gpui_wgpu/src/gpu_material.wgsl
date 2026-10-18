// Closed material ABI. All host scalars are encoded explicitly little-endian.
struct Placement {
    bounds: vec4<f32>,
    mask: vec4<f32>,
    viewport: vec2<f32>,
    opacity: f32,
    scale: f32,
    params: vec4<f32>,
    config: vec4<u32>, // schema, palette count, premultiplied target, sRGB target
    field: vec4<u32>, // width, height, logarithmic mapping, reserved zero
    source_bounds: vec4<f32>,
}
@group(0) @binding(0) var<uniform> placement: Placement;
@group(1) @binding(0) var<storage, read> data: array<u32>;
@group(1) @binding(1) var<storage, read> palette: array<vec4<f32>>;
fn data_float(index: u32) -> f32 { return bitcast<f32>(data[index]); }

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) instance: u32,
}
struct RectangleVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(perspective, sample) local: vec2<f32>,
    @location(1) @interpolate(flat) instance: u32,
}
fn screen_position(pixel: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(pixel / placement.viewport * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}
fn quad_vertex(vertex: u32, instance: u32) -> VertexOutput {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
    let local = corners[vertex];
    var pixel = placement.bounds.xy + local * placement.bounds.zw;
    if placement.config.x == 0u {
        let offset = instance * 8u;
        let origin = vec2<f32>(data_float(offset), data_float(offset + 1u)) * placement.bounds.zw;
        let extent = vec2<f32>(data_float(offset + 2u), data_float(offset + 3u)) * placement.bounds.zw;
        pixel = placement.bounds.xy + origin + local * extent;
    }
    var output: VertexOutput;
    output.position = screen_position(pixel);
    output.local = local;
    output.instance = instance;
    return output;
}
@vertex
fn material_vertex(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> VertexOutput {
    return quad_vertex(vertex, instance);
}
@vertex
fn rectangle_vertex(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> RectangleVertexOutput {
    let value = quad_vertex(vertex, instance);
    return RectangleVertexOutput(value.position, value.local, value.instance);
}
@vertex
fn segment_vertex(@builtin(vertex_index) vertex: u32, @builtin(instance_index) instance: u32) -> VertexOutput {
    let layer = instance / placement.field.x;
    let segment = instance % placement.field.x;
    let offset = placement.field.y * 4u + segment * 8u;
    let start = vec2<f32>(data_float(offset), data_float(offset + 1u)) * placement.bounds.zw;
    let end = vec2<f32>(data_float(offset + 2u), data_float(offset + 3u)) * placement.bounds.zw;
    let delta = end - start;
    // Zero-length lines produce degenerate triangles, never NaNs or a square dot.
    let normal = vec2<f32>(-delta.y, delta.x) / max(length(delta), 0.000001);
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -0.5), vec2<f32>(1.0, -0.5), vec2<f32>(0.0, 0.5),
        vec2<f32>(0.0, 0.5), vec2<f32>(1.0, -0.5), vec2<f32>(1.0, 0.5));
    let corner = corners[vertex];
    let pixel = placement.bounds.xy + mix(start, end, corner.x)
        + normal * corner.y * data_float(layer * 4u) * placement.scale;
    return VertexOutput(screen_position(pixel), vec2<f32>(0.0),
        data[offset + 4u] + data[layer * 4u + 1u]);
}
@fragment
fn segment_fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    if clipped(input.position.xy) { discard; }
    return finish_color(palette[input.instance], 1.0, true);
}

@vertex
fn path_vertex(@builtin(vertex_index) vertex: u32) -> VertexOutput {
    let offset = vertex * 4u;
    let xy = vec2<f32>(data_float(offset), data_float(offset + 1u));
    let unit = (xy - placement.source_bounds.xy) / placement.source_bounds.zw;
    var output: VertexOutput;
    output.position = screen_position(placement.bounds.xy + unit * placement.bounds.zw);
    output.local = vec2<f32>(data_float(offset + 2u), data_float(offset + 3u));
    output.instance = 0u;
    return output;
}
fn clipped(pixel: vec2<f32>) -> bool {
    return any(pixel < placement.mask.xy) || any(pixel >= placement.mask.xy + placement.mask.zw);
}
fn finish_color(sample: vec4<f32>, coverage: f32, premultiplied: bool) -> vec4<f32> {
    var color = sample;
    // Interpolation occurs in numerical sRGB space, before attachment conversion.
    if placement.config.w != 0u {
        color = vec4<f32>(select(pow((color.rgb + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), color.rgb / 12.92, color.rgb <= vec3<f32>(0.04045)), color.a);
    }
    let alpha = color.a * placement.opacity * coverage;
    return vec4<f32>(color.rgb * select(1.0, alpha, premultiplied), alpha);
}
fn continuous_palette(coordinate: f32) -> vec4<f32> {
    let position = clamp(coordinate, 0.0, 1.0) * f32(placement.config.y - 1u);
    let lower = u32(floor(position));
    return mix(palette[lower], palette[min(lower + 1u, placement.config.y - 1u)], fract(position));
}
fn nearest_palette(value: f32) -> vec4<f32> {
    var coordinate = 0.0;
    if placement.field.z != 0u {
        if value > 0.0 {
            coordinate = clamp((10.0 * log2(value) * 0.3010299956639812 + placement.params.y) / placement.params.y, 0.0, 1.0);
        }
    } else {
        coordinate = clamp((value - placement.params.x) / placement.params.y, 0.0, 1.0);
    }
    return palette[min(u32(floor(coordinate * f32(placement.config.y - 1u) + 0.5)), placement.config.y - 1u)];
}
fn field_value(position: vec2<i32>) -> u32 {
    let clamped = vec2<u32>(clamp(position, vec2<i32>(0), vec2<i32>(placement.field.xy) - vec2<i32>(1)));
    return data[clamped.y * placement.field.x + clamped.x];
}
fn field_palette(bits: u32) -> vec4<f32> {
    if (bits & 0x7f800000u) == 0x7f800000u { return palette[0u]; }
    return nearest_palette(bitcast<f32>(bits));
}
fn field_color(uv: vec2<f32>) -> vec4<f32> {
    let position = uv * vec2<f32>(placement.field.xy) - vec2<f32>(0.5);
    let lower = vec2<i32>(floor(position));
    let weight = fract(position);
    let a = field_value(lower);
    let b = field_value(lower + vec2<i32>(1, 0));
    let c = field_value(lower + vec2<i32>(0, 1));
    let d = field_value(lower + vec2<i32>(1, 1));
    return mix(mix(field_palette(a), field_palette(b), weight.x), mix(field_palette(c), field_palette(d), weight.x), weight.y);
}
@fragment
fn material_fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    if clipped(input.position.xy) { discard; }
    return finish_color(field_color(input.local), 1.0, placement.config.z != 0u);
}
@fragment
fn rectangle_fragment(input: RectangleVertexOutput) -> @location(0) vec4<f32> {
    if clipped(input.position.xy) { discard; }
    let offset = input.instance * 8u;
    let bottom = placement.bounds.y + (data_float(offset + 1u) + data_float(offset + 3u)) * placement.bounds.w;
    let bottom_distance = bottom - input.position.y;
    let height = placement.params.x * placement.scale;
    let gap = placement.params.y * placement.scale;
    if height > 0.0 && gap > 0.0 {
        let period = height + gap;
        let phase = bottom_distance - floor(bottom_distance / period) * period;
        // Match rasterized rectangles: top included, bottom excluded.
        if phase <= 0.0 || phase > height { discard; }
    }
    let coordinate = mix(data_float(offset + 4u), data_float(offset + 5u), 1.0 - input.local.y);
    // Coverage belongs to MSAA samples, not alpha ramps on adjacent instances.
    // The shared intermediate is premultiplied; its compositor applies surface alpha.
    return finish_color(continuous_palette(coordinate), 1.0, true);
}
@fragment
fn path_fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    let coverage = path_coverage(input.local, dpdx(input.local), dpdy(input.local));
    if clipped(input.position.xy) { discard; }
    let y = (input.position.y - placement.bounds.y) / placement.bounds.w;
    let coordinate = mix(placement.params.x, placement.params.y, 1.0 - y);
    // The shared path intermediate is always premultiplied, regardless of the
    // window surface alpha mode; its existing compositor owns the final blend.
    return finish_color(continuous_palette(coordinate), coverage, true);
}
