//! The undo tree, as plain data: the shape of a buffer's undo history and the wording of its
//! relative times. No gpui and no editor types, so every rule here is tested without a window.

pub mod shape;
pub mod time;

#[cfg(test)]
mod time_tests;

pub use shape::{Shape, ShapeNode};
pub use time::{Now, relative_time};
