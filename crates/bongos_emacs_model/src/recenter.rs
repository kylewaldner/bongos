/// Where recenter puts the caret line; repeated C-l cycles through them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cycle {
    #[default]
    Middle,
    Top,
    Bottom,
}

impl Cycle {
    /// The position after this one: middle, top, bottom, middle.
    pub fn next(self) -> Self {
        match self {
            Cycle::Middle => Cycle::Top,
            Cycle::Top => Cycle::Bottom,
            Cycle::Bottom => Cycle::Middle,
        }
    }

    /// The position for a press that follows `previous` (`None` when the last
    /// command was not a recenter, which starts at the middle).
    pub fn after(previous: Option<Cycle>) -> Self {
        previous.map_or(Cycle::Middle, Cycle::next)
    }
}
