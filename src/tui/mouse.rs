//! Mouse support: what is clickable where. Every `draw` records the screen
//! areas it paints as hits; a click or wheel event is matched against the hits
//! of the last frame. Rects are captured at draw time because the layouts
//! depend on runtime state (the approval banner, the number of projects).

use std::cell::RefCell;

use ratatui::layout::{Position, Rect};

/// What a screen area is, for the click and wheel handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Top-bar tab: 1 dashboard, 2 projects, 3 health.
    Tab(u8),
    DashBanner,
    DashProjects,
    DashServices,
    DashRuns,
    DashOutput,
    ProjPipelines,
    ProjServices,
    /// The Projects screen: the list, and the fetch, push and pull button columns.
    DevRepos,
    DevFetch,
    DevPush,
    DevPull,
    RunSteps,
    RunOutput,
    // Overlays. While one is open only these can be hit.
    TextOverlay,
    HelpPanel,
    ConfirmYes,
    ConfirmNo,
    DialogEnv,
    DialogField(usize),
    DialogStart,
    DialogDry,
    DialogCancel,
}

impl Target {
    fn is_overlay(self) -> bool {
        matches!(
            self,
            Target::TextOverlay
                | Target::HelpPanel
                | Target::ConfirmYes
                | Target::ConfirmNo
                | Target::DialogEnv
                | Target::DialogField(_)
                | Target::DialogStart
                | Target::DialogDry
                | Target::DialogCancel
        )
    }

    /// A list whose rows a double click opens, like `enter`.
    pub fn is_list(self) -> bool {
        matches!(
            self,
            Target::DashProjects
                | Target::DashServices
                | Target::DashRuns
                | Target::ProjPipelines
                | Target::ProjServices
                | Target::DevRepos
                | Target::RunSteps
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct Hit {
    rect: Rect,
    target: Target,
    /// Index of the item on the rect's first row (lists that scroll).
    first: usize,
}

/// The hits of the last frame. Drawing only has `&App`, hence the interior
/// mutability.
#[derive(Default)]
pub struct HitMap(RefCell<Vec<Hit>>);

impl HitMap {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    pub fn add(&self, rect: Rect, target: Target, first: usize) {
        if rect.width > 0 && rect.height > 0 {
            self.0.borrow_mut().push(Hit { rect, target, first });
        }
    }

    /// The topmost hit under a cell and the item row there. `overlay` says
    /// whether an overlay is open: then only overlay hits count, so a click
    /// never reaches the screen underneath.
    pub fn at(&self, x: u16, y: u16, overlay: bool) -> Option<(Target, usize)> {
        let pos = Position::new(x, y);
        self.0
            .borrow()
            .iter()
            .rev()
            .find(|h| h.target.is_overlay() == overlay && h.rect.contains(pos))
            .map(|h| (h.target, h.first + (y - h.rect.y) as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_is_offset_by_the_first_item() {
        let m = HitMap::default();
        m.add(Rect::new(2, 5, 10, 4), Target::DashRuns, 7);
        assert_eq!(m.at(2, 5, false), Some((Target::DashRuns, 7)));
        assert_eq!(m.at(11, 8, false), Some((Target::DashRuns, 10)));
        assert_eq!(m.at(12, 8, false), None);
        assert_eq!(m.at(2, 9, false), None);
    }

    #[test]
    fn later_hits_win_and_empty_rects_are_dropped() {
        let m = HitMap::default();
        m.add(Rect::new(0, 0, 10, 10), Target::DashProjects, 0);
        m.add(Rect::new(0, 0, 5, 1), Target::Tab(1), 0);
        m.add(Rect::new(0, 0, 0, 3), Target::DashOutput, 0);
        assert_eq!(m.at(1, 0, false), Some((Target::Tab(1), 0)));
        assert_eq!(m.at(6, 0, false), Some((Target::DashProjects, 0)));
    }

    #[test]
    fn an_open_overlay_hides_the_screen_underneath() {
        let m = HitMap::default();
        m.add(Rect::new(0, 0, 20, 20), Target::DashProjects, 0);
        m.add(Rect::new(5, 5, 4, 1), Target::DialogStart, 0);
        assert_eq!(m.at(6, 5, true), Some((Target::DialogStart, 0)));
        assert_eq!(m.at(1, 1, true), None);
        assert_eq!(m.at(6, 5, false), Some((Target::DashProjects, 5)));
    }

    #[test]
    fn clear_forgets_the_frame() {
        let m = HitMap::default();
        m.add(Rect::new(0, 0, 3, 3), Target::RunSteps, 0);
        m.clear();
        assert_eq!(m.at(1, 1, false), None);
    }
}
