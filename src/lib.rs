mod ansi;
mod app;
mod config;
mod effort;
mod fallback;
mod git;
mod render;
mod status;
mod widgets;

pub use app::run;

pub const NAME: &str = "ccstatusline-native";
pub const REFERENCE_CCSTATUSLINE_VERSION: &str = "2.2.22";
