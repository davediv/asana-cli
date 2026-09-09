//! Output helpers shared by every command: colour, tables, relative times,
//! text wrapping, and the notes renderer. Nothing here talks to the API.

pub mod color;
pub mod notes;
pub mod table;
pub mod text;
pub mod time;

pub use color::Palette;
pub use table::Table;
