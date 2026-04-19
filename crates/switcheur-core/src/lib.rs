//! LeSwitcheur core: domain types, fuzzy matching, config, UI state machine.
//!
//! Pure Rust, no platform dependencies — the whole crate is testable in isolation.

pub mod app_match;
pub mod config;
pub mod exclusions;
pub mod license;
pub mod matcher;
pub mod js;
pub mod math;
pub mod model;
pub mod sort;
pub mod state;
pub mod url;

pub use app_match::{AppMatch, AppMatchSet};
pub use config::{Appearance, Config, HotkeySpec};
pub use exclusions::{ExclusionFilter, ExclusionRule};
pub use matcher::{FuzzyMatcher, MatchResult};
pub use model::{AppRef, DirRef, DirSource, Item, LlmProvider, ProgramRef, WindowRef};
pub use sort::{sort_items, RecencyTracker, SortOrder};
pub use state::{Section, SwitcherState};
