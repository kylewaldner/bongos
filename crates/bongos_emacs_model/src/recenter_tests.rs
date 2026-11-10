use crate::recenter::Cycle;

#[test]
fn cycles_middle_top_bottom_middle() {
    let mut cycle = Cycle::default();
    let mut seen = vec![cycle];
    for _ in 0..3 {
        cycle = cycle.next();
        seen.push(cycle);
    }
    assert_eq!(
        seen,
        [Cycle::Middle, Cycle::Top, Cycle::Bottom, Cycle::Middle]
    );
}

#[test]
fn a_press_after_another_command_starts_in_the_middle() {
    assert_eq!(Cycle::after(None), Cycle::Middle);
    assert_eq!(Cycle::after(Some(Cycle::Middle)), Cycle::Top);
    assert_eq!(Cycle::after(Some(Cycle::Top)), Cycle::Bottom);
    assert_eq!(Cycle::after(Some(Cycle::Bottom)), Cycle::Middle);
}
