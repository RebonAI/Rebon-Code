//! What a Claude Code mod draws, drawn in the terminal.
//!
//! A mod answers a `ui.render` ask with a tree of plain elements
//! (`rebon_types::ModUiNode`) that the registry already validated against the
//! terminal's element table: `Box`, `Text`, `Button`, `Input`, `Select`,
//! `Link`, `Code`, `Markdown`. This lays such a tree out in cells the way Ink
//! would at a fixed width — a `Box` is a row unless it says `column`, a border
//! takes a cell on each side, text wraps — and paints it into a rect,
//! recording where each Button, Link, Input and Select landed so a click on
//! it, or the keyboard once its site holds the focus (`mods_keys`), can act
//! on it for the mod.
//!
//! Two places draw one: the band above the prompt (`AbovePrompt`) and a pane
//! docked beside the transcript (`Pane`). The site holding the keyboard is
//! drawn with its control under the ring highlighted, an Input with the text
//! typed so far and a Select turned to the option the arrows reached.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use rebon_types::{ModUiChild, ModUiNode};
use rebon_width::WidthStr;

use crate::tui::app::{
    AppState, ModFocus, ModHit, ModHitKind, ModSelectOption, ModSite, TerminalModPane,
};

/// What a drawn cell is, for a press or the focus ring.
#[derive(Clone, Debug, PartialEq)]
struct HitTarget {
    element: String,
    kind: ModHitKind,
    auto_focus: bool,
}

/// The color the focus ring and a focused site's border are drawn in.
const FOCUS_COLOR: Color = Color::Cyan;

/// How the site being laid out is focused: the ring's control and what the
/// person typed or turned there. Default for a site without the keyboard.
#[derive(Clone, Copy, Default)]
struct Look<'a> {
    focus: Option<&'a ModFocus>,
}

impl<'a> Look<'a> {
    fn has_ring(&self, key: Option<&str>) -> bool {
        match (self.focus.and_then(|focus| focus.element.as_deref()), key) {
            (Some(ring), Some(key)) => ring == key,
            _ => false,
        }
    }

    fn draft(&self, key: Option<&str>) -> Option<&'a str> {
        self.focus?.drafts.get(key?).map(String::as_str)
    }

    fn choice(&self, key: Option<&str>) -> Option<usize> {
        self.focus?.choices.get(key?).copied()
    }
}

/// A Select's options as the tree carries them: `{ label, value }` rows,
/// a bare string standing for both.
fn select_options(node: &ModUiNode) -> Vec<ModSelectOption> {
    node.props
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| match option {
                    serde_json::Value::String(text) => Some(ModSelectOption {
                        label: text.clone(),
                        value: text.clone(),
                    }),
                    serde_json::Value::Object(row) => {
                        let value = match row.get("value")? {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        let label = row
                            .get("label")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.clone());
                        Some(ModSelectOption { label, value })
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The option a Select shows: the one the arrows turned it to, else the
/// one its `value` names, else the first.
pub(in crate::tui::runner) fn shown_choice(
    options: &[ModSelectOption],
    value: Option<&str>,
    turned: Option<usize>,
) -> Option<usize> {
    if options.is_empty() {
        return None;
    }
    turned
        .filter(|index| *index < options.len())
        .or_else(|| value.and_then(|value| options.iter().position(|o| o.value == value)))
        .or(Some(0))
}

/// One run of styled text on a line, and what pressing it does.
#[derive(Clone, Debug)]
struct Seg {
    text: String,
    style: Style,
    hit: Option<HitTarget>,
}

impl Seg {
    fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            hit: None,
        }
    }

    fn width(&self) -> usize {
        self.text.as_str().width()
    }
}

type Row = Vec<Seg>;

fn row_width(row: &Row) -> usize {
    row.iter().map(Seg::width).sum()
}

fn block_width(block: &[Row]) -> usize {
    block.iter().map(row_width).max().unwrap_or(0)
}

/// How deep a tree is laid out; the registry refuses deeper ones already.
const MAX_DEPTH: usize = 48;

/// A tree laid out in at most `width` cells per row.
fn layout(node: &ModUiNode, width: usize, inherited: Style, depth: usize, look: Look) -> Vec<Row> {
    if width == 0 || depth > MAX_DEPTH {
        return Vec::new();
    }
    match node.ty.as_str() {
        "Box" => layout_box(node, width, inherited, depth, look),
        "Text" => wrap_segs(inline_segs(node, text_style(node, inherited), look), width),
        "Button" => {
            let label = node
                .prop_str("label")
                .map(str::to_owned)
                .unwrap_or_else(|| node.text());
            let hotkey = node
                .prop_str("hotkey")
                .and_then(|hotkey| {
                    let mut chars = hotkey.chars();
                    chars.next().filter(|_| chars.next().is_none())
                })
                .filter(|hotkey| hotkey.is_ascii_digit() || hotkey.is_ascii_lowercase());
            let plain = node.prop_bool("plain");
            let mut style = if plain {
                inherited
            } else {
                inherited.add_modifier(Modifier::REVERSED)
            };
            if node.prop_str("variant") == Some("primary") {
                style = style.add_modifier(Modifier::BOLD);
            }
            if node.prop_bool("dimColor") {
                style = style.add_modifier(Modifier::DIM);
            }
            if node.prop_bool("isDisabled") || node.prop_bool("disabled") {
                style = inherited.add_modifier(Modifier::DIM);
            }
            if look.has_ring(node.key()) {
                style = inherited
                    .fg(FOCUS_COLOR)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            let text = match (plain, hotkey) {
                (true, Some(hotkey)) => format!("{hotkey}: {}", label.trim()),
                (true, None) => label.trim().to_owned(),
                (false, _) => format!(" {} ", label.trim()),
            };
            vec![vec![Seg {
                text: truncate(&text, width),
                style,
                hit: node.key().map(|key| HitTarget {
                    element: key.to_owned(),
                    kind: ModHitKind::Button { hotkey },
                    auto_focus: node.prop_bool("autoFocus"),
                }),
            }]]
        }
        "Link" => {
            let href = node.prop_str("href").map(str::to_owned);
            let label = {
                let text = node.text();
                if text.is_empty() {
                    href.clone().unwrap_or_default()
                } else {
                    text
                }
            };
            let element = node.key().unwrap_or("link");
            let mut style = inherited.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
            if look.has_ring(Some(element)) {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            let hit = Some(HitTarget {
                element: element.to_owned(),
                kind: ModHitKind::Link { href },
                auto_focus: node.prop_bool("autoFocus"),
            });
            wrap_segs(
                vec![Seg {
                    text: label,
                    style,
                    hit,
                }],
                width,
            )
        }
        "Code" => {
            let style = inherited.add_modifier(Modifier::DIM);
            let source = node
                .prop_str("code")
                .map(str::to_owned)
                .unwrap_or_else(|| node.text());
            source
                .lines()
                .flat_map(|line| wrap_segs(vec![Seg::plain(line, style)], width))
                .collect()
        }
        "Markdown" => {
            let source = node
                .prop_str("content")
                .or_else(|| node.prop_str("children"))
                .map(str::to_owned)
                .unwrap_or_else(|| node.text());
            source
                .lines()
                .flat_map(|line| markdown_line(line, inherited, width))
                .collect()
        }
        "Client" => layout_client(node, width, inherited, depth, look),
        "Input" => layout_input(node, width, inherited, look),
        "Select" => layout_select(node, width, inherited, look),
        // An element the terminal has no drawing for: its text, so nothing
        // the mod said is lost.
        _ => wrap_segs(inline_segs(node, inherited, look), width),
    }
}

/// A Client's region: what its surface module drew (the host ran it and
/// sent the tree as the Client's child), as wide and as tall as its props
/// give it, every cell of it the Client's to click or put the ring on.
fn layout_client(
    node: &ModUiNode,
    width: usize,
    inherited: Style,
    depth: usize,
    look: Look,
) -> Vec<Row> {
    let region = box_width(node, width).unwrap_or(width);
    let mut rows: Vec<Row> = node
        .children
        .iter()
        .flat_map(|child| layout_child(child, region, inherited, depth, look))
        .collect();
    if let Some(height) = node.props.get("height").and_then(serde_json::Value::as_u64) {
        rows.resize(height.min(MAX_CLIENT_ROWS) as usize, Vec::new());
    }
    let hit = node.key().map(|key| HitTarget {
        element: key.to_owned(),
        kind: ModHitKind::Client,
        auto_focus: false,
    });
    rows.into_iter()
        .map(|row| {
            let mut row = fit_row(row, region, inherited);
            for seg in &mut row {
                seg.hit = hit.clone();
            }
            row
        })
        .collect()
}

/// The most rows a Client's `height` takes, whatever it asks.
const MAX_CLIENT_ROWS: u64 = 200;

/// `label › text`, the text being what the person typed while the field
/// holds the ring (with a cursor and what Enter does beside it), the drawn
/// `value` otherwise, and the placeholder when that is empty. The whole row
/// is the field's to press.
fn layout_input(node: &ModUiNode, width: usize, inherited: Style, look: Look) -> Vec<Row> {
    let key = node.key();
    let focused = look.has_ring(key);
    let drawn = node.prop_str("value").unwrap_or_default();
    let value = look.draft(key).unwrap_or(drawn);
    let hit = key.map(|key| HitTarget {
        element: key.to_owned(),
        kind: ModHitKind::Input {
            value: drawn.to_owned(),
        },
        auto_focus: node.prop_bool("autoFocus"),
    });
    let mark_style = if focused {
        inherited.fg(FOCUS_COLOR)
    } else {
        inherited.add_modifier(Modifier::DIM)
    };
    let mut row = Vec::new();
    if let Some(label) = node.prop_str("label").filter(|label| !label.is_empty()) {
        row.push(Seg::plain(format!("{label} "), inherited));
    }
    row.push(Seg::plain("› ", mark_style));
    let hint = focused.then(|| {
        format!(
            " ↵ {}",
            node.prop_str("submitLabel")
                .filter(|label| !label.is_empty())
                .unwrap_or("submit")
        )
    });
    let room = width
        .saturating_sub(row_width(&row))
        .saturating_sub(hint.as_deref().map_or(0, |hint| hint.width()));
    if value.is_empty() && !focused {
        row.push(Seg::plain(
            truncate(node.prop_str("placeholder").unwrap_or_default(), room),
            inherited.add_modifier(Modifier::DIM),
        ));
    } else if focused {
        // The end of what was typed stays in view: a long draft shows its
        // tail, with the cursor after it.
        let shown = tail(value, room.saturating_sub(1));
        row.push(Seg::plain(
            shown,
            inherited.add_modifier(Modifier::UNDERLINED),
        ));
        row.push(Seg::plain("▏", inherited.fg(FOCUS_COLOR)));
    } else {
        row.push(Seg::plain(truncate(value, room), inherited));
    }
    if let Some(hint) = hint {
        row.push(Seg::plain(hint, inherited.add_modifier(Modifier::DIM)));
    }
    for seg in &mut row {
        seg.hit = hit.clone();
    }
    vec![row]
}

/// `label ▾ option`; while it holds the ring, `◂ option ▸` with the option
/// the arrows turned it to, and a `↵` while that is not yet the one picked.
fn layout_select(node: &ModUiNode, width: usize, inherited: Style, look: Look) -> Vec<Row> {
    let key = node.key();
    let focused = look.has_ring(key);
    let options = select_options(node);
    let value = node.props.get("value").map(|value| match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    let committed = shown_choice(&options, value.as_deref(), None);
    let shown = shown_choice(&options, value.as_deref(), look.choice(key));
    let label = shown
        .and_then(|index| options.get(index))
        .map(|option| option.label.clone())
        .or_else(|| value.clone())
        .unwrap_or_default();
    let mut row = Vec::new();
    if let Some(prefix) = node.prop_str("label").filter(|label| !label.is_empty()) {
        row.push(Seg::plain(format!("{prefix} "), inherited));
    }
    if focused {
        row.push(Seg::plain(
            format!("◂ {label} ▸"),
            inherited.fg(FOCUS_COLOR).add_modifier(Modifier::BOLD),
        ));
        if shown != committed {
            row.push(Seg::plain(" ↵", inherited.add_modifier(Modifier::DIM)));
        }
    } else {
        row.push(Seg::plain(format!("▾ {label}"), inherited));
    }
    let hit = key.map(|key| HitTarget {
        element: key.to_owned(),
        kind: ModHitKind::Select {
            options: options.clone(),
            value: value.clone(),
        },
        auto_focus: node.prop_bool("autoFocus"),
    });
    let text: String = row.iter().map(|seg| seg.text.as_str()).collect();
    if text.as_str().width() > width {
        // Too wide: one run, cut, keeping the focused look of its last part.
        let style = row.last().map(|seg| seg.style).unwrap_or(inherited);
        row = vec![Seg::plain(truncate(&text, width), style)];
    }
    for seg in &mut row {
        seg.hit = hit.clone();
    }
    vec![row]
}

fn number(node: &ModUiNode, name: &str) -> usize {
    node.props
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(16) as usize)
        .unwrap_or(0)
}

/// A Box's `width` in cells at `available`: a number of cells, or a
/// percentage of what it was given. `None` when it says neither.
fn box_width(node: &ModUiNode, available: usize) -> Option<usize> {
    match node.props.get("width")? {
        serde_json::Value::Number(cells) => cells.as_u64().map(|cells| cells as usize),
        serde_json::Value::String(text) => {
            let percent: usize = text.trim().strip_suffix('%')?.trim().parse().ok()?;
            Some(available * percent.min(100) / 100)
        }
        _ => None,
    }
    .map(|cells| cells.min(available))
}

/// How a row spreads what its children leave of its width: the cells before
/// the first child, and the cells between each two, as `justifyContent`
/// says. `widths` are the children's, `gap` the least between two.
fn justify(how: Option<&str>, widths: &[usize], gap: usize, width: usize) -> (usize, Vec<usize>) {
    let between = widths.len().saturating_sub(1);
    let used = widths.iter().sum::<usize>() + gap * between;
    let free = width.saturating_sub(used);
    let mut gaps = vec![gap; between];
    let lead = match how {
        Some("center") => free / 2,
        Some("flex-end") | Some("end") => free,
        Some("space-between") if between > 0 => {
            for (index, slot) in gaps.iter_mut().enumerate() {
                *slot += free / between + usize::from(index < free % between);
            }
            0
        }
        Some("space-around") | Some("space-evenly") if !widths.is_empty() => {
            let share = free / (widths.len() + 1);
            for slot in &mut gaps {
                *slot += share;
            }
            share
        }
        _ => 0,
    };
    (lead, gaps)
}

/// A row cut or padded to exactly `width` cells.
fn fit_row(row: Row, width: usize, style: Style) -> Row {
    let mut out = Vec::new();
    let mut used = 0usize;
    for seg in row {
        let w = seg.width();
        if used + w <= width {
            used += w;
            out.push(seg);
            continue;
        }
        let text = truncate(&seg.text, width - used);
        used += text.as_str().width();
        if !text.is_empty() {
            out.push(Seg { text, ..seg });
        }
        break;
    }
    if used < width {
        out.push(Seg::plain(" ".repeat(width - used), style));
    }
    out
}

fn layout_box(
    node: &ModUiNode,
    width: usize,
    inherited: Style,
    depth: usize,
    look: Look,
) -> Vec<Row> {
    let fixed = box_width(node, width);
    let margin_left = number(node, "marginLeft").max(number(node, "marginX"));
    let margin_right = number(node, "marginRight").max(number(node, "marginX"));
    let width = fixed
        .unwrap_or(width)
        .saturating_sub(margin_left + margin_right);
    let mut out = layout_box_body(node, width, inherited, depth, look);
    if fixed.is_some() || margin_left > 0 {
        out = out
            .into_iter()
            .map(|row| {
                let row = if fixed.is_some() {
                    fit_row(row, width, inherited)
                } else {
                    row
                };
                if margin_left == 0 {
                    return row;
                }
                let mut line = vec![Seg::plain(" ".repeat(margin_left), inherited)];
                line.extend(row);
                line
            })
            .collect();
    }
    out
}

/// A Box inside its margins, at `width`.
fn layout_box_body(
    node: &ModUiNode,
    width: usize,
    inherited: Style,
    depth: usize,
    look: Look,
) -> Vec<Row> {
    let bordered = node
        .prop_str("borderStyle")
        .is_some_and(|style| !style.is_empty() && style != "none");
    let pad_x = number(node, "paddingX").max(number(node, "padding"));
    let pad_y = number(node, "paddingY").max(number(node, "padding"));
    let frame = if bordered { 2 } else { 0 };
    let inner = width.saturating_sub(frame + pad_x * 2);
    if inner == 0 {
        return Vec::new();
    }
    let column = node
        .prop_str("flexDirection")
        .is_some_and(|d| d.starts_with("column"));
    let gap = number(node, if column { "rowGap" } else { "columnGap" }).max(number(node, "gap"));
    let mut body: Vec<Row> = Vec::new();
    if column {
        for (index, child) in node.children.iter().enumerate() {
            if index > 0 {
                for _ in 0..gap {
                    body.push(Vec::new());
                }
            }
            body.extend(layout_child(child, inner, inherited, depth, look));
        }
    } else {
        // A row: each child at its natural width, side by side, in what is
        // left of the row; a child that no longer fits goes on below.
        let mut lines: Vec<Vec<Vec<Row>>> = Vec::new();
        let mut current: Vec<Vec<Row>> = Vec::new();
        let mut used = 0usize;
        for child in &node.children {
            let spacing = if current.is_empty() { 0 } else { gap };
            let remaining = inner.saturating_sub(used + spacing);
            // Measured at the row's whole width: a child that would have to
            // be squeezed to fit beside the others starts the next line.
            let block = layout_child(child, inner, inherited, depth, look);
            let block_w = block_width(&block);
            if !current.is_empty() && block_w > remaining {
                lines.push(std::mem::take(&mut current));
                used = block_w;
                current.push(block);
                continue;
            }
            used += block_w + spacing;
            current.push(block);
        }
        if !current.is_empty() {
            lines.push(current);
        }
        let how = node.prop_str("justifyContent");
        for blocks in lines {
            let widths: Vec<usize> = blocks.iter().map(|block| block_width(block)).collect();
            let (lead, gaps) = justify(how, &widths, gap, inner);
            body.extend(side_by_side(&blocks, lead, &gaps));
        }
    }
    let mut out: Vec<Row> = Vec::new();
    let content_w = if bordered {
        inner
    } else {
        block_width(&body).min(inner)
    };
    let border_style = node
        .prop_str("borderColor")
        .and_then(parse_color)
        .map(|color| inherited.fg(color))
        .unwrap_or(inherited.add_modifier(Modifier::DIM));
    let outer_w = content_w + pad_x * 2;
    if bordered {
        out.push(vec![Seg::plain(
            format!("╭{}╮", "─".repeat(outer_w)),
            border_style,
        )]);
    }
    let pad_row = || -> Row {
        let mut row = Vec::new();
        if bordered {
            row.push(Seg::plain("│", border_style));
        }
        row.push(Seg::plain(" ".repeat(outer_w), inherited));
        if bordered {
            row.push(Seg::plain("│", border_style));
        }
        row
    };
    for _ in 0..pad_y {
        out.push(pad_row());
    }
    for row in body {
        let mut line = Vec::new();
        if bordered {
            line.push(Seg::plain("│", border_style));
        }
        if pad_x > 0 {
            line.push(Seg::plain(" ".repeat(pad_x), inherited));
        }
        let filled = row_width(&row);
        line.extend(row);
        if bordered || pad_x > 0 {
            line.push(Seg::plain(
                " ".repeat(content_w.saturating_sub(filled) + pad_x),
                inherited,
            ));
        }
        if bordered {
            line.push(Seg::plain("│", border_style));
        }
        out.push(line);
    }
    for _ in 0..pad_y {
        out.push(pad_row());
    }
    if bordered {
        out.push(vec![Seg::plain(
            format!("╰{}╯", "─".repeat(outer_w)),
            border_style,
        )]);
    }
    out
}

fn layout_child(
    child: &ModUiChild,
    width: usize,
    inherited: Style,
    depth: usize,
    look: Look,
) -> Vec<Row> {
    match child {
        ModUiChild::Text(text) => wrap_segs(vec![Seg::plain(text.clone(), inherited)], width),
        ModUiChild::Node(node) => layout(node, width, inherited, depth + 1, look),
    }
}

/// Blocks placed left to right, each padded to its own width: `lead` cells
/// before the first, `gaps[i]` between block `i` and the next.
fn side_by_side(blocks: &[Vec<Row>], lead: usize, gaps: &[usize]) -> Vec<Row> {
    let height = blocks.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = blocks.iter().map(|block| block_width(block)).collect();
    (0..height)
        .map(|y| {
            let mut row = Vec::new();
            if lead > 0 {
                row.push(Seg::plain(" ".repeat(lead), Style::default()));
            }
            for (index, block) in blocks.iter().enumerate() {
                let gap = if index > 0 { gaps[index - 1] } else { 0 };
                if gap > 0 {
                    row.push(Seg::plain(" ".repeat(gap), Style::default()));
                }
                let piece = block.get(y).cloned().unwrap_or_default();
                let filled = row_width(&piece);
                row.extend(piece);
                if index + 1 < blocks.len() && filled < widths[index] {
                    row.push(Seg::plain(
                        " ".repeat(widths[index] - filled),
                        Style::default(),
                    ));
                }
            }
            row
        })
        .collect()
}

/// The runs of a Text, its nested Texts and Links included, in order.
fn inline_segs(node: &ModUiNode, style: Style, look: Look) -> Vec<Seg> {
    let mut out = Vec::new();
    for child in &node.children {
        match child {
            ModUiChild::Text(text) => out.push(Seg::plain(text.clone(), style)),
            ModUiChild::Node(inner) if inner.ty == "Text" => {
                out.extend(inline_segs(inner, text_style(inner, style), look));
            }
            ModUiChild::Node(inner) => {
                for row in layout(inner, usize::MAX / 4, style, 1, look) {
                    out.extend(row);
                }
            }
        }
    }
    out
}

fn text_style(node: &ModUiNode, inherited: Style) -> Style {
    let mut style = inherited;
    if node.prop_bool("bold") {
        style = style.add_modifier(Modifier::BOLD);
    }
    if node.prop_bool("dimColor") || node.prop_bool("dim") {
        style = style.add_modifier(Modifier::DIM);
    }
    if node.prop_bool("italic") {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if node.prop_bool("underline") {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if node.prop_bool("strikethrough") {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if node.prop_bool("inverse") {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if let Some(color) = node.prop_str("color").and_then(parse_color) {
        style = style.fg(color);
    }
    if let Some(color) = node.prop_str("backgroundColor").and_then(parse_color) {
        style = style.bg(color);
    }
    style
}

fn parse_color(name: &str) -> Option<Color> {
    let name = name.trim();
    if let Some(hex) = name.strip_prefix('#') {
        if hex.len() == 6 {
            let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
            return Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
        }
        return None;
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "redbright" => Color::LightRed,
        "greenbright" => Color::LightGreen,
        "yellowbright" => Color::LightYellow,
        "bluebright" => Color::LightBlue,
        "magentabright" => Color::LightMagenta,
        "cyanbright" => Color::LightCyan,
        _ => return None,
    })
}

/// One Markdown source line, with headings and list marks read plainly.
fn markdown_line(line: &str, style: Style, width: usize) -> Vec<Row> {
    let trimmed = line.trim_start();
    let (text, style) = if let Some(rest) = trimmed.strip_prefix('#') {
        (
            rest.trim_start_matches('#').trim().to_owned(),
            style.add_modifier(Modifier::BOLD),
        )
    } else if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        (format!("• {rest}"), style)
    } else {
        (line.replace("**", "").replace('`', ""), style)
    };
    if text.is_empty() {
        return vec![Vec::new()];
    }
    wrap_segs(vec![Seg::plain(text, style)], width)
}

/// Runs wrapped at `width` cells, splitting a run where it crosses the edge.
fn wrap_segs(segs: Vec<Seg>, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows: Vec<Row> = vec![Vec::new()];
    let mut used = 0usize;
    for seg in segs {
        let mut piece = String::new();
        for ch in seg.text.chars() {
            if ch == '\n' {
                rows.last_mut().expect("a row").push(Seg {
                    text: std::mem::take(&mut piece),
                    ..seg.clone()
                });
                rows.push(Vec::new());
                used = 0;
                continue;
            }
            let w = ch.to_string().as_str().width();
            if used + w > width && used > 0 {
                rows.last_mut().expect("a row").push(Seg {
                    text: std::mem::take(&mut piece),
                    ..seg.clone()
                });
                rows.push(Vec::new());
                used = 0;
            }
            piece.push(ch);
            used += w;
        }
        if !piece.is_empty() {
            rows.last_mut().expect("a row").push(Seg {
                text: piece,
                ..seg.clone()
            });
        }
    }
    for row in &mut rows {
        row.retain(|seg| !seg.text.is_empty());
    }
    rows
}

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = ch.to_string().as_str().width();
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// The last cells of `text` that fit in `width`.
fn tail(text: &str, width: usize) -> String {
    let mut out: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in text.chars().rev() {
        let w = ch.to_string().as_str().width();
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.into_iter().rev().collect()
}

/// Rows a tree takes at `width`, for sizing a region before painting it.
pub(in crate::tui::runner) fn mod_tree_height(tree: &ModUiNode, width: u16) -> u16 {
    layout(
        tree,
        usize::from(width),
        Style::default(),
        0,
        Look::default(),
    )
    .len()
    .min(u16::MAX as usize) as u16
}

/// The focus, when it is on `site`.
fn focus_on<'a>(app: &'a AppState, site: &ModSite) -> Option<&'a ModFocus> {
    app.mods_focus.as_ref().filter(|focus| focus.site == *site)
}

/// Paints a tree into `area`, drawn as `focus` has it when the site holds
/// the keyboard, and records where its controls landed as hits on `site`.
pub(in crate::tui::runner) fn render_mod_tree(
    frame: &mut Frame,
    area: Rect,
    tree: &ModUiNode,
    site: &ModSite,
    focus: Option<&ModFocus>,
    hits: &mut Vec<ModHit>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rows = layout(
        tree,
        usize::from(area.width),
        Style::default(),
        0,
        Look { focus },
    );
    let mut lines = Vec::with_capacity(rows.len().min(usize::from(area.height)));
    for (y, row) in rows.into_iter().take(usize::from(area.height)).enumerate() {
        let mut x = 0u16;
        let mut spans = Vec::with_capacity(row.len());
        for seg in row {
            let w = seg.width().min(usize::from(u16::MAX)) as u16;
            if let Some(hit) = &seg.hit {
                let visible = w.min(area.width.saturating_sub(x));
                if visible > 0 {
                    hits.push(ModHit {
                        area: Rect::new(area.x + x, area.y + y as u16, visible, 1),
                        site: site.clone(),
                        element: hit.element.clone(),
                        kind: hit.kind.clone(),
                        auto_focus: hit.auto_focus,
                    });
                }
            }
            x = x.saturating_add(w);
            spans.push(Span::styled(seg.text, seg.style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The band above the prompt: its rows, taken from the bottom of `area`.
/// Returns what is left of `area` above it.
pub(in crate::tui::runner) fn render_mods_band(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
) -> Rect {
    let Some((plugin, tree)) = app.mods_band.clone() else {
        return area;
    };
    // At most a quarter of the screen, and never the whole transcript.
    let cap = (frame.area().height / 4)
        .max(1)
        .min(area.height.saturating_sub(3));
    let height = mod_tree_height(&tree, area.width).min(cap);
    if height == 0 {
        return area;
    }
    let band = Rect::new(area.x, area.bottom() - height, area.width, height);
    paint_band(frame, band, app, plugin, &tree);
    Rect::new(area.x, area.y, area.width, area.height - height)
}

/// The band's drawing in exactly `band`.
fn paint_band(frame: &mut Frame, band: Rect, app: &mut AppState, plugin: String, tree: &ModUiNode) {
    super::frame::clear_chunk_background(frame, band);
    let site = ModSite {
        plugin,
        component: "AbovePrompt".to_owned(),
        request_id: "AbovePrompt".to_owned(),
    };
    let mut hits = std::mem::take(&mut app.mods_hits);
    render_mod_tree(frame, band, tree, &site, focus_on(app, &site), &mut hits);
    app.mods_hits = hits;
    app.mods_sites.push(site);
}

/// The band's rows at `width`: at most a quarter of the terminal.
fn band_height(app: &AppState, width: u16, terminal_height: u16) -> u16 {
    app.mods_band.as_ref().map_or(0, |(_, tree)| {
        mod_tree_height(tree, width).min((terminal_height / 4).max(1))
    })
}

/// The rows one pane takes in the inline live region, its frame included:
/// its drawing's height, at most a third of the terminal.
fn inline_pane_height(pane: &TerminalModPane, width: u16, terminal_height: u16) -> u16 {
    let body = match (&pane.tree, &pane.error) {
        (Some(tree), _) => mod_tree_height(tree, width.saturating_sub(2)),
        (None, Some(_)) => 1,
        (None, None) => 1,
    };
    body.saturating_add(2).min((terminal_height / 3).max(3))
}

/// The panes the inline live region shows at this width.
fn inline_seated(app: &AppState, columns: u16) -> Vec<TerminalModPane> {
    app.mods_panes
        .iter()
        .filter(|pane| pane_seats(pane, columns))
        .cloned()
        .collect()
}

/// Rows the mods take in the inline live region, above the prompt: each
/// seated pane as a framed block, then the band. The inline terminal has
/// no column to dock a pane in, so a pane stacks over the band instead.
pub(in crate::tui::runner) fn inline_mods_height(
    app: &AppState,
    width: u16,
    terminal_height: u16,
) -> u16 {
    if width == 0 {
        return 0;
    }
    inline_seated(app, width)
        .iter()
        .map(|pane| inline_pane_height(pane, width, terminal_height))
        .fold(
            band_height(app, width, terminal_height),
            u16::saturating_add,
        )
}

/// Paints the inline live region's mods into `area`, top-down: the panes,
/// then the band against the prompt.
pub(in crate::tui::runner) fn render_inline_mods(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    terminal_height: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    super::frame::clear_chunk_background(frame, area);
    let band = band_height(app, area.width, terminal_height).min(area.height);
    let mut y = area.y;
    let panes_bottom = area.bottom() - band;
    let mut hits = std::mem::take(&mut app.mods_hits);
    for pane in inline_seated(app, area.width) {
        if y >= panes_bottom {
            break;
        }
        let height = inline_pane_height(&pane, area.width, terminal_height).min(panes_bottom - y);
        let rect = Rect::new(area.x, y, area.width, height);
        let site = pane.site();
        draw_pane(frame, rect, &pane, &site, focus_on(app, &site), &mut hits);
        app.mods_sites.push(site);
        y = y.saturating_add(height);
    }
    app.mods_hits = hits;
    if let Some((plugin, tree)) = app.mods_band.clone().filter(|_| band > 0) {
        let rect = Rect::new(area.x, area.bottom() - band, area.width, band);
        paint_band(frame, rect, app, plugin, &tree);
    }
}

/// One pane in `rect`: its frame, titled, and its drawing (or what went
/// wrong drawing it) inside.
fn draw_pane(
    frame: &mut Frame,
    rect: Rect,
    pane: &TerminalModPane,
    site: &ModSite,
    focus: Option<&ModFocus>,
    hits: &mut Vec<ModHit>,
) {
    let border = if focus.is_some() {
        Style::default().fg(FOCUS_COLOR)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let title = truncate(
        &if focus.is_some() {
            format!(" {} · {} · Esc ", pane.title, pane.plugin_name)
        } else {
            format!(" {} · {} ", pane.title, pane.plugin_name)
        },
        usize::from(rect.width.saturating_sub(2)),
    );
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    match (&pane.tree, &pane.error) {
        (Some(tree), _) => render_mod_tree(frame, inner, tree, site, focus, hits),
        (None, Some(error)) => frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(
                    error,
                    usize::from(inner.width) * usize::from(inner.height.max(1)),
                ),
                Style::default().add_modifier(Modifier::DIM),
            )))
            .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        ),
        (None, None) => {}
    }
}

/// The columns a docked pane takes, inside its border.
pub(in crate::tui::runner) const PANE_BODY_COLUMNS: u16 = 46;

/// Whether a pane seats at this terminal width: one the person asked for
/// from 100 columns, one a mod opened unasked from 144, the width at which
/// Claude Code docks an unasked pane too.
pub(in crate::tui::runner) fn pane_seats(pane: &TerminalModPane, columns: u16) -> bool {
    if pane.unasked {
        columns >= 144
    } else {
        columns >= 100
    }
}

/// The docked panes, at the right of `area`, stacked. Returns what is left
/// of `area` for the transcript.
pub(in crate::tui::runner) fn render_mods_panes(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
) -> Rect {
    let columns = frame.area().width;
    let seated: Vec<TerminalModPane> = app
        .mods_panes
        .iter()
        .filter(|pane| pane_seats(pane, columns))
        .cloned()
        .collect();
    if seated.is_empty() || area.height < 4 {
        return area;
    }
    let outer = PANE_BODY_COLUMNS + 2;
    if area.width <= outer + 40 {
        return area;
    }
    let column = Rect::new(area.right() - outer, area.y, outer, area.height);
    super::frame::clear_chunk_background(frame, column);
    let share = (area.height / seated.len() as u16).max(4);
    let mut y = area.y;
    let mut hits = std::mem::take(&mut app.mods_hits);
    for pane in seated {
        if y >= area.bottom() {
            break;
        }
        let height = share.min(area.bottom() - y);
        let rect = Rect::new(column.x, y, outer, height);
        let site = pane.site();
        draw_pane(frame, rect, &pane, &site, focus_on(app, &site), &mut hits);
        app.mods_sites.push(site);
        y = y.saturating_add(height);
    }
    app.mods_hits = hits;
    Rect::new(area.x, area.y, area.width - outer, area.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(value: serde_json::Value) -> ModUiNode {
        serde_json::from_value(value).unwrap()
    }

    fn text_of(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| row.iter().map(|seg| seg.text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn a_row_puts_children_side_by_side_and_a_column_stacks_them() {
        let row = tree(json!({ "type": "Box", "props": { "gap": 1 }, "children": [
            { "type": "Text", "props": {}, "children": ["Last turn: 3s"] },
            { "type": "Button", "props": { "key": "hide", "label": "Hide" }, "children": [] },
        ] }));
        assert_eq!(
            text_of(&layout(&row, 40, Style::default(), 0, Look::default())),
            vec!["Last turn: 3s  Hide "]
        );
        let column = tree(
            json!({ "type": "Box", "props": { "flexDirection": "column" }, "children": [
            { "type": "Text", "props": { "bold": true }, "children": ["1 click"] },
            { "type": "Button", "props": { "key": "more" }, "children": ["more"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&column, 40, Style::default(), 0, Look::default())),
            vec!["1 click", " more "]
        );
    }

    #[test]
    fn a_client_fills_its_region_and_every_cell_of_it_is_the_clients() {
        let client = tree(
            json!({ "type": "Client", "props": { "key": "snake", "module": "./g.tsx", "width": 6, "height": 3 }, "children": [
            { "type": "Text", "props": {}, "children": ["score 1234567890"] },
        ] }),
        );
        let rows = layout(&client, 20, Style::default(), 0, Look::default());
        assert_eq!(
            text_of(&rows),
            vec!["score ", "123456", "7890  "],
            "wrapped at its 6 columns, cut to its 3 rows, padded to a solid region"
        );
        for row in &rows {
            for seg in row {
                let hit = seg.hit.as_ref().expect("every cell is the client's");
                assert_eq!(hit.element, "snake");
                assert_eq!(hit.kind, ModHitKind::Client);
            }
        }
        let short = tree(
            json!({ "type": "Client", "props": { "key": "c", "module": "./g.tsx", "height": 2 }, "children": [] }),
        );
        assert_eq!(
            text_of(&layout(&short, 4, Style::default(), 0, Look::default())),
            vec!["    ", "    "],
            "no width: the room it was given; nothing drawn yet: its rows still taken"
        );
    }

    #[test]
    fn justify_content_spreads_a_row_across_its_width() {
        let row = |how: &str| {
            tree(
                json!({ "type": "Box", "props": { "justifyContent": how }, "children": [
                { "type": "Text", "props": {}, "children": ["ab"] },
                { "type": "Text", "props": {}, "children": ["cd"] },
            ] }),
            )
        };
        let at = |how: &str| text_of(&layout(&row(how), 10, Style::default(), 0, Look::default()));
        assert_eq!(at("space-between"), vec!["ab      cd"]);
        assert_eq!(at("flex-end"), vec!["      abcd"]);
        assert_eq!(at("center"), vec!["   abcd"]);
        assert_eq!(at("flex-start"), vec!["abcd"]);
    }

    #[test]
    fn justify_shares_the_spare_cells_with_the_first_gaps_taking_the_remainder() {
        assert_eq!(
            justify(Some("space-between"), &[1, 1, 1], 0, 6),
            (0, vec![2, 1])
        );
        assert_eq!(justify(Some("space-between"), &[4], 1, 10), (0, vec![]));
        assert_eq!(justify(Some("space-evenly"), &[2, 2], 0, 10), (2, vec![2]));
        assert_eq!(justify(None, &[2, 2], 1, 10), (0, vec![1]));
        assert_eq!(
            justify(Some("center"), &[8], 0, 4),
            (0, vec![]),
            "an overfull row is not shifted"
        );
    }

    #[test]
    fn column_gap_spaces_a_row_and_row_gap_a_column() {
        let row = tree(
            json!({ "type": "Box", "props": { "columnGap": 2 }, "children": [
            { "type": "Text", "props": {}, "children": ["a"] },
            { "type": "Text", "props": {}, "children": ["b"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&row, 10, Style::default(), 0, Look::default())),
            vec!["a  b"]
        );
        let column = tree(
            json!({ "type": "Box", "props": { "flexDirection": "column", "rowGap": 1 }, "children": [
            { "type": "Text", "props": {}, "children": ["a"] },
            { "type": "Text", "props": {}, "children": ["b"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&column, 10, Style::default(), 0, Look::default())),
            vec!["a", "", "b"]
        );
    }

    #[test]
    fn a_fixed_or_percent_width_cuts_and_pads_and_a_margin_indents() {
        let fixed = tree(
            json!({ "type": "Box", "props": { "width": 4 }, "children": [
            { "type": "Text", "props": {}, "children": ["abcdef"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&fixed, 10, Style::default(), 0, Look::default())),
            vec!["abcd", "ef  "]
        );
        let half = tree(
            json!({ "type": "Box", "props": { "width": "50%" }, "children": [
            { "type": "Text", "props": {}, "children": ["x"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&half, 10, Style::default(), 0, Look::default())),
            vec!["x    "]
        );
        let indented = tree(
            json!({ "type": "Box", "props": { "marginLeft": 2 }, "children": [
            { "type": "Text", "props": {}, "children": ["hi"] },
        ] }),
        );
        assert_eq!(
            text_of(&layout(&indented, 10, Style::default(), 0, Look::default())),
            vec!["  hi"]
        );
    }

    #[test]
    fn a_border_frames_the_box_at_the_width_it_was_given() {
        let boxed = tree(
            json!({ "type": "Box", "props": { "borderStyle": "round" }, "children": [
            { "type": "Text", "props": {}, "children": ["hi"] },
        ] }),
        );
        let rows = text_of(&layout(&boxed, 8, Style::default(), 0, Look::default()));
        assert_eq!(rows, vec!["╭──────╮", "│hi    │", "╰──────╯"]);
    }

    #[test]
    fn text_wraps_at_the_width_cjk_counted_as_two() {
        let text = tree(json!({ "type": "Text", "props": {}, "children": ["你好世界abc"] }));
        assert_eq!(
            text_of(&layout(&text, 5, Style::default(), 0, Look::default())),
            vec!["你好", "世界a", "bc"]
        );
    }

    #[test]
    fn a_row_that_overflows_moves_the_next_child_below() {
        let row = tree(json!({ "type": "Box", "props": {}, "children": [
            { "type": "Text", "props": {}, "children": ["aaaaaa"] },
            { "type": "Text", "props": {}, "children": ["bbbbbb"] },
        ] }));
        assert_eq!(
            text_of(&layout(&row, 8, Style::default(), 0, Look::default())),
            vec!["aaaaaa", "bbbbbb"]
        );
    }

    #[test]
    fn buttons_and_links_carry_their_press() {
        let row = tree(json!({ "type": "Box", "props": {}, "children": [
            { "type": "Button", "props": { "key": "go" }, "children": ["Go"] },
            { "type": "Link", "props": { "href": "https://example.com" }, "children": ["docs"] },
        ] }));
        let rows = layout(&row, 40, Style::default(), 0, Look::default());
        let hits: Vec<HitTarget> = rows[0].iter().filter_map(|seg| seg.hit.clone()).collect();
        assert_eq!(hits[0].element, "go");
        assert_eq!(hits[0].kind, ModHitKind::Button { hotkey: None });
        assert_eq!(
            hits[1].kind,
            ModHitKind::Link {
                href: Some("https://example.com".into())
            }
        );
    }

    #[test]
    fn an_unasked_pane_waits_for_a_wide_terminal() {
        let pane = |unasked| TerminalModPane {
            id: "p".into(),
            plugin: "m".into(),
            plugin_name: "m".into(),
            title: "P".into(),
            unasked,
            close_on_escape: false,
            version: 1,
            drawn_version: 0,
            tree: None,
            error: None,
        };
        assert!(pane_seats(&pane(false), 100));
        assert!(!pane_seats(&pane(true), 120));
        assert!(pane_seats(&pane(true), 144));
    }

    fn focus_on_element(element: &str) -> ModFocus {
        ModFocus {
            element: Some(element.into()),
            ..ModFocus::default()
        }
    }

    fn hit_of(rows: &[Row]) -> HitTarget {
        rows[0]
            .iter()
            .find_map(|seg| seg.hit.clone())
            .expect("a hit")
    }

    #[test]
    fn a_button_carries_its_hotkey_and_auto_focus_and_a_plain_one_shows_the_key() {
        let button = tree(json!({ "type": "Button", "props": {
            "key": "yes", "label": "Yes", "hotkey": "y", "plain": true, "autoFocus": true,
        }, "children": [] }));
        let rows = layout(&button, 20, Style::default(), 0, Look::default());
        assert_eq!(text_of(&rows), vec!["y: Yes"]);
        let hit = hit_of(&rows);
        assert_eq!(hit.kind, ModHitKind::Button { hotkey: Some('y') });
        assert!(hit.auto_focus);
        let bad = tree(json!({ "type": "Button", "props": {
            "key": "k", "label": "K", "hotkey": "Ctrl+K",
        }, "children": [] }));
        assert_eq!(
            hit_of(&layout(&bad, 20, Style::default(), 0, Look::default())).kind,
            ModHitKind::Button { hotkey: None },
            "only one digit or lowercase letter is a hotkey"
        );
    }

    #[test]
    fn the_ring_draws_its_button_in_the_focus_color() {
        let button =
            tree(json!({ "type": "Button", "props": { "key": "go" }, "children": ["Go"] }));
        let plain = layout(&button, 20, Style::default(), 0, Look::default());
        assert_ne!(plain[0][0].style.fg, Some(FOCUS_COLOR));
        let focus = focus_on_element("go");
        let ringed = layout(
            &button,
            20,
            Style::default(),
            0,
            Look {
                focus: Some(&focus),
            },
        );
        assert_eq!(ringed[0][0].style.fg, Some(FOCUS_COLOR));
        let elsewhere = focus_on_element("other");
        let not_ringed = layout(
            &button,
            20,
            Style::default(),
            0,
            Look {
                focus: Some(&elsewhere),
            },
        );
        assert_ne!(not_ringed[0][0].style.fg, Some(FOCUS_COLOR));
    }

    #[test]
    fn an_input_shows_placeholder_value_or_the_draft_with_a_cursor_and_its_submit_label() {
        let field = |value: &str| {
            tree(json!({ "type": "Input", "props": {
                "key": "q", "label": "Ask", "placeholder": "type here", "value": value,
                "submitLabel": "send",
            }, "children": [] }))
        };
        assert_eq!(
            text_of(&layout(
                &field(""),
                40,
                Style::default(),
                0,
                Look::default()
            )),
            vec!["Ask › type here"]
        );
        assert_eq!(
            text_of(&layout(
                &field("hi"),
                40,
                Style::default(),
                0,
                Look::default()
            )),
            vec!["Ask › hi"]
        );
        let mut focus = focus_on_element("q");
        let rows = layout(
            &field("hi"),
            40,
            Style::default(),
            0,
            Look {
                focus: Some(&focus),
            },
        );
        assert_eq!(text_of(&rows), vec!["Ask › hi▏ ↵ send"]);
        focus.drafts.insert("q".into(), "hello".into());
        let rows = layout(
            &field("hi"),
            40,
            Style::default(),
            0,
            Look {
                focus: Some(&focus),
            },
        );
        assert_eq!(text_of(&rows), vec!["Ask › hello▏ ↵ send"]);
        assert_eq!(
            hit_of(&rows).kind,
            ModHitKind::Input { value: "hi".into() },
            "the hit keeps the drawn value; the draft lives in the focus"
        );
        assert!(
            rows[0].iter().all(|seg| seg.hit.is_some()),
            "the whole row is the field's to click"
        );
    }

    #[test]
    fn a_long_draft_shows_its_tail() {
        let field = tree(json!({ "type": "Input", "props": { "key": "q" }, "children": [] }));
        let mut focus = focus_on_element("q");
        focus.drafts.insert("q".into(), "abcdefghij".into());
        let rows = layout(
            &field,
            16,
            Style::default(),
            0,
            Look {
                focus: Some(&focus),
            },
        );
        let text = text_of(&rows).remove(0);
        assert_eq!(text, "› ghij▏ ↵ submit");
        assert!(text.as_str().width() <= 16);
    }

    #[test]
    fn a_select_shows_its_value_and_turns_while_it_holds_the_ring() {
        let select = tree(json!({ "type": "Select", "props": {
            "key": "lvl", "label": "Level", "value": "mid",
            "options": [ { "label": "Low", "value": "low" }, { "label": "Mid", "value": "mid" }, "high" ],
        }, "children": [] }));
        assert_eq!(
            text_of(&layout(&select, 40, Style::default(), 0, Look::default())),
            vec!["Level ▾ Mid"]
        );
        let mut focus = focus_on_element("lvl");
        assert_eq!(
            text_of(&layout(
                &select,
                40,
                Style::default(),
                0,
                Look {
                    focus: Some(&focus)
                }
            )),
            vec!["Level ◂ Mid ▸"]
        );
        focus.choices.insert("lvl".into(), 2);
        let rows = layout(
            &select,
            40,
            Style::default(),
            0,
            Look {
                focus: Some(&focus),
            },
        );
        assert_eq!(text_of(&rows), vec!["Level ◂ high ▸ ↵"]);
        let ModHitKind::Select { options, value } = hit_of(&rows).kind else {
            panic!("a select hit");
        };
        assert_eq!(value.as_deref(), Some("mid"));
        assert_eq!(
            options
                .iter()
                .map(|o| (o.label.as_str(), o.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("Low", "low"), ("Mid", "mid"), ("high", "high")]
        );
    }

    #[test]
    fn shown_choice_prefers_the_turn_then_the_value_then_the_first() {
        let options: Vec<ModSelectOption> = ["a", "b", "c"]
            .iter()
            .map(|v| ModSelectOption {
                label: v.to_string(),
                value: v.to_string(),
            })
            .collect();
        assert_eq!(shown_choice(&options, Some("b"), Some(2)), Some(2));
        assert_eq!(shown_choice(&options, Some("b"), None), Some(1));
        assert_eq!(shown_choice(&options, Some("zz"), None), Some(0));
        assert_eq!(
            shown_choice(&options, None, Some(9)),
            Some(0),
            "a stale turn is ignored"
        );
        assert_eq!(shown_choice(&[], Some("a"), None), None);
    }
}
