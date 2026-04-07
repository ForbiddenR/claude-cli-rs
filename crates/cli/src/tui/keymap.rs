use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyContext {
    /// Always-active bindings.
    Global,
    /// Default prompt input editing.
    Input,
    /// Slash command typeahead is visible.
    CommandTypeahead,
    /// Vim NORMAL mode (input is not directly editable).
    VimNormal,
    /// Vim INSERT mode (input is editable).
    VimInsert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Quit,
    ClearChat,
    CompactChat,
    ShowHelp,
    ShowCost,
    ShowModelPicker,
    ResumeSession,
    SearchTranscript,

    TypeaheadNext,
    TypeaheadPrev,
    TypeaheadAccept,
    TypeaheadExecute,
    TypeaheadDismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPress {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyPress {
    pub fn from_event(key: KeyEvent) -> Self {
        let mut code = key.code;
        let mut modifiers = key.modifiers;

        // Normalize: Shift shouldn't create distinct bindings for ASCII letters.
        if let KeyCode::Char(ch) = code {
            if ch.is_ascii_alphabetic() {
                code = KeyCode::Char(ch.to_ascii_lowercase());
                modifiers.remove(KeyModifiers::SHIFT);
            }
        }

        Self { code, modifiers }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySequence {
    steps: Vec<KeyPress>,
}

impl KeySequence {
    pub fn steps(&self) -> &[KeyPress] {
        &self.steps
    }

    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            anyhow::bail!("empty key sequence");
        }

        let mut steps: Vec<KeyPress> = Vec::new();
        for raw_step in spec.split_whitespace() {
            let (code, modifiers) = parse_step(raw_step)?;
            steps.push(KeyPress { code, modifiers });
        }

        Ok(Self { steps })
    }
}

#[derive(Debug, Clone)]
struct Binding {
    contexts: Vec<KeyContext>,
    keys: KeySequence,
    action: KeyAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    NoMatch,
    PendingChord,
    Matched(KeyAction),
}

pub struct KeybindingResolver {
    bindings: Vec<Binding>,
    pending: Vec<KeyPress>,
    pending_since: Option<Instant>,
    chord_timeout: Duration,
}

impl KeybindingResolver {
    pub fn new(chord_timeout: Duration) -> Self {
        Self {
            bindings: Vec::new(),
            pending: Vec::new(),
            pending_since: None,
            chord_timeout,
        }
    }

    pub fn add_binding(&mut self, contexts: &[KeyContext], keys: KeySequence, action: KeyAction) {
        self.bindings.push(Binding {
            contexts: contexts.to_vec(),
            keys,
            action,
        });
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_since = None;
    }

    pub fn resolve(&mut self, ctx: KeyContext, key: KeyEvent) -> ResolveOutcome {
        let press = KeyPress::from_event(key);
        let now = Instant::now();

        if let Some(since) = self.pending_since {
            if now.duration_since(since) > self.chord_timeout {
                self.clear_pending();
            }
        }

        if !self.pending.is_empty() {
            let mut seq = self.pending.clone();
            seq.push(press);
            if let Some(action) = self.find_exact(ctx, &seq) {
                self.clear_pending();
                return ResolveOutcome::Matched(action);
            }
            if self.any_prefix(ctx, &seq) {
                self.pending = seq;
                // Keep original start time so the whole chord has one timeout window.
                return ResolveOutcome::PendingChord;
            }

            // Not a match; cancel the pending chord and fall through to treating this key
            // as a potential new chord prefix.
            self.clear_pending();
        }

        if let Some(action) = self.find_exact(ctx, &[press]) {
            return ResolveOutcome::Matched(action);
        }
        if self.any_prefix(ctx, &[press]) {
            self.pending = vec![press];
            self.pending_since = Some(now);
            return ResolveOutcome::PendingChord;
        }

        ResolveOutcome::NoMatch
    }

    pub fn apply_user_overrides(&mut self, overrides: &HashMap<String, String>) -> Vec<String> {
        let mut warnings: Vec<String> = Vec::new();

        for (raw_action, raw_keys) in overrides {
            let Some(action) = parse_action_name(raw_action) else {
                warnings.push(format!("unknown keybinding action: {raw_action}"));
                continue;
            };

            let seq = match KeySequence::parse(raw_keys) {
                Ok(s) => s,
                Err(err) => {
                    warnings.push(format!(
                        "invalid keybinding for {raw_action}={raw_keys}: {err}"
                    ));
                    continue;
                }
            };

            let contexts = default_contexts_for_action(action);
            self.bindings
                .retain(|b| !(b.action == action && b.contexts == contexts));
            self.add_binding(&contexts, seq, action);
        }

        warnings
    }

    fn binding_active(binding: &Binding, ctx: KeyContext) -> bool {
        binding.contexts.contains(&KeyContext::Global) || binding.contexts.contains(&ctx)
    }

    fn find_exact(&self, ctx: KeyContext, seq: &[KeyPress]) -> Option<KeyAction> {
        for b in &self.bindings {
            if !Self::binding_active(b, ctx) {
                continue;
            }
            if b.keys.steps() == seq {
                return Some(b.action);
            }
        }
        None
    }

    fn any_prefix(&self, ctx: KeyContext, seq: &[KeyPress]) -> bool {
        for b in &self.bindings {
            if !Self::binding_active(b, ctx) {
                continue;
            }
            let steps = b.keys.steps();
            if steps.len() <= seq.len() {
                continue;
            }
            if steps[..seq.len()] == *seq {
                return true;
            }
        }
        false
    }
}

fn parse_action_name(raw: &str) -> Option<KeyAction> {
    let k = raw.trim().to_ascii_lowercase();
    match k.as_str() {
        "quit" | "exit" => Some(KeyAction::Quit),
        "clear" | "clear_chat" | "clearchat" => Some(KeyAction::ClearChat),
        "compact" | "compact_chat" | "compactchat" => Some(KeyAction::CompactChat),
        "help" => Some(KeyAction::ShowHelp),
        "cost" => Some(KeyAction::ShowCost),
        "model" | "model_picker" | "show_model_picker" => Some(KeyAction::ShowModelPicker),
        "resume" | "resume_session" | "resumesession" => Some(KeyAction::ResumeSession),
        "search" | "find" | "search_transcript" | "searchtranscript" => {
            Some(KeyAction::SearchTranscript)
        }
        "typeahead_next" | "command_next" | "typeahead.next" => Some(KeyAction::TypeaheadNext),
        "typeahead_prev" | "command_prev" | "typeahead.prev" => Some(KeyAction::TypeaheadPrev),
        "typeahead_accept" | "command_accept" | "typeahead.accept" => {
            Some(KeyAction::TypeaheadAccept)
        }
        "typeahead_execute" | "command_execute" | "typeahead.execute" => {
            Some(KeyAction::TypeaheadExecute)
        }
        "typeahead_dismiss" | "command_dismiss" | "typeahead.dismiss" => {
            Some(KeyAction::TypeaheadDismiss)
        }
        _ => None,
    }
}

fn default_contexts_for_action(action: KeyAction) -> Vec<KeyContext> {
    match action {
        KeyAction::TypeaheadNext
        | KeyAction::TypeaheadPrev
        | KeyAction::TypeaheadAccept
        | KeyAction::TypeaheadExecute
        | KeyAction::TypeaheadDismiss => vec![KeyContext::CommandTypeahead],
        _ => vec![KeyContext::Global],
    }
}

fn parse_step(raw_step: &str) -> anyhow::Result<(KeyCode, KeyModifiers)> {
    let step = raw_step.trim();
    if step.is_empty() {
        anyhow::bail!("empty chord step");
    }

    let mut modifiers = KeyModifiers::empty();
    let parts: Vec<&str> = step.split('+').collect();
    if parts.is_empty() {
        anyhow::bail!("invalid chord step: {raw_step}");
    }

    let key_part = *parts.last().unwrap_or(&"");
    for m in &parts[..parts.len().saturating_sub(1)] {
        match m.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            "cmd" | "meta" | "super" => modifiers.insert(KeyModifiers::SUPER),
            other if other.is_empty() => {}
            other => anyhow::bail!("unknown modifier: {other}"),
        }
    }

    let code = parse_key_code(key_part)?;

    // Apply the same normalization as runtime events.
    let kp = KeyPress { code, modifiers };
    let normalized = KeyPress::from_event(KeyEvent::new(kp.code, kp.modifiers));
    Ok((normalized.code, normalized.modifiers))
}

fn parse_key_code(raw: &str) -> anyhow::Result<KeyCode> {
    let k = raw.trim();
    if k.is_empty() {
        anyhow::bail!("missing key name");
    }

    let lower = k.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        _ => {
            if k.chars().count() == 1 {
                let ch = k.chars().next().unwrap_or(' ');
                KeyCode::Char(ch)
            } else {
                anyhow::bail!("unknown key: {raw}");
            }
        }
    };
    Ok(code)
}
