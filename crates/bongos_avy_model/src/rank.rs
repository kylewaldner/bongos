#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenPoint {
    pub x: f32,
    pub y: f32,
}

/// Indices of `points` by ascending distance from `origin`; ties go to the
/// smaller y, then the smaller x, then the earlier input position.
pub fn rank(origin: ScreenPoint, points: &[ScreenPoint]) -> Vec<usize> {
    let distance = |p: &ScreenPoint| (p.x - origin.x).hypot(p.y - origin.y);
    let mut indices: Vec<usize> = (0..points.len()).collect();
    indices.sort_by(|&a, &b| {
        let (pa, pb) = (&points[a], &points[b]);
        distance(pa)
            .total_cmp(&distance(pb))
            .then(pa.y.total_cmp(&pb.y))
            .then(pa.x.total_cmp(&pb.x))
            .then(a.cmp(&b))
    });
    indices
}
