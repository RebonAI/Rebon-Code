use std::io;

use crate::{
    backend::{Backend, ClearType},
    buffer::{Buffer, Cell},
    layout::{Position, Rect, Size},
    CompletedFrame, Frame, TerminalOptions, Viewport,
};

/// An interface to interact and draw [`Frame`]s on the user's terminal.
///
/// This is the main entry point for Ratatui. It is responsible for drawing and maintaining the
/// state of the buffers, cursor and viewport.
///
/// The [`Terminal`] is generic over a [`Backend`] implementation which is used to interface with
/// the underlying terminal library. The [`Backend`] trait is implemented for three popular Rust
/// terminal libraries: [Crossterm], [Termion] and [Termwiz]. See the [`backend`] module for more
/// information.
///
/// The `Terminal` struct maintains two buffers: the current and the previous.
/// When the widgets are drawn, the changes are accumulated in the current buffer.
/// At the end of each draw pass, the two buffers are compared, and only the changes
/// between these buffers are written to the terminal, avoiding any redundant operations.
/// After flushing these changes, the buffers are swapped to prepare for the next draw cycle.
///
/// The terminal also has a viewport which is the area of the terminal that is currently visible to
/// the user. It can be either fullscreen, inline or fixed. See [`Viewport`] for more information.
///
/// Applications should detect terminal resizes and call [`Terminal::draw`] to redraw the
/// application with the new size. This will automatically resize the internal buffers to match the
/// new size for inline and fullscreen viewports. Fixed viewports are not resized automatically.
///
/// # Examples
///
/// ```rust,no_run
/// use std::io::stdout;
///
/// use ratatui::{backend::CrosstermBackend, widgets::Paragraph, Terminal};
///
/// let backend = CrosstermBackend::new(stdout());
/// let mut terminal = Terminal::new(backend)?;
/// terminal.draw(|frame| {
///     let area = frame.area();
///     frame.render_widget(Paragraph::new("Hello World!"), area);
/// })?;
/// # std::io::Result::Ok(())
/// ```
///
/// [Crossterm]: https://crates.io/crates/crossterm
/// [Termion]: https://crates.io/crates/termion
/// [Termwiz]: https://crates.io/crates/termwiz
/// [`backend`]: crate::backend
/// [`Backend`]: crate::backend::Backend
/// [`Buffer`]: crate::buffer::Buffer
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Terminal<B>
where
    B: Backend,
{
    /// The backend used to interface with the terminal
    backend: B,
    /// Holds the results of the current and previous draw calls. The two are compared at the end
    /// of each draw pass to output the necessary updates to the terminal
    buffers: [Buffer; 2],
    /// Index of the current buffer in the previous array
    current: usize,
    /// Whether the cursor is currently hidden
    hidden_cursor: bool,
    /// Viewport
    viewport: Viewport,
    /// Area of the viewport
    viewport_area: Rect,
    /// Last known area of the terminal. Used to detect if the internal buffers have to be resized.
    last_known_area: Rect,
    /// Last known position of the cursor. Used to find the new area when the viewport is inlined
    /// and the terminal resized.
    last_known_cursor_pos: Position,
    /// Number of frames rendered up until current time.
    frame_count: usize,
}

/// Options to pass to [`Terminal::with_options`]
#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Options {
    /// Viewport used to draw to the terminal
    pub viewport: Viewport,
}

impl<B> Drop for Terminal<B>
where
    B: Backend,
{
    fn drop(&mut self) {
        // Attempt to restore the cursor state
        if self.hidden_cursor {
            if let Err(err) = self.show_cursor() {
                eprintln!("Failed to show the cursor: {err}");
            }
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend,
{
    /// Creates a new [`Terminal`] with the given [`Backend`] with a full screen viewport.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::io::stdout;
    ///
    /// use ratatui::{backend::CrosstermBackend, Terminal};
    ///
    /// let backend = CrosstermBackend::new(stdout());
    /// let terminal = Terminal::new(backend)?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn new(backend: B) -> io::Result<Self> {
        Self::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
    }

    /// Creates a new [`Terminal`] with the given [`Backend`] and [`TerminalOptions`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::io::stdout;
    ///
    /// use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal, TerminalOptions, Viewport};
    ///
    /// let backend = CrosstermBackend::new(stdout());
    /// let viewport = Viewport::Fixed(Rect::new(0, 0, 10, 10));
    /// let terminal = Terminal::with_options(backend, TerminalOptions { viewport })?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn with_options(mut backend: B, options: TerminalOptions) -> io::Result<Self> {
        let area = match options.viewport {
            Viewport::Fullscreen | Viewport::Inline(_) => {
                Rect::from((Position::ORIGIN, backend.size()?))
            }
            Viewport::Fixed(area) => area,
        };
        let (viewport_area, cursor_pos) = match options.viewport {
            Viewport::Fullscreen => (area, Position::ORIGIN),
            Viewport::Inline(height) => {
                compute_inline_size(&mut backend, height, area.as_size(), 0)?
            }
            Viewport::Fixed(area) => (area, area.as_position()),
        };
        Ok(Self {
            backend,
            buffers: [Buffer::empty(viewport_area), Buffer::empty(viewport_area)],
            current: 0,
            hidden_cursor: false,
            viewport: options.viewport,
            viewport_area,
            last_known_area: area,
            last_known_cursor_pos: cursor_pos,
            frame_count: 0,
        })
    }

    /// Get a Frame object which provides a consistent view into the terminal state for rendering.
    pub fn get_frame(&mut self) -> Frame<'_> {
        let count = self.frame_count;
        Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
            count,
        }
    }

    /// Gets the current buffer as a mutable reference.
    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Gets the backend
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Gets the backend as a mutable reference
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Obtains a difference between the previous and the current buffer and passes it to the
    /// current backend for drawing.
    pub fn flush(&mut self) -> io::Result<()> {
        let previous_buffer = &self.buffers[1 - self.current];
        let current_buffer = &self.buffers[self.current];
        let updates = previous_buffer.diff(current_buffer);
        if let Some((col, row, _)) = updates.last() {
            self.last_known_cursor_pos = Position { x: *col, y: *row };
        }
        self.backend.draw(updates.into_iter())
    }

    /// Updates the Terminal so that internal buffers match the requested area.
    ///
    /// Requested area will be saved to remain consistent when rendering. This leads to a full clear
    /// of the screen.
    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        let next_area = match self.viewport {
            Viewport::Inline(height) => {
                let offset_in_previous_viewport = self
                    .last_known_cursor_pos
                    .y
                    .saturating_sub(self.viewport_area.top());
                compute_inline_size(
                    &mut self.backend,
                    height,
                    area.as_size(),
                    offset_in_previous_viewport,
                )?
                .0
            }
            Viewport::Fixed(_) | Viewport::Fullscreen => area,
        };
        let previous_viewport_area = self.viewport_area;
        self.set_viewport_area(next_area);
        if matches!(self.viewport, Viewport::Inline(_)) {
            self.viewport = Viewport::Inline(next_area.height);
        }
        self.last_known_area = area;
        if matches!(self.viewport, Viewport::Inline(_)) {
            let clear_top = previous_viewport_area.top().min(next_area.top());
            self.clear_rows(clear_top, area.bottom())?;
            self.last_known_cursor_pos = Position {
                x: 0,
                y: next_area.bottom().saturating_sub(1),
            };
            self.buffers[1 - self.current].reset();
        } else {
            self.clear()?;
        }

        Ok(())
    }

    fn set_viewport_area(&mut self, area: Rect) {
        self.buffers[self.current].resize(area);
        self.buffers[1 - self.current].resize(area);
        self.viewport_area = area;
    }

    fn clear_rows(&mut self, start: u16, end: u16) -> io::Result<()> {
        if self.last_known_area.width == 0 {
            return Ok(());
        }
        let start = start.min(self.last_known_area.bottom());
        let end = end.min(self.last_known_area.bottom());
        for y in start..end {
            self.backend.set_cursor_position(Position { x: 0, y })?;
            self.backend.clear_region(ClearType::CurrentLine)?;
        }
        Ok(())
    }

    /// Force the next `draw()` to repaint the entire viewport — including blank
    /// cells — without issuing a physical screen clear.
    ///
    /// [`Self::clear`] would also achieve the full redraw, but its
    /// `clear_region(AfterCursor)` blanks the viewport on the wire *before* the
    /// redraw refills it. On every viewport resize or inline commit that is a
    /// visible blank flash. Instead, poison the previous buffer with a sentinel
    /// cell that cannot match normal rendered output, so `Buffer::diff` emits the
    /// next frame's real cells (spaces included) directly over the old screen.
    /// **Rebon-vendored fork only.**
    fn force_full_inline_redraw(&mut self) {
        for cell in &mut self.buffers[1 - self.current].content {
            cell.set_symbol("\0");
        }
    }

    /// Re-seed the inline viewport after the caller has physically erased the
    /// screen **and** scrollback (e.g. `ESC[2J ESC[3J`) and homed the cursor
    /// to `(0, 0)`.
    ///
    /// **Rebon-vendored fork only.** The inline full-repaint-on-resize path
    /// (see the event loop) wipes the terminal — width-reflowed committed rows
    /// and all — then rebuilds everything at the new width through the normal
    /// startup sequence (`set_viewport_height` + banner `insert_before` +
    /// transcript re-commit + `draw`). For that rebuild to land, the
    /// Terminal's tracked geometry must match the freshly cleared screen
    /// exactly as [`Self::with_options`] first established it:
    /// - both buffers reset so the next `draw` has no stale diff baseline and
    ///   repaints the viewport in full,
    /// - `last_known_area` set to the current backend size so
    ///   [`Self::autoresize`] sees no change and skips its targeted
    ///   `clear_rows`,
    /// - the viewport re-anchored against the homed cursor via
    ///   [`compute_inline_size`].
    ///
    /// The viewport is seeded to a single top row at `(0, 0)`; the caller is
    /// expected to immediately grow it to the real height with
    /// [`Self::set_viewport_height`]. Seeding one row keeps
    /// `compute_inline_size`'s `append_lines` at zero so this never scrolls —
    /// important on a height shrink, where the previous inline height can
    /// exceed the new (shorter) screen and a larger seed would scroll blank
    /// rows back into the just-cleared scrollback. No-op for non-inline
    /// viewports.
    pub fn reseed_inline_viewport_after_clear(&mut self) -> io::Result<()> {
        if !matches!(self.viewport, Viewport::Inline(_)) {
            return Ok(());
        }
        let area = Rect::from((Position::ORIGIN, self.backend.size()?));
        let (viewport_area, cursor_pos) =
            compute_inline_size(&mut self.backend, 1, area.as_size(), 0)?;
        self.set_viewport_area(viewport_area);
        self.viewport = Viewport::Inline(viewport_area.height);
        self.last_known_area = area;
        self.last_known_cursor_pos = cursor_pos;
        self.buffers[0].reset();
        self.buffers[1].reset();
        Ok(())
    }

    /// Resize the inline viewport to `new_height` rows.
    ///
    /// While the viewport is still above the bottom of the screen, resizing is
    /// top-anchored so temporary prompt suffixes (pickers, queues) use the
    /// blank space below instead of scrolling committed rows above. Once the
    /// viewport reaches the bottom, resizing is bottom-anchored so the prompt/UI
    /// block stays at the visual bottom.
    ///
    /// **Rebon-vendored fork only.** Backport of the upstream API proposed in
    /// ratatui PR [#1964](https://github.com/ratatui/ratatui/pull/1964), which
    /// has not yet shipped in a released ratatui version. When upstream lands
    /// it, swap the [patch.crates-io] override out and remove this method.
    ///
    /// Behavior by [`Viewport`] variant:
    /// - [`Viewport::Inline`]: resizing is top-anchored while there is unused
    ///   screen space below the viewport. Growing consumes that space first;
    ///   shrinking releases (and clears) rows below the viewport. Once the
    ///   viewport reaches the screen bottom, additional growth scrolls only the
    ///   missing rows into scrollback and shrink keeps the prompt/UI block
    ///   anchored to the visual bottom, clearing the rows the closing surface
    ///   vacated at the top. This bottom-anchored shrink does **not** scroll the
    ///   screen — see the inline note below for why, and for how the common
    ///   streaming "commit then shrink" case is handled by
    ///   [`Self::shrink_inline_viewport_keeping_top`] instead. Both paths update
    ///   the stored `Viewport::Inline(height)` so subsequent autoresize calls
    ///   honor the new height instead of snapping back to the original.
    /// - [`Viewport::Fullscreen`] / [`Viewport::Fixed`]: no-op; those modes
    ///   don't have a configurable inline height. Returns `Ok(())`.
    ///
    /// `new_height` is clamped to `[1, screen_height]`.
    pub fn set_viewport_height(&mut self, new_height: u16) -> io::Result<()> {
        let current_inline = match self.viewport {
            Viewport::Inline(h) => h,
            Viewport::Fullscreen | Viewport::Fixed(_) => return Ok(()),
        };
        let screen_height = self.last_known_area.height;
        if screen_height == 0 {
            return Ok(());
        }
        let new_height = new_height.clamp(1, screen_height);
        if new_height == current_inline {
            return Ok(());
        }

        let current_area = self.viewport_area;
        let screen_bottom = self.last_known_area.bottom();
        if new_height > current_inline {
            let delta = new_height - current_inline;
            let available_below = screen_bottom.saturating_sub(current_area.bottom());
            let grow_down = delta.min(available_below);
            let grow_up = delta - grow_down;
            if grow_up > 0 {
                // Once the inline viewport has reached the bottom of the
                // screen, additional height has to be claimed above it. Scroll
                // only for that remainder so temporary picker growth before the
                // viewport bottoms out does not push the startup banner away.
                #[cfg(not(feature = "scrolling-regions"))]
                self.scroll_up(grow_up)?;
                #[cfg(feature = "scrolling-regions")]
                {
                    let viewport_top = current_area.top();
                    if viewport_top > 0 {
                        let scroll = grow_up.min(viewport_top);
                        self.backend.scroll_region_up(0..viewport_top, scroll)?;
                    }
                }
            }
            let new_area = Rect {
                x: current_area.x,
                y: current_area.y.saturating_sub(grow_up),
                width: current_area.width,
                height: new_height,
            };
            self.set_viewport_area(new_area);
        } else {
            let delta = current_inline - new_height;
            if current_area.bottom() < screen_bottom {
                // Not yet glued to the bottom: top-anchored shrink, releasing
                // (and clearing) the rows below the viewport.
                let new_area = Rect {
                    x: current_area.x,
                    y: current_area.y,
                    width: current_area.width,
                    height: new_height,
                };
                self.clear_rows(new_area.bottom(), current_area.bottom())?;
                self.set_viewport_area(new_area);
            } else {
                // Glued to the bottom: bottom-anchored shrink keeps the prompt
                // / UI block at the visual bottom (e.g. an Explorer/Agent
                // full-height surface closing). The `delta` rows it vacated at
                // the TOP are cleared. We deliberately do NOT scroll the screen
                // here: scrolling *down* to slide committed content flush
                // against the prompt manufactures blank rows at the top of the
                // screen that the next at-bottom grow folds permanently into
                // scrollback as a mid-history blank band, and it makes
                // streaming output flicker (scroll up to commit, then scroll
                // down to shrink, every frame).
                //
                // The common case — a shrink because the live tail shortened
                // when rows were sealed/committed — is handled a step earlier
                // by the inline event loop: it shrinks the viewport *before* the
                // commit via [`Self::shrink_inline_viewport_keeping_top`], so
                // the following `insert_before` refills the freed rows with the
                // committed content and re-anchors the viewport to the bottom
                // with zero scrollback churn. Only genuinely commit-less shrinks
                // (a closing modal/picker) reach this branch.
                let new_area = Rect {
                    x: current_area.x,
                    y: current_area.y.saturating_add(delta),
                    width: current_area.width,
                    height: new_height,
                };
                self.clear_rows(current_area.y, new_area.y)?;
                self.set_viewport_area(new_area);
            }
        }

        self.viewport = Viewport::Inline(new_height);
        // Force a full redraw on the next `draw()` so the buffer diff doesn't
        // compare against the previous frame at the old area — but WITHOUT a
        // physical `clear()`, whose blank-then-refill flash is the streaming
        // flicker (the viewport resizes nearly every frame). The vacated rows
        // were already cleared by `clear_rows`/left blank by `scroll_up` above.
        self.force_full_inline_redraw();
        Ok(())
    }

    fn shrink_inline_viewport_keeping_top_impl(
        &mut self,
        new_height: u16,
        clear_released_rows: bool,
        keep_released_rows: u16,
    ) -> io::Result<()> {
        let current_inline = match self.viewport {
            Viewport::Inline(h) => h,
            Viewport::Fullscreen | Viewport::Fixed(_) => return Ok(()),
        };
        if self.last_known_area.height == 0 {
            return Ok(());
        }
        let new_height = new_height.clamp(1, current_inline);
        if new_height >= current_inline {
            return Ok(());
        }

        let current_area = self.viewport_area;
        let new_area = Rect {
            x: current_area.x,
            y: current_area.y,
            width: current_area.width,
            height: new_height,
        };
        if clear_released_rows {
            self.clear_rows(new_area.bottom(), current_area.bottom())?;
        } else {
            let released_rows = current_area.bottom().saturating_sub(new_area.bottom());
            let clear_start = new_area
                .bottom()
                .saturating_add(keep_released_rows.min(released_rows));
            self.clear_rows(clear_start, current_area.bottom())?;
        }
        self.set_viewport_area(new_area);
        self.viewport = Viewport::Inline(new_height);
        // Force a full redraw on the next `draw()` without a physical `clear()`
        // flash — see `force_full_inline_redraw`. When `clear_released_rows` is
        // true the freed bottom rows were already cleared above; otherwise the
        // caller keeps only the rows the immediate `insert_before` will reuse.
        self.force_full_inline_redraw();
        Ok(())
    }

    /// Shrink the inline viewport to `new_height`, keeping its TOP row fixed and
    /// releasing (clearing) rows at the BOTTOM — even when the viewport is glued
    /// to the screen bottom.
    ///
    /// **Rebon-vendored fork only.** This is the companion to
    /// [`Self::set_viewport_height`] for prompt-suffix/picker surfaces that close
    /// without committing rows first.
    ///
    /// No-op (returns `Ok(())`) when the viewport is fullscreen/fixed, when the
    /// screen has zero height, or when `new_height` would not actually shrink
    /// the viewport. `new_height` is clamped to `[1, current_height]`.
    pub fn shrink_inline_viewport_keeping_top(&mut self, new_height: u16) -> io::Result<()> {
        self.shrink_inline_viewport_keeping_top_impl(new_height, true, 0)
    }

    /// Shrink the inline viewport to `new_height`, keeping its TOP row fixed,
    /// while preserving only the rows that the immediate [`Self::insert_before`]
    /// call is expected to reuse.
    ///
    /// **Rebon-vendored fork only.** Used as the prelude to an immediate
    /// [`Self::insert_before`] commit: rows that will become part of the shifted
    /// viewport are not blanked on the wire, so streaming does not flash at the
    /// bottom; any extra released rows are cleared so a smaller-than-shrink commit
    /// (for example a queued message being consumed while its banner disappears)
    /// cannot leave a stale second prompt below the real viewport.
    pub fn shrink_inline_viewport_keeping_top_for_insert_before(
        &mut self,
        new_height: u16,
        expected_insert_height: u16,
    ) -> io::Result<()> {
        self.shrink_inline_viewport_keeping_top_impl(new_height, false, expected_insert_height)
    }

    /// Current inline viewport height in rows, or `None` when the viewport is
    /// fullscreen/fixed.
    ///
    /// **Rebon-vendored fork only.** Lets the inline event loop re-read the
    /// viewport height after [`Self::autoresize`] runs: a resize onto a smaller
    /// screen clamps the inline height down (`resize` stores
    /// `Viewport::Inline(min(new_screen, old_height))`), so the loop's tracked
    /// height would otherwise drift out of sync and mis-measure the next
    /// grow/shrink delta.
    pub fn inline_viewport_height(&self) -> Option<u16> {
        match self.viewport {
            Viewport::Inline(_) => Some(self.viewport_area.height),
            Viewport::Fullscreen | Viewport::Fixed(_) => None,
        }
    }

    /// Queries the backend for size and resizes if it doesn't match the previous size.
    pub fn autoresize(&mut self) -> io::Result<()> {
        // fixed viewports do not get autoresized
        if matches!(self.viewport, Viewport::Fullscreen | Viewport::Inline(_)) {
            let area = Rect::from((Position::ORIGIN, self.size()?));
            if area != self.last_known_area {
                self.resize(area)?;
            }
        };
        Ok(())
    }

    /// Draws a single frame to the terminal.
    ///
    /// Returns a [`CompletedFrame`] if successful, otherwise a [`std::io::Error`].
    ///
    /// If the render callback passed to this method can fail, use [`try_draw`] instead.
    ///
    /// Applications should call `draw` or [`try_draw`] in a loop to continuously render the
    /// terminal. These methods are the main entry points for drawing to the terminal.
    ///
    /// [`try_draw`]: Terminal::try_draw
    ///
    /// This method will:
    ///
    /// - autoresize the terminal if necessary
    /// - call the render callback, passing it a [`Frame`] reference to render to
    /// - flush the current internal state by copying the current buffer to the backend
    /// - move the cursor to the last known position if it was set during the rendering closure
    /// - return a [`CompletedFrame`] with the current buffer and the area of the terminal
    ///
    /// The [`CompletedFrame`] returned by this method can be useful for debugging or testing
    /// purposes, but it is often not used in regular applicationss.
    ///
    /// The render callback should fully render the entire frame when called, including areas that
    /// are unchanged from the previous frame. This is because each frame is compared to the
    /// previous frame to determine what has changed, and only the changes are written to the
    /// terminal. If the render callback does not fully render the frame, the terminal will not be
    /// in a consistent state.
    ///
    /// # Examples
    ///
    /// ```
    /// # let backend = ratatui::backend::TestBackend::new(10, 10);
    /// # let mut terminal = ratatui::Terminal::new(backend)?;
    /// use ratatui::{layout::Position, widgets::Paragraph};
    ///
    /// // with a closure
    /// terminal.draw(|frame| {
    ///     let area = frame.area();
    ///     frame.render_widget(Paragraph::new("Hello World!"), area);
    ///     frame.set_cursor_position(Position { x: 0, y: 0 });
    /// })?;
    ///
    /// // or with a function
    /// terminal.draw(render)?;
    ///
    /// fn render(frame: &mut ratatui::Frame) {
    ///     frame.render_widget(Paragraph::new("Hello World!"), frame.area());
    /// }
    /// # std::io::Result::Ok(())
    /// ```
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<CompletedFrame<'_>>
    where
        F: FnOnce(&mut Frame),
    {
        self.try_draw(|frame| {
            render_callback(frame);
            io::Result::Ok(())
        })
    }

    /// Tries to draw a single frame to the terminal.
    ///
    /// Returns [`Result::Ok`] containing a [`CompletedFrame`] if successful, otherwise
    /// [`Result::Err`] containing the [`std::io::Error`] that caused the failure.
    ///
    /// This is the equivalent of [`Terminal::draw`] but the render callback is a function or
    /// closure that returns a `Result` instead of nothing.
    ///
    /// Applications should call `try_draw` or [`draw`] in a loop to continuously render the
    /// terminal. These methods are the main entry points for drawing to the terminal.
    ///
    /// [`draw`]: Terminal::draw
    ///
    /// This method will:
    ///
    /// - autoresize the terminal if necessary
    /// - call the render callback, passing it a [`Frame`] reference to render to
    /// - flush the current internal state by copying the current buffer to the backend
    /// - move the cursor to the last known position if it was set during the rendering closure
    /// - return a [`CompletedFrame`] with the current buffer and the area of the terminal
    ///
    /// The render callback passed to `try_draw` can return any [`Result`] with an error type that
    /// can be converted into an [`std::io::Error`] using the [`Into`] trait. This makes it possible
    /// to use the `?` operator to propagate errors that occur during rendering. If the render
    /// callback returns an error, the error will be returned from `try_draw` as an
    /// [`std::io::Error`] and the terminal will not be updated.
    ///
    /// The [`CompletedFrame`] returned by this method can be useful for debugging or testing
    /// purposes, but it is often not used in regular applicationss.
    ///
    /// The render callback should fully render the entire frame when called, including areas that
    /// are unchanged from the previous frame. This is because each frame is compared to the
    /// previous frame to determine what has changed, and only the changes are written to the
    /// terminal. If the render function does not fully render the frame, the terminal will not be
    /// in a consistent state.
    ///
    /// # Examples
    ///
    /// ```should_panic
    /// # use ratatui::layout::Position;;
    /// # let backend = ratatui::backend::TestBackend::new(10, 10);
    /// # let mut terminal = ratatui::Terminal::new(backend)?;
    /// use std::io;
    ///
    /// use ratatui::widgets::Paragraph;
    ///
    /// // with a closure
    /// terminal.try_draw(|frame| {
    ///     let value: u8 = "not a number".parse().map_err(io::Error::other)?;
    ///     let area = frame.area();
    ///     frame.render_widget(Paragraph::new("Hello World!"), area);
    ///     frame.set_cursor_position(Position { x: 0, y: 0 });
    ///     io::Result::Ok(())
    /// })?;
    ///
    /// // or with a function
    /// terminal.try_draw(render)?;
    ///
    /// fn render(frame: &mut ratatui::Frame) -> io::Result<()> {
    ///     let value: u8 = "not a number".parse().map_err(io::Error::other)?;
    ///     frame.render_widget(Paragraph::new("Hello World!"), frame.area());
    ///     Ok(())
    /// }
    /// # io::Result::Ok(())
    /// ```
    pub fn try_draw<F, E>(&mut self, render_callback: F) -> io::Result<CompletedFrame<'_>>
    where
        F: FnOnce(&mut Frame) -> Result<(), E>,
        E: Into<io::Error>,
    {
        // Autoresize - otherwise we get glitches if shrinking or potential desync between widgets
        // and the terminal (if growing), which may OOB.
        self.autoresize()?;

        let mut frame = self.get_frame();

        render_callback(&mut frame).map_err(Into::into)?;

        // We can't change the cursor position right away because we have to flush the frame to
        // stdout first. But we also can't keep the frame around, since it holds a &mut to
        // Buffer. Thus, we're taking the important data out of the Frame and dropping it.
        let cursor_position = frame.cursor_position;

        // Draw to stdout
        self.flush()?;

        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        self.swap_buffers();

        // Flush
        self.backend.flush()?;

        let completed_frame = CompletedFrame {
            buffer: &self.buffers[1 - self.current],
            area: self.last_known_area,
            count: self.frame_count,
        };

        // increment frame count before returning from draw
        self.frame_count = self.frame_count.wrapping_add(1);

        Ok(completed_frame)
    }

    /// Hides the cursor.
    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    /// Shows the cursor.
    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    /// Gets the current cursor position.
    ///
    /// This is the position of the cursor after the last draw call and is returned as a tuple of
    /// `(x, y)` coordinates.
    #[deprecated = "the method get_cursor_position indicates more clearly what about the cursor to get"]
    pub fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
        let Position { x, y } = self.get_cursor_position()?;
        Ok((x, y))
    }

    /// Sets the cursor position.
    #[deprecated = "the method set_cursor_position indicates more clearly what about the cursor to set"]
    pub fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        self.set_cursor_position(Position { x, y })
    }

    /// Gets the current cursor position.
    ///
    /// This is the position of the cursor after the last draw call.
    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Sets the cursor position.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    /// Record `position` as the last-known cursor position **without emitting a
    /// move**.
    ///
    /// **Rebon-vendored fork only.** The inline event loop positions the prompt
    /// caret out-of-band — a raw `MoveTo` written straight to stdout — to dodge
    /// ratatui's `show_cursor` ordering that flickers the caret and drifts the
    /// Windows IME. That bypass leaves [`Self::last_known_cursor_pos`] pointing
    /// at the last buffer-diff cell (typically the box bottom-border or footer,
    /// a row or two below the caret) instead of the caret itself. On the next
    /// [`Self::resize`], `compute_inline_size` derives
    /// `offset_in_previous_viewport` from that stale row, so the inline viewport
    /// is placed one or two rows too high and `resize`'s targeted clear eats
    /// into the committed rows above it (e.g. the startup banner). Call this
    /// immediately after the out-of-band move so the tracked position stays
    /// consistent with the real caret. The cursor is already there, so unlike
    /// [`Self::set_cursor_position`] nothing is written to the backend.
    pub fn note_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.last_known_cursor_pos = position.into();
    }

    /// Clear the terminal and force a full redraw on the next draw call.
    pub fn clear(&mut self) -> io::Result<()> {
        match self.viewport {
            Viewport::Fullscreen => self.backend.clear_region(ClearType::All)?,
            Viewport::Inline(_) => {
                self.backend
                    .set_cursor_position(self.viewport_area.as_position())?;
                self.backend.clear_region(ClearType::AfterCursor)?;
            }
            Viewport::Fixed(_) => {
                let area = self.viewport_area;
                for y in area.top()..area.bottom() {
                    self.backend.set_cursor_position(Position { x: 0, y })?;
                    self.backend.clear_region(ClearType::AfterCursor)?;
                }
            }
        }
        // Reset the back buffer to make sure the next update will redraw everything.
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    /// Clears the inactive buffer and swaps it with the current buffer
    pub fn swap_buffers(&mut self) {
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
    }

    /// Queries the real size of the backend.
    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    /// Insert some content before the current inline viewport. This has no effect when the
    /// viewport is not inline.
    ///
    /// The `draw_fn` closure will be called to draw into a writable `Buffer` that is `height`
    /// lines tall. The content of that `Buffer` will then be inserted before the viewport.
    ///
    /// If the viewport isn't yet at the bottom of the screen, inserted lines will push it towards
    /// the bottom. Once the viewport is at the bottom of the screen, inserted lines will scroll
    /// the area of the screen above the viewport upwards.
    ///
    /// Before:
    /// ```ignore
    /// +---------------------+
    /// | pre-existing line 1 |
    /// | pre-existing line 2 |
    /// +---------------------+
    /// |       viewport      |
    /// +---------------------+
    /// |                     |
    /// |                     |
    /// +---------------------+
    /// ```
    ///
    /// After inserting 2 lines:
    /// ```ignore
    /// +---------------------+
    /// | pre-existing line 1 |
    /// | pre-existing line 2 |
    /// |   inserted line 1   |
    /// |   inserted line 2   |
    /// +---------------------+
    /// |       viewport      |
    /// +---------------------+
    /// +---------------------+
    /// ```
    ///
    /// After inserting 2 more lines:
    /// ```ignore
    /// +---------------------+
    /// | pre-existing line 2 |
    /// |   inserted line 1   |
    /// |   inserted line 2   |
    /// |   inserted line 3   |
    /// |   inserted line 4   |
    /// +---------------------+
    /// |       viewport      |
    /// +---------------------+
    /// ```
    ///
    /// If more lines are inserted than there is space on the screen, then the top lines will go
    /// directly into the terminal's scrollback buffer. At the limit, if the viewport takes up the
    /// whole screen, all lines will be inserted directly into the scrollback buffer.
    ///
    /// # Examples
    ///
    /// ## Insert a single line before the current viewport
    ///
    /// ```rust
    /// use ratatui::{
    ///     backend::TestBackend,
    ///     style::{Color, Style},
    ///     text::{Line, Span},
    ///     widgets::{Paragraph, Widget},
    ///     Terminal,
    /// };
    /// # let backend = TestBackend::new(10, 10);
    /// # let mut terminal = Terminal::new(backend).unwrap();
    /// terminal.insert_before(1, |buf| {
    ///     Paragraph::new(Line::from(vec![
    ///         Span::raw("This line will be added "),
    ///         Span::styled("before", Style::default().fg(Color::Blue)),
    ///         Span::raw(" the current viewport"),
    ///     ]))
    ///     .render(buf.area, buf);
    /// });
    /// ```
    pub fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        match self.viewport {
            #[cfg(feature = "scrolling-regions")]
            Viewport::Inline(_) => self.insert_before_scrolling_regions(height, draw_fn),
            #[cfg(not(feature = "scrolling-regions"))]
            Viewport::Inline(_) => self.insert_before_no_scrolling_regions(height, draw_fn),
            _ => Ok(()),
        }
    }

    /// Implement `Self::insert_before` using standard backend capabilities.
    #[cfg(not(feature = "scrolling-regions"))]
    fn insert_before_no_scrolling_regions(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut Buffer),
    ) -> io::Result<()> {
        // The approach of this function is to first render all of the lines to insert into a
        // temporary buffer, and then to loop drawing chunks from the buffer to the screen. drawing
        // this buffer onto the screen.
        let area = Rect {
            x: 0,
            y: 0,
            width: self.viewport_area.width,
            height,
        };
        let mut buffer = Buffer::empty(area);
        draw_fn(&mut buffer);
        let mut buffer = buffer.content.as_slice();

        // Use i32 variables so we don't have worry about overflowed u16s when adding, or about
        // negative results when subtracting.
        let mut drawn_height: i32 = self.viewport_area.top().into();
        let mut buffer_height: i32 = height.into();
        let viewport_height: i32 = self.viewport_area.height.into();
        let screen_height: i32 = self.last_known_area.height.into();

        // The algorithm here is to loop, drawing large chunks of text (up to a screen-full at a
        // time), until the remainder of the buffer plus the viewport fits on the screen. We choose
        // this loop condition because it guarantees that we can write the remainder of the buffer
        // with just one call to Self::draw_lines().
        while buffer_height + viewport_height > screen_height {
            // We will draw as much of the buffer as possible on this iteration in order to make
            // forward progress. So we have:
            //
            //     to_draw = min(buffer_height, screen_height)
            //
            // We may need to scroll the screen up to make room to draw. We choose the minimal
            // possible scroll amount so we don't end up with the viewport sitting in the middle of
            // the screen when this function is done. The amount to scroll by is:
            //
            //     scroll_up = max(0, drawn_height + to_draw - screen_height)
            //
            // We want `scroll_up` to be enough so that, after drawing, we have used the whole
            // screen (drawn_height - scroll_up + to_draw = screen_height). However, there might
            // already be enough room on the screen to draw without scrolling (drawn_height +
            // to_draw <= screen_height). In this case, we just don't scroll at all.
            let to_draw = buffer_height.min(screen_height);
            let scroll_up = 0.max(drawn_height + to_draw - screen_height);
            self.scroll_up(scroll_up as u16)?;
            buffer = self.draw_lines((drawn_height - scroll_up) as u16, to_draw as u16, buffer)?;
            drawn_height += to_draw - scroll_up;
            buffer_height -= to_draw;
        }

        // There is now enough room on the screen for the remaining buffer plus the viewport,
        // though we may still need to scroll up some of the existing text first. It's possible
        // that by this point we've drained the buffer, but we may still need to scroll up to make
        // room for the viewport.
        //
        // We want to scroll up the exact amount that will leave us completely filling the screen.
        // However, it's possible that the viewport didn't start on the bottom of the screen and
        // the added lines weren't enough to push it all the way to the bottom. We deal with this
        // case by just ensuring that our scroll amount is non-negative.
        //
        // We want:
        //   screen_height = drawn_height - scroll_up + buffer_height + viewport_height
        // Or, equivalently:
        //   scroll_up = drawn_height + buffer_height + viewport_height - screen_height
        let scroll_up = 0.max(drawn_height + buffer_height + viewport_height - screen_height);
        self.scroll_up(scroll_up as u16)?;
        self.draw_lines(
            (drawn_height - scroll_up) as u16,
            buffer_height as u16,
            buffer,
        )?;
        drawn_height += buffer_height - scroll_up;

        self.set_viewport_area(Rect {
            y: drawn_height as u16,
            ..self.viewport_area
        });

        // Repaint the viewport at its new position on the next `draw()` without
        // physically clearing it first. A physical clear blanks the viewport on
        // the wire before the redraw refills it, which is the visible bottom
        // flicker during streaming commits.
        self.force_full_inline_redraw();

        Ok(())
    }

    /// Implement `Self::insert_before` using scrolling regions.
    ///
    /// If a terminal supports scrolling regions, it means that we can define a subset of rows of
    /// the screen, and then tell the terminal to scroll up or down just within that region. The
    /// rows outside of the region are not affected.
    ///
    /// This function utilizes this feature to avoid having to redraw the viewport. This is done
    /// either by splitting the screen at the top of the viewport, and then creating a gap by
    /// either scrolling the viewport down, or scrolling the area above it up. The lines to insert
    /// are then drawn into the gap created.
    #[cfg(feature = "scrolling-regions")]
    fn insert_before_scrolling_regions(
        &mut self,
        mut height: u16,
        draw_fn: impl FnOnce(&mut Buffer),
    ) -> io::Result<()> {
        // The approach of this function is to first render all of the lines to insert into a
        // temporary buffer, and then to loop drawing chunks from the buffer to the screen. drawing
        // this buffer onto the screen.
        let area = Rect {
            x: 0,
            y: 0,
            width: self.viewport_area.width,
            height,
        };
        let mut buffer = Buffer::empty(area);
        draw_fn(&mut buffer);
        let mut buffer = buffer.content.as_slice();

        // Handle the special case where the viewport takes up the whole screen.
        if self.viewport_area.height == self.last_known_area.height {
            // "Borrow" the top line of the viewport. Draw over it, then immediately scroll it into
            // scrollback. Do this repeatedly until the whole buffer has been put into scrollback.
            let mut first = true;
            while !buffer.is_empty() {
                buffer = if first {
                    self.draw_lines(0, 1, buffer)?
                } else {
                    self.draw_lines_over_cleared(0, 1, buffer)?
                };
                first = false;
                self.backend.scroll_region_up(0..1, 1)?;
            }

            // Redraw the top line of the viewport.
            let width = self.viewport_area.width as usize;
            let top_line = self.buffers[1 - self.current].content[0..width].to_vec();
            self.draw_lines_over_cleared(0, 1, &top_line)?;
            return Ok(());
        }

        // Handle the case where the viewport isn't yet at the bottom of the screen.
        {
            let viewport_top = self.viewport_area.top();
            let viewport_bottom = self.viewport_area.bottom();
            let screen_bottom = self.last_known_area.bottom();
            if viewport_bottom < screen_bottom {
                let to_draw = height.min(screen_bottom - viewport_bottom);
                self.backend
                    .scroll_region_down(viewport_top..viewport_bottom + to_draw, to_draw)?;
                buffer = self.draw_lines_over_cleared(viewport_top, to_draw, buffer)?;
                self.set_viewport_area(Rect {
                    y: viewport_top + to_draw,
                    ..self.viewport_area
                });
                height -= to_draw;
            }
        }

        let viewport_top = self.viewport_area.top();
        while height > 0 {
            let to_draw = height.min(viewport_top);
            self.backend.scroll_region_up(0..viewport_top, to_draw)?;
            buffer = self.draw_lines_over_cleared(viewport_top - to_draw, to_draw, buffer)?;
            height -= to_draw;
        }

        Ok(())
    }

    /// Draw lines at the given vertical offset. The slice of cells must contain enough cells
    /// for the requested lines. A slice of the unused cells are returned.
    fn draw_lines<'a>(
        &mut self,
        y_offset: u16,
        lines_to_draw: u16,
        cells: &'a [Cell],
    ) -> io::Result<&'a [Cell]> {
        let width: usize = self.last_known_area.width.into();
        let (to_draw, remainder) = cells.split_at(width * lines_to_draw as usize);
        if lines_to_draw > 0 {
            let iter = to_draw.iter().enumerate().filter_map(|(i, c)| {
                (!c.skip).then_some(((i % width) as u16, y_offset + (i / width) as u16, c))
            });
            self.backend.draw(iter)?;
            self.backend.flush()?;
        }
        Ok(remainder)
    }

    /// Draw lines at the given vertical offset, assuming that the lines they are replacing on the
    /// screen are cleared. The slice of cells must contain enough cells for the requested lines. A
    /// slice of the unused cells are returned.
    #[cfg(feature = "scrolling-regions")]
    fn draw_lines_over_cleared<'a>(
        &mut self,
        y_offset: u16,
        lines_to_draw: u16,
        cells: &'a [Cell],
    ) -> io::Result<&'a [Cell]> {
        let width: usize = self.last_known_area.width.into();
        let (to_draw, remainder) = cells.split_at(width * lines_to_draw as usize);
        if lines_to_draw > 0 {
            let area = Rect::new(0, y_offset, width as u16, y_offset + lines_to_draw);
            let old = Buffer::empty(area);
            let new = Buffer {
                area,
                content: to_draw.to_vec(),
            };
            self.backend.draw(old.diff(&new).into_iter())?;
            self.backend.flush()?;
        }
        Ok(remainder)
    }

    /// Scroll the whole screen up by the given number of lines.
    #[cfg(not(feature = "scrolling-regions"))]
    fn scroll_up(&mut self, lines_to_scroll: u16) -> io::Result<()> {
        if lines_to_scroll > 0 {
            self.set_cursor_position(Position::new(
                0,
                self.last_known_area.height.saturating_sub(1),
            ))?;
            self.backend.append_lines(lines_to_scroll)?;
        }
        Ok(())
    }
}

fn compute_inline_size<B: Backend>(
    backend: &mut B,
    height: u16,
    size: Size,
    offset_in_previous_viewport: u16,
) -> io::Result<(Rect, Position)> {
    let pos = backend.get_cursor_position()?;
    let mut row = pos.y;

    let max_height = size.height.min(height);

    let lines_after_cursor = height
        .saturating_sub(offset_in_previous_viewport)
        .saturating_sub(1);

    backend.append_lines(lines_after_cursor)?;

    let available_lines = size.height.saturating_sub(row).saturating_sub(1);
    let missing_lines = lines_after_cursor.saturating_sub(available_lines);
    if missing_lines > 0 {
        row = row.saturating_sub(missing_lines);
    }
    row = row.saturating_sub(offset_in_previous_viewport);

    Ok((
        Rect {
            x: 0,
            y: row,
            width: size.width,
            height: max_height,
        },
        pos,
    ))
}

// Behavioral tests for `set_viewport_height` live in `rebon-cli` — see
// `crates/rebon-cli/tests/inline_set_viewport_height.rs`. We don't add
// them here because the vendored fork strips ratatui's dev-deps so its
// own test binary won't compile against this minimal Cargo.toml.
