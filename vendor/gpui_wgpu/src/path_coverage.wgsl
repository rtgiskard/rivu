// Shared by the existing path renderer and the numerical palette material.
fn path_coverage(st_position: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) -> f32 {
    if (length(vec2<f32>(dx.x, dy.x)) < 0.001) {
        return 1.0;
    }
    let gradient = 2.0 * st_position.xx * vec2<f32>(dx.x, dy.x) - vec2<f32>(dx.y, dy.y);
    let f = st_position.x * st_position.x - st_position.y;
    return clamp(0.5 - f / length(gradient), 0.0, 1.0);
}
