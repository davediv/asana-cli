//! Output helpers shared by every command: colour, tables, relative times,
//! text wrapping, the notes renderer, and the `--json`/`--jq` flags.
//! Nothing here talks to the API.

pub mod color;
pub mod json;
pub mod notes;
pub mod table;
pub mod text;
pub mod time;

pub use color::Palette;
pub use table::Table;
