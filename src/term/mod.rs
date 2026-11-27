//! The terminal core that tai and tai-gui share (ADR 0014): everything that
//! decides what the terminal does and what to draw, apart from each front
//! end's window, toolkit events and GPU calls. tai builds it as `mod term`,
//! tai-gui through `#[path]`.

pub mod images;
pub mod setup;
