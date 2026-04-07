#[derive(Debug, Clone, Default)]
pub struct InputSnapshot {
    pub text: String,
    pub cursor: usize,
}

#[derive(Debug, Clone, Default)]
pub struct InputBuffer {
    text: String,
    cursor: usize,               // byte index, always a char boundary
    selection_anchor: Option<usize>, // byte index, always a char boundary
}

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn snapshot(&self) -> InputSnapshot {
        InputSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.selection_anchor = None;
    }

    pub fn set_text_with_cursor(&mut self, text: String, cursor: usize) {
        self.text = text;
        self.cursor = clamp_to_char_boundary(&self.text, cursor.min(self.text.len()));
        self.selection_anchor = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.cursor, s);
        self.cursor = self.cursor.saturating_add(s.len());
        self.selection_anchor = None;
    }

    pub fn insert_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.insert_str(s);
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
    }

    pub fn move_left(&mut self, selecting: bool) {
        if !selecting {
            if let Some((start, _end)) = self.selection_range() {
                self.cursor = start;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else {
            self.begin_selection();
        }

        self.cursor = prev_char_boundary(&self.text, self.cursor);
        self.clear_zero_width_selection();
    }

    pub fn move_right(&mut self, selecting: bool) {
        if !selecting {
            if let Some((_start, end)) = self.selection_range() {
                self.cursor = end;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else {
            self.begin_selection();
        }

        self.cursor = next_char_boundary(&self.text, self.cursor);
        self.clear_zero_width_selection();
    }

    pub fn move_word_left(&mut self, selecting: bool) {
        if !selecting {
            if let Some((start, _end)) = self.selection_range() {
                self.cursor = start;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else {
            self.begin_selection();
        }

        let mut i = self.cursor;
        // Skip whitespace
        while i > 0 {
            let prev = prev_char_boundary(&self.text, i);
            let ch = self.text[prev..i].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = prev;
        }
        // Skip non-whitespace
        while i > 0 {
            let prev = prev_char_boundary(&self.text, i);
            let ch = self.text[prev..i].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            i = prev;
        }

        self.cursor = i;
        self.clear_zero_width_selection();
    }

    pub fn move_word_right(&mut self, selecting: bool) {
        if !selecting {
            if let Some((_start, end)) = self.selection_range() {
                self.cursor = end;
                self.selection_anchor = None;
                return;
            }
            self.selection_anchor = None;
        } else {
            self.begin_selection();
        }

        let mut i = self.cursor;
        let len = self.text.len();
        // Skip whitespace
        while i < len {
            let next = next_char_boundary(&self.text, i);
            let ch = self.text[i..next].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = next;
        }
        // Skip non-whitespace
        while i < len {
            let next = next_char_boundary(&self.text, i);
            let ch = self.text[i..next].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            i = next;
        }

        self.cursor = i;
        self.clear_zero_width_selection();
    }

    pub fn move_to_start(&mut self, selecting: bool) {
        if selecting {
            self.begin_selection();
        } else if let Some((start, _end)) = self.selection_range() {
            self.cursor = start;
            self.selection_anchor = None;
            return;
        } else {
            self.selection_anchor = None;
        }

        self.cursor = 0;
        self.clear_zero_width_selection();
    }

    pub fn move_to_end(&mut self, selecting: bool) {
        if selecting {
            self.begin_selection();
        } else if let Some((_start, end)) = self.selection_range() {
            self.cursor = end;
            self.selection_anchor = None;
            return;
        } else {
            self.selection_anchor = None;
        }

        self.cursor = self.text.len();
        self.clear_zero_width_selection();
    }

    pub fn move_up_line(&mut self, selecting: bool) {
        if selecting {
            self.begin_selection();
        } else {
            self.selection_anchor = None;
        }

        let (line_start, _line_end) = current_line_bounds(&self.text, self.cursor);
        if line_start == 0 {
            self.clear_zero_width_selection();
            return;
        }

        let prev_line_end = line_start.saturating_sub(1);
        let prev_line_start = self.text[..prev_line_end]
            .rfind('\n')
            .map(|i| i.saturating_add(1))
            .unwrap_or(0);

        let col = self.text[line_start..self.cursor].chars().count();
        let prev_line = &self.text[prev_line_start..prev_line_end];
        let prev_len = prev_line.chars().count();
        let target_col = col.min(prev_len);
        let byte_off = byte_index_for_char_col(prev_line, target_col);
        self.cursor = prev_line_start.saturating_add(byte_off);

        self.clear_zero_width_selection();
    }

    pub fn move_down_line(&mut self, selecting: bool) {
        if selecting {
            self.begin_selection();
        } else {
            self.selection_anchor = None;
        }

        let (_line_start, line_end) = current_line_bounds(&self.text, self.cursor);
        if line_end >= self.text.len() {
            self.clear_zero_width_selection();
            return;
        }

        let next_line_start = line_end.saturating_add(1);
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map(|i| next_line_start.saturating_add(i))
            .unwrap_or(self.text.len());

        let (cur_line_start, _cur_line_end) = current_line_bounds(&self.text, self.cursor);
        let col = self.text[cur_line_start..self.cursor].chars().count();

        let next_line = &self.text[next_line_start..next_line_end];
        let next_len = next_line.chars().count();
        let target_col = col.min(next_len);
        let byte_off = byte_index_for_char_col(next_line, target_col);
        self.cursor = next_line_start.saturating_add(byte_off);

        self.clear_zero_width_selection();
    }

    fn begin_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    fn clear_zero_width_selection(&mut self) {
        if let Some(anchor) = self.selection_anchor {
            if anchor == self.cursor {
                self.selection_anchor = None;
            }
        }
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.selection_anchor = None;
        true
    }
}

#[derive(Debug, Default)]
pub struct PromptHistory {
    entries: Vec<String>,
    nav_index: usize, // 0..=entries.len(); entries.len() => scratch input
    scratch: InputSnapshot,
}

impl PromptHistory {
    pub fn new(entries: Vec<String>) -> Self {
        let nav_index = entries.len();
        Self {
            entries,
            nav_index,
            scratch: InputSnapshot::default(),
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_navigating(&self) -> bool {
        self.nav_index < self.entries.len()
    }

    pub fn reset_navigation(&mut self) {
        self.nav_index = self.entries.len();
        self.scratch = InputSnapshot::default();
    }

    pub fn cancel_navigation(&mut self, input: &mut InputBuffer) {
        if self.nav_index == self.entries.len() {
            return;
        }
        input.set_text_with_cursor(self.scratch.text.clone(), self.scratch.cursor);
        self.reset_navigation();
    }

    pub fn push(&mut self, entry: String) {
        if entry.trim().is_empty() {
            return;
        }
        if self.entries.last().is_some_and(|e| e == &entry) {
            self.nav_index = self.entries.len();
            return;
        }
        self.entries.push(entry);
        self.nav_index = self.entries.len();
    }

    pub fn prev(&mut self, input: &mut InputBuffer) {
        if self.entries.is_empty() {
            return;
        }

        if self.nav_index == self.entries.len() {
            self.scratch = input.snapshot();
        }

        if self.nav_index == 0 {
            return;
        }

        self.nav_index = self.nav_index.saturating_sub(1);
        if let Some(val) = self.entries.get(self.nav_index).cloned() {
            input.set_text(val);
        }
    }

    pub fn next(&mut self, input: &mut InputBuffer) {
        if self.entries.is_empty() {
            return;
        }

        if self.nav_index >= self.entries.len() {
            return;
        }

        self.nav_index = self.nav_index.saturating_add(1);
        if self.nav_index == self.entries.len() {
            input.set_text_with_cursor(self.scratch.text.clone(), self.scratch.cursor);
        } else if let Some(val) = self.entries.get(self.nav_index).cloned() {
            input.set_text(val);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReverseHistorySearch {
    query: String,
    original: InputSnapshot,
    next_start: isize,
}

impl ReverseHistorySearch {
    pub fn new(input: &InputBuffer, history_len: usize) -> Self {
        Self {
            query: String::new(),
            original: input.snapshot(),
            next_start: history_len as isize - 1,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cancel(&self, input: &mut InputBuffer) {
        input.set_text_with_cursor(self.original.text.clone(), self.original.cursor);
    }

    pub fn accept(&self) {}

    pub fn restart(&mut self, history_len: usize) {
        self.next_start = history_len as isize - 1;
    }

    pub fn push_char(&mut self, ch: char, history: &[String], input: &mut InputBuffer) {
        self.query.push(ch);
        self.restart(history.len());
        self.search_next(history, input);
    }

    pub fn push_str(&mut self, s: &str, history: &[String], input: &mut InputBuffer) {
        if s.is_empty() {
            return;
        }
        self.query.push_str(s);
        self.restart(history.len());
        self.search_next(history, input);
    }

    pub fn backspace(&mut self, history: &[String], input: &mut InputBuffer) {
        if self.query.pop().is_none() {
            return;
        }
        self.restart(history.len());
        self.search_next(history, input);
    }

    pub fn next_match(&mut self, history: &[String], input: &mut InputBuffer) {
        self.search_next(history, input);
    }

    pub fn search_next(&mut self, history: &[String], input: &mut InputBuffer) {
        if history.is_empty() {
            return;
        }

        if self.next_start < 0 {
            self.restart(history.len());
        }

        let mut i = self.next_start.min(history.len() as isize - 1);
        while i >= 0 {
            let idx = i as usize;
            let ent = &history[idx];
            if self.query.is_empty() || ent.contains(&self.query) {
                input.set_text(ent.clone());
                self.next_start = i - 1;
                return;
            }
            i -= 1;
        }

        // No match; keep the current input as-is, but restart so repeated Ctrl+R
        // doesn't get stuck.
        self.restart(history.len());
    }
}

fn clamp_to_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    idx
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i = i.saturating_sub(1);
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx.saturating_add(1);
    while i < s.len() && !s.is_char_boundary(i) {
        i = i.saturating_add(1);
    }
    i.min(s.len())
}

fn current_line_bounds(s: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(s.len());
    let before = &s[..cursor];
    let line_start = before
        .rfind('\n')
        .map(|i| i.saturating_add(1))
        .unwrap_or(0);

    let after = &s[cursor..];
    let line_end = after
        .find('\n')
        .map(|i| cursor.saturating_add(i))
        .unwrap_or(s.len());

    (line_start, line_end)
}

fn byte_index_for_char_col(s: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    s.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_inserts_at_cursor() {
        let mut buf = InputBuffer::new();
        buf.set_text("abc".to_string());
        buf.move_left(false);
        buf.insert_char('X');
        assert_eq!(buf.as_str(), "abXc");
        assert_eq!(buf.cursor(), "abX".len());
    }

    #[test]
    fn input_selection_delete() {
        let mut buf = InputBuffer::new();
        buf.set_text("abcd".to_string());
        buf.move_left(true); // select d
        buf.move_left(true); // select cd
        assert_eq!(buf.selection_range(), Some(("ab".len(), "abcd".len())));
        buf.backspace();
        assert_eq!(buf.as_str(), "ab");
        assert_eq!(buf.cursor(), "ab".len());
        assert!(!buf.has_selection());
    }

    #[test]
    fn input_multiline_alt_up_down_helpers() {
        let mut buf = InputBuffer::new();
        buf.set_text("ab\ncdef".to_string());
        buf.move_to_start(false);
        buf.move_down_line(false);
        assert_eq!(buf.cursor(), "ab\n".len()); // start of 2nd line
        buf.move_to_end(false);
        buf.move_up_line(false);
        assert!(buf.as_str()[..buf.cursor()].ends_with("ab"));
    }

    #[test]
    fn history_prev_next_restores_scratch() {
        let mut input = InputBuffer::new();
        input.set_text("scratch".to_string());
        let mut hist = PromptHistory::new(vec!["one".to_string(), "two".to_string()]);

        hist.prev(&mut input);
        assert_eq!(input.as_str(), "two");

        hist.prev(&mut input);
        assert_eq!(input.as_str(), "one");

        hist.next(&mut input);
        assert_eq!(input.as_str(), "two");

        hist.next(&mut input);
        assert_eq!(input.as_str(), "scratch");
    }

    #[test]
    fn reverse_search_cycles_and_filters() {
        let mut input = InputBuffer::new();
        input.set_text("scratch".to_string());
        let hist = vec![
            "hello".to_string(),
            "world".to_string(),
            "hello there".to_string(),
        ];

        let mut rs = ReverseHistorySearch::new(&input, hist.len());
        rs.search_next(&hist, &mut input);
        assert_eq!(input.as_str(), "hello there");

        rs.next_match(&hist, &mut input);
        assert_eq!(input.as_str(), "world");

        rs.push_char('h', &hist, &mut input);
        assert_eq!(input.as_str(), "hello there");
    }
}
