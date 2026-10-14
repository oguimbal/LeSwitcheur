//! GPUI views and bindings for the switcher panel.

pub mod actions;
pub mod assets;
pub mod list;
pub mod onboarding_view;
pub mod openclaw_setup_view;
pub mod open_with_popover;
pub mod settings_view;
pub mod switcher_view;
pub mod thanks_view;
pub mod theme;
pub mod theme_adapter;

pub use assets::Assets;
pub use onboarding_view::{OnboardingView, OnboardingViewEvent};
pub use openclaw_setup_view::{
    AgentAvatar, AgentChoice, OpenClawSetupEvent, OpenClawSetupView, SetupStatus,
};
pub use open_with_popover::{OpenWithEntry, OpenWithPopoverEvent, OpenWithPopoverView};
pub use settings_view::{SettingsView, SettingsViewEvent};
pub use list::OpenClawRow;
pub use switcher_view::{InstructionPhase, NagPhase, SwitcherView, SwitcherViewEvent};
pub use thanks_view::{ThanksState, ThanksView, ThanksViewEvent};
pub use theme::Theme;

mod local_source;
