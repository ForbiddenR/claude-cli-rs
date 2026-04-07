use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

#[derive(Clone)]
pub struct MarkdownRenderer {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl MarkdownRenderer {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .or_else(|| theme_set.themes.values().next())
            .cloned()
            .unwrap_or_default();
        Self { syntax_set, theme }
    }

    pub fn render(&self, markdown: &str, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);

        let options = Options::all();
        let parser = Parser::new_ext(markdown, options);

        let mut out: Vec<Line<'static>> = Vec::new();
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut style_stack: Vec<Style> = vec![Style::default()];

        let mut list_stack: Vec<ListState> = Vec::new();
        let mut pending_prefix: Option<Span<'static>> = None;

        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_buf = String::new();

        for ev in parser {
            match ev {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {}
                    Tag::Heading { level, .. } => {
                        flush_line(&mut out, &mut cur, width);
                        style_stack.push(heading_style(style_stack.last().copied(), level as u8));
                    }
                    Tag::Emphasis => {
                        style_stack.push(
                            style_stack
                                .last()
                                .copied()
                                .unwrap_or_default()
                                .add_modifier(Modifier::ITALIC),
                        );
                    }
                    Tag::Strong => {
                        style_stack.push(
                            style_stack
                                .last()
                                .copied()
                                .unwrap_or_default()
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                    Tag::Strikethrough => {
                        style_stack.push(
                            style_stack
                                .last()
                                .copied()
                                .unwrap_or_default()
                                .add_modifier(Modifier::CROSSED_OUT),
                        );
                    }
                    Tag::List(start) => {
                        flush_line(&mut out, &mut cur, width);
                        list_stack.push(ListState::new(start));
                    }
                    Tag::Item => {
                        flush_line(&mut out, &mut cur, width);
                        pending_prefix = Some(list_item_prefix(&mut list_stack));
                    }
                    Tag::CodeBlock(kind) => {
                        flush_line(&mut out, &mut cur, width);
                        in_code_block = true;
                        code_lang = match kind {
                            CodeBlockKind::Fenced(lang) => lang.to_string(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        code_buf.clear();
                    }
                    Tag::BlockQuote(_) => {
                        flush_line(&mut out, &mut cur, width);
                        style_stack.push(
                            style_stack
                                .last()
                                .copied()
                                .unwrap_or_default()
                                .fg(Color::Gray)
                                .add_modifier(Modifier::ITALIC),
                        );
                    }
                    Tag::Link { .. } => {
                        style_stack.push(
                            style_stack
                                .last()
                                .copied()
                                .unwrap_or_default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::UNDERLINED),
                        );
                    }
                    _ => {}
                },
                Event::End(tag_end) => match tag_end {
                    TagEnd::Paragraph => {
                        flush_line(&mut out, &mut cur, width);
                        push_blank_line(&mut out);
                    }
                    TagEnd::Heading(_) => {
                        flush_line(&mut out, &mut cur, width);
                        let _ = style_stack.pop();
                        push_blank_line(&mut out);
                    }
                    TagEnd::Emphasis
                    | TagEnd::Strong
                    | TagEnd::Strikethrough
                    | TagEnd::BlockQuote
                    | TagEnd::Link => {
                        let _ = style_stack.pop();
                    }
                    TagEnd::Item => {
                        if cur.is_empty() {
                            if let Some(prefix) = pending_prefix.take() {
                                cur.push(prefix);
                            }
                        }
                        flush_line(&mut out, &mut cur, width);
                    }
                    TagEnd::List(_) => {
                        flush_line(&mut out, &mut cur, width);
                        let _ = list_stack.pop();
                        push_blank_line(&mut out);
                    }
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        let lang = std::mem::take(&mut code_lang);
                        let code = std::mem::take(&mut code_buf);
                        out.extend(self.highlight_code_block(&code, &lang));
                        push_blank_line(&mut out);
                    }
                    _ => {}
                },
                Event::Text(text) => {
                    if in_code_block {
                        code_buf.push_str(&text);
                        continue;
                    }
                    maybe_push_prefix(&mut cur, &mut pending_prefix);
                    push_text(
                        &mut cur,
                        style_stack.last().copied().unwrap_or_default(),
                        &text,
                    );
                }
                Event::Code(code) => {
                    maybe_push_prefix(&mut cur, &mut pending_prefix);
                    cur.push(Span::styled(
                        code.to_string(),
                        style_stack
                            .last()
                            .copied()
                            .unwrap_or_default()
                            .fg(Color::Yellow),
                    ));
                }
                Event::SoftBreak => {
                    if in_code_block {
                        code_buf.push('\n');
                        continue;
                    }
                    maybe_push_prefix(&mut cur, &mut pending_prefix);
                    cur.push(Span::raw(" "));
                }
                Event::HardBreak => {
                    if in_code_block {
                        code_buf.push('\n');
                        continue;
                    }
                    flush_line(&mut out, &mut cur, width);
                }
                Event::Rule => {
                    flush_line(&mut out, &mut cur, width);
                    out.push(Line::from(Span::styled(
                        "-".repeat(width),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                Event::Html(_)
                | Event::InlineHtml(_)
                | Event::InlineMath(_)
                | Event::DisplayMath(_)
                | Event::FootnoteReference(_)
                | Event::TaskListMarker(_) => {}
            }
        }

        flush_line(&mut out, &mut cur, width);
        trim_trailing_blank_lines(&mut out);
        if out.is_empty() {
            out.push(Line::from(""));
        }
        out
    }

    fn highlight_code_block(&self, code: &str, lang: &str) -> Vec<Line<'static>> {
        let lang_norm = lang.trim().to_ascii_lowercase();
        if lang_norm == "diff" || lang_norm == "patch" {
            return highlight_diff_block(code);
        }

        let syntax = self
            .syntax_set
            .find_syntax_by_token(lang)
            .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut out: Vec<Line<'static>> = Vec::new();

        for raw_line in code.lines() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let ranges = highlighter.highlight_line(raw_line, &self.syntax_set);
            if let Ok(ranges) = ranges {
                for (style, text) in ranges {
                    spans.push(Span::styled(text.to_string(), syntect_to_tui_style(style)));
                }
            } else {
                spans.push(Span::raw(raw_line.to_string()));
            }

            out.push(Line::from(spans));
        }

        if out.is_empty() {
            out.push(Line::from(""));
        }
        out
    }
}

fn highlight_diff_block(code: &str) -> Vec<Line<'static>> {
    let header_style = Style::default().fg(Color::DarkGray);
    let hunk_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let add_style = Style::default().fg(Color::Green);
    let del_style = Style::default().fg(Color::Red);

    let mut out: Vec<Line<'static>> = Vec::new();
    for raw_line in code.lines() {
        let style = if raw_line.starts_with("diff ")
            || raw_line.starts_with("index ")
            || raw_line.starts_with("---")
            || raw_line.starts_with("+++")
        {
            header_style
        } else if raw_line.starts_with("@@") {
            hunk_style
        } else if raw_line.starts_with('+') && !raw_line.starts_with("+++") {
            add_style
        } else if raw_line.starts_with('-') && !raw_line.starts_with("---") {
            del_style
        } else {
            Style::default()
        };

        out.push(Line::from(Span::styled(raw_line.to_string(), style)));
    }

    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

pub struct StreamingMarkdown {
    stable_lines: Vec<Line<'static>>,
    stable_offset: usize,
    tail_lines: Vec<Line<'static>>,
}

impl StreamingMarkdown {
    pub fn new() -> Self {
        Self {
            stable_lines: Vec::new(),
            stable_offset: 0,
            tail_lines: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.stable_lines.clear();
        self.stable_offset = 0;
        self.tail_lines.clear();
    }

    pub fn update(&mut self, full_text: &str, renderer: &MarkdownRenderer, width: usize) {
        let new_stable_offset = find_safe_stable_offset(full_text, self.stable_offset);
        if new_stable_offset > self.stable_offset {
            let section = &full_text[self.stable_offset..new_stable_offset];
            self.stable_lines.extend(renderer.render(section, width));
            self.stable_offset = new_stable_offset;
        }

        let tail = &full_text[self.stable_offset..];
        self.tail_lines = renderer.render(tail, width);
    }

    pub fn iter_lines(&self) -> impl Iterator<Item = &Line<'static>> {
        self.stable_lines.iter().chain(self.tail_lines.iter())
    }

    pub fn line_count(&self) -> usize {
        self.stable_lines.len() + self.tail_lines.len()
    }

    pub fn into_static(
        mut self,
        full_text: &str,
        renderer: &MarkdownRenderer,
        width: usize,
    ) -> Vec<Line<'static>> {
        self.update(full_text, renderer, width);
        let mut out = self.stable_lines;
        out.extend(self.tail_lines);
        out
    }
}

#[derive(Debug)]
struct ListState {
    ordered: bool,
    next_num: u64,
}

impl ListState {
    fn new(start: Option<u64>) -> Self {
        Self {
            ordered: start.is_some(),
            next_num: start.unwrap_or(1),
        }
    }
}

fn list_item_prefix(list_stack: &mut [ListState]) -> Span<'static> {
    let depth = list_stack.len().saturating_sub(1);
    let indent = "  ".repeat(depth);

    let Some(last) = list_stack.last_mut() else {
        return Span::raw("- ");
    };

    let prefix = if last.ordered {
        let n = last.next_num;
        last.next_num = last.next_num.saturating_add(1);
        format!("{indent}{n}. ")
    } else {
        format!("{indent}- ")
    };

    Span::styled(
        prefix,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )
}

fn heading_style(base: Option<Style>, level: u8) -> Style {
    let base = base.unwrap_or_default();
    let color = match level {
        1 => Color::Yellow,
        2 => Color::Green,
        _ => Color::Cyan,
    };
    base.fg(color).add_modifier(Modifier::BOLD)
}

fn maybe_push_prefix(cur: &mut Vec<Span<'static>>, pending: &mut Option<Span<'static>>) {
    if cur.is_empty() {
        if let Some(p) = pending.take() {
            cur.push(p);
        }
    }
}

fn push_text(cur: &mut Vec<Span<'static>>, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    cur.push(Span::styled(text.to_string(), style));
}

fn flush_line(out: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>, width: usize) {
    if cur.is_empty() {
        return;
    }
    let line = Line::from(std::mem::take(cur));
    out.extend(wrap_line(line, width));
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);

    if line.width() <= width {
        return vec![line];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_style: Option<Style> = None;
    let mut cur_buf = String::new();
    let mut col: usize = 0;

    let flush_segment =
        |spans: &mut Vec<Span<'static>>, style: &mut Option<Style>, buf: &mut String| {
            if buf.is_empty() {
                return;
            }
            let s = std::mem::take(buf);
            spans.push(Span::styled(s, style.unwrap_or_default()));
        };

    let flush_wrapped_line = |out: &mut Vec<Line<'static>>,
                              spans: &mut Vec<Span<'static>>,
                              style: &mut Option<Style>,
                              buf: &mut String| {
        flush_segment(spans, style, buf);
        out.push(Line::from(std::mem::take(spans)));
        *style = None;
    };

    for span in line.spans {
        let style = span.style;
        for ch in span.content.chars() {
            if col >= width {
                flush_wrapped_line(&mut out, &mut cur_spans, &mut cur_style, &mut cur_buf);
                col = 0;
            }
            if cur_style != Some(style) {
                flush_segment(&mut cur_spans, &mut cur_style, &mut cur_buf);
                cur_style = Some(style);
            }
            cur_buf.push(ch);
            col += 1;
        }
    }

    flush_segment(&mut cur_spans, &mut cur_style, &mut cur_buf);
    if !cur_spans.is_empty() || out.is_empty() {
        out.push(Line::from(cur_spans));
    }
    out
}

fn push_blank_line(out: &mut Vec<Line<'static>>) {
    out.push(Line::from(""));
}

fn trim_trailing_blank_lines(lines: &mut Vec<Line<'static>>) {
    while lines.last().map_or(false, |l| l.width() == 0) {
        lines.pop();
    }
}

fn syntect_to_tui_style(style: syntect::highlighting::Style) -> Style {
    let mut out = Style::default();

    let fg = style.foreground;
    out = out.fg(Color::Rgb(fg.r, fg.g, fg.b));

    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }

    out
}

fn find_safe_stable_offset(full_text: &str, current: usize) -> usize {
    const TAIL_GUARD_BYTES: usize = 200;

    if full_text.len() <= current + 2 {
        return current;
    }

    let search_end = full_text.len().saturating_sub(TAIL_GUARD_BYTES);
    if search_end <= current {
        return current;
    }

    let mut candidate = full_text[..search_end]
        .rfind("\n\n")
        .map(|idx| idx + 2)
        .unwrap_or(current);
    candidate = candidate.max(current);

    // Avoid stabilizing inside a fenced code block. Heuristic: count ``` up to candidate.
    while candidate > current && is_inside_fence(full_text, candidate) {
        let prefix = &full_text[..candidate.saturating_sub(2)];
        let Some(prev) = prefix.rfind("\n\n") else {
            return current;
        };
        candidate = prev + 2;
    }

    candidate
}

fn is_inside_fence(text: &str, idx: usize) -> bool {
    text[..idx].match_indices("```").count() % 2 == 1
}
