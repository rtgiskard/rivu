//! Logical stereo analysis weights for native WAVE speaker positions.
//!
//! Device output keeps its native layout; consumers choose whether to fold it.

/// Normalized left/right weights in the supplied standard speaker-bit order.
/// The caller validates speaker identities and uniqueness with its source format.
pub fn analysis_weights(layout: impl IntoIterator<Item = u32>) -> Vec<[f32; 2]> {
    let mut weights: Vec<_> = layout
        .into_iter()
        .map(|speaker| match speaker {
            0 | 4 | 6 | 9 | 12 | 15 => [1.0, 0.0],
            1 | 5 | 7 | 10 | 14 | 17 => [0.0, 1.0],
            _ => [1.0, 1.0],
        })
        .collect();
    let total = weights.iter().fold([0.0_f32; 2], |sum, weight| {
        [sum[0] + weight[0], sum[1] + weight[1]]
    });
    for weight in &mut weights {
        weight[0] /= total[0].max(1.0);
        weight[1] /= total[1].max(1.0);
    }
    weights
}
