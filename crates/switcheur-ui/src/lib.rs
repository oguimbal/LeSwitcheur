//! GPUI views and bindings for the switcher panel.

pub mod actions;
pub mod assets;
pub mod input;
pub mod list;
pub mod onboarding_view;
pub mod open_with_popover;
pub mod settings_view;
pub mod switcher_view;
pub mod thanks_view;
pub mod theme;

pub use assets::Assets;
pub use onboarding_view::{OnboardingView, OnboardingViewEvent};
pub use open_with_popover::{OpenWithEntry, OpenWithPopoverEvent, OpenWithPopoverView};
pub use settings_view::{SettingsView, SettingsViewEvent};
pub use switcher_view::{NagPhase, SwitcherView, SwitcherViewEvent};
pub use thanks_view::{ThanksState, ThanksView, ThanksViewEvent};
pub use theme::Theme;
