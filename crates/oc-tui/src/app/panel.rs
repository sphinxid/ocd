//! The session side panel (`src/context/panel.tsx`, `src/component/panel-host.tsx`): one panel at a
//! time, bound to a session, docked beside the transcript when there is room for both (the panel
//! width threshold) or taking over the screen.

use super::App;

/// The session keeps its preferred width beside a minimum-width pane (`SESSION_PANEL_SPLIT_MIN_WIDTH`).
pub const SESSION_PANEL_SPLIT_MIN_WIDTH: u16 = 64 + 44;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presentation {
    #[default]
    Panel,
    Fullscreen,
}

/// The selected panel (`PanelTarget`, built-in plugins only: `diff`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelTarget {
    pub name: String,
    pub session_id: String,
}

impl App {
    /// The width a docked panel splits (`panels.width()`): the terminal minus a vertical tab rail
    /// that fits, measured even while a full-screen panel hides the tabs.
    pub fn panels_width(&self) -> u16 {
        let rail = if self.tabs_available_base() && self.vertical_tabs_fit() { self.rail_width() } else { 0 };
        self.size.0.saturating_sub(rail)
    }

    pub fn panel_can_split(&self) -> bool {
        self.panels_width() >= SESSION_PANEL_SPLIT_MIN_WIDTH
    }

    /// The requested presentation, or full screen when there is no room to split.
    pub fn panel_presentation(&self) -> Presentation {
        if self.panel_can_split() { self.panel_requested } else { Presentation::Fullscreen }
    }

    /// The panel of the routed session.
    pub fn active_panel(&self) -> Option<&PanelTarget> {
        let sid = self.session_id()?;
        self.panel.as_ref().filter(|p| p.session_id == sid)
    }

    pub fn panel_fullscreen(&self) -> bool {
        self.active_panel().is_some() && self.panel_presentation() == Presentation::Fullscreen
    }

    /// `panels.open`: select the panel (keeping it when it is already the selection) and focus it.
    /// A selected terminal and an opened sidebar give way to it.
    pub fn open_panel(&mut self, name: &str, session_id: &str, presentation: Presentation) {
        self.panel_requested = presentation;
        let target = PanelTarget { name: name.to_string(), session_id: session_id.to_string() };
        if self.panel.as_ref() != Some(&target) {
            self.panel = Some(target);
        }
        self.terminal_open = false;
        if let Some(v) = self.views.get_mut(session_id) {
            v.sidebar_open = None;
        }
        self.right_focus = true;
        self.dirty = true;
    }

    pub fn close_panel(&mut self) {
        if self.panel.take().is_some_and(|p| p.name == "diff") {
            self.diff = None;
        }
        self.right_focus = false;
        self.dirty = true;
    }

    /// `panels.toggleFullscreen`: ignored while there is no room to split.
    pub fn toggle_panel_fullscreen(&mut self) {
        if !self.panel_can_split() {
            return;
        }
        self.panel_requested = match self.panel_requested {
            Presentation::Panel => Presentation::Fullscreen,
            Presentation::Fullscreen => Presentation::Panel,
        };
        self.dirty = true;
    }

    /// Focus the panel (a click on it, or its file menu).
    pub fn focus_panel(&mut self) {
        if self.active_panel().is_some() && !self.right_focus {
            self.right_focus = true;
            self.dirty = true;
        }
    }

    /// Whether the panel takes the keyboard: focused, or full screen (always the active scope).
    pub fn panel_has_focus(&self) -> bool {
        self.active_panel().is_some() && (self.right_focus || self.panel_fullscreen())
    }
}
