pub mod keymap;
pub mod model;
pub mod parser;
pub mod update;
pub mod view;

// Re-export the surface used outside this module (main.rs).
pub use model::{Mode, Model};
pub use parser::InputParser;
pub use update::{process_input, LoopAction};
pub use view::render;
