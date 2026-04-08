use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
}

impl ThemeName {
    pub const ALL: &'static [ThemeName] = &[ThemeName::Dark, ThemeName::Light];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim().to_ascii_lowercase();
        match s.as_str() {
            "dark" | "d" => Some(ThemeName::Dark),
            "light" | "l" => Some(ThemeName::Light),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: ThemeName,

    // Chrome
    pub header: Style,
    pub status: Style,
    pub border: Style,
    pub selection: Style,

    // Roles
    pub role_user: Style,
    pub role_assistant: Style,
    pub role_tool: Style,
    pub role_system: Style,

    // Toasts
    pub toast_info: Style,
    pub toast_warn: Style,
    pub toast_error: Style,
}

impl Theme {
    pub fn new(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Theme {
                name,
                header: Style::default()
                    .fg(Color::Rgb(235, 243, 250))
                    .bg(Color::Rgb(30, 32, 48))
                    .add_modifier(Modifier::BOLD),
                status: Style::default()
                    .fg(Color::Rgb(166, 173, 200))
                    .bg(Color::Rgb(24, 24, 37))
                    .add_modifier(Modifier::DIM),
                border: Style::default().fg(Color::Rgb(108, 112, 134)),
                selection: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(137, 180, 250)),
                role_user: Style::default()
                    .fg(Color::Rgb(166, 227, 161))
                    .add_modifier(Modifier::BOLD),
                role_assistant: Style::default()
                    .fg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD),
                role_tool: Style::default()
                    .fg(Color::Rgb(249, 226, 175))
                    .add_modifier(Modifier::BOLD),
                role_system: Style::default()
                    .fg(Color::Rgb(147, 153, 178))
                    .add_modifier(Modifier::DIM),
                toast_info: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD),
                toast_warn: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(250, 179, 135))
                    .add_modifier(Modifier::BOLD),
                toast_error: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(243, 139, 168))
                    .add_modifier(Modifier::BOLD),
            },
            ThemeName::Light => Theme {
                name,
                header: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(245, 245, 245))
                    .add_modifier(Modifier::BOLD),
                status: Style::default()
                    .fg(Color::Rgb(70, 70, 70))
                    .bg(Color::Rgb(250, 250, 250))
                    .add_modifier(Modifier::DIM),
                border: Style::default().fg(Color::Rgb(120, 120, 120)),
                selection: Style::default().fg(Color::White).bg(Color::Blue),
                role_user: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                role_assistant: Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                role_tool: Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                role_system: Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                toast_info: Style::default()
                    .fg(Color::White)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                toast_warn: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                toast_error: Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            },
        }
    }

    pub fn available_names() -> Vec<&'static str> {
        ThemeName::ALL.iter().map(|t| t.as_str()).collect()
    }
}

