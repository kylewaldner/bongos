use crate::rank::{ScreenPoint, rank};

fn point(x: f32, y: f32) -> ScreenPoint {
    ScreenPoint { x, y }
}

#[test]
fn ranking_orders_by_distance_with_deterministic_ties() {
    let origin = point(10.0, 10.0);
    let points = [
        point(10.0, 30.0),
        point(10.0, 12.0),
        point(13.0, 14.0),
        point(10.0, 6.0),
        point(7.0, 10.0),
        point(13.0, 10.0),
        point(10.0, 6.0),
    ];
    // Distances 20, 2, 5, 4, 3, 3, 4: ties break by y, then x, then input order.
    assert_eq!(rank(origin, &points), vec![1, 4, 5, 3, 6, 2, 0]);
    assert!(rank(origin, &[]).is_empty());
}
