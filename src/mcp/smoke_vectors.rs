pub(super) fn deterministic_smoke_vectors(chunks: &[String], dim: usize) -> Vec<Vec<f32>> {
    chunks
        .iter()
        .map(|chunk| deterministic_smoke_vector(chunk, dim))
        .collect()
}

fn deterministic_smoke_vector(content: &str, dim: usize) -> Vec<f32> {
    if dim == 0 {
        return Vec::new();
    }
    let hash = blake3::hash(content.as_bytes());
    let hash_bytes = hash.as_bytes();
    let mut vector = Vec::with_capacity(dim);
    for index in 0..dim {
        let byte = hash_bytes[index % hash_bytes.len()];
        let value = (f32::from(byte) / 255.0) * 2.0 - 1.0;
        vector.push(if value.abs() < f32::EPSILON {
            0.003_921_569
        } else {
            value
        });
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}
