use super::input::InputBuffer;

const MAX_VIM_COUNT: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextObjectScope {
    Inner,
    Around,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimHandleResult {
    Handled,
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalState {
    Idle,
    Count(String),
    Operator {
        op: Operator,
        count: u32,
    },
    OperatorCount {
        op: Operator,
        count: u32,
        digits: String,
    },
    OperatorTextObject {
        op: Operator,
        count: u32,
        scope: TextObjectScope,
    },
    Replace {
        count: u32,
    },
    G {
        count: u32,
    },
    OperatorG {
        op: Operator,
        count: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InsertReplay {
    InsertBefore,
    InsertAfter,
    InsertLineStart,
    InsertLineEnd,
    OpenLineAbove,
    OpenLineBelow,
    ChangeMotion {
        motion: Motion,
        count: u32,
    },
    ChangeTextObject {
        object: char,
        scope: TextObjectScope,
        count: u32,
    },
    ChangeLine {
        count: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedChange {
    Insert {
        replay: InsertReplay,
        text: String,
    },
    DeleteMotion {
        motion: Motion,
        count: u32,
    },
    DeleteTextObject {
        object: char,
        scope: TextObjectScope,
        count: u32,
    },
    DeleteLine {
        count: u32,
    },
    X {
        count: u32,
    },
    Replace {
        ch: char,
        count: u32,
    },
    Paste {
        after: bool,
        count: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InsertSession {
    typed: String,
    replay: Option<InsertReplay>,
}

impl Default for InsertSession {
    fn default() -> Self {
        Self {
            typed: String::new(),
            replay: Some(InsertReplay::InsertBefore),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VimMachine {
    mode: VimMode,
    state: NormalState,
    register: String,
    register_linewise: bool,
    last_change: Option<RecordedChange>,
    insert_session: InsertSession,
}

impl Default for VimMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl VimMachine {
    pub fn new() -> Self {
        Self {
            mode: VimMode::Insert,
            state: NormalState::Idle,
            register: String::new(),
            register_linewise: false,
            last_change: None,
            insert_session: InsertSession::default(),
        }
    }

    pub fn mode(&self) -> VimMode {
        self.mode
    }

    pub fn is_insert(&self) -> bool {
        self.mode == VimMode::Insert
    }

    pub fn is_normal(&self) -> bool {
        self.mode == VimMode::Normal
    }

    pub fn reset_insert_tracking(&mut self) {
        if self.is_insert() {
            self.insert_session.typed.clear();
            self.insert_session.replay = Some(InsertReplay::InsertBefore);
        }
    }

    pub fn on_insert_text(&mut self, text: &str) {
        if self.is_insert() {
            self.insert_session.typed.push_str(text);
        }
    }

    pub fn on_insert_backspace(&mut self) {
        if !self.is_insert() {
            return;
        }
        self.insert_session.typed.pop();
    }

    pub fn enter_normal_mode(&mut self, buffer: &mut InputBuffer) {
        if !self.is_insert() {
            self.state = NormalState::Idle;
            return;
        }

        if let Some(replay) = self.insert_session.replay.take() {
            let should_record =
                !self.insert_session.typed.is_empty() || replay.is_meaningful_with_empty_text();
            if should_record {
                self.last_change = Some(RecordedChange::Insert {
                    replay,
                    text: self.insert_session.typed.clone(),
                });
            }
        }

        self.insert_session.typed.clear();
        self.mode = VimMode::Normal;
        self.state = NormalState::Idle;
        buffer.clear_selection();

        let cursor = buffer.cursor();
        if cursor > 0 {
            let prev = prev_char_boundary(buffer.as_str(), cursor);
            if prev < buffer.len() && buffer.as_str()[prev..].chars().next() != Some('\n') {
                buffer.set_cursor(prev);
            }
        }
    }

    pub fn cancel_pending(&mut self) {
        self.state = NormalState::Idle;
    }

    pub fn handle_normal_key(&mut self, key: VimKey, buffer: &mut InputBuffer) -> VimHandleResult {
        match key {
            VimKey::Esc => {
                self.state = NormalState::Idle;
                VimHandleResult::Handled
            }
            VimKey::Enter => VimHandleResult::Submit,
            VimKey::Backspace => {
                self.process_input('h', buffer);
                VimHandleResult::Handled
            }
            VimKey::Delete => {
                self.process_input('x', buffer);
                VimHandleResult::Handled
            }
            VimKey::Left => {
                self.process_input('h', buffer);
                VimHandleResult::Handled
            }
            VimKey::Right => {
                self.process_input('l', buffer);
                VimHandleResult::Handled
            }
            VimKey::Up => {
                self.process_input('k', buffer);
                VimHandleResult::Handled
            }
            VimKey::Down => {
                self.process_input('j', buffer);
                VimHandleResult::Handled
            }
            VimKey::Char(ch) => {
                self.process_input(ch, buffer);
                VimHandleResult::Handled
            }
        }
    }

    fn process_input(&mut self, input: char, buffer: &mut InputBuffer) {
        let state = self.state.clone();
        match state {
            NormalState::Idle => self.from_idle(input, buffer),
            NormalState::Count(digits) => self.from_count(digits, input, buffer),
            NormalState::Operator { op, count } => self.from_operator(op, count, input, buffer),
            NormalState::OperatorCount { op, count, digits } => {
                self.from_operator_count(op, count, digits, input, buffer)
            }
            NormalState::OperatorTextObject { op, count, scope } => {
                self.from_operator_text_object(op, count, scope, input, buffer)
            }
            NormalState::Replace { count } => {
                self.execute_replace(input, count, buffer);
                self.state = NormalState::Idle;
            }
            NormalState::G { count } => self.from_g(count, input, buffer),
            NormalState::OperatorG { op, count } => self.from_operator_g(op, count, input, buffer),
        }
    }

    fn from_idle(&mut self, input: char, buffer: &mut InputBuffer) {
        if let Some(digit) = parse_count_start(input) {
            self.state = NormalState::Count(digit.to_string());
            return;
        }
        if input == '0' {
            self.execute_motion(Motion::LineStart, 1, buffer);
            return;
        }
        self.handle_normal_input(input, 1, buffer);
    }

    fn from_count(&mut self, digits: String, input: char, buffer: &mut InputBuffer) {
        if input.is_ascii_digit() {
            let new_digits = format!("{digits}{input}");
            let parsed = parse_count(&new_digits);
            self.state = NormalState::Count(parsed.to_string());
            return;
        }

        let count = digits.parse::<u32>().unwrap_or(1).min(MAX_VIM_COUNT).max(1);
        if !self.handle_normal_input(input, count, buffer) {
            self.state = NormalState::Idle;
        }
    }

    fn from_operator(&mut self, op: Operator, count: u32, input: char, buffer: &mut InputBuffer) {
        if input == operator_char(op) {
            self.execute_line_op(op, count, buffer);
            self.state = NormalState::Idle;
            return;
        }

        if input.is_ascii_digit() {
            self.state = NormalState::OperatorCount {
                op,
                count,
                digits: input.to_string(),
            };
            return;
        }

        if !self.handle_operator_input(op, count, input, buffer) {
            self.state = NormalState::Idle;
        }
    }

    fn from_operator_count(
        &mut self,
        op: Operator,
        count: u32,
        digits: String,
        input: char,
        buffer: &mut InputBuffer,
    ) {
        if input.is_ascii_digit() {
            let new_digits = format!("{digits}{input}");
            let parsed = parse_count(&new_digits);
            self.state = NormalState::OperatorCount {
                op,
                count,
                digits: parsed.to_string(),
            };
            return;
        }

        let motion_count = digits.parse::<u32>().unwrap_or(1).min(MAX_VIM_COUNT).max(1);
        let effective_count = count.saturating_mul(motion_count).min(MAX_VIM_COUNT).max(1);
        if !self.handle_operator_input(op, effective_count, input, buffer) {
            self.state = NormalState::Idle;
        }
    }

    fn from_operator_text_object(
        &mut self,
        op: Operator,
        count: u32,
        scope: TextObjectScope,
        input: char,
        buffer: &mut InputBuffer,
    ) {
        if !is_supported_text_object(input) {
            self.state = NormalState::Idle;
            return;
        }
        self.execute_operator_text_object(op, input, scope, count, buffer);
        self.state = NormalState::Idle;
    }

    fn from_g(&mut self, count: u32, input: char, buffer: &mut InputBuffer) {
        if input == 'g' {
            let target = if count > 1 {
                line_start_for_number(buffer.as_str(), count)
            } else {
                0
            };
            buffer.set_cursor(target);
        }
        self.state = NormalState::Idle;
    }

    fn from_operator_g(&mut self, op: Operator, count: u32, input: char, buffer: &mut InputBuffer) {
        if input == 'g' {
            self.execute_operator_motion(op, Motion::Top, count, buffer);
        }
        self.state = NormalState::Idle;
    }

    fn handle_normal_input(&mut self, input: char, count: u32, buffer: &mut InputBuffer) -> bool {
        if let Some(op) = operator_from_char(input) {
            self.state = NormalState::Operator { op, count };
            return true;
        }

        if let Some(motion) = motion_from_char(input) {
            self.execute_motion(motion, count, buffer);
            self.state = NormalState::Idle;
            return true;
        }

        match input {
            'x' => {
                self.execute_x(count, buffer);
                self.state = NormalState::Idle;
                true
            }
            'r' => {
                self.state = NormalState::Replace { count };
                true
            }
            'p' => {
                self.execute_paste(true, count, buffer);
                self.state = NormalState::Idle;
                true
            }
            'P' => {
                self.execute_paste(false, count, buffer);
                self.state = NormalState::Idle;
                true
            }
            'i' => {
                self.begin_insert(buffer, InsertReplay::InsertBefore);
                true
            }
            'a' => {
                let cursor = advance_for_append(buffer.as_str(), buffer.cursor());
                buffer.set_cursor(cursor);
                self.begin_insert(buffer, InsertReplay::InsertAfter);
                true
            }
            'I' => {
                let target = first_non_blank(buffer.as_str(), buffer.cursor());
                buffer.set_cursor(target);
                self.begin_insert(buffer, InsertReplay::InsertLineStart);
                true
            }
            'A' => {
                let target = line_end_exclusive(buffer.as_str(), buffer.cursor());
                buffer.set_cursor(target);
                self.begin_insert(buffer, InsertReplay::InsertLineEnd);
                true
            }
            'o' => {
                self.open_line_below(buffer);
                self.begin_insert(buffer, InsertReplay::OpenLineBelow);
                true
            }
            'O' => {
                self.open_line_above(buffer);
                self.begin_insert(buffer, InsertReplay::OpenLineAbove);
                true
            }
            'g' => {
                self.state = NormalState::G { count };
                true
            }
            'G' => {
                let target = if count > 1 {
                    line_start_for_number(buffer.as_str(), count)
                } else {
                    start_of_last_line(buffer.as_str())
                };
                buffer.set_cursor(target);
                self.state = NormalState::Idle;
                true
            }
            '.' => {
                self.repeat_last_change(count, buffer);
                self.state = NormalState::Idle;
                true
            }
            _ => false,
        }
    }

    fn handle_operator_input(
        &mut self,
        op: Operator,
        count: u32,
        input: char,
        buffer: &mut InputBuffer,
    ) -> bool {
        if let Some(scope) = text_object_scope_from_char(input) {
            self.state = NormalState::OperatorTextObject { op, count, scope };
            return true;
        }

        if let Some(motion) = motion_from_char(input) {
            self.execute_operator_motion(op, motion, count, buffer);
            self.state = NormalState::Idle;
            return true;
        }

        match input {
            'g' => {
                self.state = NormalState::OperatorG { op, count };
                true
            }
            'G' => {
                self.execute_operator_motion(op, Motion::Bottom, count, buffer);
                self.state = NormalState::Idle;
                true
            }
            _ => false,
        }
    }

    fn begin_insert(&mut self, buffer: &mut InputBuffer, replay: InsertReplay) {
        self.mode = VimMode::Insert;
        self.state = NormalState::Idle;
        buffer.clear_selection();
        self.insert_session = InsertSession {
            typed: String::new(),
            replay: Some(replay),
        };
    }

    fn execute_motion(&mut self, motion: Motion, count: u32, buffer: &mut InputBuffer) {
        let new_cursor = apply_motion(buffer.as_str(), buffer.cursor(), motion, count);
        buffer.set_cursor(new_cursor);
    }

    fn execute_x(&mut self, count: u32, buffer: &mut InputBuffer) {
        let text = buffer.as_str();
        let start = buffer.cursor();
        let end = advance_by_chars(text, start, count as usize);
        if start == end {
            return;
        }
        self.set_register(text[start..end].to_string(), false);
        buffer.delete_range(start, end);
        self.normalize_normal_cursor(buffer);
        self.last_change = Some(RecordedChange::X { count });
    }

    fn execute_replace(&mut self, ch: char, count: u32, buffer: &mut InputBuffer) {
        if ch == '\0' {
            return;
        }
        let mut text = buffer.as_str().to_string();
        let start = buffer.cursor();
        if start >= text.len() {
            return;
        }
        let mut pos = start;
        let mut replaced = 0u32;
        while pos < text.len() && replaced < count {
            let next = next_char_boundary(&text, pos);
            text.replace_range(pos..next, &ch.to_string());
            pos = pos.saturating_add(ch.len_utf8());
            replaced = replaced.saturating_add(1);
        }
        let new_cursor = if pos == 0 {
            0
        } else {
            prev_char_boundary(&text, pos.min(text.len()))
        };
        buffer.set_text_with_cursor(text, new_cursor);
        self.normalize_normal_cursor(buffer);
        self.last_change = Some(RecordedChange::Replace { ch, count });
    }

    fn execute_paste(&mut self, after: bool, count: u32, buffer: &mut InputBuffer) {
        if self.register.is_empty() {
            return;
        }
        let text = buffer.as_str();
        if self.register_linewise {
            let lines = self
                .register
                .trim_end_matches('\n')
                .split('\n')
                .collect::<Vec<_>>();
            if lines.is_empty() {
                return;
            }
            let repeated = std::iter::repeat(lines.clone())
                .take(count as usize)
                .flatten()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let current_line = line_number_at(text, buffer.cursor());
            let insert_line = if after {
                current_line + 1
            } else {
                current_line
            };
            let mut all_lines = split_lines_preserve_empty(text);
            let idx = insert_line.min(all_lines.len());
            for (offset, line) in repeated.into_iter().enumerate() {
                all_lines.insert(idx + offset, line);
            }
            let new_text = all_lines.join("\n");
            let new_cursor = line_start_for_number(&new_text, insert_line as u32 + 1);
            buffer.set_text_with_cursor(new_text, new_cursor);
        } else {
            let insert_at = if after {
                advance_for_append(text, buffer.cursor())
            } else {
                buffer.cursor()
            };
            let insert_text = self.register.repeat(count as usize);
            let mut new_text = String::with_capacity(text.len() + insert_text.len());
            new_text.push_str(&text[..insert_at]);
            new_text.push_str(&insert_text);
            new_text.push_str(&text[insert_at..]);
            let new_cursor = if insert_text.is_empty() {
                insert_at
            } else {
                insert_at + insert_text.len() - last_char_len(&insert_text)
            };
            buffer.set_text_with_cursor(new_text, new_cursor);
        }
        self.normalize_normal_cursor(buffer);
        self.last_change = Some(RecordedChange::Paste { after, count });
    }

    fn execute_line_op(&mut self, op: Operator, count: u32, buffer: &mut InputBuffer) {
        let text = buffer.as_str().to_string();
        let line_start = line_start(text.as_str(), buffer.cursor());
        let mut line_end = line_start;
        let total_lines = split_lines_preserve_empty(&text).len();
        let current_line = line_number_at(&text, buffer.cursor());
        let lines_to_affect = count
            .min(total_lines.saturating_sub(current_line) as u32)
            .max(1);

        for _ in 0..lines_to_affect {
            let end = line_end_exclusive(&text, line_end);
            line_end = if end < text.len() { end + 1 } else { end };
        }

        let mut content = text[line_start..line_end].to_string();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        self.set_register(content, true);

        match op {
            Operator::Yank => {}
            Operator::Delete => {
                let mut delete_start = line_start;
                let delete_end = line_end;
                if delete_end == text.len()
                    && delete_start > 0
                    && text.as_bytes()[delete_start - 1] == b'\n'
                {
                    delete_start -= 1;
                }
                let mut new_text = String::new();
                new_text.push_str(&text[..delete_start]);
                new_text.push_str(&text[delete_end..]);
                buffer.set_text_with_cursor(new_text, delete_start.min(buffer.len()));
                self.normalize_normal_cursor(buffer);
                self.last_change = Some(RecordedChange::DeleteLine { count });
            }
            Operator::Change => {
                let lines = split_lines_preserve_empty(&text);
                if lines.len() == 1 {
                    buffer.set_text_with_cursor(String::new(), 0);
                } else {
                    let current = current_line;
                    let before = &lines[..current];
                    let after = &lines[current + lines_to_affect as usize..];
                    let mut combined = Vec::with_capacity(before.len() + 1 + after.len());
                    combined.extend(before.iter().cloned());
                    combined.push(String::new());
                    combined.extend(after.iter().cloned());
                    buffer.set_text_with_cursor(combined.join("\n"), line_start);
                }
                self.begin_insert(buffer, InsertReplay::ChangeLine { count });
            }
        }
    }

    fn execute_operator_motion(
        &mut self,
        op: Operator,
        motion: Motion,
        count: u32,
        buffer: &mut InputBuffer,
    ) {
        let text = buffer.as_str().to_string();
        let cursor = buffer.cursor();
        let Some(range) = operator_motion_range(&text, cursor, motion, count, op) else {
            return;
        };
        match op {
            Operator::Delete => {
                self.apply_delete_range(buffer, &text, range.start, range.end, range.linewise);
                self.last_change = Some(RecordedChange::DeleteMotion { motion, count });
            }
            Operator::Yank => {
                self.apply_yank_range(buffer, &text, range.start, range.end, range.linewise);
            }
            Operator::Change => {
                self.apply_change_range(
                    buffer,
                    &text,
                    range.start,
                    range.end,
                    range.linewise,
                    InsertReplay::ChangeMotion { motion, count },
                );
            }
        }
    }

    fn execute_operator_text_object(
        &mut self,
        op: Operator,
        object: char,
        scope: TextObjectScope,
        count: u32,
        buffer: &mut InputBuffer,
    ) {
        let text = buffer.as_str().to_string();
        let cursor = buffer.cursor();
        let Some((start, end)) = find_text_object(&text, cursor, object, scope) else {
            return;
        };
        match op {
            Operator::Delete => {
                self.apply_delete_range(buffer, &text, start, end, false);
                self.last_change = Some(RecordedChange::DeleteTextObject {
                    object,
                    scope,
                    count,
                });
            }
            Operator::Yank => {
                self.apply_yank_range(buffer, &text, start, end, false);
            }
            Operator::Change => {
                self.apply_change_range(
                    buffer,
                    &text,
                    start,
                    end,
                    false,
                    InsertReplay::ChangeTextObject {
                        object,
                        scope,
                        count,
                    },
                );
            }
        }
    }

    fn apply_delete_range(
        &mut self,
        buffer: &mut InputBuffer,
        text: &str,
        start: usize,
        end: usize,
        linewise: bool,
    ) {
        let mut content = text[start..end].to_string();
        if linewise && !content.ends_with('\n') {
            content.push('\n');
        }
        self.set_register(content, linewise);
        buffer.delete_range(start, end);
        self.normalize_normal_cursor(buffer);
    }

    fn apply_yank_range(
        &mut self,
        buffer: &mut InputBuffer,
        text: &str,
        start: usize,
        end: usize,
        linewise: bool,
    ) {
        let mut content = text[start..end].to_string();
        if linewise && !content.ends_with('\n') {
            content.push('\n');
        }
        self.set_register(content, linewise);
        buffer.set_cursor(start);
        self.normalize_normal_cursor(buffer);
    }

    fn apply_change_range(
        &mut self,
        buffer: &mut InputBuffer,
        text: &str,
        start: usize,
        end: usize,
        linewise: bool,
        replay: InsertReplay,
    ) {
        let mut content = text[start..end].to_string();
        if linewise && !content.ends_with('\n') {
            content.push('\n');
        }
        self.set_register(content, linewise);
        buffer.delete_range(start, end);
        self.begin_insert(buffer, replay);
    }

    fn set_register(&mut self, content: String, linewise: bool) {
        self.register = content;
        self.register_linewise = linewise;
    }

    fn normalize_normal_cursor(&self, buffer: &mut InputBuffer) {
        if self.mode != VimMode::Normal {
            return;
        }
        if buffer.is_empty() {
            buffer.set_cursor(0);
            return;
        }
        let cursor = buffer.cursor();
        if cursor >= buffer.len() {
            let prev = prev_char_boundary(buffer.as_str(), buffer.len());
            buffer.set_cursor(prev);
        }
    }

    fn repeat_last_change(&mut self, count: u32, buffer: &mut InputBuffer) {
        let Some(change) = self.last_change.clone() else {
            return;
        };
        for _ in 0..count.max(1) {
            self.replay_change(&change, buffer);
        }
    }

    fn replay_change(&mut self, change: &RecordedChange, buffer: &mut InputBuffer) {
        match change {
            RecordedChange::Insert { replay, text } => self.replay_insert(replay, text, buffer),
            RecordedChange::DeleteMotion { motion, count } => {
                self.execute_operator_motion(Operator::Delete, *motion, *count, buffer);
            }
            RecordedChange::DeleteTextObject {
                object,
                scope,
                count,
            } => {
                self.execute_operator_text_object(
                    Operator::Delete,
                    *object,
                    *scope,
                    *count,
                    buffer,
                );
            }
            RecordedChange::DeleteLine { count } => {
                self.execute_line_op(Operator::Delete, *count, buffer)
            }
            RecordedChange::X { count } => self.execute_x(*count, buffer),
            RecordedChange::Replace { ch, count } => self.execute_replace(*ch, *count, buffer),
            RecordedChange::Paste { after, count } => self.execute_paste(*after, *count, buffer),
        }
    }

    fn replay_insert(&mut self, replay: &InsertReplay, text: &str, buffer: &mut InputBuffer) {
        match replay {
            InsertReplay::InsertBefore => insert_text_at(buffer, buffer.cursor(), text),
            InsertReplay::InsertAfter => {
                let at = advance_for_append(buffer.as_str(), buffer.cursor());
                insert_text_at(buffer, at, text);
            }
            InsertReplay::InsertLineStart => {
                let at = first_non_blank(buffer.as_str(), buffer.cursor());
                insert_text_at(buffer, at, text);
            }
            InsertReplay::InsertLineEnd => {
                let at = line_end_exclusive(buffer.as_str(), buffer.cursor());
                insert_text_at(buffer, at, text);
            }
            InsertReplay::OpenLineAbove => {
                let at = line_start(buffer.as_str(), buffer.cursor());
                let prefix = if at == 0 {
                    String::new()
                } else {
                    "\n".to_string()
                };
                let mut insert = prefix;
                insert.push_str(text);
                insert_text_at(buffer, at, &insert);
                if !text.is_empty() {
                    let cursor = at + insert.len() - last_char_len(text);
                    buffer.set_cursor(cursor);
                } else {
                    buffer.set_cursor(at);
                }
            }
            InsertReplay::OpenLineBelow => {
                let line_end = line_end_exclusive(buffer.as_str(), buffer.cursor());
                let at = line_end;
                let mut insert = String::from("\n");
                insert.push_str(text);
                insert_text_at(buffer, at, &insert);
                if !text.is_empty() {
                    let cursor = at + insert.len() - last_char_len(text);
                    buffer.set_cursor(cursor);
                } else {
                    buffer.set_cursor(at + 1);
                }
            }
            InsertReplay::ChangeMotion { motion, count } => {
                let text_snapshot = buffer.as_str().to_string();
                let cursor = buffer.cursor();
                if let Some(range) =
                    operator_motion_range(&text_snapshot, cursor, *motion, *count, Operator::Change)
                {
                    self.set_register(
                        text_snapshot[range.start..range.end].to_string(),
                        range.linewise,
                    );
                    buffer.delete_range(range.start, range.end);
                    insert_text_at(buffer, range.start, text);
                }
            }
            InsertReplay::ChangeTextObject {
                object,
                scope,
                count: _,
            } => {
                let text_snapshot = buffer.as_str().to_string();
                let cursor = buffer.cursor();
                if let Some((start, end)) =
                    find_text_object(&text_snapshot, cursor, *object, *scope)
                {
                    self.set_register(text_snapshot[start..end].to_string(), false);
                    buffer.delete_range(start, end);
                    insert_text_at(buffer, start, text);
                }
            }
            InsertReplay::ChangeLine { count } => {
                let text_snapshot = buffer.as_str().to_string();
                let cursor = buffer.cursor();
                let line_start = line_start(&text_snapshot, cursor);
                let current_line = line_number_at(&text_snapshot, cursor);
                let lines = split_lines_preserve_empty(&text_snapshot);
                let lines_to_affect = (*count)
                    .min(lines.len().saturating_sub(current_line) as u32)
                    .max(1);
                let before = &lines[..current_line];
                let after = &lines[current_line + lines_to_affect as usize..];
                let mut combined = Vec::with_capacity(before.len() + 1 + after.len());
                combined.extend(before.iter().cloned());
                combined.push(text.to_string());
                combined.extend(after.iter().cloned());
                buffer.set_text_with_cursor(
                    combined.join("\n"),
                    if text.is_empty() {
                        line_start
                    } else {
                        line_start + text.len() - last_char_len(text)
                    },
                );
            }
        }
        self.normalize_normal_cursor(buffer);
    }

    fn open_line_below(&mut self, buffer: &mut InputBuffer) {
        let text = buffer.as_str().to_string();
        let line_end = line_end_exclusive(&text, buffer.cursor());
        let mut new_text = String::new();
        new_text.push_str(&text[..line_end]);
        new_text.push('\n');
        new_text.push_str(&text[line_end..]);
        buffer.set_text_with_cursor(new_text, line_end + 1);
    }

    fn open_line_above(&mut self, buffer: &mut InputBuffer) {
        let text = buffer.as_str().to_string();
        let line_start = line_start(&text, buffer.cursor());
        let mut new_text = String::new();
        if line_start > 0 {
            new_text.push_str(&text[..line_start]);
            new_text.push('\n');
            new_text.push_str(&text[line_start..]);
            buffer.set_text_with_cursor(new_text, line_start);
        } else {
            new_text.push('\n');
            new_text.push_str(&text);
            buffer.set_text_with_cursor(new_text, 0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimKey {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Backspace,
    Delete,
    Enter,
    Esc,
}

#[derive(Debug, Clone, Copy)]
struct OperatorRange {
    start: usize,
    end: usize,
    linewise: bool,
}

impl InsertReplay {
    fn is_meaningful_with_empty_text(&self) -> bool {
        matches!(
            self,
            InsertReplay::OpenLineAbove
                | InsertReplay::OpenLineBelow
                | InsertReplay::ChangeMotion { .. }
                | InsertReplay::ChangeTextObject { .. }
                | InsertReplay::ChangeLine { .. }
        )
    }
}

fn parse_count_start(input: char) -> Option<u32> {
    input.to_digit(10).filter(|digit| *digit > 0)
}

fn parse_count(digits: &str) -> u32 {
    digits.parse::<u32>().unwrap_or(1).min(MAX_VIM_COUNT).max(1)
}

fn operator_char(op: Operator) -> char {
    match op {
        Operator::Delete => 'd',
        Operator::Change => 'c',
        Operator::Yank => 'y',
    }
}

fn operator_from_char(input: char) -> Option<Operator> {
    match input {
        'd' => Some(Operator::Delete),
        'c' => Some(Operator::Change),
        'y' => Some(Operator::Yank),
        _ => None,
    }
}

fn motion_from_char(input: char) -> Option<Motion> {
    match input {
        'h' => Some(Motion::Left),
        'l' => Some(Motion::Right),
        'j' => Some(Motion::Down),
        'k' => Some(Motion::Up),
        'w' => Some(Motion::WordForward),
        'b' => Some(Motion::WordBackward),
        'e' => Some(Motion::WordEnd),
        '0' => Some(Motion::LineStart),
        '^' => Some(Motion::FirstNonBlank),
        '$' => Some(Motion::LineEnd),
        _ => None,
    }
}

fn text_object_scope_from_char(input: char) -> Option<TextObjectScope> {
    match input {
        'i' => Some(TextObjectScope::Inner),
        'a' => Some(TextObjectScope::Around),
        _ => None,
    }
}

fn is_supported_text_object(input: char) -> bool {
    matches!(
        input,
        'w' | 'W' | '"' | '\'' | '`' | '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>'
    )
}

fn apply_motion(text: &str, cursor: usize, motion: Motion, count: u32) -> usize {
    let mut pos = cursor.min(text.len());
    for _ in 0..count.max(1) {
        let next = match motion {
            Motion::Left => prev_char_boundary(text, pos),
            Motion::Right => advance_for_append(text, pos),
            Motion::Up => move_up_line(text, pos),
            Motion::Down => move_down_line(text, pos),
            Motion::WordForward => next_vim_word(text, pos),
            Motion::WordBackward => prev_vim_word(text, pos),
            Motion::WordEnd => end_of_vim_word(text, pos),
            Motion::LineStart => line_start(text, pos),
            Motion::FirstNonBlank => first_non_blank(text, pos),
            Motion::LineEnd => line_end_cursor(text, pos),
            Motion::Top => 0,
            Motion::Bottom => start_of_last_line(text),
        };
        if next == pos {
            break;
        }
        pos = next;
    }
    pos
}

fn operator_motion_range(
    text: &str,
    cursor: usize,
    motion: Motion,
    count: u32,
    op: Operator,
) -> Option<OperatorRange> {
    let target = apply_motion(text, cursor, motion, count);
    if target == cursor
        && !matches!(
            motion,
            Motion::LineEnd | Motion::LineStart | Motion::FirstNonBlank
        )
    {
        return None;
    }

    let mut start = cursor.min(target);
    let mut end = cursor.max(target);
    let mut linewise = false;

    if op == Operator::Change && motion == Motion::WordForward {
        let word_end = change_word_end(text, cursor, count);
        end = word_end;
    } else if matches!(
        motion,
        Motion::Up | Motion::Down | Motion::Top | Motion::Bottom
    ) {
        linewise = true;
        start = line_start(text, start);
        end = line_end_exclusive(text, end);
        if end < text.len() {
            end += 1;
        } else if start > 0 && text.as_bytes()[start - 1] == b'\n' {
            start -= 1;
        }
    } else if matches!(motion, Motion::WordEnd | Motion::LineEnd) && cursor <= target {
        end = next_char_boundary(text, end);
    }

    if start == end {
        return None;
    }

    Some(OperatorRange {
        start,
        end,
        linewise,
    })
}

fn change_word_end(text: &str, cursor: usize, count: u32) -> usize {
    let mut pos = cursor;
    for _ in 1..count {
        pos = next_vim_word(text, pos);
    }
    let end = end_of_vim_word(text, pos);
    next_char_boundary(text, end)
}

fn find_text_object(
    text: &str,
    cursor: usize,
    object: char,
    scope: TextObjectScope,
) -> Option<(usize, usize)> {
    match object {
        'w' => find_word_text_object(text, cursor, scope, false),
        'W' => find_word_text_object(text, cursor, scope, true),
        '"' | '\'' | '`' => find_quote_text_object(text, cursor, object, scope),
        '(' | ')' | 'b' => find_bracket_text_object(text, cursor, '(', ')', scope),
        '[' | ']' => find_bracket_text_object(text, cursor, '[', ']', scope),
        '{' | '}' | 'B' => find_bracket_text_object(text, cursor, '{', '}', scope),
        '<' | '>' => find_bracket_text_object(text, cursor, '<', '>', scope),
        _ => None,
    }
}

fn find_word_text_object(
    text: &str,
    cursor: usize,
    scope: TextObjectScope,
    big_word: bool,
) -> Option<(usize, usize)> {
    if text.is_empty() {
        return None;
    }
    let mut pos = cursor.min(text.len().saturating_sub(1));
    if pos == text.len() && !text.is_empty() {
        pos = prev_char_boundary(text, pos);
    }

    let classify = |ch: char| {
        if ch.is_whitespace() {
            0
        } else if big_word {
            1
        } else if is_vim_word_char(ch) {
            1
        } else {
            2
        }
    };

    let current = char_at(text, pos)?;
    let class = classify(current);
    let mut start = pos;
    let mut end = next_char_boundary(text, pos);

    while start > 0 {
        let prev = prev_char_boundary(text, start);
        let ch = char_at(text, prev)?;
        if classify(ch) != class {
            break;
        }
        start = prev;
    }

    while end < text.len() {
        let ch = char_at(text, end)?;
        if classify(ch) != class {
            break;
        }
        end = next_char_boundary(text, end);
    }

    if scope == TextObjectScope::Around && class != 0 {
        let mut extended = end;
        while extended < text.len() {
            let ch = char_at(text, extended)?;
            if !ch.is_whitespace() {
                break;
            }
            extended = next_char_boundary(text, extended);
        }
        if extended == end {
            while start > 0 {
                let prev = prev_char_boundary(text, start);
                let ch = char_at(text, prev)?;
                if !ch.is_whitespace() {
                    break;
                }
                start = prev;
            }
        } else {
            end = extended;
        }
    }

    Some((start, end))
}

fn find_quote_text_object(
    text: &str,
    cursor: usize,
    quote: char,
    scope: TextObjectScope,
) -> Option<(usize, usize)> {
    let start = line_start(text, cursor);
    let end = line_end_exclusive(text, cursor);
    let line = &text[start..end];
    let pos_in_line = cursor.saturating_sub(start);

    let positions = line
        .char_indices()
        .filter_map(|(idx, ch)| if ch == quote { Some(idx) } else { None })
        .collect::<Vec<_>>();

    let mut i = 0;
    while i + 1 < positions.len() {
        let left = positions[i];
        let right = positions[i + 1];
        if left <= pos_in_line && pos_in_line <= right {
            return Some(match scope {
                TextObjectScope::Inner => (start + left + quote.len_utf8(), start + right),
                TextObjectScope::Around => (start + left, start + right + quote.len_utf8()),
            });
        }
        i += 2;
    }

    None
}

fn find_bracket_text_object(
    text: &str,
    cursor: usize,
    open: char,
    close: char,
    scope: TextObjectScope,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let open_b = open as u8;
    let close_b = close as u8;
    let mut depth = 0usize;
    let mut start = None;
    let mut idx = cursor.min(text.len().saturating_sub(1));
    loop {
        let b = *bytes.get(idx)?;
        if b == close_b && idx != cursor {
            depth += 1;
        } else if b == open_b {
            if depth == 0 {
                start = Some(idx);
                break;
            }
            depth -= 1;
        }
        if idx == 0 {
            break;
        }
        idx -= 1;
    }
    let start = start?;

    depth = 0;
    let mut end = None;
    for (idx, b) in bytes.iter().enumerate().skip(start + 1) {
        if *b == open_b {
            depth += 1;
        } else if *b == close_b {
            if depth == 0 {
                end = Some(idx);
                break;
            }
            depth -= 1;
        }
    }
    let end = end?;

    Some(match scope {
        TextObjectScope::Inner => (start + 1, end),
        TextObjectScope::Around => (start, end + 1),
    })
}

fn insert_text_at(buffer: &mut InputBuffer, at: usize, text: &str) {
    let current = buffer.as_str().to_string();
    let mut new_text = String::with_capacity(current.len() + text.len());
    new_text.push_str(&current[..at]);
    new_text.push_str(text);
    new_text.push_str(&current[at..]);
    let new_cursor = if text.is_empty() {
        at
    } else {
        at + text.len() - last_char_len(text)
    };
    buffer.set_text_with_cursor(new_text, new_cursor);
}

fn last_char_len(text: &str) -> usize {
    text.chars().last().map(char::len_utf8).unwrap_or(0)
}

fn char_at(text: &str, idx: usize) -> Option<char> {
    text.get(idx..)?.chars().next()
}

fn prev_char_boundary(text: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx.saturating_sub(1);
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(text: &str, idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    let mut i = idx + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i.min(text.len())
}

fn advance_by_chars(text: &str, mut idx: usize, count: usize) -> usize {
    for _ in 0..count {
        let next = next_char_boundary(text, idx);
        if next == idx {
            break;
        }
        idx = next;
    }
    idx
}

fn advance_for_append(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        text.len()
    } else {
        next_char_boundary(text, cursor)
    }
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

fn line_end_exclusive(text: &str, cursor: usize) -> usize {
    text[cursor.min(text.len())..]
        .find('\n')
        .map(|idx| cursor.min(text.len()) + idx)
        .unwrap_or(text.len())
}

fn line_end_cursor(text: &str, cursor: usize) -> usize {
    let end = line_end_exclusive(text, cursor);
    if end == line_start(text, cursor) {
        end
    } else {
        prev_char_boundary(text, end)
    }
}

fn first_non_blank(text: &str, cursor: usize) -> usize {
    let start = line_start(text, cursor);
    let end = line_end_exclusive(text, cursor);
    let line = &text[start..end];
    line.char_indices()
        .find_map(|(idx, ch)| {
            if ch.is_whitespace() {
                None
            } else {
                Some(start + idx)
            }
        })
        .unwrap_or(start)
}

fn move_up_line(text: &str, cursor: usize) -> usize {
    let start = line_start(text, cursor);
    if start == 0 {
        return 0;
    }
    let prev_end = start - 1;
    let prev_start = line_start(text, prev_end);
    let col = text[start..cursor.min(text.len())].chars().count();
    let prev_line = &text[prev_start..prev_end];
    prev_start + byte_index_for_char_col(prev_line, col.min(prev_line.chars().count()))
}

fn move_down_line(text: &str, cursor: usize) -> usize {
    let end = line_end_exclusive(text, cursor);
    if end >= text.len() {
        return text.len();
    }
    let next_start = end + 1;
    let next_end = line_end_exclusive(text, next_start);
    let cur_start = line_start(text, cursor);
    let col = text[cur_start..cursor.min(text.len())].chars().count();
    let next_line = &text[next_start..next_end];
    next_start + byte_index_for_char_col(next_line, col.min(next_line.chars().count()))
}

fn byte_index_for_char_col(text: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    text.char_indices()
        .nth(col)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn is_vim_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_vim_punctuation(ch: char) -> bool {
    !ch.is_whitespace() && !is_vim_word_char(ch)
}

fn next_vim_word(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut pos = cursor;
    let current = match char_at(text, pos) {
        Some(ch) => ch,
        None => return pos,
    };

    if is_vim_word_char(current) {
        while pos < text.len() {
            let Some(ch) = char_at(text, pos) else {
                break;
            };
            if !is_vim_word_char(ch) {
                break;
            }
            pos = next_char_boundary(text, pos);
        }
    } else if is_vim_punctuation(current) {
        while pos < text.len() {
            let Some(ch) = char_at(text, pos) else {
                break;
            };
            if !is_vim_punctuation(ch) {
                break;
            }
            pos = next_char_boundary(text, pos);
        }
    }

    while pos < text.len() {
        let Some(ch) = char_at(text, pos) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        pos = next_char_boundary(text, pos);
    }
    pos
}

fn prev_vim_word(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut pos = prev_char_boundary(text, cursor);
    while pos > 0 {
        let Some(ch) = char_at(text, pos) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        pos = prev_char_boundary(text, pos);
    }

    let Some(ch) = char_at(text, pos) else {
        return pos;
    };
    if is_vim_word_char(ch) {
        while pos > 0 {
            let prev = prev_char_boundary(text, pos);
            let Some(prev_ch) = char_at(text, prev) else {
                break;
            };
            if !is_vim_word_char(prev_ch) {
                break;
            }
            pos = prev;
        }
    } else if is_vim_punctuation(ch) {
        while pos > 0 {
            let prev = prev_char_boundary(text, pos);
            let Some(prev_ch) = char_at(text, prev) else {
                break;
            };
            if !is_vim_punctuation(prev_ch) {
                break;
            }
            pos = prev;
        }
    }
    pos
}

fn end_of_vim_word(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut pos = next_char_boundary(text, cursor);
    while pos < text.len() {
        let Some(ch) = char_at(text, pos) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        pos = next_char_boundary(text, pos);
    }
    if pos >= text.len() {
        return text.len();
    }
    let Some(ch) = char_at(text, pos) else {
        return pos;
    };
    if is_vim_word_char(ch) {
        while pos < text.len() {
            let next = next_char_boundary(text, pos);
            if next >= text.len() {
                break;
            }
            let Some(next_ch) = char_at(text, next) else {
                break;
            };
            if !is_vim_word_char(next_ch) {
                break;
            }
            pos = next;
        }
    } else if is_vim_punctuation(ch) {
        while pos < text.len() {
            let next = next_char_boundary(text, pos);
            if next >= text.len() {
                break;
            }
            let Some(next_ch) = char_at(text, next) else {
                break;
            };
            if !is_vim_punctuation(next_ch) {
                break;
            }
            pos = next;
        }
    }
    pos
}

fn start_of_last_line(text: &str) -> usize {
    text.rfind('\n').map(|idx| idx + 1).unwrap_or(0)
}

fn line_start_for_number(text: &str, line_number: u32) -> usize {
    let target = line_number.saturating_sub(1) as usize;
    let mut offset = 0usize;
    let mut line = 0usize;
    for segment in split_lines_preserve_empty(text) {
        if line == target {
            return offset;
        }
        offset += segment.len() + 1;
        line += 1;
    }
    start_of_last_line(text)
}

fn line_number_at(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
}

fn split_lines_preserve_empty(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.split('\n').map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_enters_normal_and_insert_returns() {
        let mut vim = VimMachine::new();
        let mut buffer = InputBuffer::new();
        buffer.set_text("hello".to_string());
        vim.on_insert_text("hello");
        vim.enter_normal_mode(&mut buffer);
        assert_eq!(vim.mode(), VimMode::Normal);
        assert_eq!(buffer.cursor(), 4);

        vim.handle_normal_key(VimKey::Char('i'), &mut buffer);
        assert_eq!(vim.mode(), VimMode::Insert);
    }

    #[test]
    fn normal_word_delete_and_paste_work() {
        let mut vim = VimMachine::new();
        let mut buffer = InputBuffer::new();
        buffer.set_text_with_cursor("hello world".to_string(), 0);
        vim.enter_normal_mode(&mut buffer);
        vim.handle_normal_key(VimKey::Char('d'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('w'), &mut buffer);
        assert_eq!(buffer.as_str(), "world");
        vim.handle_normal_key(VimKey::Char('P'), &mut buffer);
        assert_eq!(buffer.as_str(), "hello world");
    }

    #[test]
    fn change_inside_word_records_dot_repeat() {
        let mut vim = VimMachine::new();
        let mut buffer = InputBuffer::new();
        buffer.set_text_with_cursor("alpha beta".to_string(), 0);
        vim.enter_normal_mode(&mut buffer);

        vim.handle_normal_key(VimKey::Char('c'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('i'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('w'), &mut buffer);
        assert_eq!(vim.mode(), VimMode::Insert);
        buffer.insert_str("ONE");
        vim.on_insert_text("ONE");
        vim.enter_normal_mode(&mut buffer);
        assert_eq!(buffer.as_str(), "ONE beta");

        vim.handle_normal_key(VimKey::Char('w'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('.'), &mut buffer);
        assert_eq!(buffer.as_str(), "ONE ONE");
    }

    #[test]
    fn count_prefix_repeats_motion_and_delete() {
        let mut vim = VimMachine::new();
        let mut buffer = InputBuffer::new();
        buffer.set_text_with_cursor("one two three four".to_string(), 0);
        vim.enter_normal_mode(&mut buffer);

        vim.handle_normal_key(VimKey::Char('2'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('w'), &mut buffer);
        assert_eq!(&buffer.as_str()[buffer.cursor()..], "three four");

        vim.handle_normal_key(VimKey::Char('2'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('d'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('w'), &mut buffer);
        assert_eq!(buffer.as_str(), "one two ");
    }

    #[test]
    fn text_objects_support_quotes() {
        let mut vim = VimMachine::new();
        let mut buffer = InputBuffer::new();
        buffer.set_text("say \"hello world\" now".to_string());
        buffer.set_cursor(6);
        vim.enter_normal_mode(&mut buffer);
        vim.handle_normal_key(VimKey::Char('d'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('i'), &mut buffer);
        vim.handle_normal_key(VimKey::Char('"'), &mut buffer);
        assert_eq!(buffer.as_str(), "say \"\" now");
    }
}
