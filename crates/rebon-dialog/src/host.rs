//! The stack every hosted dialog lives on.
//!
//! One stack answers all of the host's questions: the top model takes the
//! keys, the top model is the one painted, and the top model says whether it
//! owns the viewport. Adding a dialog means implementing [`DialogModel`] and
//! pushing it — there is no dispatcher, renderer or "is a modal open"
//! predicate to keep in step with it.
//!
//! Nothing here paints or reads a terminal event. The surface translates its
//! own key events into [`DialogKey`](crate::model::DialogKey) and paints the
//! [`crate::model::ViewSpec`] the stack hands back, which is what lets the
//! same stack sit under a terminal, a native window, and a web page.

use crate::model::{DialogAction, DialogModel, DialogOutcome, KeyPress, ViewSpec};

/// What the surface should do with a key it offered to the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackKey {
    /// No dialog is open; the key belongs to whatever is behind.
    NotConsumed,
    /// The top dialog took the key and asked for nothing further.
    Consumed,
    /// The top dialog took the key and emitted an action to route.
    /// It has already been popped if the action asked for that.
    Action(DialogAction),
}

/// A stack of open dialogs. The last entry is on top: it takes the
/// keys and it is the one painted.
#[derive(Default)]
pub struct DialogStack {
    stack: Vec<Box<dyn DialogModel>>,
    /// Body rows the surface painted for the top dialog last frame.
    /// Only the surface knows this, and only the two dialogs that page
    /// by screenful read it, so it rides along on each [`KeyPress`]
    /// instead of forcing every model to carry a cache.
    viewport_rows: Option<u16>,
}

impl DialogStack {
    /// Open `dialog` on top of the stack.
    pub fn push(&mut self, dialog: impl DialogModel + 'static) {
        self.push_boxed(Box::new(dialog));
    }

    /// Open an already-boxed dialog, which is what a registry factory
    /// hands back.
    pub fn push_boxed(&mut self, dialog: Box<dyn DialogModel>) {
        self.stack.push(dialog);
        // The recorded viewport belonged to whatever was on top before.
        self.viewport_rows = None;
    }

    /// Whether any dialog is open.
    pub fn is_open(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Id of the dialog on top, if any.
    pub fn top_id(&self) -> Option<&'static str> {
        self.stack.last().map(|dialog| dialog.id())
    }

    /// The dialog on top, if any.
    pub fn top(&self) -> Option<&dyn DialogModel> {
        self.stack.last().map(|dialog| dialog.as_ref())
    }

    /// Whether the top dialog owns the whole viewport, and therefore
    /// input focus. An empty stack means no.
    pub fn top_is_fullscreen(&self) -> bool {
        self.stack
            .last()
            .is_some_and(|dialog| dialog.is_fullscreen())
    }

    /// Close the top dialog, if any.
    pub fn close_top(&mut self) {
        self.stack.pop();
        self.viewport_rows = None;
    }

    /// Close every dialog.
    pub fn close_all(&mut self) {
        self.stack.clear();
        self.viewport_rows = None;
    }

    /// Close the top dialog when it is `id`, and report whether it was.
    pub fn close_if(&mut self, id: &str) -> bool {
        if self.top_id() == Some(id) {
            self.close_top();
            return true;
        }
        false
    }

    /// The top dialog, when it is a `T`. For the handful of actions
    /// that need something the action itself does not carry: read it
    /// before dispatching the key, because a closing action pops the
    /// dialog as it fires.
    pub fn top_as<T: DialogModel + 'static>(&self) -> Option<&T> {
        self.stack.last()?.as_any().downcast_ref::<T>()
    }

    /// The top dialog, mutably, when it is a `T`. The one way a host
    /// writes back into a dialog it left open: the dialog emits an
    /// action, the host computes something, and hands the result back
    /// through here.
    pub fn top_as_mut<T: DialogModel + 'static>(&mut self) -> Option<&mut T> {
        self.stack.last_mut()?.as_any_mut().downcast_mut::<T>()
    }

    /// The top dialog's view, if any.
    pub fn top_view(&self) -> Option<ViewSpec> {
        self.stack.last().map(|dialog| dialog.view())
    }

    /// Preferred height for the top dialog, for hosts that size
    /// themselves to their content.
    pub fn top_desired_height(&self) -> Option<u16> {
        match self.top_view()? {
            ViewSpec::List(view) => Some(view.desired_height()),
            // A panel reports one only when it wants to be sized to its
            // content; the rest fill the host's own rect.
            ViewSpec::Panel(view) => view.desired_height,
            ViewSpec::Outline(_) | ViewSpec::Search(_) => None,
        }
    }

    /// Whether the top dialog can be painted by the generic list
    /// painter, which every surface has.
    pub fn top_is_list(&self) -> bool {
        matches!(self.top_view(), Some(ViewSpec::List(_)))
    }

    /// Record the size the surface is painting the top dialog at: for
    /// the next key to page by a real screenful, and for a panel whose
    /// own layout depends on the frame.
    pub fn note_viewport(&mut self, rows: u16, cols: u16) {
        self.viewport_rows = Some(rows);
        if let Some(dialog) = self.stack.last_mut() {
            dialog.note_viewport(rows, cols);
        }
    }

    /// Offer a key to the top dialog. `None` is a key the surface
    /// could not map onto anything a dialog understands.
    ///
    /// While a dialog is open every key is consumed, unmapped ones
    /// included: a modal that let stray keys through to the prompt
    /// behind it would type into an invisible buffer.
    pub fn on_key(&mut self, press: Option<KeyPress>) -> StackKey {
        let Some(dialog) = self.stack.last_mut() else {
            return StackKey::NotConsumed;
        };
        let Some(mut press) = press else {
            return StackKey::Consumed;
        };
        press.viewport_rows = self.viewport_rows;
        match dialog.on_key(press) {
            DialogOutcome::None => StackKey::Consumed,
            DialogOutcome::Close => {
                self.close_top();
                StackKey::Consumed
            }
            DialogOutcome::Action(action) => {
                if action.close {
                    self.close_top();
                }
                StackKey::Action(action)
            }
        }
    }
}

impl Clone for DialogStack {
    fn clone(&self) -> Self {
        Self {
            stack: self.stack.iter().map(|dialog| dialog.box_clone()).collect(),
            viewport_rows: self.viewport_rows,
        }
    }
}

impl core::fmt::Debug for DialogStack {
    /// The models are not `Debug`, and dumping their innards would
    /// drown every application-state dump anyway. The ids, bottom to
    /// top, are what anyone reading a log wants.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("DialogStack")
            .field(&self.stack.iter().map(|d| d.id()).collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DialogKey, ListView, OutlineView};

    /// A dialog that records its keys, so these tests pin routing
    /// without depending on any real dialog's behaviour.
    #[derive(Clone)]
    struct Probe {
        id: &'static str,
        fullscreen: bool,
        native: bool,
        seen: usize,
        reply: DialogOutcome,
    }

    impl Probe {
        fn new(id: &'static str) -> Self {
            Self {
                id,
                fullscreen: true,
                native: false,
                seen: 0,
                reply: DialogOutcome::None,
            }
        }
    }

    impl DialogModel for Probe {
        crate::dialog_plumbing!();

        fn id(&self) -> &'static str {
            self.id
        }

        fn on_key(&mut self, _press: KeyPress) -> DialogOutcome {
            self.seen += 1;
            self.reply.clone()
        }

        fn view(&self) -> ViewSpec {
            if self.native {
                ViewSpec::Outline(OutlineView {
                    title: format!(" {} ", self.id),
                    ..OutlineView::default()
                })
            } else {
                ViewSpec::List(ListView {
                    title: format!(" {} ", self.id),
                    footer: vec!["Esc".into()],
                    ..ListView::default()
                })
            }
        }

        fn is_fullscreen(&self) -> bool {
            self.fullscreen
        }
    }

    fn seen(stack: &DialogStack, index: usize) -> usize {
        stack.stack[index]
            .as_any()
            .downcast_ref::<Probe>()
            .unwrap()
            .seen
    }

    #[test]
    fn an_empty_stack_consumes_nothing() {
        let mut stack = DialogStack::default();
        assert!(!stack.is_open());
        assert_eq!(stack.top_id(), None);
        assert!(!stack.top_is_fullscreen());
        assert_eq!(
            stack.on_key(Some(KeyPress::from(DialogKey::Enter))),
            StackKey::NotConsumed
        );
        assert_eq!(stack.top_desired_height(), None);
        assert!(!stack.top_is_list());
    }

    #[test]
    fn the_top_of_the_stack_takes_the_keys() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("under"));
        stack.push(Probe::new("over"));
        assert_eq!(stack.top_id(), Some("over"));
        assert_eq!(
            stack.on_key(Some(KeyPress::from(DialogKey::Down))),
            StackKey::Consumed
        );
        assert_eq!(seen(&stack, 0), 0, "the buried dialog must not see keys");
        assert_eq!(seen(&stack, 1), 1);
    }

    #[test]
    fn close_pops_one_level_and_reveals_the_dialog_underneath() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("under"));
        let mut top = Probe::new("over");
        top.reply = DialogOutcome::Close;
        stack.push(top);
        assert_eq!(
            stack.on_key(Some(KeyPress::from(DialogKey::Escape))),
            StackKey::Consumed
        );
        assert_eq!(stack.top_id(), Some("under"));
        assert!(stack.is_open());
    }

    #[test]
    fn a_closing_action_pops_before_it_is_returned() {
        let mut stack = DialogStack::default();
        let mut probe = Probe::new("picker");
        probe.reply = DialogOutcome::Action(DialogAction::closing("picker", "select", "high"));
        stack.push(probe);
        match stack.on_key(Some(KeyPress::from(DialogKey::Enter))) {
            StackKey::Action(action) => assert_eq!(action.value(), "high"),
            other => panic!("expected an action, got {other:?}"),
        }
        assert!(!stack.is_open());
    }

    #[test]
    fn a_staying_action_leaves_the_dialog_open() {
        let mut stack = DialogStack::default();
        let mut probe = Probe::new("browser");
        probe.reply = DialogOutcome::Action(DialogAction::staying("browser", "open", "a.md"));
        stack.push(probe);
        assert!(matches!(
            stack.on_key(Some(KeyPress::from(DialogKey::Enter))),
            StackKey::Action(_)
        ));
        assert_eq!(stack.top_id(), Some("browser"));
    }

    #[test]
    fn keys_the_surface_could_not_map_are_still_swallowed() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("picker"));
        assert_eq!(stack.on_key(None), StackKey::Consumed);
        assert_eq!(seen(&stack, 0), 0, "the reducer must not see it");
    }

    #[test]
    fn fullscreen_is_the_top_models_answer_not_a_hand_written_chain() {
        let mut stack = DialogStack::default();
        let mut inline = Probe::new("inline");
        inline.fullscreen = false;
        stack.push(inline);
        assert!(!stack.top_is_fullscreen());
        stack.push(Probe::new("full"));
        assert!(stack.top_is_fullscreen());
        stack.close_top();
        assert!(!stack.top_is_fullscreen());
    }

    #[test]
    fn close_if_only_pops_a_matching_top_and_close_all_empties() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("a"));
        assert!(!stack.close_if("b"));
        assert_eq!(stack.top_id(), Some("a"));
        assert!(stack.close_if("a"));
        assert!(!stack.is_open());
        stack.push(Probe::new("a"));
        stack.push(Probe::new("b"));
        stack.close_all();
        assert!(!stack.is_open());
    }

    #[test]
    fn a_view_that_is_not_a_list_reports_no_list_and_no_height() {
        let mut stack = DialogStack::default();
        let mut outline = Probe::new("outline");
        outline.native = true;
        stack.push(outline);
        assert!(!stack.top_is_list());
        assert_eq!(stack.top_desired_height(), None);
        stack.close_top();
        stack.push(Probe::new("list"));
        assert!(stack.top_is_list());
        // Zero rows + border(2) + blank(1) + footer(1).
        assert_eq!(stack.top_desired_height(), Some(4));
    }

    #[test]
    fn cloning_the_stack_carries_every_dialog() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("a"));
        stack.push(Probe::new("b"));
        let copy = stack.clone();
        assert_eq!(copy.top_id(), Some("b"));
        assert_eq!(format!("{copy:?}"), r#"DialogStack(["a", "b"])"#);
    }

    #[test]
    fn top_as_downcasts_only_a_matching_top() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("a"));
        assert_eq!(stack.top_as::<Probe>().map(|p| p.id), Some("a"));
    }

    #[test]
    fn top_as_mut_writes_back_into_a_dialog_left_open() {
        let mut stack = DialogStack::default();
        stack.push(Probe::new("a"));
        stack.top_as_mut::<Probe>().unwrap().seen = 7;
        assert_eq!(stack.top_as::<Probe>().unwrap().seen, 7);
    }
}
