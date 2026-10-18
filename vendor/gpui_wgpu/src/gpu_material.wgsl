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
    } else if placement.config.x == 1u && placement.field.w != 0u {
        let left = data_float(instance * 2u) * placement.bounds.z;
        let right = data_float(instance * 2u + 1u) * placement.bounds.z;
        pixel.x = placement.bounds.x + left + local.x * max(right - left, placement.scale);
    }
    var output: VertexOutput;
    output.position = screen_position(pixel);
    output.local = select(local, vec2<f32>((f32(instance) + 0.5) / f32(placement.field.x), local.y),
        placement.config.x == 1u && placement.field.w != 0u);
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
    let unit_start = vec2<f32>(data_float(offset), data_float(offset + 1u));
    let unit_end = vec2<f32>(data_float(offset + 2u), data_float(offset + 3u));
    var start = vec2<f32>(0.0);
    var end = vec2<f32>(0.0);
    let endpoint_magnitude = max(abs(unit_start), abs(unit_end));
    let bound_magnitude = max(placement.bounds.z, placement.bounds.w);
    var largest = 0.0;
    let color = data[offset + 4u] + data[layer * 4u + 1u];
    let width = data_float(layer * 4u) * placement.scale;
    var normal = vec2<f32>(0.0);
    // Keep ordinary strokes on the direct normalization path. Distant vertices
    // also need rebasing: even a valid normal loses its width when added to a
    // huge endpoint. This threshold selects arithmetic, not an admitted range.
    let raster_limit = min(max(max(placement.viewport.x, placement.viewport.y), 1.0) * 1024.0, 1.0e18);
    var direct = false;
    if all(endpoint_magnitude <= vec2<f32>(1.0)) && bound_magnitude <= raster_limit {
        // This is a strict subset of the guarded direct path below.
        start = unit_start * placement.bounds.zw;
        end = unit_end * placement.bounds.zw;
        direct = true;
    } else {
        largest = max(endpoint_magnitude.x, endpoint_magnitude.y);
        // Inspect exponents before multiplying, rather than relying on an
        // infinity from overflow (not portable shader arithmetic).
        let product_exponent = (bitcast<u32>(largest) >> 23u)
            + (bitcast<u32>(bound_magnitude) >> 23u);
        if product_exponent <= 378u {
            start = unit_start * placement.bounds.zw;
            end = unit_end * placement.bounds.zw;
            direct = all(abs(start) <= vec2<f32>(raster_limit)) && all(abs(end) <= vec2<f32>(raster_limit));
        }
    }
    if direct {
        let delta = end - start;
        normal = vec2<f32>(-delta.y, delta.x) / max(length(delta), 1.0e-6);
    } else {
        // Scale only at overflow risk, before subtraction/products. The exact
        // power of two has exponent <=126 so its reciprocal remains normal.
        var scaled_start = unit_start;
        var scaled_end = unit_end;
        if largest > 1.0e18 {
            let endpoint_scale = bitcast<f32>(min(bitcast<u32>(largest) & 0x7f800000u, 0x7e800000u));
            scaled_start /= endpoint_scale;
            scaled_end /= endpoint_scale;
        }
        let unit_delta = scaled_end - scaled_start;
        let delta = unit_delta * placement.bounds.zw;
        let direction_scale = max(abs(delta.x), abs(delta.y));
        if direction_scale == 0.0 {
            return VertexOutput(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec2<f32>(0.0), color);
        }
        let direction = delta / direction_scale;
        normal = vec2<f32>(-direction.y, direction.x) * inverseSqrt(dot(direction, direction));

        // Solve on the dominant physical axis. Its bounded slope lets us clip
        // the centerline before extrusion without cancelling huge endpoints.
        let major = select(1u, 0u, abs(delta.x) >= abs(delta.y));
        let minor = 1u - major;
        let slope = unit_delta[minor] / unit_delta[major];
        // Compensated difference of products preserves an intercept when huge
        // diagonal terms cancel. Retain the unscaled minor coordinate so a
        // horizontal/vertical line near f32::MAX keeps its small offset.
        // Eighth-sized operands keep both products and their difference finite.
        let a = scaled_start[major];
        let b = scaled_end[major];
        let c = unit_start[minor] * 0.125;
        let d = unit_end[minor] * 0.125;
        let product = b * c;
        let determinant = fma(a, d, -product) + fma(-b, c, product);
        let intercept_eighth = determinant / (a - b);
        let clip_min = (-placement.bounds.xy - vec2<f32>(width * 0.5)) / placement.bounds.zw;
        let clip_max = (placement.viewport - placement.bounds.xy + vec2<f32>(width * 0.5)) / placement.bounds.zw;
        var lower = max(min(unit_start[major], unit_end[major]), clip_min[major]);
        var upper = min(max(unit_start[major], unit_end[major]), clip_max[major]);
        let slope_lower = slope * lower;
        let slope_upper = slope * upper;
        let intercept_min = (clip_min[minor] - max(slope_lower, slope_upper)) * 0.125;
        let intercept_max = (clip_max[minor] - min(slope_lower, slope_upper)) * 0.125;
        if lower >= upper || intercept_eighth < intercept_min || intercept_eighth > intercept_max {
            return VertexOutput(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec2<f32>(0.0), color);
        }
        let intercept = intercept_eighth * 8.0;
        if slope != 0.0 {
            let first = (clip_min[minor] - intercept) / slope;
            let second = (clip_max[minor] - intercept) / slope;
            lower = max(lower, min(first, second));
            upper = min(upper, max(first, second));
        }
        if lower >= upper {
            return VertexOutput(vec4<f32>(0.0, 0.0, 0.0, 1.0), vec2<f32>(0.0), color);
        }
        start[major] = lower * placement.bounds[major + 2u];
        end[major] = upper * placement.bounds[major + 2u];
        start[minor] = fma(slope, lower, intercept) * placement.bounds[minor + 2u];
        end[minor] = fma(slope, upper, intercept) * placement.bounds[minor + 2u];
    }
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -0.5), vec2<f32>(1.0, -0.5), vec2<f32>(0.0, 0.5),
        vec2<f32>(0.0, 0.5), vec2<f32>(1.0, -0.5), vec2<f32>(1.0, 0.5));
    let corner = corners[vertex];
    let pixel = placement.bounds.xy + mix(start, end, corner.x)
        + normal * corner.y * width;
    return VertexOutput(screen_position(pixel), vec2<f32>(0.0), color);
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
fn palette_coordinate(value: f32) -> f32 {
    var coordinate = 0.0;
    if placement.field.z != 0u {
        if value > 0.0 {
            coordinate = clamp((10.0 * log2(value) * 0.3010299956639812 + placement.params.y) / placement.params.y, 0.0, 1.0);
        }
    } else {
        coordinate = clamp((value - placement.params.x) / placement.params.y, 0.0, 1.0);
    }
    return coordinate;
}
fn field_value(position: vec2<i32>) -> u32 {
    let clamped = vec2<u32>(clamp(position, vec2<i32>(0), vec2<i32>(placement.field.xy) - vec2<i32>(1)));
    return data[placement.field.w * placement.field.x * 2u + clamped.y * placement.field.x + clamped.x];
}
fn field_palette(bits: u32) -> vec4<f32> {
    let nonfinite = (bits & 0x7f800000u) == 0x7f800000u;
    if nonfinite {
        if placement.field.w != 0u { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }
        return palette[0u];
    }
    let value = bitcast<f32>(bits);
    let coordinate = palette_coordinate(value);
    // Distinguish the floor from palette quantization. Linear division may
    // underflow even above min, so retain its direct scalar comparison.
    let below_floor = select(value <= placement.params.x, coordinate <= 0.0, placement.field.z != 0u);
    if placement.field.w != 0u && below_floor {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return palette[min(u32(floor(coordinate * f32(placement.config.y - 1u) + 0.5)), placement.config.y - 1u)];
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
