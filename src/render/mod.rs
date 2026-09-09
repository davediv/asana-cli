//! Output helpers shared by every command: colour, tables, relative times,
//! text wrapping. Nothing here talks to the API.

pub mod color;
pub mod table;
pub mod text;
pub mod time;

pub use color::Palette;
pub use table::Table;
