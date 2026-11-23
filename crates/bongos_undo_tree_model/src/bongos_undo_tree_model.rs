//! The undo tree, as plain data: the shape of a buffer's undo history and the wording of its
//! relative times. No gpui and no editor types, so every rule here is tested without a window.

pub mod layout;
pub mod nav;
pub mod shape;
pub mod time;

#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod nav_tests;
#[cfg(test)]
mod time_tests;

pub use layout::{NodeMark, Row, layout};
pub use nav::{NO_OTHER_BRANCH, backward, forward, next, previous};
pub use shape::{Shape, ShapeNode};
pub use time::{Now, relative_time};
