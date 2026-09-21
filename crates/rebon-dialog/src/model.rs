//! The `DialogModel` seam — a dialog's keyboard reducer plus a
//! declarative view, with no terminal-backend types anywhere.
//!
//! A dialog has exactly two jobs: turn key presses into state changes
//! and host actions ([`DialogModel::on_key`]), and describe what it
//! wants painted ([`DialogModel::view`]). *How* that description is
//! painted is the surface's job — a rendering front end turns it into
//! widgets and decides where on the screen they land. That is why
//! [`ViewSpec`] carries data, never widgets, and why [`DialogAction`]
//! carries strings, never trait objects: the same model has to cross a
//! process boundary unchanged.
//!
//! Every dialog describes one of these shapes. There is no escape
//! hatch: a panel that could not be described declaratively would have
//! to be re-implemented on every surface, which is the cost this seam
//! exists to avoid.

/// A key press, free of any terminal-backend types.
///
/// The host translates its native key events into this before handing
/// them to a model, and drops anything a dialog cannot act on (key
/// releases, modifier chords) rather than inventing a variant for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    /// Move the selection up one row.
    Up,
    /// Move the selection down one row.
    Down,
    /// Jump to the first row.
    Home,
    /// Jump to the last row.
    End,
    /// Collapse the highlighted row, or step one field/tab left.
    Left,
    /// Expand the highlighted row, or step one field/tab right.
    Right,
    /// Step to the next tab.
    Tab,
    /// Step to the previous tab.
    BackTab,
    /// Delete the character before the cursor while editing a field.
    Backspace,
    /// Scroll or page the selection up.
    PageUp,
    /// Scroll or page the selection down.
    PageDown,
    /// Commit the highlighted row.
    Enter,
    /// Delete the highlighted row.
    Delete,
    /// Dismiss the dialog.
    Escape,
    /// A printable character (`j` / `k` navigation, digit shortcuts).
    ///
    /// `plain` is false when Ctrl or Alt was held. Most dialogs ignore
    /// it and take the character either way; one that binds a bare
    /// letter to an action (`r` to rerun) matches on `plain: true` so a
    /// chord does not trigger it.
    Char {
        /// The character produced by the key.
        value: char,
        /// Whether the key came without Ctrl or Alt.
        plain: bool,
    },
}

impl DialogKey {
    /// A character key with no Ctrl or Alt held.
    pub fn plain(value: char) -> Self {
        Self::Char { value, plain: true }
    }
}

/// One key delivered to a dialog.
///
/// `repeat` is separate from the key itself because it is a property of
/// the delivery, not of the key: a dialog guarding a destructive
/// confirmation refuses to let a held key confirm it, and nothing else
/// in the codebase cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    /// Which key it was.
    pub key: DialogKey,
    /// True when this is auto-repeat from a held key.
    pub repeat: bool,
    /// Body rows the surface last painted for this dialog, for the two
    /// that page by screenful. `None` before the first paint. It rides
    /// on the press rather than living in the model because only the
    /// surface knows it, and a model that cached it would need interior
    /// mutability to be told.
    pub viewport_rows: Option<u16>,
}

impl KeyPress {
    /// The viewport the surface reported, or a workable default for a
    /// dialog asked to page before anything was painted.
    pub fn viewport_rows_or(&self, fallback: u16) -> u16 {
        self.viewport_rows.unwrap_or(fallback).max(1)
    }
}

impl From<DialogKey> for KeyPress {
    /// A fresh press with no viewport reported, which is what a test
    /// almost always means.
    fn from(key: DialogKey) -> Self {
        Self {
            key,
            repeat: false,
            viewport_rows: None,
        }
    }
}

/// A side effect a dialog hands back to its host.
///
/// Deliberately data-only. The host matches on
/// (`dialog`, `action`) and reads `values`; nothing here is a closure
/// or a `Box<dyn Any>`, so the same action survives serialization to
/// another surface. That is also why a multi-select's whole selection
/// travels in `values` rather than being read back off the dialog: an
/// action must carry everything its route needs, because a closing
/// action pops the dialog before the route runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAction {
    /// Stable id of the dialog that emitted this action.
    pub dialog: &'static str,
    /// Stable id of the action within that dialog.
    pub action: &'static str,
    /// The action's payload: one entry for a single-value action, a
    /// fixed positional list for a compound one, the whole selection
    /// for a multi-select. Empty when the action carries none.
    pub values: Vec<String>,
    /// Whether the host pops the dialog before routing the action.
    pub close: bool,
}

impl DialogAction {
    /// A single-value action that closes the dialog as it fires.
    pub fn closing(dialog: &'static str, action: &'static str, value: impl Into<String>) -> Self {
        Self::closing_many(dialog, action, vec![value.into()])
    }

    /// A single-value action that leaves the dialog open.
    pub fn staying(dialog: &'static str, action: &'static str, value: impl Into<String>) -> Self {
        Self::staying_many(dialog, action, vec![value.into()])
    }

    /// A multi-value action that closes the dialog as it fires.
    pub fn closing_many(dialog: &'static str, action: &'static str, values: Vec<String>) -> Self {
        Self {
            dialog,
            action,
            values,
            close: true,
        }
    }

    /// A multi-value action that leaves the dialog open.
    pub fn staying_many(dialog: &'static str, action: &'static str, values: Vec<String>) -> Self {
        Self {
            dialog,
            action,
            values,
            close: false,
        }
    }

    /// The action's first value, or `""` when it carries none.
    pub fn value(&self) -> &str {
        self.value_at(0)
    }

    /// The action's value at `index`, or `""` when there is none.
    pub fn value_at(&self, index: usize) -> &str {
        self.values.get(index).map_or("", String::as_str)
    }
}

/// What the host should do after handing a key to the top dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// The key was consumed; nothing else to do.
    None,
    /// Pop this dialog off the host stack.
    Close,
    /// Route this action, then pop iff [`DialogAction::close`].
    Action(DialogAction),
}

/// One row of a [`ListView`].
///
/// The parts are painted in order and each keeps its own style role,
/// which is what lets one painter reproduce pickers that look quite
/// different: a numbered option list with descriptions, a bare list of
/// model ids, a checkbox list of skills.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListRow {
    /// A checkbox ahead of the prefix. `None` draws none at all,
    /// which is what a single-select list wants.
    pub checked: Option<bool>,
    /// Text ahead of the label, already formatted (`"1. "`).
    pub prefix: Option<String>,
    /// The row's primary text, already padded to its column width.
    pub label: String,
    /// Secondary text after the label.
    pub detail: Option<String>,
    /// A trailing tag such as `"  (current)"`.
    pub badge: Option<ListBadge>,
}

/// A trailing tag on a [`ListRow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListBadge {
    /// The tag text, including any leading spaces it wants.
    pub text: String,
    /// Whether the tag is emphasised (bold) as well as coloured.
    pub bold: bool,
}

/// A bordered, keyboard-driven list: the shape this crate's small pickers
/// and option panels share.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListView {
    /// Border title, including its surrounding spaces.
    pub title: String,
    /// Dimmed lines above the rows. A trailing empty entry is the
    /// blank separator, so the caller controls spacing exactly.
    pub header: Vec<String>,
    /// The selectable rows, in display order.
    pub rows: Vec<ListRow>,
    /// Index into `rows` of the highlighted row.
    pub selected: usize,
    /// Dimmed lines below the rows, after one blank separator.
    pub footer: Vec<String>,
    /// Row cap before the list scrolls. `None` renders every row and
    /// lets the viewport clip, which is what an always-short list
    /// (five reasoning levels) wants.
    pub max_visible: Option<usize>,
    /// Which palette role highlights the selected row.
    pub accent: ListAccent,
    /// Whether a row's `detail` takes the accent along with its label
    /// when selected. False keeps every detail dim, which is what a
    /// list whose detail is a subtitle rather than a description wants.
    pub detail_follows_selection: bool,
}

/// The palette role a [`ListView`] highlights its selection with.
///
/// Two dialogs of the same shape can still disagree about this — the
/// reasoning-level picker marks its selection in the success colour,
/// the model picker in the brand colour — so it is the model's
/// choice, not the painter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListAccent {
    /// The product's brand colour.
    #[default]
    Brand,
    /// The success colour.
    Success,
}

impl ListView {
    /// Rows actually shown at once, before any viewport clamp.
    pub fn visible_rows(&self) -> usize {
        match self.max_visible {
            Some(cap) => self.rows.len().min(cap),
            None => self.rows.len(),
        }
    }

    /// Rows of chrome around the list: the border, the header lines,
    /// the blank separator, and the footer lines.
    pub fn chrome_height(&self) -> usize {
        2 + self.header.len() + 1 + self.footer.len()
    }

    /// Preferred host height so an inline layout can size the dialog
    /// to its content instead of guessing.
    pub fn desired_height(&self) -> u16 {
        let rows = self.visible_rows().min(u16::MAX as usize) as u16;
        rows.saturating_add(self.chrome_height().min(u16::MAX as usize) as u16)
    }
}

/// One line of an [`OutlineView`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutlineRow {
    /// The whole line, already formatted — indentation, expand markers
    /// and separators included.
    pub text: String,
    /// Whether the line reads as content or as supporting detail.
    pub tone: RowTone,
}

impl OutlineRow {
    /// A content line.
    pub fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: RowTone::Normal,
        }
    }

    /// A supporting-detail line.
    pub fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: RowTone::Dim,
        }
    }
}

/// How a line of an [`OutlineView`], or one run of a [`TextSpan`], reads.
///
/// Roles rather than colours: a model says a word is a warning, and each
/// surface decides what a warning looks like in its own palette. The set
/// is closed on purpose — a model that wants a ninth role is describing
/// its own theme, which is the surface's to own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowTone {
    /// Reads as content.
    #[default]
    Normal,
    /// Reads as supporting detail.
    Dim,
    /// Content, emphasised: a heading, a field label ahead of its value.
    Strong,
    /// The product's brand colour, emphasised.
    Brand,
    /// The colour a surface marks a focused row or a key hint with.
    Focus,
    /// Reads as a good outcome.
    Success,
    /// Reads as something needing attention.
    Warning,
    /// Reads as a failure.
    Error,
}

/// A scrolling pane of pre-formatted lines with one highlighted row and
/// a footer pinned to the bottom.
///
/// The shape a browser has when its rows are not a uniform list: an
/// outline whose children indent under their parent, a table of bars.
/// Unlike [`ListView`] the rows are already text, the highlight is a
/// reversed row rather than an accent, and scrolling is by line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutlineView {
    /// Border title, including its surrounding spaces.
    pub title: String,
    /// Every line of the body, in display order.
    pub rows: Vec<OutlineRow>,
    /// Index into `rows` of the highlighted line, if one is highlighted.
    pub selected: Option<usize>,
    /// `Some(n)` scrolls to line `n`. `None` asks the surface to scroll
    /// the least it can to keep `selected` on screen, which is what a
    /// browser that only moves by selection wants.
    pub scroll: Option<u16>,
    /// One dim line pinned to the bottom, outside the scrolling body.
    pub footer: String,
    /// Shown in place of the rows when there are none.
    pub empty_text: Option<String>,
}

/// One result row of a [`SearchView`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchRow {
    /// The row's text, already built to the list's row width.
    pub text: String,
    /// Whether this is the focused result.
    pub focused: bool,
}

/// One run of footer text. The footer alternates emphasised key names
/// with dim descriptions, so it is segments rather than a line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchFooterSegment {
    /// The text of this run, including its surrounding spaces.
    pub text: String,
    /// Whether it reads as a key name rather than a description.
    pub emphasis: bool,
}

/// Where the preview pane goes and how wide each half is.
///
/// The numbers come from the panel's own layout maths, which knows what
/// its rows look like; the painter only places what it is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchLayout {
    /// Columns for the result list when the preview is beside it.
    pub list_width: usize,
    /// Rows for the preview pane when it is below.
    pub preview_rows: usize,
    /// Whether the preview sits to the right rather than below.
    pub preview_on_right: bool,
    /// Stacked, `Some(n)` fixes the list at `n` rows and lets the
    /// preview take the rest; `None` does the opposite. Which way round
    /// depends on whether the list or the preview is the thing being
    /// read, so the panel says.
    pub stacked_list_rows: Option<usize>,
    /// Rows the body is never squeezed below.
    pub body_min_rows: u16,
}

/// A filter box over a result list, with a preview of the focused result.
///
/// Shaped for the panels that search a snapshot: a query line, a bordered
/// list whose windowing the panel has already done, a bordered preview,
/// and a segmented footer. A panel that searches asynchronously or reads
/// files to build its preview needs a source seam this does not carry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchView {
    /// Border title, including its surrounding spaces.
    pub title: String,
    /// The typed filter. Empty shows `placeholder` instead.
    pub query: String,
    /// Shown dim in place of an empty query.
    pub placeholder: String,
    /// The visible window of results, in display order.
    pub rows: Vec<SearchRow>,
    /// Shown in both panes when there is nothing to show.
    pub empty_message: String,
    /// A line above the preview naming what is being previewed.
    pub preview_header: Option<String>,
    /// The focused result's preview, already wrapped to width.
    pub preview: Vec<String>,
    /// Whether the preview reads as content or as supporting detail.
    pub preview_tone: RowTone,
    /// Shown when there is no preview. Often the same as
    /// `empty_message`, but a panel that can list a result it cannot
    /// preview needs its own words.
    pub preview_empty_message: String,
    /// Lines the preview had to drop, or zero.
    pub preview_more: usize,
    /// The footer runs, left to right.
    pub footer: Vec<SearchFooterSegment>,
    /// Where the preview goes.
    pub layout: SearchLayout,
}

/// One styled run of text inside a [`PanelRow`] or a panel footer.
///
/// A panel row is a list of runs rather than one string because the
/// lines these panels draw carry several roles at once — a bold field
/// label ahead of its value, a status word coloured by what it says, a
/// key name emphasised inside a hint. Flattening that to one tone per
/// line would throw away the only thing the colour was carrying.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextSpan {
    /// The run's text, including any spaces it wants around itself.
    pub text: String,
    /// How the run reads.
    pub tone: RowTone,
}

impl TextSpan {
    /// A run in an explicit tone.
    pub fn new(text: impl Into<String>, tone: RowTone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }

    /// A run that reads as content.
    pub fn normal(text: impl Into<String>) -> Self {
        Self::new(text, RowTone::Normal)
    }

    /// A run that reads as supporting detail.
    pub fn dim(text: impl Into<String>) -> Self {
        Self::new(text, RowTone::Dim)
    }

    /// A run that reads as emphasised content.
    pub fn strong(text: impl Into<String>) -> Self {
        Self::new(text, RowTone::Strong)
    }
}

/// One row of a [`PanelPane`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelRow {
    /// The row's runs, left to right.
    pub spans: Vec<TextSpan>,
    /// Whether the surface reverses the whole row.
    ///
    /// Separate from the runs' tones because it is a different kind of
    /// statement: the tones say what each run *is*, this says the row is
    /// the one under the cursor. A panel that marks its selection some
    /// other way — a leading marker, a brand-coloured label — leaves it
    /// false and says so in the runs.
    pub highlighted: bool,
}

impl PanelRow {
    /// A row of one run.
    pub fn one(span: TextSpan) -> Self {
        Self {
            spans: vec![span],
            highlighted: false,
        }
    }

    /// A row of several runs.
    pub fn spans(spans: Vec<TextSpan>) -> Self {
        Self {
            spans,
            highlighted: false,
        }
    }

    /// A blank spacer row.
    pub fn blank() -> Self {
        Self::default()
    }

    /// The same row, reversed.
    pub fn highlighted(mut self) -> Self {
        self.highlighted = true;
        self
    }

    /// Every run's text, concatenated: what a test asserts on and what a
    /// surface measures when it needs the row's width.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// One pane of a [`PanelView`]: a scrolling column of rows, optionally
/// inside a border of its own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelPane {
    /// Border title, including its surrounding spaces. `None` draws no
    /// border, which is what a pane filling the panel wants.
    pub title: Option<String>,
    /// The rows, in display order.
    pub rows: Vec<PanelRow>,
    /// First row painted; the rows above it scroll off the top.
    pub scroll: u16,
    /// Wrap a row wider than the pane instead of clipping it.
    pub wrap: bool,
    /// Painted dim in place of the rows when there are none.
    pub empty_text: Option<String>,
}

impl PanelPane {
    /// A borderless pane of rows.
    pub fn rows(rows: Vec<PanelRow>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// The same pane inside a titled border.
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// One tab of a [`PanelView`]'s strip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelTab {
    /// The tab's name as shown.
    pub label: String,
    /// Whether this is the tab whose body is on screen.
    pub active: bool,
}

/// How a two-pane panel divides its body.
///
/// The numbers belong to the panel, not to the painter: a config list
/// that is unreadable under 34 columns says so, and the painter obeys
/// rather than re-deriving a rule it cannot know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanelSplit {
    /// Share of the body width the first pane takes, as a percentage.
    /// `0` divides it evenly.
    pub first_percent: u16,
    /// Columns the first pane is never squeezed below.
    pub first_min_columns: u16,
    /// Blank columns between the two panes.
    pub gap: u16,
    /// Body width below which the panes stack instead of sitting side by
    /// side. `0` never stacks.
    pub stack_below: u16,
    /// Rows the second pane takes when the two are stacked.
    pub stacked_second_rows: u16,
}

/// A bordered panel: an optional tab strip, one or two panes of styled
/// rows, and a segmented footer.
///
/// The shape shared by the panels a list and an outline cannot describe
/// — several roles on one line, a detail pane beside a browser, tabs
/// above both. Nothing here names a particular panel: drop the tabs and
/// the side pane and it is a scrolling report; keep them and it is a
/// tabbed settings surface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelView {
    /// Border title, including its surrounding spaces.
    pub title: String,
    /// The tab strip above the body. Empty draws no strip.
    pub tabs: Vec<PanelTab>,
    /// The main pane.
    pub body: PanelPane,
    /// A second pane beside the body, or below it on a narrow frame.
    /// `None` is a single-pane panel.
    pub side: Option<PanelPane>,
    /// How the two panes divide the body. Ignored without a `side`.
    pub split: PanelSplit,
    /// The footer runs, left to right. Empty draws no footer row.
    pub footer: Vec<TextSpan>,
    /// Preferred host height, for a panel that wants to be sized to its
    /// content. `None` fills whatever the host offers.
    pub desired_height: Option<u16>,
}

/// What a dialog wants painted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewSpec {
    /// A bordered list with optional header and footer text.
    List(ListView),
    /// A scrolling pane of pre-formatted lines with a pinned footer.
    Outline(OutlineView),
    /// A filter box over a result list with a preview pane.
    Search(SearchView),
    /// A tabbed panel of one or two panes of styled rows.
    Panel(PanelView),
}

/// Apply a navigation action to an optional [`NavigationState`] in
/// place.
///
/// `NavigationState::dispatch` consumes and returns the state, so a panel
/// holding one behind an `Option` needs the same take-and-put-back
/// three-liner; this is that three-liner, once.
///
/// [`NavigationState`]: rebon_customselect::NavigationState
pub fn dispatch_nav<T: rebon_customselect::OptionId>(
    slot: &mut Option<rebon_customselect::NavigationState<T>>,
    action: rebon_customselect::NavigationAction<T>,
) {
    if let Some(nav) = slot.take() {
        *slot = Some(nav.dispatch(action));
    }
}

/// Emit the three plumbing methods every [`DialogModel`] implements
/// identically: the two downcast hooks and the boxed clone. Write
/// `rebon_dialog::dialog_plumbing!();` as the first line of the `impl`
/// block. Requires the type to be `Clone`.
///
/// These exist because `Clone` is not object safe and trait upcasting
/// to `Any` landed after this workspace's minimum Rust version. Neither
/// is a decision a dialog author should have to re-read.
#[macro_export]
macro_rules! dialog_plumbing {
    () => {
        fn as_any(&self) -> &dyn ::core::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn ::core::any::Any {
            self
        }

        fn box_clone(&self) -> ::std::boxed::Box<dyn $crate::model::DialogModel> {
            ::std::boxed::Box::new(self.clone())
        }
    };
}

/// A dialog: a keyboard reducer plus a declarative view.
pub trait DialogModel: Send {
    /// Stable id, used for action routing and native view lookup.
    fn id(&self) -> &'static str;

    /// Downcast hook, so a host that opened a panel by id can reach the
    /// concrete state behind it. Models a host never writes back into
    /// never need it, but the one-line body is cheaper than splitting
    /// the trait.
    ///
    /// Implement it as `fn as_any(&self) -> &dyn Any { self }`. Trait
    /// upcasting would make this unnecessary; it landed after this
    /// workspace's declared minimum Rust version.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Mutable half of [`DialogModel::as_any`], for a host writing a
    /// recomputed result back into a dialog it left open. Implement it
    /// as `fn as_any_mut(&mut self) -> &mut dyn Any { self }`.
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;

    /// Clone into a fresh box, so a host that lives on a cloneable
    /// application state can be cloned with it. `Clone` itself is not
    /// object safe; implement this as `Box::new(self.clone())`.
    fn box_clone(&self) -> Box<dyn DialogModel>;

    /// Apply a key press.
    fn on_key(&mut self, press: KeyPress) -> DialogOutcome;

    /// Describe what to paint.
    fn view(&self) -> ViewSpec;

    /// Tell the dialog the size it is about to be painted at.
    ///
    /// Called by the host immediately before [`Self::view`], so a panel
    /// whose layout depends on the frame — a search pane deciding
    /// whether its preview fits beside the list — can answer for the
    /// size it is actually getting rather than a guess. Most dialogs
    /// ignore it; none may paint from it, since painting is the
    /// surface's.
    fn note_viewport(&mut self, _rows: u16, _cols: u16) {}

    /// Whether this dialog owns the whole viewport (and therefore
    /// input focus) rather than sitting inside the prompt area.
    fn is_fullscreen(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> ListRow {
        ListRow {
            label: label.into(),
            ..ListRow::default()
        }
    }

    #[test]
    fn uncapped_list_height_is_every_row_plus_chrome() {
        let view = ListView {
            rows: vec![row("a"), row("b"), row("c")],
            footer: vec!["help".into()],
            ..ListView::default()
        };
        // 3 rows + border(2) + blank(1) + footer(1).
        assert_eq!(view.visible_rows(), 3);
        assert_eq!(view.desired_height(), 7);
    }

    #[test]
    fn capped_list_height_stops_at_the_cap() {
        let view = ListView {
            rows: (0..25).map(|i| row(&format!("r{i}"))).collect(),
            header: vec!["Active: x".into(), String::new()],
            footer: vec!["help".into()],
            max_visible: Some(10),
            ..ListView::default()
        };
        // 10 rows + border(2) + header(2) + blank(1) + footer(1).
        assert_eq!(view.visible_rows(), 10);
        assert_eq!(view.desired_height(), 16);
    }

    #[test]
    fn action_constructors_set_the_close_flag() {
        let closing = DialogAction::closing("effort", "select", "high");
        assert!(closing.close);
        assert_eq!(closing.value(), "high");
        let staying = DialogAction::staying("memory", "open", "REBON.md");
        assert!(!staying.close);
        assert_eq!(staying.dialog, "memory");
        assert_eq!(staying.action, "open");
    }

    #[test]
    fn a_compound_action_is_read_by_position_and_runs_out_to_empty() {
        let action =
            DialogAction::closing_many("model", "select", vec!["gpt".into(), "openai".into()]);
        assert_eq!(action.value(), "gpt");
        assert_eq!(action.value_at(1), "openai");
        assert_eq!(action.value_at(2), "");
        assert_eq!(DialogAction::closing_many("x", "y", Vec::new()).value(), "");
    }
}
