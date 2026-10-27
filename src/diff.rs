//! A patch-backed diff document. Hidden context is stored as ranges; only the
//! visible viewport is turned into terminal spans. Full new-side content is
//! optional and is supplied by the caller after an asynchronous read.
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

const EXPAND_LINES: usize = 20;
const DEFAULT_CONTEXT_LINES: usize = 5;
const EMPHASIS_LIMIT: usize = 4096;
const NO_FINAL_NEWLINE: &str = " [no final newline]";
pub(crate) const MAX_REVIEW_BYTES: usize = 128 * 1024;
const REVIEW_MAX_LINES: usize = 2_000;

#[derive(Debug, PartialEq, Eq)]
pub enum ExpandResult {
    Unchanged,
    Changed { anchor: usize },
    NeedsContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Context,
    Delete,
    Insert,
}

#[derive(Debug)]
struct Code {
    kind: Kind,
    old: Option<usize>,
    new: Option<usize>,
    text: String,
    emphasis: Option<Range<usize>>,
    no_final_newline: bool,
}

#[derive(Debug)]
struct Gap {
    old: usize,
    new: usize,
    // Only the trailing interval has unknown length before the full text arrives.
    len: Option<usize>,
    top: usize,
    bottom: usize,
}

#[derive(Debug)]
enum Block {
    Changes(Vec<Code>),
    Gap(Gap),
}

#[derive(Debug)]
enum ProjectionKind {
    Changes,
    Context { start: usize },
    Fold,
}

#[derive(Debug)]
struct Projection {
    block: usize,
    start: usize,
    end: usize,
    kind: ProjectionKind,
}

#[derive(Debug)]
struct Source {
    text: String,
    starts: Vec<usize>,
    max_columns: usize,
}
impl Source {
    fn new(text: String) -> Self {
        let mut starts = Vec::new();
        let mut max_columns = 0;
        let mut column = 0;
        if !text.is_empty() {
            starts.push(0);
            for (byte, ch) in text.char_indices() {
                if ch == '\n' {
                    max_columns = max_columns.max(column);
                    column = 0;
                    if byte + 1 < text.len() {
                        starts.push(byte + 1);
                    }
                } else {
                    column += display_width_of(ch, column);
                }
            }
            if !text.ends_with('\n') {
                column += NO_FINAL_NEWLINE.len();
            }
            max_columns = max_columns.max(column);
        }
        Self {
            text,
            starts,
            max_columns,
        }
    }

    fn no_final_newline(&self, no: usize) -> bool {
        no == self.starts.len() && !self.text.is_empty() && !self.text.ends_with('\n')
    }

    fn line(&self, no: usize) -> Option<&str> {
        let index = no.checked_sub(1)?;
        let start = *self.starts.get(index)?;
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.text.len());
        Some(
            self.text[start..end]
                .strip_suffix('\n')
                .unwrap_or(&self.text[start..end]),
        )
    }
}

/// Full-file indexing is prepared off the terminal thread before attachment.
#[derive(Debug)]
pub struct PreparedContext {
    source: Source,
}

#[derive(Debug)]
pub struct Document {
    blocks: Vec<Block>,
    // Only lines already present in the patch are stored here. Gaps in the
    // original file never allocate one object per hidden line.
    known: Vec<Code>,
    source: Option<Source>,
    projection: Vec<Projection>,
    len: usize,
    gutter_digits: usize,
    message: String,
    metadata: String,
    max_columns: usize,
}

impl Document {
    pub fn new(patch: &str) -> Self {
        let mut document = Self {
            blocks: Vec::new(),
            known: Vec::new(),
            source: None,
            projection: Vec::new(),
            len: 0,
            gutter_digits: 4,
            message: "No changes in this view".into(),
            metadata: String::new(),
            max_columns: 0,
        };
        let mut old = 1;
        let mut new = 1;
        let mut old_left = 0;
        let mut new_left = 0;
        let mut cursor_old = 1;
        let mut cursor_new = 1;
        let mut has_hunk = false;
        let mut whole_file = false;
        let mut messages = Vec::new();
        // The marker immediately after a patch line belongs to that exact
        // side; it must not consume a source line or hunk count.
        let mut last_code = None;
        for raw in patch.split_inclusive('\n') {
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            if let Some((o, oc, n, nc)) = parse_hunk(line) {
                old = if oc == 0 { o + 1 } else { o };
                new = if nc == 0 { n + 1 } else { n };
                old_left = oc;
                new_left = nc;
                has_hunk = true;
                continue;
            }
            // Hunk counts, rather than the prefix alone, determine whether
            // --- and +++ denote code or file metadata.
            let kind = match line.as_bytes().first() {
                Some(b' ') if old_left > 0 && new_left > 0 => Some(Kind::Context),
                Some(b'-') if old_left > 0 => Some(Kind::Delete),
                Some(b'+') if new_left > 0 => Some(Kind::Insert),
                _ => None,
            };
            if let Some(kind) = kind {
                let consumes_old = kind != Kind::Insert;
                let consumes_new = kind != Kind::Delete;
                if old > cursor_old && new > cursor_new {
                    document.push_gap(
                        cursor_old,
                        cursor_new,
                        (old - cursor_old).min(new - cursor_new),
                    );
                }
                let code = Code {
                    kind,
                    old: consumes_old.then_some(old),
                    new: consumes_new.then_some(new),
                    text: line[1..].to_string(),
                    emphasis: None,
                    no_final_newline: false,
                };
                if kind == Kind::Context {
                    document.push_gap(old, new, 1);
                    document.known.push(code);
                    last_code = Some((None, document.known.len() - 1));
                } else {
                    match document.blocks.last_mut() {
                        Some(Block::Changes(lines)) => lines.push(code),
                        _ => document.blocks.push(Block::Changes(vec![code])),
                    }
                    let block = document.blocks.len() - 1;
                    if let Block::Changes(lines) = &document.blocks[block] {
                        last_code = Some((Some(block), lines.len() - 1));
                    }
                }
                if consumes_old {
                    old += 1;
                    old_left -= 1;
                }
                if consumes_new {
                    new += 1;
                    new_left -= 1;
                }
                cursor_old = old;
                cursor_new = new;
            } else if line == "\\ No newline at end of file" {
                match last_code {
                    Some((Some(block), index)) => {
                        if let Block::Changes(lines) = &mut document.blocks[block] {
                            lines[index].no_final_newline = true;
                        }
                    }
                    Some((None, index)) => document.known[index].no_final_newline = true,
                    None => (),
                }
            } else if line.starts_with("--- /dev/null") || line.starts_with("+++ /dev/null") {
                whole_file = true;
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                messages.push("Binary file — no text diff".to_string());
            } else if line.starts_with("old mode ")
                || line.starts_with("new mode ")
                || line.starts_with("rename from ")
                || line.starts_with("rename to ")
                || line.starts_with("new file mode ")
                || line.starts_with("deleted file mode ")
            {
                messages.push(safe_text(line));
            }
        }
        if has_hunk && !whole_file {
            match document.blocks.last_mut() {
                Some(Block::Gap(gap)) => gap.len = None,
                _ => document.blocks.push(Block::Gap(Gap {
                    old: cursor_old,
                    new: cursor_new,
                    len: None,
                    top: 0,
                    bottom: 0,
                })),
            }
        }
        if !messages.is_empty() {
            document.message = messages.join(" · ");
            if has_hunk {
                document.metadata = document.message.clone();
            }
        }
        for block in &mut document.blocks {
            if let Block::Changes(lines) = block {
                compute_emphasis(lines);
            }
        }
        document.max_columns = document
            .known
            .iter()
            .chain(document.blocks.iter().flat_map(|block| match block {
                Block::Changes(lines) => lines.iter(),
                Block::Gap(_) => [].iter(),
            }))
            .map(|code| {
                display_width(&code.text)
                    + if code.no_final_newline {
                        NO_FINAL_NEWLINE.len()
                    } else {
                        0
                    }
            })
            .max()
            .unwrap_or(0);
        document.gutter_digits = old.max(new).to_string().len().max(4);
        document.show_initial_context();
        document.rebuild();
        document
    }

    /// Initialize only from context already carried by the patch. This runs
    /// once: attaching full content later must preserve explicit user folds.
    fn show_initial_context(&mut self) {
        for index in 0..self.blocks.len() {
            let Block::Gap(gap) = &self.blocks[index] else {
                continue;
            };
            let before_change = matches!(self.blocks.get(index + 1), Some(Block::Changes(_)));
            let after_change = index > 0 && matches!(self.blocks[index - 1], Block::Changes(_));
            let top = if after_change {
                (0..DEFAULT_CONTEXT_LINES.min(gap.len.unwrap_or(usize::MAX)))
                    .take_while(|offset| self.context_line(gap.new + offset).is_some())
                    .count()
            } else {
                0
            };
            let bottom = if before_change {
                let len = gap.len.unwrap_or(0);
                (0..DEFAULT_CONTEXT_LINES.min(len.saturating_sub(top)))
                    .take_while(|offset| self.context_line(gap.new + len - 1 - offset).is_some())
                    .count()
            } else {
                0
            };
            let Block::Gap(gap) = &mut self.blocks[index] else {
                unreachable!();
            };
            gap.top = top;
            gap.bottom = bottom;
        }
    }

    fn push_gap(&mut self, old: usize, new: usize, len: usize) {
        if len == 0 {
            return;
        }
        if let Some(Block::Gap(gap)) = self.blocks.last_mut() {
            if gap.old + gap.len.unwrap_or(0) == old && gap.new + gap.len.unwrap_or(0) == new {
                gap.len = gap.len.map(|count| count + len);
                return;
            }
        }
        self.blocks.push(Block::Gap(Gap {
            old,
            new,
            len: Some(len),
            top: 0,
            bottom: 0,
        }));
    }

    pub fn prepare_context(text: String) -> PreparedContext {
        PreparedContext {
            source: Source::new(text),
        }
    }

    #[cfg(test)]
    pub fn provide_context(&mut self, text: String) -> Result<(), String> {
        self.provide_prepared(Self::prepare_context(text))
    }

    pub fn provide_prepared(&mut self, context: PreparedContext) -> Result<(), String> {
        let source = context.source;
        for code in self
            .known
            .iter()
            .chain(self.blocks.iter().flat_map(|block| match block {
                Block::Changes(lines) => lines.iter(),
                Block::Gap(_) => [].iter(),
            }))
        {
            if let Some(no) = code.new {
                if source.line(no) != Some(code.text.as_str())
                    || source.no_final_newline(no) != code.no_final_newline
                {
                    return Err("File changed while loading context; press r to refresh".into());
                }
            }
        }
        for block in &self.blocks {
            if let Block::Gap(gap) = block {
                if let Some(len) = gap.len {
                    if gap.new - 1 + len > source.starts.len() {
                        return Err("Context does not match this diff; press r to refresh".into());
                    }
                }
            }
        }
        for block in &mut self.blocks {
            if let Block::Gap(gap) = block {
                if gap.len.is_none() {
                    gap.len = Some(source.starts.len().saturating_sub(gap.new - 1));
                }
            }
        }
        self.gutter_digits = self
            .gutter_digits
            .max(source.starts.len().to_string().len());
        self.max_columns = self.max_columns.max(source.max_columns);
        self.source = Some(source);
        self.rebuild();
        Ok(())
    }

    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    /// Includes the fixed two-number gutter in `viewport_width`. Width was
    /// computed while preparing content, so clamping never scans source text.
    pub fn horizontal_limit(&self, viewport_width: u16) -> usize {
        let available = usize::from(viewport_width).saturating_sub(self.gutter_digits * 2 + 4);
        self.max_columns.saturating_sub(available)
    }

    pub fn len(&self) -> usize {
        self.len.max(1)
    }

    pub fn is_empty(&self) -> bool {
        !self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Changes(_)))
    }

    pub fn is_fold(&self, index: usize) -> bool {
        self.locate(index)
            .is_some_and(|projection| matches!(projection.kind, ProjectionKind::Fold))
    }

    /// Reveal the context next to a change first. A leading fold borders its
    /// first change at the bottom; other folds border an earlier change above.
    pub fn expand_from_bottom(&self, index: usize) -> bool {
        self.locate(index).is_some_and(|projection| {
            projection.block == 0 && matches!(projection.kind, ProjectionKind::Fold)
        })
    }

    /// Source coordinates survive fold changes and let a refreshed viewer
    /// restore its reading position without keeping obsolete display indices.
    pub fn source_line(&self, index: usize) -> Option<(Option<usize>, Option<usize>)> {
        let projection = self.locate(index)?;
        match (&self.blocks[projection.block], &projection.kind) {
            (Block::Changes(lines), ProjectionKind::Changes) => {
                let code = &lines[index - projection.start];
                Some((code.old, code.new))
            }
            (Block::Gap(gap), kind) => {
                let offset = match kind {
                    ProjectionKind::Context { start } => start + index - projection.start,
                    ProjectionKind::Fold => gap.top,
                    ProjectionKind::Changes => return None,
                };
                Some((Some(gap.old + offset), Some(gap.new + offset)))
            }
            _ => None,
        }
    }

    /// Export selected display rows without display clipping or hidden source.
    /// Fold rows remain explicit omissions; source lines retain both coordinates.
    pub fn selected_patch(&self, start: usize, end: usize) -> Result<String, String> {
        if self.is_empty() || start >= self.len || end >= self.len {
            return Err("Select source lines from this diff before sending".into());
        }
        let mut output = String::new();
        let mut code_lines = 0;
        for index in start.min(end)..=start.max(end) {
            let projection = self
                .locate(index)
                .ok_or("Selected diff row is unavailable")?;
            match (&self.blocks[projection.block], &projection.kind) {
                (Block::Changes(lines), ProjectionKind::Changes) => {
                    let code = &lines[index - projection.start];
                    code_lines += 1;
                    if code_lines > REVIEW_MAX_LINES {
                        return Err(
                            "Selection exceeds 2,000 source lines; select a smaller range".into(),
                        );
                    }
                    append_review_line(
                        &mut output,
                        code.old,
                        code.new,
                        code.kind,
                        &code.text,
                        code.no_final_newline,
                    )?;
                }
                (Block::Gap(gap), ProjectionKind::Context { start }) => {
                    let offset = start + index - projection.start;
                    let new = gap.new + offset;
                    let text = self
                        .context_line(new)
                        .ok_or("Selected context is not loaded")?;
                    code_lines += 1;
                    if code_lines > REVIEW_MAX_LINES {
                        return Err(
                            "Selection exceeds 2,000 source lines; select a smaller range".into(),
                        );
                    }
                    append_review_line(
                        &mut output,
                        Some(gap.old + offset),
                        Some(new),
                        Kind::Context,
                        text,
                        self.context_no_final_newline(new),
                    )?;
                }
                (Block::Gap(gap), ProjectionKind::Fold) => {
                    let old = gap.old + gap.top;
                    let new = gap.new + gap.top;
                    let omission = if let Some(len) = gap.len {
                        let count = len.saturating_sub(gap.top + gap.bottom);
                        format!(
                            "… {count} unchanged lines omitted (old:{old}-{} new:{new}-{})\n",
                            old + count.saturating_sub(1),
                            new + count.saturating_sub(1),
                        )
                    } else {
                        format!("… remaining unchanged lines omitted (old:{old}… new:{new}…)\n")
                    };
                    append_review(&mut output, &omission)?;
                }
                _ => (),
            }
        }
        if code_lines == 0 {
            return Err(
                "Selection contains only folded context; select source lines before sending".into(),
            );
        }
        Ok(output)
    }

    pub fn row_for_source(&self, old: Option<usize>, new: Option<usize>) -> Option<usize> {
        // Prefer new-side coordinates globally before falling back to the old
        // side: insertions and deletions can give the sides different offsets.
        for (target, new_side) in [(new, true), (old, false)] {
            let Some(target) = target else {
                continue;
            };
            for projection in &self.projection {
                match (&self.blocks[projection.block], &projection.kind) {
                    (Block::Changes(lines), ProjectionKind::Changes) => {
                        if let Some(offset) = lines.iter().position(|code| {
                            (if new_side { code.new } else { code.old }) == Some(target)
                        }) {
                            return Some(projection.start + offset);
                        }
                    }
                    (Block::Gap(gap), kind) => {
                        let base = if new_side { gap.new } else { gap.old };
                        let Some(offset) = target.checked_sub(base) else {
                            continue;
                        };
                        match kind {
                            ProjectionKind::Context { start }
                                if offset >= *start
                                    && offset < start + projection.end - projection.start =>
                            {
                                return Some(projection.start + offset - start);
                            }
                            ProjectionKind::Fold
                                if offset >= gap.top
                                    && gap.len.is_none_or(|len| offset < len - gap.bottom) =>
                            {
                                return Some(projection.start);
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }
        }
        None
    }

    fn locate(&self, index: usize) -> Option<&Projection> {
        self.projection
            .get(
                self.projection
                    .partition_point(|projection| projection.end <= index),
            )
            .filter(|projection| index >= projection.start)
    }

    fn context_line(&self, no: usize) -> Option<&str> {
        if let Some(source) = &self.source {
            return source.line(no);
        }
        let index = self
            .known
            .binary_search_by_key(&no, |code| code.new.unwrap())
            .ok()?;
        Some(&self.known[index].text)
    }

    fn context_no_final_newline(&self, no: usize) -> bool {
        if let Some(source) = &self.source {
            return source.no_final_newline(no);
        }
        self.known
            .binary_search_by_key(&no, |code| code.new.unwrap())
            .ok()
            .is_some_and(|index| self.known[index].no_final_newline)
    }

    pub fn line(&self, index: usize, horizontal: usize, width: u16) -> Line<'static> {
        let Some(projection) = self.locate(index) else {
            return Line::styled(self.message.clone(), Style::new().fg(Color::Gray));
        };
        match (&self.blocks[projection.block], &projection.kind) {
            (Block::Changes(lines), ProjectionKind::Changes) => {
                let code = &lines[index - projection.start];
                self.code_line(
                    code.old,
                    code.new,
                    code.kind,
                    &code.text,
                    code.emphasis.as_ref(),
                    code.no_final_newline,
                    horizontal,
                    width,
                )
            }
            (Block::Gap(gap), ProjectionKind::Context { start }) => {
                let offset = start + index - projection.start;
                self.code_line(
                    Some(gap.old + offset),
                    Some(gap.new + offset),
                    Kind::Context,
                    self.context_line(gap.new + offset).unwrap_or(""),
                    None,
                    self.context_no_final_newline(gap.new + offset),
                    horizontal,
                    width,
                )
            }
            (Block::Gap(gap), ProjectionKind::Fold) => {
                let count = gap.len.map(|len| len.saturating_sub(gap.top + gap.bottom));
                let text = match count {
                    Some(count) => format!(
                        "  ⋯ {count} unchanged lines  [Enter expand · t ↓20 · b ↑20 · a all]"
                    ),
                    None => "  ⋯ remaining unchanged lines  [Enter to load]".into(),
                };
                Line::styled(
                    text,
                    Style::new().fg(Color::Gray).add_modifier(Modifier::DIM),
                )
            }
            _ => Line::default(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn code_line(
        &self,
        old: Option<usize>,
        new: Option<usize>,
        kind: Kind,
        text: &str,
        emphasis: Option<&Range<usize>>,
        no_final_newline: bool,
        horizontal: usize,
        width: u16,
    ) -> Line<'static> {
        let digits = self.gutter_digits;
        let marker = match kind {
            Kind::Context => ' ',
            Kind::Delete => '-',
            Kind::Insert => '+',
        };
        let gutter = format!(
            "{:>digits$} {:>digits$} {marker} ",
            old.map(|no| no.to_string()).unwrap_or_default(),
            new.map(|no| no.to_string()).unwrap_or_default()
        );
        let gutter_width = digits * 2 + 4;
        let color = match kind {
            Kind::Context => Color::Reset,
            Kind::Delete => Color::Red,
            Kind::Insert => Color::Green,
        };
        let normal = match kind {
            Kind::Context => Style::new(),
            Kind::Delete | Kind::Insert => Style::new().fg(color).bg(color),
        };
        let emphasized = normal.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let mut spans = vec![Span::styled(gutter, Style::new().fg(Color::Gray))];
        let available = usize::from(width).saturating_sub(gutter_width);
        let mut column = 0;
        let mut run = String::new();
        let mut last_emphasized = false;
        let mut started = false;
        let suffix = if no_final_newline {
            NO_FINAL_NEWLINE
        } else {
            ""
        };
        let end_column = horizontal.saturating_add(available);
        let characters = text.char_indices().chain(
            suffix
                .char_indices()
                .map(|(byte, ch)| (text.len() + byte, ch)),
        );
        for (byte, ch) in characters {
            let highlighted = emphasis.is_some_and(|range| range.contains(&byte));
            if highlighted != last_emphasized && !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    if last_emphasized { emphasized } else { normal },
                ));
            }
            last_emphasized = highlighted;
            let tab_spaces = 4 - column % 4;
            // Ordinary characters go straight into the viewport buffer. Tabs
            // and escaped controls stream their display chars without creating
            // a temporary String for each character we skip or display.
            let mut emit = |visible: char| {
                let cells = visible.width().unwrap_or(0);
                if column >= end_column && cells > 0 {
                    return false;
                }
                if column >= horizontal {
                    if column + cells <= end_column && (cells > 0 || started) {
                        run.push(visible);
                        started = true;
                    } else if cells > 0 {
                        return false;
                    }
                } else if column + cells > horizontal {
                    run.push(' ');
                    started = true;
                }
                column += cells;
                true
            };
            let keep_going = if ch == '\t' {
                (0..tab_spaces).all(|_| emit(' '))
            } else if ch.is_control() {
                ch.escape_default().all(&mut emit)
            } else {
                emit(ch)
            };
            if !keep_going || column >= end_column {
                break;
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(
                run,
                if last_emphasized { emphasized } else { normal },
            ));
        }
        Line::from(spans).style(normal)
    }

    pub fn toggle(&mut self, index: usize, from_bottom: bool, all: bool) -> ExpandResult {
        let Some(projection) = self.locate(index) else {
            return ExpandResult::Unchanged;
        };
        if !matches!(projection.kind, ProjectionKind::Fold) {
            return ExpandResult::Unchanged;
        }
        let block = projection.block;
        let Block::Gap(gap) = &self.blocks[block] else {
            return ExpandResult::Unchanged;
        };
        let (len, mut count) = if let Some(len) = gap.len {
            let hidden = len.saturating_sub(gap.top + gap.bottom);
            (
                len,
                if all {
                    hidden
                } else {
                    hidden.min(EXPAND_LINES)
                },
            )
        } else {
            // A trailing hunk may already contain useful context even though
            // we do not yet know how much text follows it in the full file.
            if from_bottom || all {
                return ExpandResult::NeedsContext;
            }
            let count = (0..EXPAND_LINES)
                .take_while(|offset| self.context_line(gap.new + gap.top + offset).is_some())
                .count();
            if count == 0 {
                return ExpandResult::NeedsContext;
            }
            (gap.top + count, count)
        };
        if self.source.is_none() {
            if all {
                let start = gap.top;
                if (start..start + count)
                    .any(|offset| self.context_line(gap.new + offset).is_none())
                {
                    return ExpandResult::NeedsContext;
                }
            } else {
                // Reveal the available patch context immediately. A second
                // expansion loads the missing remainder instead of delaying
                // these already available lines behind a full-file read.
                count = (0..count)
                    .take_while(|offset| {
                        let position = if from_bottom {
                            len - gap.bottom - 1 - offset
                        } else {
                            gap.top + offset
                        };
                        self.context_line(gap.new + position).is_some()
                    })
                    .count();
                if count == 0 {
                    return ExpandResult::NeedsContext;
                }
            }
        }
        let start = if from_bottom {
            len - gap.bottom - count
        } else {
            gap.top
        };
        if count == 0 {
            return ExpandResult::Unchanged;
        }
        let Block::Gap(gap) = &mut self.blocks[block] else {
            unreachable!();
        };
        if from_bottom {
            gap.bottom += count;
        } else {
            gap.top += count;
        }
        self.rebuild();
        let anchor = self
            .projection
            .iter()
            .find_map(|projection| {
                if projection.block != block {
                    return None;
                }
                match projection.kind {
                    ProjectionKind::Context { start: first }
                        if start >= first && start < first + projection.end - projection.start =>
                    {
                        Some(projection.start + start - first)
                    }
                    _ => None,
                }
            })
            .unwrap_or(index.min(self.len().saturating_sub(1)));
        ExpandResult::Changed { anchor }
    }

    pub fn collapse(&mut self, index: usize) -> Option<usize> {
        let block = self.locate(index)?.block;
        let Block::Gap(gap) = &mut self.blocks[block] else {
            return None;
        };
        if gap.top == 0 && gap.bottom == 0 {
            return None;
        }
        gap.top = 0;
        gap.bottom = 0;
        self.rebuild();
        self.projection
            .iter()
            .find(|projection| projection.block == block)
            .map(|projection| projection.start)
    }

    pub fn next_change(&self, index: usize, forward: bool) -> Option<usize> {
        let changes = self
            .projection
            .iter()
            .filter(|projection| matches!(projection.kind, ProjectionKind::Changes));
        if forward {
            changes
                .filter(|projection| projection.start > index)
                .map(|projection| projection.start)
                .next()
        } else {
            changes
                .filter(|projection| projection.start < index)
                .map(|projection| projection.start)
                .next_back()
        }
    }

    fn rebuild(&mut self) {
        self.projection.clear();
        self.len = 0;
        for (block, content) in self.blocks.iter().enumerate() {
            let mut push = |kind, len: usize| {
                if len > 0 {
                    self.projection.push(Projection {
                        block,
                        start: self.len,
                        end: self.len + len,
                        kind,
                    });
                    self.len += len;
                }
            };
            match content {
                Block::Changes(lines) => push(ProjectionKind::Changes, lines.len()),
                Block::Gap(gap) => {
                    if let Some(len) = gap.len {
                        if gap.top + gap.bottom == len {
                            // The default windows of neighboring changes can
                            // overlap. Project their union once, without a fold.
                            push(ProjectionKind::Context { start: 0 }, len);
                            continue;
                        }
                    }
                    push(ProjectionKind::Context { start: 0 }, gap.top);
                    if gap.len.is_none_or(|len| gap.top + gap.bottom < len) {
                        push(ProjectionKind::Fold, 1);
                    }
                    if let Some(len) = gap.len {
                        push(
                            ProjectionKind::Context {
                                start: len - gap.bottom,
                            },
                            gap.bottom,
                        );
                    }
                }
            }
        }
    }
}

fn append_review(output: &mut String, text: &str) -> Result<(), String> {
    if text.len() > MAX_REVIEW_BYTES.saturating_sub(output.len()) {
        return Err("Selection exceeds 128 KiB; select a smaller range".into());
    }
    output.push_str(text);
    Ok(())
}

fn append_review_line(
    output: &mut String,
    old: Option<usize>,
    new: Option<usize>,
    kind: Kind,
    text: &str,
    no_final_newline: bool,
) -> Result<(), String> {
    let marker = match kind {
        Kind::Context => ' ',
        Kind::Delete => '-',
        Kind::Insert => '+',
    };
    append_review(
        output,
        &format!(
            "old:{} new:{} {marker}",
            old.map_or_else(|| "-".into(), |line| line.to_string()),
            new.map_or_else(|| "-".into(), |line| line.to_string()),
        ),
    )?;
    // Reject oversized lines before scanning their text, and keep terminal
    // controls inert while preserving actual tabs and Unicode source characters.
    if text.len() > MAX_REVIEW_BYTES.saturating_sub(output.len()) {
        return Err("Selection exceeds 128 KiB; select a smaller range".into());
    }
    if text.chars().any(|ch| ch.is_control() && ch != '\t') {
        for ch in text.chars() {
            if ch.is_control() && ch != '\t' {
                for escaped in ch.escape_default() {
                    append_review(output, escaped.encode_utf8(&mut [0; 4]))?;
                }
            } else {
                append_review(output, ch.encode_utf8(&mut [0; 4]))?;
            }
        }
    } else {
        append_review(output, text)?;
    }
    if no_final_newline {
        append_review(output, NO_FINAL_NEWLINE)?;
    }
    append_review(output, "\n")
}

fn parse_hunk(line: &str) -> Option<(usize, usize, usize, usize)> {
    let mut parts = line.strip_prefix("@@ -")?.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?.strip_prefix('+')?;
    if parts.next()? != "@@" {
        return None;
    }
    let range = |value: &str| -> Option<(usize, usize)> {
        let (start, len) = value.split_once(',').unwrap_or((value, "1"));
        Some((start.parse().ok()?, len.parse().ok()?))
    };
    let (old, old_len) = range(old)?;
    let (new, new_len) = range(new)?;
    Some((old, old_len, new, new_len))
}

fn display_width_of(ch: char, column: usize) -> usize {
    if ch == '\t' {
        4 - column % 4
    } else if ch.is_control() {
        ch.escape_default().count()
    } else {
        ch.width().unwrap_or(0)
    }
}

fn display_width(text: &str) -> usize {
    text.chars()
        .fold(0, |column, ch| column + display_width_of(ch, column))
}

fn safe_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

// A bounded, linear comparison highlights the edited portion of paired lines.
// Very long lines or differently sized blocks retain ordinary diff colors.
fn compute_emphasis(lines: &mut [Code]) {
    let split = lines
        .iter()
        .position(|line| line.kind == Kind::Insert)
        .unwrap_or(lines.len());
    if split == 0 || split * 2 != lines.len() || split > 128 {
        return;
    }
    let (old, new) = lines.split_at_mut(split);
    for (old, new) in old.iter_mut().zip(new) {
        if old.text.len().max(new.text.len()) > EMPHASIS_LIMIT {
            continue;
        }
        let prefix = old
            .text
            .chars()
            .zip(new.text.chars())
            .take_while(|(a, b)| a == b)
            .map(|(ch, _)| ch.len_utf8())
            .sum::<usize>();
        let suffix = old.text[prefix..]
            .chars()
            .rev()
            .zip(new.text[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(ch, _)| ch.len_utf8())
            .sum::<usize>();
        old.emphasis = Some(prefix..old.text.len() - suffix);
        new.emphasis = Some(prefix..new.text.len() - suffix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(document: &Document, index: usize) -> String {
        document
            .line(index, 0, 180)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn selected_patch_preserves_both_sides_context_unicode_and_tabs() {
        let document = Document::new("@@ -1,3 +1,3 @@\n before\n-old\t中文\n+new\t文字\n after\n");
        let selected = document.selected_patch(0, 3).unwrap();
        assert_eq!(
            selected,
            "old:1 new:1  before\nold:2 new:- -old\t中文\nold:- new:2 +new\t文字\nold:3 new:3  after\n"
        );
        assert_eq!(document.selected_patch(3, 0).unwrap(), selected);
        assert!(document.line(2, 8, 20).width() <= 20);
        assert!(document.selected_patch(2, 2).unwrap().contains("new\t文字"));
    }

    #[test]
    fn selected_patch_reports_hidden_intervals_without_expanding_them() {
        let document = Document::new(
            "@@ -1 +1 @@\n-old1\n+new1\n@@ -100000 +100000 @@\n-old_last\n+new_last\n",
        );
        let last = document.row_for_source(None, Some(100_000)).unwrap();
        let selected = document.selected_patch(0, last).unwrap();
        assert!(selected.contains("99998 unchanged lines omitted (old:2-99999 new:2-99999)"));
        assert!(selected.contains("old:- new:100000 +new_last"));
        assert!(selected.len() < 300);
        let fold = (0..document.len())
            .find(|row| document.is_fold(*row))
            .unwrap();
        assert!(document
            .selected_patch(fold, fold)
            .unwrap_err()
            .contains("only folded"));
        assert!(Document::new("").selected_patch(0, 0).is_err());
        assert!(document.selected_patch(0, usize::MAX).is_err());
    }

    #[test]
    fn selected_patch_preserves_missing_newline_and_escapes_terminal_controls() {
        let document = Document::new("@@ -1 +1 @@\n-hello\n\\ No newline at end of file\n+hello\n");
        let selected = document.selected_patch(0, 1).unwrap();
        assert!(selected.contains("old:1 new:- -hello [no final newline]\n"));
        assert!(selected.contains("old:- new:1 +hello\n"));
        let document = Document::new("@@ -1 +1 @@\n-old\n+\u{1b}[31mtext\t中\n");
        let selected = document.selected_patch(1, 1).unwrap();
        assert!(!selected.contains('\u{1b}'));
        assert!(selected.contains("\\u{1b}[31mtext\t中"));
    }

    #[test]
    fn selected_patch_exports_loaded_context_with_original_coordinates() {
        let mut document = Document::new("@@ -20 +20 @@\n-old\n+new\n");
        let source = (1..=25)
            .map(|line| {
                if line == 20 {
                    "new".into()
                } else {
                    format!("source{line}")
                }
            })
            .collect::<Vec<String>>()
            .join("\n");
        document.provide_context(source).unwrap();
        document.toggle(0, false, false);
        assert_eq!(
            document.selected_patch(0, 0).unwrap(),
            "old:1 new:1  source1\n"
        );
        let tail = document.row_for_source(None, Some(25)).unwrap();
        assert!(document.is_fold(tail));
        document.toggle(tail, true, false);
        let last = document.row_for_source(None, Some(25)).unwrap();
        assert_eq!(
            document.selected_patch(last, last).unwrap(),
            "old:25 new:25  source25 [no final newline]\n"
        );
    }

    #[test]
    fn selected_patch_rejects_oversized_source_ranges_and_long_lines() {
        let mut patch = "--- /dev/null\n+++ b/new\n@@ -0,0 +1,2001 @@\n".to_string();
        patch.push_str(&"+line\n".repeat(2_001));
        let document = Document::new(&patch);
        assert!(document.selected_patch(0, 1_999).is_ok());
        assert!(document
            .selected_patch(0, 2_000)
            .unwrap_err()
            .contains("2,000"));
        let patch = format!(
            "--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+{}\n",
            "中".repeat(MAX_REVIEW_BYTES / 3 + 1)
        );
        let document = Document::new(&patch);
        assert!(document
            .selected_patch(0, 0)
            .unwrap_err()
            .contains("128 KiB"));
        let patch = format!(
            "--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+{}\n",
            "\u{1b}".repeat(30_000)
        );
        assert!(Document::new(&patch)
            .selected_patch(0, 0)
            .unwrap_err()
            .contains("128 KiB"));
    }

    #[test]
    fn default_context_preserves_header_like_code_and_source_line_numbers() {
        let mut document = Document::new(
            "--- a/x\n+++ b/x\n@@ -10,3 +10,3 @@\n before\n---comment\n+++comment\n after\n",
        );
        assert_eq!(document.len(), 6);
        assert!(text(&document, 1).ends_with("before"));
        assert!(text(&document, 2).contains("  11      - --comment"));
        assert!(text(&document, 3).contains("       11 + ++comment"));
        assert!(text(&document, 4).ends_with("after"));
        assert_eq!(document.toggle(0, true, false), ExpandResult::NeedsContext);
        document
            .provide_context(
                (1..=12)
                    .map(|i| match i {
                        10 => "before".to_string(),
                        11 => "++comment".to_string(),
                        12 => "after".to_string(),
                        _ => i.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            )
            .unwrap();
        assert!(matches!(
            document.toggle(0, true, false),
            ExpandResult::Changed { .. }
        ));
        assert!(text(&document, 0).contains("   1    1"));
    }

    #[test]
    fn default_patch_context_can_be_collapsed_and_reopened_without_loading() {
        let mut document = Document::new("@@ -1,3 +1,3 @@\n one\n-old\n+new\n three\n");
        assert!(text(&document, 0).ends_with("one"));
        assert!(text(&document, 3).ends_with("three"));
        assert_eq!(document.collapse(0), Some(0));
        assert!(document.is_fold(0));
        assert_eq!(
            document.toggle(0, false, false),
            ExpandResult::Changed { anchor: 0 }
        );
        assert!(text(&document, 0).ends_with("one"));
        assert!(document.source.is_none());
    }

    #[test]
    fn default_context_shows_five_known_lines_per_edge_and_keeps_other_lines_folded() {
        let mut patch = "@@ -10,15 +10,15 @@\n".to_string();
        for number in 10..=16 {
            patch.push_str(&format!(" line{number}\n"));
        }
        patch.push_str("-old17\n+new17\n");
        for number in 18..=24 {
            patch.push_str(&format!(" line{number}\n"));
        }
        let document = Document::new(&patch);
        let visible = (0..document.len())
            .map(|row| text(&document, row))
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 14);
        assert!(document.is_fold(0));
        for number in (12..=16).chain(18..=22) {
            assert!(visible
                .iter()
                .any(|line| line.ends_with(&format!("line{number}"))));
        }
        for number in [10, 11, 23, 24] {
            assert!(!visible
                .iter()
                .any(|line| line.ends_with(&format!("line{number}"))));
        }
        assert!(document.is_fold(document.len() - 1));
        assert!(document.projection.len() <= 6);
        assert!(document.source.is_none());
    }

    #[test]
    fn overlapping_default_context_windows_are_projected_once() {
        let mut patch = "@@ -1,13 +1,13 @@\n line1\n line2\n-old3\n+new3\n".to_string();
        for number in 4..=10 {
            patch.push_str(&format!(" line{number}\n"));
        }
        patch.push_str("-old11\n+new11\n line12\n line13\n");
        let document = Document::new(&patch);
        assert_eq!(document.len(), 16);
        let internal_gap = document
            .blocks
            .iter()
            .position(|block| matches!(block, Block::Gap(gap) if gap.new == 4))
            .unwrap();
        assert_eq!(
            document
                .projection
                .iter()
                .filter(|projection| projection.block == internal_gap)
                .count(),
            1
        );
        for number in 4..=10 {
            let occurrences = (0..document.len())
                .filter(|row| text(&document, *row).ends_with(&format!("line{number}")))
                .count();
            assert_eq!(occurrences, 1, "line {number} must not be duplicated");
        }
        assert_eq!(
            document.source_line(document.row_for_source(None, Some(11)).unwrap()),
            Some((None, Some(11)))
        );
    }

    #[test]
    fn context_loading_keeps_manual_collapse_and_additional_expansion() {
        let mut patch = "@@ -10,15 +10,15 @@\n".to_string();
        for number in 10..=16 {
            patch.push_str(&format!(" line{number}\n"));
        }
        patch.push_str("-old17\n+new17\n");
        for number in 18..=24 {
            patch.push_str(&format!(" line{number}\n"));
        }
        let mut document = Document::new(&patch);
        let leading = document.row_for_source(None, Some(16)).unwrap();
        document.collapse(leading).unwrap();
        let trailing = document.len() - 1;
        assert!(matches!(
            document.toggle(trailing, false, false),
            ExpandResult::Changed { .. }
        ));
        let content = (1..=30)
            .map(|number| {
                if number == 17 {
                    "new17\n".to_string()
                } else {
                    format!("line{number}\n")
                }
            })
            .collect();
        document.provide_context(content).unwrap();
        assert!(document.is_fold(0));
        assert_eq!(
            document.row_for_source(None, Some(16)),
            Some(0),
            "loading must not reopen a manual fold"
        );
        let last_known = document.row_for_source(None, Some(24)).unwrap();
        assert!(
            text(&document, last_known).ends_with("line24"),
            "loading must preserve manually expanded lines"
        );
        assert!(!text(&document, document.len() - 1).ends_with("line25"));
    }

    #[test]
    fn large_hidden_ranges_and_expansion_use_bounded_projection() {
        let mut document = Document::new("@@ -50000 +50000 @@\n-old\n+new\n");
        assert_eq!(document.len(), 4);
        assert_eq!(document.blocks.len(), 3);
        let content = (1..=100000)
            .map(|i| {
                if i == 50000 {
                    "new".to_string()
                } else {
                    i.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        document.provide_context(content).unwrap();
        assert!(matches!(
            document.toggle(0, false, false),
            ExpandResult::Changed { .. }
        ));
        assert_eq!(document.len(), 24);
        assert!(document.projection.len() <= 4);
        assert!(matches!(
            document.toggle(20, true, true),
            ExpandResult::Changed { .. }
        ));
        assert_eq!(document.len(), 50002);
        assert!(document.projection.len() <= 5);
        assert_eq!(document.collapse(20000), Some(0));
        assert_eq!(document.len(), 4);
    }

    #[test]
    fn full_new_and_deleted_files_have_no_unknown_tail() {
        let inserted = Document::new("--- /dev/null\n+++ b/x\n@@ -0,0 +1,2 @@\n+one\n+two\n");
        assert_eq!(inserted.len(), 2);
        assert!(text(&inserted, 0).contains("    1 + one"));
        let deleted = Document::new("--- a/x\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n");
        assert_eq!(deleted.len(), 2);
        assert!(text(&deleted, 0).contains("   1      - one"));
    }

    #[test]
    fn context_validation_rejects_changed_content_without_modifying_document() {
        let mut document = Document::new("@@ -1,2 +1,2 @@\n same\n-old\n+new\n");
        let before = document.len();
        assert!(document.provide_context("same\nchanged\n".into()).is_err());
        assert_eq!(document.len(), before);
        assert!(document.source.is_none());
    }

    #[test]
    fn clipping_keeps_gutters_and_handles_tabs_wide_chars_and_controls() {
        let document = Document::new("--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+中\tx\u{1b}z\n");
        let line = document.line(0, 1, 30);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.starts_with("        1 + "));
        assert!(rendered.ends_with("   x\\u{1b}z"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(line.width() <= 30);
    }

    #[test]
    fn trailing_patch_context_is_visible_before_loading_the_remainder() {
        let mut document = Document::new("@@ -1,2 +1,2 @@\n-old\n+new\n known\n");
        assert!(text(&document, 2).ends_with("known"));
        assert_eq!(document.toggle(3, false, false), ExpandResult::NeedsContext);
        document
            .provide_context("new\nknown\nlast\n".into())
            .unwrap();
        assert_eq!(
            document.toggle(3, false, false),
            ExpandResult::Changed { anchor: 3 }
        );
        assert!(text(&document, 3).ends_with("last"));
    }

    #[test]
    fn source_anchors_resolve_both_expanded_and_hidden_context() {
        let mut document = Document::new("@@ -3 +3 @@\n-old\n+new\n");
        assert_eq!(document.source_line(1), Some((Some(3), None)));
        assert_eq!(document.row_for_source(None, Some(1)), Some(0));
        assert_eq!(document.row_for_source(Some(3), None), Some(1));
        document
            .provide_context("one\ntwo\nnew\nlast\n".into())
            .unwrap();
        document.toggle(0, false, true);
        assert_eq!(document.row_for_source(None, Some(2)), Some(1));
        assert_eq!(document.row_for_source(None, Some(3)), Some(3));
        assert_eq!(document.row_for_source(None, Some(4)), Some(4));
        document.collapse(0);
        assert_eq!(document.row_for_source(None, Some(2)), Some(0));
    }

    #[test]
    fn tab_and_space_changes_remain_visible_and_unicode_emphasis_is_valid() {
        let document = Document::new("@@ -1,2 +1,2 @@\n-\t中a\n-   word\n+    中b\n+    word\n");
        assert_eq!(document.blocks.len(), 2);
        assert!(text(&document, 0).contains("中a"));
        assert!(text(&document, 2).contains("中b"));
        let line = document.line(2, 0, 80);
        assert!(line
            .spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[test]
    fn empty_mode_only_and_binary_changes_have_explicit_messages() {
        assert!(text(&Document::new(""), 0).contains("No changes"));
        assert!(text(&Document::new("old mode 100644\nnew mode 100755\n"), 0).contains("100755"));
        assert!(text(&Document::new("Binary files a/x and b/x differ\n"), 0).contains("Binary"));
    }

    #[test]
    fn a_wide_character_at_the_right_edge_does_not_scan_or_show_later_text() {
        let document = Document::new("--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+a中b\n");
        assert!(text(&document, 0).ends_with("a中b"));
        let line = document.line(0, 0, 14);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.ends_with('a'));
        assert!(!rendered.ends_with('b'));
        assert!(line.width() <= 14);
    }

    #[test]
    fn final_newline_only_changes_keep_the_side_specific_marker() {
        let document = Document::new("@@ -1 +1 @@\n-hello\n\\ No newline at end of file\n+hello\n");
        assert!(text(&document, 0).ends_with("hello [no final newline]"));
        assert!(text(&document, 1).ends_with("hello"));
        assert!(!text(&document, 1).contains("no final newline"));
        let mut removed =
            Document::new("@@ -1 +1 @@\n-hello\n+hello\n\\ No newline at end of file\n");
        assert!(text(&removed, 1).ends_with("hello [no final newline]"));
        assert!(removed.provide_context("hello\n".into()).is_err());
        removed.provide_context("hello".into()).unwrap();
    }

    #[test]
    fn loaded_last_context_line_keeps_its_missing_newline_marker() {
        let mut document = Document::new("@@ -1 +1 @@\n-old\n+new\n");
        let prepared = Document::prepare_context("new\nlast".into());
        assert_eq!(prepared.source.starts, vec![0, 4]);
        document.provide_prepared(prepared).unwrap();
        document.toggle(2, false, true);
        assert!(text(&document, 2).ends_with("last [no final newline]"));
        assert!(document.horizontal_limit(13) >= "last [no final newline]".len() - 1);
    }

    #[test]
    fn mode_metadata_remains_available_with_text_changes() {
        let document = Document::new("old mode 100644\nnew mode 100755\n@@ -1 +1 @@\n-old\n+new\n");
        assert!(document.metadata().contains("old mode 100644"));
        assert!(document.metadata().contains("new mode 100755"));
        assert!(text(&document, 0).ends_with("old"));
        assert!(Document::new("@@ -1 +1 @@\n-a\n+b\n").metadata().is_empty());
    }

    #[test]
    fn horizontal_limits_are_cached_and_include_tabs_and_newline_markers() {
        let document = Document::new(
            "--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+中\tx\n\\ No newline at end of file\n",
        );
        let content_width = 5 + NO_FINAL_NEWLINE.len();
        assert_eq!(document.horizontal_limit(12), content_width);
        assert_eq!(document.horizontal_limit(16), content_width - 4);
        assert_eq!(document.horizontal_limit(200), 0);
        let rendered = document.line(0, document.horizontal_limit(16), 16);
        assert_eq!(rendered.width(), 16);
        let text: String = rendered
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.ends_with("ine]"));
    }

    #[test]
    fn hunk_navigation_skips_context_and_tracks_expanded_positions() {
        let mut document = Document::new("@@ -1 +1 @@\n-a\n+b\n@@ -5 +5 @@\n-c\n+d\n");
        assert_eq!(document.next_change(0, true), Some(3));
        document.provide_context("b\n2\n3\n4\nd\n".into()).unwrap();
        assert!(matches!(
            document.toggle(2, false, true),
            ExpandResult::Changed { .. }
        ));
        assert_eq!(document.next_change(0, true), Some(5));
        assert_eq!(document.next_change(6, false), Some(5));
    }
}
