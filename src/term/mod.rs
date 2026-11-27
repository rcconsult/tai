//! The terminal core that tai and tai-gui share (ADR 0014): everything that
//! decides what the terminal does and what to draw, apart from each front
//! end's window, toolkit events and GPU calls. tai builds it as `mod term`,
//! tai-gui through `#[path]`.

pub mod images;
pub mod pointer;
pub mod setup;

/// The space between a pane's edge and its first cell, in logical units,
/// in both front ends.
pub const PADDING: f32 = 6.0;
